// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converts GitHub Actions workflows into a `.hatchrun/config.yml`.
//!
//! Each workflow file becomes one entry of `workflows:`, keyed by the file
//! stem, and each of its jobs becomes `jobs.<stem>-<job>`. `actions/checkout`
//! stays a `uses:` step at the ref it was written with. Keys hatchrun has
//! no place for are dropped with a warning. Docker steps and runner labels
//! that name no template are errors; the job is dropped and the rest of
//! the files still convert.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

use crate::config::{
    Config, Job, MAX_SOURCE, Matrix, Parameter, ParameterKind, Require, RunStep, Step, UsesStep,
    Workflow, WorkflowJob, too_big, yaml_options,
};
use crate::diagnostic::Diagnostic;
use crate::environment::valid_environment_name;
use crate::expr::{self, ExprError};
use crate::permissions::{Level, Permissions, SCOPES};
use crate::plan;
use crate::templates::{template, template_labels, vendor_template};
use crate::trigger::{
    Dispatch, Filter, PullRequestTrigger, PushTrigger, RELEASE_TYPES, ReleaseTrigger, Schedule,
    Triggers, WORKFLOW_RUN_TYPES, WorkflowRunTrigger,
};

/// The result of a conversion: the config and what was dropped on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// The converted config, without the jobs and files that had errors.
    pub config: Config,
    /// Keys that were dropped or changed, naming the file and job.
    pub warnings: Vec<Diagnostic>,
    /// Problems that dropped a job, an event or a whole file. The config
    /// is still usable, but it does not do everything the files did.
    pub errors: Vec<Diagnostic>,
}

/// Converts GitHub workflow files into one config.
///
/// Each pair is a file name and its contents. The workflow is keyed by the
/// file stem, so `.github/workflows/ci.yml` becomes workflow `ci`.
///
/// A job with a problem (a `runs-on` that names no runner template, a
/// `docker://` step, a `needs` that names no job, a job key another file
/// already uses, a read of the `strategy` or `jobs` context) is dropped,
/// like a reusable workflow call. Every job that needs a dropped job,
/// directly or through others, is dropped too, with a warning naming the
/// chain of jobs it needed. A file over [`MAX_SOURCE`](crate::MAX_SOURCE)
/// bytes, one that does not parse, has no name before its extension or
/// repeats another file's name is skipped. Each problem is in
/// [`Conversion::errors`]. A workflow left with no event hatchrun runs
/// on, such as one that runs only on `issues`, is dropped with its jobs
/// and a warning.
///
/// A job with `uses: ./.github/workflows/x.yml` (or a `.forgejo` or
/// `.gitea` path) whose file is among the files is replaced by the jobs
/// of that file, `<stem>-<job>-<called job>`, with the file's
/// `on.workflow_call.inputs` as parameters; calls nest four levels deep.
/// A file whose only event is `workflow_call` and that some file calls is
/// not reported as dropped. A call of a file in another repository, or
/// one that was not given, is dropped with a warning.
///
/// `on.workflow_run` names workflows as GitHub does, by their `name:`,
/// or by their file name when they have none. Each name becomes the stem of
/// the file that has it; a name no converted file has is dropped with a
/// warning, and so is the event when none is left.
///
/// # Errors
///
/// Returns the problems when there were some and nothing converted.
pub fn convert(files: &[(String, String)]) -> Result<Conversion, Vec<Diagnostic>> {
    let mut cx = Cx::default();
    let mut stems: BTreeMap<String, &str> = BTreeMap::new();
    let mut parsed = Vec::new();
    for (file, source) in files {
        let stem = stem(file);
        if stem.is_empty() {
            cx.errors.push(Diagnostic::new(format!(
                "`{file}` has no name before its extension; rename it"
            )));
            continue;
        }
        if let Some(other) = stems.insert(stem.to_owned(), file) {
            cx.errors.push(Diagnostic::new(format!(
                "`{other}` and `{file}` both become workflow `{stem}`; rename one of them"
            )));
            continue;
        }
        if let Some(mut diag) = too_big(&format!("`{file}`"), source) {
            diag.file = Some(file.clone());
            cx.errors.push(diag);
            continue;
        }
        match serde_saphyr::from_str_with_options::<GhWorkflow>(source, yaml_options()) {
            Ok(gh) => {
                let name = gh.name.as_ref().and_then(scalar);
                let name = name.unwrap_or_else(|| file.clone());
                cx.names.entry(name).or_default().push(stem.to_owned());
                parsed.push((file, stem, gh));
            }
            Err(err) => {
                let mut diag = Diagnostic::from(&err);
                diag.file = Some(file.clone());
                cx.errors.push(diag);
            }
        }
    }
    for (file, _, gh) in &parsed {
        if calls_workflow(&gh.on) {
            cx.library.insert((*file).clone(), gh.clone());
        }
    }
    for (_, _, gh) in &parsed {
        for (_, job) in &gh.jobs.0 {
            let called = job.get("uses").and_then(Value::as_str);
            if let Some(file) = called.and_then(|uses| cx.callee(uses)) {
                cx.called.insert(file);
            }
        }
    }
    for (file, stem, gh) in parsed {
        cx.workflow(file, stem, gh);
    }
    cx.unreachable_runs(&stems);
    if !cx.errors.is_empty() && cx.config.workflows.is_empty() {
        return Err(cx.errors);
    }
    // validate refuses these, so they are errors here, not a file that
    // only fails later.
    for (name, wf) in &cx.config.workflows {
        if plan::over_job_limit(&cx.config, wf) {
            cx.errors.push(Diagnostic::new(plan::too_many_jobs(name)));
        }
    }
    let written = to_yaml(&cx.config).len();
    if written > MAX_SOURCE {
        cx.errors.push(Diagnostic::new(format!(
            "the converted config is {written} bytes; the limit is {MAX_SOURCE}"
        )));
    }
    Ok(Conversion {
        config: cx.config,
        warnings: cx.warnings,
        errors: cx.errors,
    })
}

