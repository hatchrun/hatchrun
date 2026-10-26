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

    /// Points what reads a job that called a workflow at the jobs the
    /// call became: `needs: caller` becomes a `requires` of each job the
    /// call ended with, and `needs.caller.outputs.X` becomes what the
    /// called workflow's `on.workflow_call.outputs.X` says. Jobs of a
    /// called file get their ids with the call in front, so that
    /// `needs.x` still names them.
    fn link_calls(&mut self, scope: &Scope, level: &mut Converted) {
        let mut warned = Vec::new();
        if !scope.prefix.is_empty() || !level.calls.is_empty() {
            let mut map = needs_mapper(scope, &level.local, &level.calls, "needs", &mut warned);
            for key in &level.plain {
                if let Some(job) = self.config.jobs.get_mut(key) {
                    map_job(job, &mut map);
                }
            }
        }
        warned.sort();
        warned.dedup();
        for message in warned {
            self.warn(format!("{}{}: {message}", scope.label, scope.from));
        }
        let ends: BTreeMap<String, Vec<String>> = level
            .calls
            .iter()
            .map(|(id, call)| (scope.full(id), call.requires.clone()))
            .collect();
        if ends.is_empty() {
            return;
        }
        for (_, entries) in &mut level.entries {
            for entry in entries {
                let mut seen = BTreeSet::new();
                entry.requires = entry
                    .requires
                    .iter()
                    .flat_map(|r| match ends.get(&r.job) {
                        Some(jobs) => jobs
                            .iter()
                            .map(|job| Require {
                                job: job.clone(),
                                statuses: r.statuses,
                            })
                            .collect(),
                        None => vec![r.clone()],
                    })
                    .filter(|r| seen.insert(r.job.clone()))
                    .collect();
            }
        }
    }

    /// The file `uses: ./path` names, among the files with
    /// `on.workflow_call`. The path may be under `.github`, `.forgejo` or
    /// `.gitea`; a file is found under any of them.
    fn callee(&self, uses: &str) -> Option<String> {
        let want = uses.strip_prefix("./")?.trim_start_matches("./");
        let name = WORKFLOW_DIRS
            .iter()
            .find_map(|dir| want.strip_prefix(dir))
            .unwrap_or(want);
        let at = |file: &str, path: &str| file == path || file.ends_with(&format!("/{path}"));
        self.library
            .keys()
            .find(|file| {
                let file = file.trim_start_matches("./");
                at(file, want)
                    || file == name
                    || WORKFLOW_DIRS
                        .iter()
                        .any(|dir| at(file, &format!("{dir}{name}")))
            })
            .cloned()
    }

    /// The `uses` of a call and the file it names. `Err` says what became
    /// of a call that cannot be inlined: a remote one, one of a file that
    /// was not given, a cycle or one nested too deep.
    fn resolve(&mut self, scope: &Scope, at: &str, gh: &GhJob) -> Result<(String, String), Call> {
        let Some(uses) = gh.uses.as_ref().and_then(Value::as_str) else {
            self.warn(format!(
                "{at} calls a reusable workflow, which is not supported; the job was dropped"
            ));
            return Err(Call::Dropped);
        };
        if !uses.starts_with("./") {
            let path = uses.split('@').next().unwrap_or(uses);
            let name = path.rsplit('/').next().unwrap_or(path);
            self.warn(format!(
                "{at} calls `{uses}`, a reusable workflow in another repository, which is not supported; copy `{name}` into this repository and call it as `./.github/workflows/{name}`; the job was dropped"
            ));
            return Err(Call::Dropped);
        }
        let Some(file) = self.callee(uses) else {
            self.warn(format!(
                "{at} calls `{uses}`, which is not among the converted files or has no `on: workflow_call`; the job was dropped"
            ));
            return Err(Call::Dropped);
        };
        if scope.stack.contains(&file) {
            let chain: Vec<String> = scope
                .stack
                .iter()
                .chain([&file])
                .map(|f| format!("`{f}`"))
                .collect();
            self.error(format!(
                "{at} calls `{uses}`, which makes a cycle ({}); the job was dropped",
                chain.join(" calls ")
            ));
            return Err(Call::Failed);
        }
        if scope.depth >= MAX_CALL_DEPTH {
            self.error(format!(
                "{at} calls `{uses}` more than {MAX_CALL_DEPTH} levels deep; the job was dropped"
            ));
            return Err(Call::Failed);
        }
        Ok((uses.to_owned(), file))
    }

    /// Converts a job that calls a reusable workflow. A call of a file
    /// among the converted ones is inlined: each job of the called file
    /// becomes the job `<stem>-<caller>-<job>`, with the `inputs` of
    /// `on.workflow_call` as `parameters` and the call's `with:` as the
    /// entries' `with`. A remote call, or one of a file that was not
    /// given, is dropped with a warning; a cycle, a call nested more
    /// than [`MAX_CALL_DEPTH`] levels deep or a call that does not
    /// convert is an error.
    fn call(
        &mut self,
        scope: &Scope,
        at: &str,
        id: &str,
        ids: &BTreeSet<String>,
        gh: &GhJob,
    ) -> Call {
        let (uses, file) = match self.resolve(scope, at, gh) {
            Ok(found) => found,
            Err(call) => return call,
        };
        let wf = self.library[&file].clone();
        let before = self.errors.len();
        for (key, set) in [
            ("strategy", gh.strategy.is_some()),
            ("concurrency", gh.concurrency.is_some()),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{key}` on a job that calls a reusable workflow is not supported and was dropped"
                ));
            }
        }
        let caller_permissions = self.permissions(at, gh.permissions.as_ref(), true);
        let needs = self.needs(at, gh.needs.clone(), ids);

        // The inputs of the called file and the values the call gives.
        let body = workflow_call(&wf.on).unwrap_or(Value::Null);
        let params = self.dispatch(&file, &body).inputs;
        self.reserved_inputs(at, &file, &params);
        let with = self.call_with(scope, at, &file, gh, &params);
        let mut inputs: BTreeMap<String, Value> = params
            .iter()
            .filter_map(|(name, p)| Some((name.clone(), p.default.clone()?)))
            .collect();
        inputs.extend(with.clone());
        if self.errors.len() > before {
            return Call::Failed;
        }

        let label = format!("`{file}`");
        let env = self.env(&label, "env", wf.env.as_ref());
        let defaults = self.defaults(&label, wf.defaults.as_ref());
        let wf_permissions = self.permissions(&label, wf.permissions.as_ref(), false);
        let inner = Scope {
            label,
            from: format!(" (called by {at})"),
            stem: scope.stem.clone(),
            prefix: if scope.prefix.is_empty() {
                id.to_owned()
            } else {
                format!("{}-{id}", scope.prefix)
            },
            depth: scope.depth + 1,
            stack: scope.stack.iter().cloned().chain([file.clone()]).collect(),
            inputs: Some(inputs),
        };
        let mut level = self.jobs(&inner, wf.jobs.0, &env, &defaults);
        if level.failed || level.entries.is_empty() {
            for (_, entries) in &level.entries {
                for entry in entries {
                    self.config.jobs.remove(&entry.job);
                }
            }
            if level.failed {
                return Call::Failed;
            }
            self.warn(format!(
                "{at} calls `{uses}`, which has no job left to run; the job was dropped"
            ));
            return Call::Dropped;
        }

        // What the called jobs read as `inputs` and `secrets`.
        let all: BTreeSet<String> = level
            .entries
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|e| e.job.clone()))
            .collect();
        self.bind_inputs(&mut level, &params, &with, wf_permissions.as_ref());
        self.pass_secrets(at, &file, gh.secrets.as_ref(), &body, &all);
        if let Some(permissions) = &caller_permissions {
            for key in &all {
                if let Some(job) = self.config.jobs.get_mut(key) {
                    job.permissions.get_or_insert_with(|| permissions.clone());
                }
            }
        }

        // What jobs that need the call read, and wait for.
        let required: BTreeSet<&str> = level
            .entries
            .iter()
            .flat_map(|(_, e)| e[0].requires.iter().map(|r| r.job.as_str()))
            .collect();
        let mut ends: Vec<String> = level
            .entries
            .iter()
            .map(|(id, _)| id)
            .filter(|id| !required.contains(id.as_str()))
            .cloned()
            .collect();
        let outputs = self.call_outputs(at, &inner, &level, &body, &mut ends);

        let given = inner.inputs.clone().unwrap_or_default();
        self.attach(scope, at, gh, &needs, &given, &mut level);
        Call::Done(Called {
            entries: level.entries,
            expansion: Expansion {
                requires: ends,
                outputs,
            },
        })
    }

    /// An input a workflow entry reads as its own key, such as `name`, is
    /// an error: the called jobs would take it as a parameter that no
    /// entry could set.
    fn reserved_inputs(&mut self, at: &str, file: &str, params: &BTreeMap<String, Parameter>) {
        for name in params
            .keys()
            .filter(|n| plan::ENTRY_KEYS.contains(&n.as_str()))
        {
            self.error(format!(
                "{at} calls `{file}`, whose input `{name}` would be a job parameter that a workflow entry reads as its own key; rename the input; the job was dropped"
            ));
        }
    }

    /// The values a call gives the inputs of the file it calls, as the
    /// entries' `with`: those of inputs the file does not have are dropped
    /// with a warning, and an input with no default that the call does
    /// not give is an error.
    fn call_with(
        &mut self,
        scope: &Scope,
        at: &str,
        file: &str,
        gh: &GhJob,
        params: &BTreeMap<String, Parameter>,
    ) -> BTreeMap<String, Value> {
        let mut with = BTreeMap::new();
        let given = match &gh.with_ {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(map)) => map.iter().collect(),
            Some(other) => {
                self.warn(format!(
                    "{at}: `with: {}` is not a map and was dropped",
                    show(other)
                ));
                Vec::new()
            }
        };
        for (name, value) in given {
            let Some(param) = params.get(name) else {
                self.warn(format!(
                    "{at}: `with.{name}` is not an input of `{file}` and was dropped"
                ));
                continue;
            };
            let value = match &scope.inputs {
                Some(inputs) => substitute(value, inputs),
                None => value.clone(),
            };
            let value = fit_default(&param.kind, &value).unwrap_or(value);
            with.insert(name.clone(), value);
        }
        for (name, param) in params {
            if param.default.is_none() && !with.contains_key(name) {
                self.error(format!(
                    "{at}: input `{name}` of `{file}` is required and the call does not give it"
                ));
            }
        }
        with
    }

    /// Gives the jobs a called file has itself the file's inputs as
    /// parameters and the call's values as the `with` of their entries;
    /// what they read as `inputs` becomes `parameters`.
    fn bind_inputs(
        &mut self,
        level: &mut Converted,
        params: &BTreeMap<String, Parameter>,
        with: &BTreeMap<String, Value>,
        permissions: Option<&Permissions>,
    ) {
        for key in &level.plain {
            if let Some(job) = self.config.jobs.get_mut(key) {
                for (name, param) in params {
                    job.parameters
                        .entry(name.clone())
                        .or_insert_with(|| param.clone());
                }
                job.permissions = job.permissions.take().or_else(|| permissions.cloned());
                map_job(job, &mut |seg| {
                    (seg.len() >= 2 && seg[0] == "inputs")
                        .then(|| (2, format!("parameters.{}", seg[1])))
                });
            }
        }
        for (_, entries) in &mut level.entries {
            for entry in entries {
                if level.plain.contains(&entry.job) {
                    entry.with.extend(with.clone());
                }
            }
        }
    }

    /// Puts the call's `needs` and `if` on the jobs of the called file
    /// that need nothing inside it.
    fn attach(
        &mut self,
        scope: &Scope,
        at: &str,
        gh: &GhJob,
        needs: &[String],
        given: &BTreeMap<String, Value>,
        level: &mut Converted,
    ) {
        let roots: BTreeSet<String> = level
            .entries
            .iter()
            .filter(|(_, e)| e[0].requires.is_empty())
            .flat_map(|(_, e)| e.iter().map(|e| e.job.clone()))
            .collect();
        for (_, entries) in &mut level.entries {
            if entries[0].requires.is_empty() {
                for entry in entries {
                    entry.requires = needs
                        .iter()
                        .map(|need| Require::new(scope.full(need)))
                        .collect();
                }
            }
        }
        let condition = gh
            .if_
            .as_ref()
            .and_then(scalar)
            .filter(|c| !c.trim().is_empty());
        let Some(condition) = condition else {
            return;
        };
        let condition = match &scope.inputs {
            Some(inputs) => rewrite(
                &condition,
                !condition.contains("${{"),
                &mut input_mapper(inputs),
            ),
            None => condition,
        };
        let mut kept = false;
        for key in &roots {
            let Some(job) = self.config.jobs.get_mut(key) else {
                continue;
            };
            // `parameters` can be read only by a whole `${{ }}`, and the
            // combined condition is not one: use the value the call gave.
            if let Some(current) = &job.if_
                && current.contains("parameters.")
            {
                let bare = !current.contains("${{");
                job.if_ = Some(rewrite(current, bare, &mut |parts| match parts {
                    ["parameters", name, ..] => Some((2, expr_text(literal_input(given, name)?))),
                    _ => None,
                }));
            }
            if job
                .if_
                .as_deref()
                .is_some_and(|c| c.contains("parameters."))
            {
                kept = true;
            } else if let Some(and) = both(&condition, job.if_.as_deref()) {
                job.if_ = Some(and);
            } else {
                kept = true;
            }
        }
        if kept {
            self.warn(format!(
                "{at}: `if` could not be combined with the `if` of the called jobs and was dropped for them"
            ));
        }
    }

    /// Passes the call's `secrets` to the called jobs: `inherit` passes
    /// every secret on, and a map renames the reads of each name to what
    /// the call gives it. A secret the called file declares and the call
    /// does not give is a warning.
    fn pass_secrets(
        &mut self,
        at: &str,
        file: &str,
        secrets: Option<&Value>,
        body: &Value,
        jobs: &BTreeSet<String>,
    ) {
        let mut renames: BTreeMap<String, String> = BTreeMap::new();
        let inherit = matches!(secrets, Some(Value::String(s)) if s == "inherit");
        match secrets {
            Some(Value::Object(map)) => {
                for (name, value) in map {
                    renames.insert(name.to_ascii_uppercase(), expr_text(value));
                }
            }
            Some(_) if !inherit => self.warn(format!(
                "{at}: `secrets` is not `inherit` or a map and was dropped"
            )),
            _ => {}
        }
        if !inherit && let Some(Value::Object(declared)) = body.get("secrets") {
            for name in declared.keys() {
                if !renames.contains_key(&name.to_ascii_uppercase()) {
                    self.warn(format!(
                        "{at}: `{file}` declares secret `{name}`, which the call does not pass"
                    ));
                }
            }
        }
        if renames.is_empty() {
            return;
        }
        let mut map = |seg: &[&str]| {
            let found = renames.get(&seg.get(1)?.to_ascii_uppercase())?;
            (seg[0] == "secrets").then(|| (2, found.clone()))
        };
        for key in jobs {
            if let Some(job) = self.config.jobs.get_mut(key) {
                map_job(job, &mut map);
            }
        }
    }

    /// Maps `on.workflow_call.outputs` of a called file to the
    /// expressions `needs.<call>.outputs.<name>` stands for in the jobs
    /// that need the call. An output whose value is not an expression
    /// over `jobs.<job>.outputs.<name>` of the file's jobs maps to
    /// `None` and is dropped with a warning. The jobs the outputs read
    /// are added to `ends`, so that those that need the call can read
    /// them as `needs`.
    fn call_outputs(
        &mut self,
        at: &str,
        inner: &Scope,
        level: &Converted,
        body: &Value,
        ends: &mut Vec<String>,
    ) -> BTreeMap<String, Option<String>> {
        let mut outputs = BTreeMap::new();
        let Some(Value::Object(declared)) = body.get("outputs") else {
            return outputs;
        };
        let mut warned = Vec::new();
        let mut map = needs_mapper(inner, &level.local, &level.calls, "jobs", &mut warned);
        let mut dropped = Vec::new();
        for (name, output) in declared {
            let text = output.get("value").and_then(Value::as_str);
            let mapped = text.and_then(|text| match whole(text) {
                Some(expr) => Some(rewrite(expr, true, &mut map)),
                None if !text.contains("${{") => Some(expr_text(&Value::String(text.to_owned()))),
                None => None,
            });
            let mapped = mapped.and_then(|expr| {
                let mut ok = true;
                let mut read = Vec::new();
                rewrite(&expr, true, &mut |seg| {
                    match seg {
                        ["jobs", ..] => ok = false,
                        ["needs", job, ..] => read.push((*job).to_owned()),
                        _ => {}
                    }
                    None
                });
                for job in read {
                    if level.entries.iter().any(|(id, _)| *id == job) && !ends.contains(&job) {
                        ends.push(job);
                    }
                }
                ok.then(|| {
                    if simple(&expr) {
                        expr
                    } else {
                        format!("({expr})")
                    }
                })
            });
            if mapped.is_none() {
                dropped.push(name.clone());
            }
            outputs.insert(name.clone(), mapped);
        }
        drop(map);
        for name in dropped {
            self.warn(format!(
                "{at}: output `{name}` of the called workflow is not an expression over `jobs.<job>.outputs.<name>` of its own jobs; reads of it were dropped"
            ));
        }
        warned.sort();
        warned.dedup();
        for message in warned {
            self.warn(format!("{at}: {message}"));
        }
        outputs
    }

    /// Converts a `permissions:` value. `id-token`, scopes hatchrun does
    /// not know and levels it does not know are dropped with a warning.
    /// A value of another shape is dropped too; a job that said it
    /// then gets no permissions at all, never the workflow's, which
    /// could be wider than the author meant.
    fn permissions(&mut self, at: &str, value: Option<&Value>, job: bool) -> Option<Permissions> {
        match value? {
            Value::String(text) if text == "read-all" => Some(Permissions::ReadAll),
            Value::String(text) if text == "write-all" => Some(Permissions::WriteAll),
            Value::Object(map) => {
                let mut scopes = BTreeMap::new();
                for (key, level) in map {
                    if key == "id-token" {
                        self.warn(format!("{at}: `id-token` is not supported and was dropped"));
                        continue;
                    }
                    if !SCOPES.iter().any(|(k, _)| k == key) {
                        self.warn(format!(
                            "{at}: `permissions.{key}` is not supported and was dropped"
                        ));
                        continue;
                    }
                    let level = match level.as_str() {
                        Some("read") => Level::Read,
                        Some("write") => Level::Write,
                        Some("none") => Level::None,
                        _ => {
                            self.warn(format!(
                                "{at}: `permissions.{key}` is `{}`, not read, write or none, and was dropped",
                                show(level)
                            ));
                            continue;
                        }
                    };
                    scopes.insert(key.clone(), level);
                }
                Some(Permissions::Scopes(scopes))
            }
            other => {
                self.warn(format!(
                    "{at}: `permissions: {}` is not read-all, write-all or a map and was dropped",
                    show(other)
                ));
                job.then(|| Permissions::Scopes(BTreeMap::new()))
            }
        }
    }

    /// Converts `on:` and warns about the events dropped. Returns `None`
    /// for a workflow left with no event, which is dropped with a warning.
    fn kept_triggers(&mut self, file: &str, stem: &str, on: &Value) -> Option<Triggers> {
        let (triggers, unsupported) = self.triggers(file, stem, on);
        if triggers == Triggers::default() {
            self.no_events(file, &unsupported);
            return None;
        }
        self.dropped_events(file, &unsupported);
        Some(triggers)
    }

    /// Converts `on:`, which GitHub accepts as an event name, a list of
    /// names or a map of events to their filters. Returns the events
    /// hatchrun does not run on too, so the caller can tell a workflow
    /// that keeps some events from one left with none.
    fn triggers(&mut self, file: &str, stem: &str, on: &Value) -> (Triggers, Vec<String>) {
        let mut triggers = Triggers::default();
        let mut unsupported = Vec::new();
        let mut event = |cx: &mut Self, name: &str, body: &Value| {
            if !cx.event(file, stem, name, body, &mut triggers) {
                unsupported.push(name.to_owned());
            }
        };
        match on {
            Value::String(name) => event(self, name, &Value::Null),
            Value::Array(events) => {
                for name in events {
                    match name {
                        Value::String(name) => event(self, name, &Value::Null),
                        other => self.error(format!(
                            "`{file}`: `on` lists `{other}`, which is not an event name"
                        )),
                    }
                }
            }
            Value::Object(events) => {
                for (name, body) in events {
                    event(self, name, body);
                }
            }
            other => self.error(format!(
                "`{file}`: `on` is `{other}`; it must be an event name, a list or a map"
            )),
        }
        (triggers, unsupported)
    }

    /// Converts one event into `triggers`. Returns false when hatchrun
    /// does not run on it.
    fn event(
        &mut self,
        file: &str,
        stem: &str,
        event: &str,
        body: &Value,
        triggers: &mut Triggers,
    ) -> bool {
        let bad = |err: serde_json::Error| format!("`{file}`: `on.{event}`: {err}");
        match event {
            "push" => match Option::<PushTrigger>::deserialize(body) {
                Ok(push) => triggers.push = Some(push.unwrap_or_default()),
                Err(err) => self.error(bad(err)),
            },
            "pull_request" => match Option::<PullRequestTrigger>::deserialize(body) {
                Ok(pr) => triggers.pull_request = Some(pr.unwrap_or_default()),
                Err(err) => self.error(bad(err)),
            },
            "schedule" => match Vec::<Schedule>::deserialize(body) {
                Ok(schedule) => triggers.schedule = schedule,
                Err(err) => self.error(bad(err)),
            },
            "workflow_dispatch" => {
                triggers.workflow_dispatch = Some(self.dispatch(file, body));
            }
            "workflow_run" => match GhWorkflowRun::deserialize(body) {
                Ok(run) => triggers.workflow_run = self.workflow_run(file, stem, run),
                Err(err) => self.error(bad(err)),
            },
            "release" => match Option::<GhRelease>::deserialize(body) {
                Ok(release) => {
                    let types = release.and_then(|r| r.types);
                    triggers.release = self.release(file, types.as_ref());
                }
                Err(err) => self.error(bad(err)),
            },
            _ => return false,
        }
        true
    }

    /// Warns about each event of a workflow that hatchrun does not run
    /// on, and for those only GitHub delivers, to keep them there.
    fn dropped_events(&mut self, file: &str, unsupported: &[String]) {
        for event in unsupported {
            let keep = if GITHUB_ONLY.contains(&event.as_str()) {
                format!("; keep `on.{event}` on GitHub Actions")
            } else {
                String::new()
            };
            self.warn(format!(
                "`{file}`: `on.{event}` is not supported and was dropped; hatchrun runs on {EVENTS}{keep}"
            ));
        }
    }

    /// Reports a workflow left with no event to run on, which is dropped
    /// with its jobs: they could never start. `unsupported` names the
    /// events it had that hatchrun does not run on.
    fn no_events(&mut self, file: &str, unsupported: &[String]) {
        if !unsupported.is_empty() && unsupported.iter().all(|e| e == "workflow_call") {
            if self.called.contains(file) {
                return;
            }
            self.warn(format!(
                "`{file}` was dropped: it is a reusable workflow (`on: workflow_call`), which is not supported"
            ));
            return;
        }
        if unsupported.is_empty() {
            // `on` named no event, or the events it named did not convert
            // and are errors already.
            self.warn(format!(
                "`{file}` was dropped: none of its events converted"
            ));
            return;
        }
        let shown: Vec<String> = unsupported.iter().map(|e| format!("`{e}`")).collect();
        self.warn(format!(
            "`{file}` was dropped: it runs on {}, and hatchrun runs on {EVENTS}; keep it on GitHub Actions",
            shown.join(", ")
        ));
    }

    /// Converts `on.workflow_run`, naming each workflow by the stem of
    /// the file that has its GitHub name. Names no file has, the file's
    /// own name and types other than `completed` are dropped with a
    /// warning. `None` when no workflow or no type is left.
    fn workflow_run(
        &mut self,
        file: &str,
        stem: &str,
        run: GhWorkflowRun,
    ) -> Option<WorkflowRunTrigger> {
        let at = format!("`{file}`: `on.workflow_run`");
        let mut workflows: Vec<String> = Vec::new();
        for name in run.workflows {
            let Some(found) = self.names.get(&name).cloned() else {
                self.warn(format!(
                    "{at} names workflow `{name}`, which none of the converted files is; it was dropped"
                ));
                continue;
            };
            for other in found {
                if other == stem {
                    self.warn(format!(
                        "{at} names workflow `{name}`, which is this file; it was dropped"
                    ));
                } else if !workflows.contains(&other) {
                    workflows.push(other);
                }
            }
        }
        let types = match &run.types {
            None | Some(Value::Null) => vec!["completed".to_owned()],
            Some(types) => self.types(&at, types, WORKFLOW_RUN_TYPES)?,
        };
        if workflows.is_empty() {
            self.warn(format!(
                "{at} was dropped: it names no other converted workflow"
            ));
            return None;
        }
        Some(WorkflowRunTrigger {
            workflows,
            types,
            branches: run.branches,
            branches_ignore: run.branches_ignore,
        })
    }

    /// Converts `on.release`. Types hatchrun does not run on are dropped
    /// with a warning; with no `types`, GitHub runs on every type, and
    /// the release runs on `published` only. `None` when no type is left.
    fn release(&mut self, file: &str, types: Option<&Value>) -> Option<ReleaseTrigger> {
        let at = format!("`{file}`: `on.release`");
        let Some(types) = types.filter(|t| !t.is_null()) else {
            self.warn(format!("{at} has no `types`; it runs on `published` only"));
            return Some(ReleaseTrigger::default());
        };
        Some(ReleaseTrigger {
            types: self.types(&at, types, RELEASE_TYPES)?,
            ..ReleaseTrigger::default()
        })
    }

    /// The activity types of an event out of `known`, dropping the rest
    /// with a warning. `None` when none of them is known, which drops
    /// the event with a warning.
    fn types(&mut self, at: &str, types: &Value, known: &[&str]) -> Option<Vec<String>> {
        let given: Vec<String> = match types {
            Value::Array(items) => items.iter().filter_map(scalar).collect(),
            other => scalar(other).into_iter().collect(),
        };
        let (kept, dropped): (Vec<String>, Vec<String>) =
            given.into_iter().partition(|t| known.contains(&t.as_str()));
        let shown: Vec<String> = dropped.iter().map(|t| format!("`{t}`")).collect();
        let runs = known.join(", ");
        if kept.is_empty() {
            self.warn(format!(
                "{at} was dropped: its types {} are not supported; hatchrun runs on {runs}",
                shown.join(", ")
            ));
            return None;
        }
        if !dropped.is_empty() {
            self.warn(format!(
                "{at}: types {} are not supported and were dropped; hatchrun runs on {runs}",
                shown.join(", ")
            ));
        }
        Some(kept)
    }

    /// Drops from `on.workflow_run` the workflows that did not convert,
    /// then the event when none is left, then a workflow left with no
    /// event and its jobs, until every name it keeps is a workflow.
    /// `stems` maps each stem to its file.
    fn unreachable_runs(&mut self, stems: &BTreeMap<String, &str>) {
        loop {
            let mut changed = false;
            let keys: Vec<String> = self.config.workflows.keys().cloned().collect();
            for key in keys {
                let file = stems.get(&key).copied().unwrap_or(key.as_str());
                let wf = &self.config.workflows[&key];
                let Some(run) = &wf.on.workflow_run else {
                    continue;
                };
                let gone: Vec<String> = run
                    .workflows
                    .iter()
                    .filter(|w| !self.config.workflows.contains_key(*w))
                    .cloned()
                    .collect();
                if gone.is_empty() {
                    continue;
                }
                changed = true;
                let at = format!("`{file}`: `on.workflow_run`");
                for other in &gone {
                    self.warn(format!(
                        "{at} names workflow `{other}`, which did not convert; it was dropped"
                    ));
                }
                let wf = self.config.workflows.get_mut(&key).expect("listed above");
                let run = wf.on.workflow_run.as_mut().expect("checked above");
                run.workflows.retain(|w| !gone.contains(w));
                if !run.workflows.is_empty() {
                    continue;
                }
                wf.on.workflow_run = None;
                if wf.on != Triggers::default() {
                    self.warn(format!(
                        "{at} was dropped: it names no other converted workflow"
                    ));
                    continue;
                }
                let wf = self.config.workflows.remove(&key).expect("listed above");
                for entry in wf.jobs {
                    self.config.jobs.remove(&entry.job);
                }
                self.warn(format!(
                    "`{file}` was dropped: it runs only after workflows that did not convert"
                ));
            }
            if !changed {
                break;
            }
        }
    }

    /// Converts `on.workflow_dispatch`, mapping GitHub input types onto
    /// parameter kinds.
    fn dispatch(&mut self, file: &str, body: &Value) -> Dispatch {
        let mut dispatch = Dispatch::default();
        let Some(inputs) = body.get("inputs").and_then(Value::as_object) else {
            return dispatch;
        };
        for (name, input) in inputs {
            let at = format!("`{file}`: input `{name}`");
            let input = match GhInput::deserialize(input) {
                Ok(input) => input,
                Err(err) => {
                    self.error(format!("{at}: {err}"));
                    continue;
                }
            };
            let kind = match input.kind.as_deref().unwrap_or("string") {
                "string" => ParameterKind::String,
                "boolean" => ParameterKind::Boolean,
                "number" => {
                    self.warn(format!(
                        "{at} is a number input and became an integer parameter"
                    ));
                    ParameterKind::Integer
                }
                "environment" => {
                    self.warn(format!(
                        "{at} is an environment input and became a string parameter"
                    ));
                    ParameterKind::String
                }
                "choice" => {
                    let options: Option<Vec<String>> = input
                        .options
                        .as_ref()
                        .map(|o| o.iter().filter_map(scalar).collect());
                    match options {
                        Some(options) if !options.is_empty() => ParameterKind::Enum(options),
                        _ => {
                            self.error(format!("{at} is a choice input with no `options`"));
                            continue;
                        }
                    }
                }
                other => {
                    self.error(format!(
                        "{at} has type `{other}`, which GitHub does not define"
                    ));
                    continue;
                }
            };
            let default = input.default.and_then(|value| {
                let fitted = fit_default(&kind, &value);
                if fitted.is_none() {
                    self.warn(format!(
                        "{at}: `default: {}` does not fit the input type and was dropped",
                        show(&value)
                    ));
                }
                fitted
            });
            dispatch.inputs.insert(
                name.clone(),
                Parameter {
                    kind,
                    default,
                    description: input.description,
                },
            );
        }
        dispatch
    }

    /// Reads one GitHub job, warning about keys GitHub does not define.
    fn gh_job(&mut self, at: &str, value: &Value) -> Option<GhJob> {
        let Some(map) = value.as_object() else {
            self.error(format!("{at} is not a map"));
            return None;
        };
        for key in map.keys().filter(|k| !JOB_KEYS.contains(&k.as_str())) {
            self.warn(format!("{at}: unknown key `{key}` was dropped"));
        }
        match GhJob::deserialize(value) {
            Ok(job) => Some(job),
            Err(err) => {
                self.error(format!("{at}: {err}"));
                None
            }
        }
    }

    /// Reads a job's `environment`: a name, or a map whose `name` is the
    /// name. Records an error for anything hatchrun cannot read as written.
    fn environment(&mut self, at: &str, value: Option<&Value>) -> Option<String> {
        let value = value?;
        let name = match value {
            Value::String(name) => name,
            Value::Object(map) => {
                for key in map.keys() {
                    match key.as_str() {
                        "name" => {}
                        "url" => self.warn(format!(
                            "{at}: `environment.url` is not supported and was dropped"
                        )),
                        other => self.warn(format!(
                            "{at}: unknown key `environment.{other}` was dropped"
                        )),
                    }
                }
                if let Some(Value::String(name)) = map.get("name") {
                    name
                } else {
                    self.error(format!(
                        "{at}: `environment` must be a name or a map with `name`"
                    ));
                    return None;
                }
            }
            _ => {
                self.error(format!(
                    "{at}: `environment` must be a name or a map with `name`"
                ));
                return None;
            }
        };
        if name.contains("${{") {
            self.error(format!(
                "{at}: `environment` uses an expression; hatchrun reads environment names as written"
            ));
            return None;
        }
        if let Err(rule) = valid_environment_name(name) {
            self.error(format!("{at}: `environment` `{name}`: {rule}"));
            return None;
        }
        Some(name.clone())
    }

    /// Converts one job into the job and its workflow entries: one, or
    /// more when the matrix `include` adds combinations of its own.
    /// `None` when the job has errors, which are recorded.
    fn job(
        &mut self,
        at: &str,
        id: &str,
        key: &str,
        ids: &BTreeSet<String>,
        gh: GhJob,
    ) -> Option<(Job, Vec<WorkflowJob>)> {
        let before = self.errors.len();
        for (name, set, why) in [
            ("services", gh.services.is_some(), LATER),
            ("container", gh.container.is_some(), LATER),
            ("concurrency", gh.concurrency.is_some(), ""),
            ("continue-on-error", gh.continue_on_error.is_some(), ""),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{name}` is not supported and was dropped{why}"
                ));
            }
        }
        let environment = self.environment(at, gh.environment.as_ref());
        let mut matrix = self.strategy(at, gh.strategy);
        let runs_on = self.runs_on(at, gh.runs_on.as_ref(), &mut matrix);
        let leg_key = gh
            .runs_on
            .as_ref()
            .and_then(runs_on_text)
            .and_then(matrix_key);
        let matrices = self.include(at, matrix, leg_key);
        let requires = self.needs(at, gh.needs, ids);
        let timeout_minutes = self.minutes(at, "timeout-minutes", gh.timeout_minutes.as_ref());
        let outputs = self.env(at, "outputs", gh.outputs.as_ref());
        let env = self.env(at, "env", gh.env.as_ref());
        let defaults = self.defaults(at, gh.defaults.as_ref());
        let permissions = self.permissions(at, gh.permissions.as_ref(), true);
        let mut steps: Vec<Step> = gh
            .steps
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, step)| self.step(at, i, &step))
            .collect();
        if self.errors.len() > before {
            return None;
        }
        defaults.apply(&mut steps);
        let (runs_on, parameters) = runs_on?.into_job();
        let job = Job {
            runs_on: Some(runs_on),
            parameters,
            env,
            if_: gh.if_.as_ref().and_then(scalar),
            timeout_minutes,
            outputs,
            permissions,
            environment,
            steps,
            ..Job::default()
        };
        Some((job, entries(id, key, ids, &requires, matrices)))
    }

    /// Reads `needs`: a job id or a list. An id that is not a job of
    /// the workflow is an error; repeats are dropped.
    fn needs(&mut self, at: &str, needs: Option<Value>, ids: &BTreeSet<String>) -> Vec<String> {
        let requires = match needs {
            None => Vec::new(),
            Some(Value::String(need)) => vec![need],
            Some(Value::Array(needs)) => needs.iter().filter_map(scalar).collect(),
            Some(other) => {
                self.error(format!(
                    "{at} has `needs: {other}`; it must be a job id or a list"
                ));
                Vec::new()
            }
        };
        let mut unique: Vec<String> = Vec::new();
        for need in requires {
            if !ids.contains(&need) {
                self.error(format!(
                    "{at} needs `{need}`, which is not a job of the workflow"
                ));
            } else if !unique.contains(&need) {
                unique.push(need);
            }
        }
        unique
    }

    /// Maps `runs-on` to a runner template label. A third-party label,
    /// such as `blacksmith-4vcpu-ubuntu-2404`, becomes the template of
    /// its arch and size with a warning; `{labels: ...}` reads its labels
    /// and `{group: ...}` is an error. `runs-on: ${{ matrix.KEY }}`
    /// keeps the legs whose labels name a template and drops the others,
    /// such as macOS and Windows legs, with a warning.
    fn runs_on(
        &mut self,
        at: &str,
        runs_on: Option<&Value>,
        matrix: &mut GhMatrix,
    ) -> Option<RunsOn> {
        let label = runs_on.and_then(runs_on_text);
        if let Some(text) = label
            && let Some(key) = matrix_key(text)
            && (matrix.values.contains_key(key)
                || matrix.include.iter().any(|i| i.contains_key(key)))
        {
            return self.matrix_runs_on(at, text, key, matrix);
        }
        if let Some(Value::Object(map)) = runs_on
            && let Some(group) = map.get("group")
        {
            self.error(format!(
                "{at} has `runs-on: {{group: {}}}`; runner groups are not supported; use one of {}",
                show(group),
                template_labels().join(", ")
            ));
            return None;
        }
        if let Some(old) = label
            && let Some(new) = runner_label(old)
        {
            if new != old {
                self.warn(became(at, old, new));
            }
            return Some(RunsOn::Label(new.to_owned()));
        }
        let shown = runs_on.map_or_else(|| "nothing".to_owned(), show);
        self.error(format!(
            "{at} has `runs-on: {shown}`, which names no runner template; use one of {}",
            template_labels().join(", ")
        ));
        None
    }

    /// Resolves `runs-on: ${{ matrix.KEY }}`. Legs whose value names no
    /// template are dropped, and so is a value that runs on the same
    /// template as an earlier one. When the legs left run on one
    /// template the job runs on it and the values stay as written;
    /// otherwise the job takes `KEY` as a parameter and runs on the
    /// template it names, and each value becomes its template's label.
    fn matrix_runs_on(
        &mut self,
        at: &str,
        text: &str,
        key: &str,
        matrix: &mut GhMatrix,
    ) -> Option<RunsOn> {
        let mut found: Vec<&'static str> = Vec::new();
        let mut dropped = Vec::new();
        if let Some(values) = matrix.values.get_mut(key) {
            let mut kept: Vec<String> = Vec::new();
            let mut duplicates = Vec::new();
            values.retain(|value| {
                let Some(new) = value.as_str().and_then(runner_label) else {
                    dropped.push(format!("`{}`", show(value)));
                    return false;
                };
                // Two labels of one template, such as `ubuntu-latest` and
                // `ubuntu-24.04`, are one runner: the second leg would
                // run the same job again.
                if let Some(i) = found.iter().position(|f| template(f) == template(new)) {
                    duplicates.push(format!("`{}` (same runner as `{}`)", show(value), kept[i]));
                    return false;
                }
                found.push(new);
                kept.push(show(value));
                true
            });
            // Every combination of the other keys ran on a dropped value.
            if values.is_empty() {
                matrix.values.clear();
                matrix.exclude.clear();
            }
            if !duplicates.is_empty() {
                self.warn(format!(
                    "{at}: matrix key `{key}` values {} were dropped: each runs on the same runner template as an earlier value, so its legs would repeat that value's jobs",
                    duplicates.join(", ")
                ));
            }
        }
        matrix.include.retain(|entry| {
            let Some(value) = entry.get(key) else {
                return true;
            };
            let Some(new) = value.as_str().and_then(runner_label) else {
                dropped.push(format!("`{}`", show(value)));
                return false;
            };
            if !found.contains(&new) {
                found.push(new);
            }
            true
        });
        // An `exclude` of a dropped leg has nothing left to match.
        matrix.exclude.retain(|entry| {
            entry
                .get(key)
                .is_none_or(|v| v.as_str().and_then(runner_label).is_some())
        });
        if !dropped.is_empty() {
            self.warn(format!(
                "{at}: matrix key `{key}` values {} name no runner template and were dropped; hatchrun runs Linux only",
                dropped.join(", ")
            ));
        }
        if found.is_empty() {
            self.error(format!(
                "{at} has `runs-on: {text}`, and matrix key `{key}` names no runner template; use one of {}",
                template_labels().join(", ")
            ));
            return None;
        }
        let per_leg = found.len() > 1;
        let mut warned = BTreeSet::new();
        let values = matrix.values.get_mut(key).into_iter().flatten();
        let include = matrix.include.iter_mut().filter_map(|e| e.get_mut(key));
        let exclude = matrix.exclude.iter_mut().filter_map(|e| e.get_mut(key));
        for value in values.chain(include).chain(exclude) {
            let Some(old) = value.as_str() else { continue };
            let Some(new) = runner_label(old) else {
                continue;
            };
            if new == old {
                continue;
            }
            if warned.insert(old.to_owned()) {
                self.warn(became(at, old, new));
            }
            if per_leg {
                *value = Value::String(new.to_owned());
            }
        }
        if !per_leg {
            return Some(RunsOn::Label(found[0].to_owned()));
        }
        if plan::ENTRY_KEYS.contains(&key) {
            self.error(format!(
                "{at} has `runs-on: {text}` over legs that run on more than one template, and matrix key `{key}` cannot be passed as a parameter; rename it"
            ));
            return None;
        }
        Some(RunsOn::Matrix(key.to_owned()))
    }

    /// Reads `strategy.matrix`: value lists, `exclude` and `include`. The
    /// rest of `strategy`, and a matrix or key that is an expression,
    /// such as `${{ fromJSON(...) }}`, are dropped with a warning.
    fn strategy(&mut self, at: &str, strategy: Option<BTreeMap<String, Value>>) -> GhMatrix {
        let mut matrix = GhMatrix::default();
        for (key, value) in strategy.unwrap_or_default() {
            if key != "matrix" {
                self.warn(format!(
                    "{at}: `strategy.{key}` is not supported and was dropped"
                ));
                continue;
            }
            let Value::Object(keys) = value else {
                self.warn(format!(
                    "{at}: `strategy.matrix` is not a map of value lists and was dropped"
                ));
                continue;
            };
            for (name, values) in keys {
                match values {
                    _ if name == "include" || name == "exclude" => {
                        let entries = self.combinations(at, &name, values);
                        if name == "include" {
                            matrix.include = entries;
                        } else {
                            matrix.exclude = entries;
                        }
                    }
                    Value::Array(values) => {
                        if values.is_empty() {
                            self.warn(format!(
                                "{at}: matrix key `{name}` has no values and was dropped"
                            ));
                        } else {
                            matrix.values.insert(name, values);
                        }
                    }
                    other => self.warn(format!(
                        "{at}: matrix key `{name}` is `{}`, not a list, and was dropped",
                        show(&other)
                    )),
                }
            }
        }
        // GitHub rejects these; the key may also have been dropped above.
        let values = &matrix.values;
        let mut unknown = Vec::new();
        matrix.exclude.retain(|entry| {
            let missing = entry.keys().find(|k| !values.contains_key(*k));
            if let Some(k) = missing {
                unknown.push(k.clone());
            }
            missing.is_none()
        });
        for key in unknown {
            self.warn(format!(
                "{at}: a `strategy.matrix.exclude` entry names `{key}`, which is not a matrix key, and was dropped"
            ));
        }
        matrix
    }

    /// Reads `strategy.matrix.include` or `exclude`: a list of maps.
    /// Anything else is dropped with a warning.
    fn combinations(&mut self, at: &str, name: &str, value: Value) -> Vec<BTreeMap<String, Value>> {
        let Value::Array(items) = value else {
            self.warn(format!(
                "{at}: `strategy.matrix.{name}` is `{}`, not a list of maps, and was dropped",
                show(&value)
            ));
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in items {
            match item {
                Value::Object(map) => out.push(map.into_iter().collect()),
                other => self.warn(format!(
                    "{at}: `strategy.matrix.{name}` entry `{}` is not a map and was dropped",
                    show(&other)
                )),
            }
        }
        out
    }

    /// Applies GitHub's `include` to the matrix and returns the matrix of
    /// each workflow entry the job becomes.
    ///
    /// As on GitHub, `exclude` applies first. Then each `include` entry
    /// extends every combination whose matrix values it does not change
    /// with its other keys (a later entry can change keys an earlier one
    /// added), and an entry that extends none is a combination of its
    /// own. The first matrix holds the combinations no entry extended,
    /// with the others excluded; each extended or added combination gets
    /// a matrix of its own with one value per key. When every
    /// combination was extended the same way, the added keys join the
    /// first matrix instead. A combination with no `leg_key`, which
    /// `runs-on` reads, is dropped with a warning.
    fn include(&mut self, at: &str, matrix: GhMatrix, leg_key: Option<&str>) -> Vec<Matrix> {
        let GhMatrix {
            values,
            exclude,
            include,
        } = matrix;
        let base = Matrix { values, exclude };
        if include.is_empty() {
            return vec![base];
        }
        let total = base
            .values
            .values()
            .fold(1usize, |n, v| n.saturating_mul(v.len()));
        if total > plan::MAX_COMBINATIONS {
            self.error(format!(
                "{at} has a matrix of {total} combinations with `include`; the limit is {}",
                plan::MAX_COMBINATIONS
            ));
            return Vec::new();
        }
        // Each combination left after `exclude`: its matrix values and
        // the keys `include` added.
        let mut legs: Vec<(Combo, Combo)> = Vec::new();
        if !base.values.is_empty() {
            for index in (0..total).filter(|&i| !plan::excluded(&base, i)) {
                let mut rest = index;
                let mut combo = BTreeMap::new();
                for (key, values) in base.values.iter().rev() {
                    combo.insert(key.clone(), values[rest % values.len()].clone());
                    rest /= values.len();
                }
                legs.push((combo, BTreeMap::new()));
            }
        }
        let mut added: Vec<Combo> = Vec::new();
        for entry in include {
            let mut matched = false;
            for (combo, extra) in &mut legs {
                if entry
                    .iter()
                    .all(|(k, v)| combo.get(k).is_none_or(|c| c == v))
                {
                    matched = true;
                    for (k, v) in &entry {
                        if !combo.contains_key(k) {
                            extra.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            if matched {
                continue;
            }
            if let Some(key) = leg_key
                && !entry.contains_key(key)
            {
                self.warn(format!(
                    "{at}: a `strategy.matrix.include` entry adds a combination without `{key}`, which `runs-on` reads, and was dropped"
                ));
                continue;
            }
            added.push(entry);
        }
        let mut plain = Vec::new();
        let mut extended = Vec::new();
        let mut left_out = Vec::new();
        for (combo, extra) in legs {
            if leg_key.is_some_and(|k| !combo.contains_key(k) && !extra.contains_key(k)) {
                left_out.push(combo);
            } else if extra.is_empty() {
                plain.push(combo);
            } else {
                extended.push((combo, extra));
            }
        }
        if let Some(key) = leg_key
            && !left_out.is_empty()
        {
            self.warn(format!(
                "{at}: {} matrix combinations get no `{key}`, which `runs-on` reads, from `strategy.matrix.include` and were dropped",
                left_out.len()
            ));
        }
        let mut out = group(base, plain, extended, left_out);
        out.extend(added.into_iter().map(one_each));
        if out.is_empty() {
            self.error(format!("{at} has a matrix with no combination left to run"));
        }
        out
    }

    /// Reads `defaults`, keeping `run.shell` and `run.working-directory`
    /// and dropping the rest with a warning.
    fn defaults(&mut self, at: &str, value: Option<&Value>) -> RunDefaults {
        let mut defaults = RunDefaults::default();
        let Some(value) = value else {
            return defaults;
        };
        let Value::Object(map) = value else {
            self.warn(format!("{at}: `defaults` is not a map and was dropped"));
            return defaults;
        };
        for (key, value) in map {
            if key != "run" {
                self.warn(format!(
                    "{at}: `defaults.{key}` is not supported and was dropped"
                ));
                continue;
            }
            let Value::Object(run) = value else {
                self.warn(format!("{at}: `defaults.run` is not a map and was dropped"));
                continue;
            };
            for (key, value) in run {
                let slot = match key.as_str() {
                    "shell" => &mut defaults.shell,
                    "working-directory" => &mut defaults.working_directory,
                    _ => {
                        self.warn(format!(
                            "{at}: `defaults.run.{key}` is not supported and was dropped"
                        ));
                        continue;
                    }
                };
                match scalar(value) {
                    Some(text) => *slot = Some(text),
                    None => self.warn(format!(
                        "{at}: `defaults.run.{key}` is not a string and was dropped"
                    )),
                }
            }
        }
        defaults
    }

    /// Reads a whole number of minutes, dropping an expression.
    fn minutes(&mut self, at: &str, key: &str, value: Option<&Value>) -> Option<u32> {
        let value = value?;
        let minutes = value.as_u64().and_then(|m| u32::try_from(m).ok());
        if minutes.is_none() {
            self.warn(format!(
                "{at}: `{key}: {}` is not a whole number and was dropped",
                show(value)
            ));
        }
        minutes
    }

    /// Reads a map of strings, writing numbers and booleans as text.
    fn env(
        &mut self,
        at: &str,
        key: &str,
        map: Option<&BTreeMap<String, Value>>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, value) in map.into_iter().flatten() {
            match scalar(value) {
                Some(text) => {
                    out.insert(name.clone(), text);
                }
                None => self.warn(format!(
                    "{at}: `{key}.{name}` is not a string and was dropped"
                )),
            }
        }
        out
    }

    /// Converts one step. `None` when it is an error, which is recorded.
    fn step(&mut self, job_at: &str, index: usize, value: &Value) -> Option<Step> {
        let label = ["name", "id"]
            .iter()
            .find_map(|k| value.get(k).and_then(scalar))
            .map_or_else(|| format!("{}", index + 1), |n| format!("`{n}`"));
        let at = format!("{job_at} step {label}");
        let Some(map) = value.as_object() else {
            self.error(format!("{at} is not a map"));
            return None;
        };
        for key in map.keys().filter(|k| !STEP_KEYS.contains(&k.as_str())) {
            self.warn(format!("{at}: unknown key `{key}` was dropped"));
        }
        let gh = match GhStep::deserialize(value) {
            Ok(gh) => gh,
            Err(err) => {
                self.error(format!("{at}: {err}"));
                return None;
            }
        };
        let continue_on_error = match &gh.continue_on_error {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) if s == "true" || s == "false" => s == "true",
            Some(other) => {
                self.warn(format!(
                    "{at}: `continue-on-error: {}` is an expression and was dropped; hatchrun takes only true or false",
                    show(other)
                ));
                false
            }
        };
        let env = self.env(&at, "env", gh.env.as_ref());
        let if_ = gh.if_.as_ref().and_then(scalar);
        match (gh.run, gh.uses) {
            (Some(run), None) => {
                if gh.with.as_ref().is_some_and(|w| !w.is_empty()) {
                    self.warn(format!("{at}: `with` on a run step was dropped"));
                }
                let timeout_minutes =
                    self.minutes(&at, "timeout-minutes", gh.timeout_minutes.as_ref());
                Some(Step::Run(RunStep {
                    name: gh.name,
                    id: gh.id,
                    run,
                    shell: gh.shell,
                    working_directory: gh.working_directory,
                    env,
                    if_,
                    continue_on_error,
                    timeout_minutes,
                }))
            }
            (None, Some(uses)) if uses.starts_with("docker://") => {
                self.error(format!(
                    "{at} has `uses: {uses}`; Docker steps are not supported yet and come in a later release"
                ));
                None
            }
            (None, Some(uses)) => {
                for (key, set) in [
                    ("shell", gh.shell.is_some()),
                    ("working-directory", gh.working_directory.is_some()),
                    ("timeout-minutes", gh.timeout_minutes.is_some()),
                ] {
                    if set {
                        self.warn(format!(
                            "{at}: `{key}` is valid only on run steps and was dropped"
                        ));
                    }
                }
                let step = UsesStep {
                    name: gh.name,
                    id: gh.id,
                    uses,
                    with: gh.with.unwrap_or_default(),
                    env,
                    if_,
                    continue_on_error,
                };
                Some(Step::Uses(step))
            }
            (Some(_), Some(_)) => {
                self.error(format!("{at} has both `run` and `uses`"));
                None
            }
            (None, None) => {
                self.error(format!("{at} has neither `run` nor `uses`"));
                None
            }
        }
    }
}

/// One combination of matrix keys and values.
type Combo = BTreeMap<String, Value>;

/// A matrix of one combination: one value per key.
fn one_each(combo: Combo) -> Matrix {
    Matrix {
        values: combo.into_iter().map(|(k, v)| (k, vec![v])).collect(),
        exclude: Vec::new(),
    }
}

/// The matrices of a job's combinations after `include`: `plain` ones
/// no entry extended, `extended` ones with the keys added, and
/// `left_out` ones that run nowhere. `base` runs the plain ones, with
/// the others excluded, unless that is more `exclude` entries than a
/// matrix may have; then each plain one gets a matrix of its own, as
/// each extended one does. When every combination was extended alike,
/// `base` takes the added keys and runs them all.
fn group(
    mut base: Matrix,
    plain: Vec<Combo>,
    extended: Vec<(Combo, Combo)>,
    left_out: Vec<Combo>,
) -> Vec<Matrix> {
    let mut out = Vec::new();
    let same = extended.first().map(|(_, extra)| extra);
    if plain.is_empty() && left_out.is_empty() && extended.iter().all(|(_, e)| Some(e) == same) {
        if let Some(extra) = same {
            for (k, v) in extra {
                base.values.insert(k.clone(), vec![v.clone()]);
            }
            out.push(base);
        }
        return out;
    }
    let taken = extended.len() + left_out.len();
    if !plain.is_empty() && base.exclude.len() + taken <= plan::MAX_EXCLUDE {
        base.exclude
            .extend(extended.iter().map(|(combo, _)| combo.clone()));
        base.exclude.extend(left_out);
        out.push(base);
    } else {
        out.extend(plain.into_iter().map(one_each));
    }
    out.extend(extended.into_iter().map(|(mut combo, extra)| {
        combo.extend(extra);
        one_each(combo)
    }));
    out
}

/// Lists the entries of each converted job, given with its GitHub id,
/// with `requires` naming every entry each needed job became.
fn flatten(entries: Vec<(String, Vec<WorkflowJob>)>) -> Vec<WorkflowJob> {
    let names: BTreeMap<String, Vec<String>> = entries
        .iter()
        .map(|(id, e)| {
            (
                id.clone(),
                e.iter().filter_map(|e| e.name.clone()).collect(),
            )
        })
        .collect();
    entries
        .into_iter()
        .flat_map(|(_, e)| e)
        .map(|mut entry| {
            entry.requires = entry
                .requires
                .iter()
                .flat_map(|r| {
                    names.get(&r.job).into_iter().flatten().map(|name| Require {
                        job: name.clone(),
                        statuses: r.statuses,
                    })
                })
                .collect();
            entry
        })
        .collect()
}

/// The workflow entries of job `key`, GitHub id `id`, one per matrix.
/// The first is named `id` and the others `id-2`, `id-3` and on,
/// skipping the ids of the workflow's other jobs.
fn entries(
    id: &str,
    key: &str,
    ids: &BTreeSet<String>,
    requires: &[String],
    matrices: Vec<Matrix>,
) -> Vec<WorkflowJob> {
    let mut n = 1;
    matrices
        .into_iter()
        .enumerate()
        .map(|(i, matrix)| {
            let name = if i == 0 {
                id.to_owned()
            } else {
                loop {
                    n += 1;
                    let name = format!("{id}-{n}");
                    if !ids.contains(&name) {
                        break name;
                    }
                }
            };
            WorkflowJob {
                job: key.to_owned(),
                name: Some(name),
                requires: requires
                    .iter()
                    .map(String::as_str)
                    .map(Require::new)
                    .collect(),
                matrix,
                ..WorkflowJob::default()
            }
        })
        .collect()
}

/// `defaults.run` of a workflow or job: what its run steps get when they
/// set no shell or working directory of their own.
#[derive(Default)]
struct RunDefaults {
    shell: Option<String>,
    working_directory: Option<String>,
}

impl RunDefaults {
    /// Fills in what each run step leaves unset.
    fn apply(&self, steps: &mut [Step]) {
        for step in steps {
            if let Step::Run(run) = step {
                if run.shell.is_none() {
                    run.shell.clone_from(&self.shell);
                }
                if run.working_directory.is_none() {
                    run.working_directory.clone_from(&self.working_directory);
                }
            }
        }
    }
}

/// The template label a GitHub `runs-on` label becomes, if any: the
/// label itself, the template a retired Ubuntu label moved to, or the
/// template of a third-party label's arch and size.
fn runner_label(label: &str) -> Option<&'static str> {
    if let Some((_, new)) = OLD_UBUNTU.iter().find(|(old, _)| *old == label) {
        return Some(new);
    }
    if let Some(found) = template_labels().into_iter().find(|l| *l == label) {
        return Some(found);
    }
    vendor_template(label).map(|(t, _)| t.labels[0])
}

/// The warning for a label that became another template label, saying
/// when a third-party label's size was rounded.
fn became(at: &str, old: &str, new: &str) -> String {
    let size = match vendor_template(old) {
        Some((t, asked)) if asked > t.vcpu => format!(
            "; it asks for {asked} vCPUs and the largest template has {}",
            t.vcpu
        ),
        Some((t, asked)) if asked < t.vcpu => {
            format!("; it asks for {asked} vCPUs, rounded up to {}", t.vcpu)
        }
        _ => String::new(),
    };
    format!("{at}: `runs-on: {old}` became `{new}`, which runs Ubuntu 26.04{size}")
}

/// The label `runs-on` names: a string, a list of one, or the `labels`
/// of a `{labels: ...}` map. `None` for anything else, including a
/// runner `group`.
fn runs_on_text(runs_on: &Value) -> Option<&str> {
    match runs_on {
        Value::String(label) => Some(label),
        Value::Array(labels) if labels.len() == 1 => labels[0].as_str(),
        Value::Object(map) if map.len() == 1 => runs_on_text(map.get("labels")?),
        _ => None,
    }
}

/// The key `KEY` of `${{ matrix.KEY }}`, when that is the whole text.
fn matrix_key(text: &str) -> Option<&str> {
    let key = text
        .trim()
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim()
        .strip_prefix("matrix.")?;
    let ok = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then_some(key)
}

/// A dispatch input default as a value of `kind`: numeric strings become
/// integers and `"true"`/`"false"` booleans. `None` when it does not fit.
fn fit_default(kind: &ParameterKind, value: &Value) -> Option<Value> {
    match kind {
        ParameterKind::String => scalar(value).map(Value::String),
        ParameterKind::Boolean => match value {
            Value::Bool(_) => Some(value.clone()),
            Value::String(s) if s == "true" || s == "false" => Some(Value::Bool(s == "true")),
            _ => None,
        },
        ParameterKind::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Some(value.clone()),
            Value::String(s) => s.trim().parse::<i64>().ok().map(Value::from),
            _ => None,
        },
        ParameterKind::Enum(options) => scalar(value)
            .filter(|s| options.contains(s))
            .map(Value::String),
    }
}