/// Writes a config as YAML that [`parse`](crate::parse) reads back.
///
/// # Panics
///
/// Never in practice: every part of a config serializes.
#[must_use]
pub fn to_yaml(config: &Config) -> String {
    serde_saphyr::to_string(config).expect("a config always serializes")
}

/// The file name without its directories and its `.yml` or `.yaml`, in
/// any case.
fn stem(file: &str) -> &str {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    for ext in [".yml", ".yaml"] {
        let Some(cut) = base.len().checked_sub(ext.len()) else {
            continue;
        };
        if base.is_char_boundary(cut) && base[cut..].eq_ignore_ascii_case(ext) {
            return &base[..cut];
        }
    }
    base
}

/// The keys of a GitHub job.
const JOB_KEYS: &[&str] = &[
    "name",
    "permissions",
    "needs",
    "if",
    "runs-on",
    "environment",
    "concurrency",
    "outputs",
    "env",
    "defaults",
    "steps",
    "timeout-minutes",
    "strategy",
    "continue-on-error",
    "container",
    "services",
    "uses",
    "with",
    "secrets",
];

/// The keys of a GitHub step.
const STEP_KEYS: &[&str] = &[
    "id",
    "if",
    "name",
    "uses",
    "run",
    "working-directory",
    "shell",
    "with",
    "env",
    "continue-on-error",
    "timeout-minutes",
];

/// Labels GitHub retires and the template each becomes.
const OLD_UBUNTU: &[(&str, &str)] = &[
    ("ubuntu-24.04", "ubuntu-latest"),
    ("ubuntu-22.04", "ubuntu-latest"),
    ("ubuntu-24.04-arm", "ubuntu-26.04-arm"),
    ("ubuntu-22.04-arm", "ubuntu-26.04-arm"),
];

/// GitHub contexts hatchrun does not provide.
const UNSUPPORTED_CONTEXTS: &[&str] = &["strategy", "jobs"];

/// The events hatchrun can start a workflow on.
const EVENTS: &str = "push, pull_request, schedule, workflow_dispatch, workflow_run and release";

/// Events that only GitHub can deliver, as they act on GitHub's own
/// issues and pull requests with the base repository's secrets.
const GITHUB_ONLY: &[&str] = &["pull_request_target", "issues", "issue_comment"];

/// The part of a GitHub workflow file the converter reads.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
struct GhWorkflow {
    #[serde(default)]
    name: Option<Value>,
    #[serde(default)]
    run_name: Option<Value>,
    on: Value,
    #[serde(default)]
    permissions: Option<Value>,
    #[serde(default)]
    env: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    defaults: Option<Value>,
    #[serde(default)]
    concurrency: Option<Value>,
    #[serde(default)]
    jobs: Ordered<Value>,
    #[serde(flatten)]
    unknown: BTreeMap<String, Value>,
}

/// A GitHub job.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct GhJob {
    #[serde(rename = "name")]
    _name: Option<Value>,
    permissions: Option<Value>,
    needs: Option<Value>,
    #[serde(rename = "if")]
    if_: Option<Value>,
    runs_on: Option<Value>,
    environment: Option<Value>,
    concurrency: Option<Value>,
    outputs: Option<BTreeMap<String, Value>>,
    env: Option<BTreeMap<String, Value>>,
    defaults: Option<Value>,
    steps: Option<Vec<Value>>,
    timeout_minutes: Option<Value>,
    strategy: Option<BTreeMap<String, Value>>,
    continue_on_error: Option<Value>,
    container: Option<Value>,
    services: Option<Value>,
    uses: Option<Value>,
    #[serde(rename = "with")]
    with_: Option<Value>,
    secrets: Option<Value>,
}

/// A GitHub step.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct GhStep {
    #[serde(deserialize_with = "text")]
    id: Option<String>,
    #[serde(rename = "if")]
    if_: Option<Value>,
    #[serde(deserialize_with = "text")]
    name: Option<String>,
    #[serde(deserialize_with = "text")]
    uses: Option<String>,
    #[serde(deserialize_with = "text")]
    run: Option<String>,
    #[serde(deserialize_with = "text")]
    working_directory: Option<String>,
    #[serde(deserialize_with = "text")]
    shell: Option<String>,
    with: Option<BTreeMap<String, Value>>,
    env: Option<BTreeMap<String, Value>>,
    continue_on_error: Option<Value>,
    timeout_minutes: Option<Value>,
}

/// Reads a string, number or boolean as text.
fn text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) => scalar(&value)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("expected a string, found `{value}`"))),
    }
}

/// One input of `on.workflow_dispatch.inputs`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GhInput {
    description: Option<String>,
    default: Option<Value>,
    #[serde(rename = "type")]
    kind: Option<String>,
    options: Option<Vec<Value>>,
}

/// A GitHub `strategy.matrix` as read.
#[derive(Default)]
struct GhMatrix {
    /// Each key and its values.
    values: BTreeMap<String, Vec<Value>>,
    /// Partial combinations left out.
    exclude: Vec<BTreeMap<String, Value>>,
    /// Keys added to combinations, or combinations of their own.
    include: Vec<BTreeMap<String, Value>>,
}

/// What a converted job runs on.
enum RunsOn {
    /// One template label.
    Label(String),
    /// The template the matrix key, passed as a parameter, names in each
    /// leg.
    Matrix(String),
}

impl RunsOn {
    /// The job's `runs-on` and the parameter it reads, if any.
    fn into_job(self) -> (String, BTreeMap<String, Parameter>) {
        match self {
            Self::Label(label) => (label, BTreeMap::new()),
            Self::Matrix(name) => {
                let read = format!("${{{{ parameters.{name} }}}}");
                let param = Parameter {
                    kind: ParameterKind::String,
                    default: None,
                    description: None,
                };
                (read, BTreeMap::from([(name, param)]))
            }
        }
    }
}

/// `on.workflow_run`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct GhWorkflowRun {
    workflows: Vec<String>,
    #[serde(default)]
    types: Option<Value>,
    #[serde(default)]
    branches: Option<Filter>,
    #[serde(default)]
    branches_ignore: Option<Filter>,
}

/// `on.release`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GhRelease {
    #[serde(default)]
    types: Option<Value>,
}

/// A map that keeps the order its keys were written in.
#[derive(Clone)]
struct Ordered<T>(Vec<(String, T)>);

impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Ordered<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for OrderedVisitor<T> {
            type Value = Ordered<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Ordered::default())
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }

        deserializer.deserialize_map(OrderedVisitor(PhantomData))
    }
}

/// How deep reusable workflows may call each other.
const MAX_CALL_DEPTH: usize = 4;

/// The directories workflow files, and so local calls, live in.
const WORKFLOW_DIRS: &[&str] = &[
    ".github/workflows/",
    ".forgejo/workflows/",
    ".gitea/workflows/",
];