/// Why a job that had errors is missing from those that need it.
/// What happened to a job that had errors, after "which".
const FAILED: &str = "did not convert";

/// What happened to a reusable workflow call, after "which".
const REUSABLE: &str = "calls a reusable workflow";

/// What in a converted job's expressions `validate` would reject: the
/// GitHub contexts it reads that hatchrun does not provide, in
/// [`UNSUPPORTED_CONTEXTS`] order, and, as error messages, each
/// expression that does not parse, reads an unknown context or reads
/// `parameters`.
fn expression_problems(job: &Job) -> (Vec<&'static str>, Vec<String>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut conds: Vec<&str> = job.if_.iter().map(String::as_str).collect();
    texts.extend(job.env.values().map(String::as_str));
    texts.extend(job.outputs.values().map(String::as_str));
    for step in &job.steps {
        match step {
            Step::Run(run) => {
                texts.push(&run.run);
                texts.extend(run.name.as_deref());
                texts.extend(run.shell.as_deref());
                texts.extend(run.working_directory.as_deref());
                texts.extend(run.env.values().map(String::as_str));
                conds.extend(run.if_.as_deref());
            }
            Step::Uses(uses) => {
                texts.push(&uses.uses);
                texts.extend(uses.name.as_deref());
                for value in uses.with.values() {
                    strings(value, &mut texts);
                }
                texts.extend(uses.env.values().map(String::as_str));
                conds.extend(uses.if_.as_deref());
            }
            Step::Checkout(checkout) => {
                texts.extend(checkout.r#ref.as_deref());
                texts.extend(checkout.path.as_deref());
            }
            _ => {}
        }
    }
    let mut bad = Vec::new();
    let mut exprs: Vec<(&str, usize)> = Vec::new();
    for cond in conds {
        if cond.contains("${{") {
            texts.push(cond);
        } else {
            exprs.push((cond, 0));
        }
    }
    for text in texts {
        match expr::expressions(text) {
            Ok(found) => exprs.extend(found.into_iter().map(|(start, inner)| (inner, start))),
            Err(e) => bad.push(format!("has a bad expression: {e}")),
        }
    }
    let mut read = BTreeSet::new();
    let shifted = |e: ExprError, start: usize| ExprError {
        message: e.message,
        offset: e.offset + start,
    };
    for (inner, start) in exprs {
        let names = match expr::contexts(inner) {
            Ok(names) => names,
            Err(e) => {
                bad.push(format!("has a bad expression: {}", shifted(e, start)));
                continue;
            }
        };
        for (name, offset) in names {
            if let Some(&c) = UNSUPPORTED_CONTEXTS.iter().find(|c| **c == name) {
                read.insert(c);
            } else if name == "parameters" {
                bad.push("reads `parameters`, which a converted job does not declare".to_owned());
            } else if !expr::CONTEXTS.contains(&name.as_ref()) {
                let e = ExprError::new(format!("unknown context '{name}'"), offset);
                bad.push(format!("has a bad expression: {}", shifted(e, start)));
                break;
            }
        }
    }
    let read = UNSUPPORTED_CONTEXTS
        .iter()
        .copied()
        .filter(|c| read.contains(*c))
        .collect();
    (read, bad)
}

/// Adds every string inside `value` to `out`.
fn strings<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// The reason Docker-phase keys are dropped.
const LATER: &str = "; Docker services and containers come in a later release";

/// What `on.workflow_call` is in an `on:` value, when it has one: its
/// map, or null.
fn workflow_call(on: &Value) -> Option<Value> {
    match on {
        Value::String(name) if name == "workflow_call" => Some(Value::Null),
        Value::Array(names) if names.iter().any(|n| n.as_str() == Some("workflow_call")) => {
            Some(Value::Null)
        }
        Value::Object(events) => events.get("workflow_call").cloned(),
        _ => None,
    }
}

/// Whether an `on:` value has `workflow_call`.
fn calls_workflow(on: &Value) -> bool {
    workflow_call(on).is_some()
}

/// A reader of the dotted names in an expression: given the parts of
/// one, such as `["inputs", "target"]`, it returns how many parts it
/// replaces and what with.
type Names<'a> = dyn FnMut(&[&str]) -> Option<(usize, String)> + 'a;

/// The inside of a text that is one `${{ }}` and nothing else.
fn whole(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("${{")?.strip_suffix("}}")?;
    (!inner.contains("${{") && !inner.contains("}}")).then_some(inner.trim())
}

/// Whether an expression is a plain name, which needs no parentheses.
fn simple(expr: &str) -> bool {
    !expr.is_empty()
        && expr
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// A value as an expression: an expression that was its whole text
/// stays, anything else is a literal.
fn expr_text(value: &Value) -> String {
    match value {
        Value::String(text) => match whole(text) {
            Some(expr) if simple(expr) => expr.to_owned(),
            Some(expr) => format!("({expr})"),
            None => format!("'{}'", text.replace('\'', "''")),
        },
        Value::Null => "''".to_owned(),
        other => other.to_string(),
    }
}

/// A text with each dotted name `names` answers replaced. Quoted strings
/// and the parts of longer names are left alone. `bare` says the text is
/// an expression; otherwise the expressions are the `${{ }}` in it.
fn rewrite(text: &str, bare: bool, names: &mut Names<'_>) -> String {
    if bare {
        return rewrite_expr(text, names);
    }
    let Ok(found) = expr::expressions(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for (start, inner) in found.into_iter().rev() {
        out.replace_range(start..start + inner.len(), &rewrite_expr(inner, names));
    }
    out
}

/// [`rewrite`] of one expression.
fn rewrite_expr(expr: &str, names: &mut Names<'_>) -> String {
    let bytes = expr.as_bytes();
    let part = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let mut out = String::with_capacity(expr.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == b'\'' {
                    if bytes.get(end + 1) == Some(&b'\'') {
                        end += 2;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
            let end = (end + 1).min(bytes.len());
            out.push_str(&expr[i..end]);
            i = end;
        } else if start(b) && (i == 0 || !(part(bytes[i - 1]) || bytes[i - 1] == b'.')) {
            let mut ends = Vec::new();
            let mut end = i;
            loop {
                while end < bytes.len() && part(bytes[end]) {
                    end += 1;
                }
                ends.push(end);
                if bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(|b| start(*b)) {
                    end += 1;
                } else {
                    break;
                }
            }
            let mut from = i;
            let parts: Vec<&str> = ends
                .iter()
                .map(|&to| {
                    let name = &expr[from..to];
                    from = to + 1;
                    name
                })
                .collect();
            let call = bytes.get(end) == Some(&b'(');
            if let Some((n, replacement)) = names(&parts).filter(|_| !call) {
                out.push_str(&replacement);
                i = ends[n - 1];
            } else {
                out.push_str(&expr[i..end]);
                i = end;
            }
        } else {
            let ch = expr[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8().max(1);
        }
    }
    out
}

/// Rewrites one text, which is a bare expression when the flag is set.
type Texts<'a> = dyn FnMut(&str, bool) -> String + 'a;

/// Rewrites the expressions of a value's strings, at any depth.
fn rewrite_value(value: &mut Value, names: &mut Names<'_>) {
    walk_value(value, &mut |text, bare| rewrite(text, bare, names));
}

/// [`rewrite_value`] with a rewriter of whole texts.
fn walk_value(value: &mut Value, texts: &mut Texts<'_>) {
    match value {
        Value::String(text) => *text = texts(text, false),
        Value::Array(items) => items.iter_mut().for_each(|v| walk_value(v, texts)),
        Value::Object(map) => map.values_mut().for_each(|v| walk_value(v, texts)),
        _ => {}
    }
}

/// Rewrites every expression of a converted job.
fn map_job(job: &mut Job, names: &mut Names<'_>) {
    walk_job(job, &mut |text, bare| rewrite(text, bare, names));
}

/// [`map_job`] with a rewriter of whole texts.
fn walk_job(job: &mut Job, texts: &mut Texts<'_>) {
    fn text(text: &mut String, texts: &mut Texts<'_>) {
        *text = texts(text, false);
    }
    fn opt(text: &mut Option<String>, texts: &mut Texts<'_>) {
        if let Some(t) = text {
            *t = texts(t, false);
        }
    }
    fn cond(text: &mut Option<String>, texts: &mut Texts<'_>) {
        if let Some(t) = text {
            *t = texts(t, !t.contains("${{"));
        }
    }
    job.env.values_mut().for_each(|v| text(v, texts));
    job.outputs.values_mut().for_each(|v| text(v, texts));
    opt(&mut job.runs_on, texts);
    cond(&mut job.if_, texts);
    for step in &mut job.steps {
        match step {
            Step::Run(run) => {
                opt(&mut run.name, texts);
                text(&mut run.run, texts);
                opt(&mut run.shell, texts);
                opt(&mut run.working_directory, texts);
                run.env.values_mut().for_each(|v| text(v, texts));
                cond(&mut run.if_, texts);
            }
            Step::Uses(uses) => {
                opt(&mut uses.name, texts);
                uses.with.values_mut().for_each(|v| walk_value(v, texts));
                uses.env.values_mut().for_each(|v| text(v, texts));
                cond(&mut uses.if_, texts);
            }
            _ => {}
        }
    }
}

/// The value a call gives an input, when it is a literal: not a list, a
/// map or text with an expression in it.
fn literal_input<'v>(inputs: &'v BTreeMap<String, Value>, name: &str) -> Option<&'v Value> {
    inputs.get(name).filter(|v| match v {
        Value::String(s) => !s.contains("${{"),
        Value::Array(_) | Value::Object(_) => false,
        _ => true,
    })
}

/// Makes the reads of `inputs.NAME` that are not a whole `${{ }}` into
/// the value the call gave, which `parameters` cannot do: they are
/// evaluated at run time, where parameters are not. A read of an input
/// whose value is not a literal stays, and the result is a message for
/// each such input.
fn bind_literals(job: &mut Job, inputs: &BTreeMap<String, Value>) -> Vec<String> {
    let literal = |name: &str| literal_input(inputs, name);
    let mut stuck = BTreeSet::new();
    let mut expr = |inner: &str| {
        rewrite_expr(inner, &mut |parts| match parts {
            ["inputs", name, ..] => {
                let value = literal(name);
                if value.is_none() {
                    stuck.insert((*name).to_owned());
                }
                value.map(|v| (2, expr_text(v)))
            }
            _ => None,
        })
    };
    walk_job(job, &mut |text, bare| {
        if bare {
            return expr(text);
        }
        let Ok(found) = expr::expressions(text) else {
            return text.to_owned();
        };
        let mut out = text.to_owned();
        for (start, inner) in found.into_iter().rev() {
            // `parameters` can be read as `${{ parameters.NAME }}` and
            // nothing else.
            let plain = inner
                .trim()
                .strip_prefix("inputs.")
                .is_some_and(|name| simple(name) && !name.contains('.'));
            if !plain {
                out.replace_range(start..start + inner.len(), &expr(inner));
            }
        }
        out
    });
    stuck
        .into_iter()
        .map(|name| {
            format!(
                "reads `inputs.{name}` inside an expression, and the call does not give it a literal; only a whole `${{{{ inputs.{name} }}}}` can be passed on"
            )
        })
        .collect()
}

/// Answers reads of `inputs.NAME` with the value the call gave it.
fn input_mapper(
    inputs: &BTreeMap<String, Value>,
) -> impl FnMut(&[&str]) -> Option<(usize, String)> {
    move |parts| match parts {
        ["inputs", name, ..] => Some((2, expr_text(inputs.get(*name)?))),
        _ => None,
    }
}

/// A value with the reads of `inputs.NAME` in it replaced by the values
/// the call gave. A text that is only such a read becomes the value.
fn substitute(value: &Value, inputs: &BTreeMap<String, Value>) -> Value {
    if let Value::String(text) = value
        && let Some(name) = whole(text).and_then(|e| e.strip_prefix("inputs."))
        && let Some(found) = inputs.get(name)
    {
        return found.clone();
    }
    let mut value = value.clone();
    rewrite_value(&mut value, &mut input_mapper(inputs));
    value
}

/// Answers reads of `context.JOB.outputs.NAME`, `context` being `needs`
/// in the jobs of a file and `jobs` in the outputs of a called file: a
/// job of a called file by its id in the workflow, and a job that made a
/// call by what the call says. A read of a call's output that has no
/// expression is replaced by an empty string, and a warning is added.
fn needs_mapper<'a>(
    scope: &'a Scope,
    plain: &'a BTreeSet<String>,
    calls: &'a BTreeMap<String, Expansion>,
    context: &'a str,
    warned: &'a mut Vec<String>,
) -> impl FnMut(&[&str]) -> Option<(usize, String)> + 'a {
    move |parts| {
        let [first, job, rest @ ..] = parts else {
            return None;
        };
        if *first != context {
            return None;
        }
        if plain.contains(*job) {
            let renamed = context == "jobs" || !scope.prefix.is_empty();
            return renamed.then(|| (2, format!("needs.{}", scope.full(job))));
        }
        let call = calls.get(*job)?;
        if let ["outputs", name, ..] = rest
            && let Some(Some(expr)) = call.outputs.get(*name)
        {
            return Some((4, expr.clone()));
        }
        let used = parts.len().min(4);
        warned.push(format!(
            "`{}` was dropped: the called workflow gives no such output",
            parts[..used].join(".")
        ));
        Some((used, "''".to_owned()))
    }
}

/// `a && b` for two conditions, each bare or one `${{ }}`. `None` when
/// one of them is text with expressions in it.
fn both(call: &str, job: Option<&str>) -> Option<String> {
    let bare = |c: &str| match whole(c) {
        Some(expr) => Some(expr.to_owned()),
        None => (!c.contains("${{")).then(|| c.trim().to_owned()),
    };
    let call = bare(call)?;
    match job {
        None => Some(call),
        Some(job) => Some(format!("({call}) && ({})", bare(job)?)),
    }
}

/// A string, number or boolean as text. `None` for lists and maps; null
/// is the empty string.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// A value as it appears in a message.
fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