/// Where a set of jobs is converted: a workflow file, or a file that a
/// job of another one calls.
struct Scope {
    /// The file, quoted, for messages.
    label: String,
    /// What the messages add about the call that brought the file in.
    from: String,
    /// The stem of the file at the root of the calls.
    stem: String,
    /// The ids of the calls that led here, joined with `-`; empty for a
    /// file that no call brought in.
    prefix: String,
    /// How many calls led here.
    depth: usize,
    /// The files of the calls that led here, the root's first.
    stack: Vec<String>,
    /// The values of the inputs the file was called with, defaults
    /// included.
    inputs: Option<BTreeMap<String, Value>>,
}

impl Scope {
    /// Where a job of the file is named in messages.
    fn at(&self, id: &str) -> String {
        format!("{} job `{id}`{}", self.label, self.from)
    }

    /// The id of a job in the workflow: its own in the root file, else
    /// the stem and the calls in front of it, so that ids of files
    /// called twice differ.
    fn full(&self, id: &str) -> String {
        if self.prefix.is_empty() {
            id.to_owned()
        } else {
            format!("{}-{}-{id}", self.stem, self.prefix)
        }
    }

    /// The key of a job in `jobs:`.
    fn key(&self, id: &str) -> String {
        if self.prefix.is_empty() {
            format!("{}-{id}", self.stem)
        } else {
            self.full(id)
        }
    }
}

/// The jobs converted from one file.
#[derive(Default)]
struct Converted {
    /// Each converted job's id and its workflow entries.
    entries: Vec<(String, Vec<WorkflowJob>)>,
    /// Jobs left out, with what happened to each, worded to follow
    /// "which".
    dropped: BTreeMap<String, String>,
    /// A job had an error.
    failed: bool,
    /// The keys of the jobs the file has itself, not through calls.
    plain: BTreeSet<String>,
    /// The ids those jobs have in the file.
    local: BTreeSet<String>,
    /// The calls, by the id of the job that made each.
    calls: BTreeMap<String, Expansion>,
}

/// What a job that called a workflow stands for.
struct Expansion {
    /// The ids of the jobs that jobs needing the call require.
    requires: Vec<String>,
    /// What reads of each output of the call become, as an expression;
    /// `None` for an output that cannot be mapped.
    outputs: BTreeMap<String, Option<String>>,
}

/// A call that was inlined.
struct Called {
    entries: Vec<(String, Vec<WorkflowJob>)>,
    expansion: Expansion,
}

/// What became of a job that calls a reusable workflow.
enum Call {
    Done(Called),
    /// Left out with a warning.
    Dropped,
    /// Left out with an error.
    Failed,
}

/// What the conversion has built so far.
#[derive(Default)]
struct Cx {
    config: Config,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
    /// The stems of the files that have each GitHub workflow name.
    names: BTreeMap<String, Vec<String>>,
    /// The files with `on.workflow_call`, which jobs can call.
    library: BTreeMap<String, GhWorkflow>,
    /// The files some file calls with a local `uses`.
    called: BTreeSet<String>,
}

impl Cx {
    fn warn(&mut self, message: String) {
        self.warnings.push(Diagnostic::new(message));
    }

    fn error(&mut self, message: String) {
        self.errors.push(Diagnostic::new(message));
    }

    /// Converts one workflow file.
    fn workflow(&mut self, file: &str, stem: &str, gh: GhWorkflow) {
        for (key, set) in [
            ("run-name", gh.run_name.is_some()),
            ("concurrency", gh.concurrency.is_some()),
        ] {
            if set {
                self.warn(format!(
                    "`{file}`: `{key}` is not supported and was dropped"
                ));
            }
        }
        for key in gh.unknown.keys() {
            self.warn(format!("`{file}`: unknown key `{key}` was dropped"));
        }
        let Some(on) = self.kept_triggers(file, stem, &gh.on) else {
            return;
        };
        let permissions = self.permissions(&format!("`{file}`"), gh.permissions.as_ref(), false);
        let shared_env = self.env(&format!("`{file}`"), "env", gh.env.as_ref());
        let shared_defaults = self.defaults(&format!("`{file}`"), gh.defaults.as_ref());

        let scope = Scope {
            label: format!("`{file}`"),
            from: String::new(),
            stem: stem.to_owned(),
            prefix: String::new(),
            depth: 0,
            stack: vec![file.to_owned()],
            inputs: None,
        };
        let Converted {
            entries, failed, ..
        } = self.jobs(&scope, gh.jobs.0, &shared_env, &shared_defaults);
        // A file whose every job failed converts to nothing.
        if entries.is_empty() && failed {
            return;
        }
        let jobs = flatten(entries);
        self.config.workflows.insert(
            stem.to_owned(),
            Workflow {
                on,
                permissions,
                jobs,
            },
        );
    }

    /// Converts the jobs of a workflow file, or of a file it calls.
    /// A job that calls a local reusable workflow becomes the jobs of the
    /// called file; see [`Cx::call`]. A job that needs a dropped job
    /// would run without it, so it is dropped too, and so on down the
    /// chain. The entries of one job share their `requires`.
    fn jobs(
        &mut self,
        scope: &Scope,
        jobs: Vec<(String, Value)>,
        shared_env: &BTreeMap<String, String>,
        shared_defaults: &RunDefaults,
    ) -> Converted {
        let ids: BTreeSet<String> = jobs.iter().map(|(id, _)| id.clone()).collect();
        let mut level = Converted::default();
        for (id, job) in jobs {
            let at = scope.at(&id);
            let fid = scope.full(&id);
            let Some(job) = self.gh_job(&at, &job) else {
                level.dropped.insert(fid, FAILED.to_owned());
                level.failed = true;
                continue;
            };
            if job.uses.is_some() {
                match self.call(scope, &at, &id, &ids, &job) {
                    Call::Done(done) => {
                        level.entries.extend(done.entries);
                        level.calls.insert(id, done.expansion);
                    }
                    Call::Dropped => {
                        level.dropped.insert(fid, REUSABLE.to_owned());
                    }
                    Call::Failed => {
                        level.dropped.insert(fid, FAILED.to_owned());
                        level.failed = true;
                    }
                }
                continue;
            }
            let key = scope.key(&id);
            if self.config.jobs.contains_key(&key) {
                self.error(format!(
                    "{at} becomes job `{key}`, which another workflow already uses; rename one of them"
                ));
                level.dropped.insert(fid, FAILED.to_owned());
                level.failed = true;
                continue;
            }
            let Some((mut converted, mut expanded)) = self.job(&at, &id, &key, &ids, job) else {
                level.dropped.insert(fid, FAILED.to_owned());
                level.failed = true;
                continue;
            };
            shared_defaults.apply(&mut converted.steps);
            for (name, value) in shared_env {
                converted
                    .env
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
            // validate rejects these, so the job could not run.
            let (contexts, mut bad) = expression_problems(&converted);
            if let Some(inputs) = &scope.inputs {
                bad.extend(bind_literals(&mut converted, inputs));
            }
            if !contexts.is_empty() || !bad.is_empty() {
                for context in contexts {
                    self.error(format!(
                        "{at} reads the `{context}` context, which hatchrun does not provide; the job was dropped"
                    ));
                }
                for problem in bad {
                    self.error(format!("{at} {problem}; the job was dropped"));
                }
                level.dropped.insert(fid, FAILED.to_owned());
                level.failed = true;
                continue;
            }
            for entry in &mut expanded {
                entry.name = entry.name.take().map(|n| scope.full(&n));
                for require in &mut entry.requires {
                    require.job = scope.full(&require.job);
                }
            }
            self.config.jobs.insert(key.clone(), converted);
            level.plain.insert(key);
            level.local.insert(id);
            level.entries.push((fid, expanded));
        }
        self.link_calls(scope, &mut level);
        while let Some((i, need)) = level.entries.iter().enumerate().find_map(|(i, (_, e))| {
            let need = e[0]
                .requires
                .iter()
                .find(|r| level.dropped.contains_key(&r.job))?;
            Some((i, need.job.clone()))
        }) {
            let (id, expanded) = level.entries.remove(i);
            self.config.jobs.remove(&expanded[0].job);
            let why = format!("needs `{need}`, which {}", level.dropped[&need]);
            self.warn(format!(
                "{} job `{id}`{} was dropped: it {why}",
                scope.label, scope.from
            ));
            level.dropped.insert(id, why);
        }
        level
    }
}

mod calls;
mod jobs;
mod rewrite;
mod triggers;

use self::calls::{bind_literals, scalar};
use self::jobs::{RunDefaults, flatten};
use self::rewrite::{FAILED, REUSABLE, calls_workflow, expression_problems};
