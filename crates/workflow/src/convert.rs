// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converts GitHub Actions workflows into a `.hatchrun/config.yml`.
//!
//! Each workflow file becomes one entry of `workflows:`, keyed by the file
//! stem, and each of its jobs becomes `jobs.<stem>-<job>`. Keys hatchrun has
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
    Config, Job, Parameter, ParameterKind, RunStep, Step, UsesStep, Workflow, WorkflowJob, too_big,
    yaml_options,
};
use crate::diagnostic::Diagnostic;
use crate::expr::{self, ExprError};
use crate::templates::{template, template_labels};
use crate::trigger::{Dispatch, PullRequestTrigger, PushTrigger, Schedule, Triggers};

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
/// [`Conversion::errors`].
///
/// # Errors
///
/// Returns the problems when there were some and nothing converted.
pub fn convert(files: &[(String, String)]) -> Result<Conversion, Vec<Diagnostic>> {
    let mut cx = Cx::default();
    let mut stems: BTreeMap<String, &str> = BTreeMap::new();
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
            Ok(gh) => cx.workflow(file, stem, gh),
            Err(err) => {
                let mut diag = Diagnostic::from(&err);
                diag.file = Some(file.clone());
                cx.errors.push(diag);
            }
        }
    }
    if !cx.errors.is_empty() && cx.config.workflows.is_empty() {
        return Err(cx.errors);
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
const EVENTS: &str = "push, pull_request, schedule and workflow_dispatch";

/// The part of a GitHub workflow file the converter reads.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct GhWorkflow {
    #[serde(default, rename = "name")]
    _name: Option<Value>,
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
    _with: Option<Value>,
    #[serde(rename = "secrets")]
    _secrets: Option<Value>,
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

/// A map that keeps the order its keys were written in.
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

/// What the conversion has built so far.
#[derive(Default)]
struct Cx {
    config: Config,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
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
            ("permissions", gh.permissions.is_some()),
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
        let on = self.triggers(file, &gh.on);
        let shared_env = self.env(&format!("`{file}`"), "env", gh.env.as_ref());
        let shared_defaults = self.defaults(&format!("`{file}`"), gh.defaults.as_ref());

        let ids: BTreeSet<String> = gh.jobs.0.iter().map(|(id, _)| id.clone()).collect();
        // Jobs left out, with what happened to each, worded to follow
        // "which".
        let mut dropped: BTreeMap<String, String> = BTreeMap::new();
        let mut failed = false;
        let mut entries = Vec::new();
        for (id, job) in gh.jobs.0 {
            let at = format!("`{file}` job `{id}`");
            let Some(job) = self.gh_job(&at, &job) else {
                dropped.insert(id, FAILED.to_owned());
                failed = true;
                continue;
            };
            if job.uses.is_some() {
                self.warn(format!(
                    "{at} calls a reusable workflow, which is not supported; the job was dropped"
                ));
                dropped.insert(id, REUSABLE.to_owned());
                continue;
            }
            let key = format!("{stem}-{id}");
            if self.config.jobs.contains_key(&key) {
                self.error(format!(
                    "{at} becomes job `{key}`, which another workflow already uses; rename one of them"
                ));
                dropped.insert(id, FAILED.to_owned());
                failed = true;
                continue;
            }
            let Some((mut converted, entry)) = self.job(&at, &id, &key, &ids, job) else {
                dropped.insert(id, FAILED.to_owned());
                failed = true;
                continue;
            };
            shared_defaults.apply(&mut converted.steps);
            for (name, value) in &shared_env {
                converted
                    .env
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
            // validate rejects these, so the job could not run.
            let (contexts, bad) = expression_problems(&converted);
            if !contexts.is_empty() || !bad.is_empty() {
                for context in contexts {
                    self.error(format!(
                        "{at} reads the `{context}` context, which hatchrun does not provide; the job was dropped"
                    ));
                }
                for problem in bad {
                    self.error(format!("{at} {problem}; the job was dropped"));
                }
                dropped.insert(id, FAILED.to_owned());
                failed = true;
                continue;
            }
            self.config.jobs.insert(key, converted);
            entries.push(entry);
        }
        // A job that needs a dropped job would run without it, so it is
        // dropped too, and so on down the chain.
        while let Some((i, need)) = entries.iter().enumerate().find_map(|(i, e)| {
            let need = e.requires.iter().find(|r| dropped.contains_key(*r))?;
            Some((i, need.clone()))
        }) {
            let entry = entries.remove(i);
            self.config.jobs.remove(&entry.job);
            let id = entry.name.unwrap_or_default();
            let why = format!("needs `{need}`, which {}", dropped[&need]);
            self.warn(format!("`{file}` job `{id}` was dropped: it {why}"));
            dropped.insert(id, why);
        }
        // A file whose every job failed converts to nothing.
        if entries.is_empty() && failed {
            return;
        }
        self.config
            .workflows
            .insert(stem.to_owned(), Workflow { on, jobs: entries });
    }

    /// Converts `on:`, which GitHub accepts as an event name, a list of
    /// names or a map of events to their filters.
    fn triggers(&mut self, file: &str, on: &Value) -> Triggers {
        let mut triggers = Triggers::default();
        match on {
            Value::String(event) => self.event(file, event, &Value::Null, &mut triggers),
            Value::Array(events) => {
                for event in events {
                    match event {
                        Value::String(event) => {
                            self.event(file, event, &Value::Null, &mut triggers);
                        }
                        other => self.error(format!(
                            "`{file}`: `on` lists `{other}`, which is not an event name"
                        )),
                    }
                }
            }
            Value::Object(events) => {
                for (event, body) in events {
                    self.event(file, event, body, &mut triggers);
                }
            }
            other => self.error(format!(
                "`{file}`: `on` is `{other}`; it must be an event name, a list or a map"
            )),
        }
        triggers
    }

    fn event(&mut self, file: &str, event: &str, body: &Value, triggers: &mut Triggers) {
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
            other => self.warn(format!(
                "`{file}`: `on.{other}` is not supported and was dropped; hatchrun runs on {EVENTS}"
            )),
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

    /// Converts one job into the job and its workflow entry. `None` when
    /// the job has errors, which are recorded.
    fn job(
        &mut self,
        at: &str,
        id: &str,
        key: &str,
        ids: &BTreeSet<String>,
        gh: GhJob,
    ) -> Option<(Job, WorkflowJob)> {
        let before = self.errors.len();
        for (name, set, why) in [
            ("services", gh.services.is_some(), LATER),
            ("container", gh.container.is_some(), LATER),
            ("concurrency", gh.concurrency.is_some(), ""),
            ("permissions", gh.permissions.is_some(), ""),
            ("environment", gh.environment.is_some(), ""),
            ("continue-on-error", gh.continue_on_error.is_some(), ""),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{name}` is not supported and was dropped{why}"
                ));
            }
        }
        let mut matrix = self.strategy(at, gh.strategy);
        let runs_on = self.runs_on(at, gh.runs_on.as_ref(), &mut matrix);
        let requires = match gh.needs {
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
        let mut requires_unique: Vec<String> = Vec::new();
        for need in requires {
            if !ids.contains(&need) {
                self.error(format!(
                    "{at} needs `{need}`, which is not a job of the workflow"
                ));
            } else if !requires_unique.contains(&need) {
                requires_unique.push(need);
            }
        }
        let requires = requires_unique;
        let timeout_minutes = self.minutes(at, "timeout-minutes", gh.timeout_minutes.as_ref());
        let outputs = self.env(at, "outputs", gh.outputs.as_ref());
        let env = self.env(at, "env", gh.env.as_ref());
        let defaults = self.defaults(at, gh.defaults.as_ref());
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
        let job = Job {
            runs_on,
            env,
            if_: gh.if_.as_ref().and_then(scalar),
            timeout_minutes,
            outputs,
            steps,
            ..Job::default()
        };
        let entry = WorkflowJob {
            job: key.to_owned(),
            name: Some(id.to_owned()),
            requires,
            matrix,
            ..WorkflowJob::default()
        };
        Some((job, entry))
    }

    /// Maps `runs-on` to a runner template label. `runs-on: ${{ matrix.KEY }}`
    /// over a list of labels keeps the legs whose labels name one
    /// template and drops the others, such as macOS and Windows legs,
    /// with a warning.
    fn runs_on(
        &mut self,
        at: &str,
        runs_on: Option<&Value>,
        matrix: &mut BTreeMap<String, Vec<Value>>,
    ) -> Option<String> {
        if let Some(Value::String(text)) = runs_on
            && let Some(key) = matrix_key(text)
            && let Some(values) = matrix.get_mut(key)
        {
            return self.matrix_runs_on(at, text, key, values);
        }
        let label = match runs_on {
            Some(Value::String(label)) => Some(label.as_str()),
            Some(Value::Array(labels)) if labels.len() == 1 => labels[0].as_str(),
            _ => None,
        };
        if let Some(old) = label
            && let Some(new) = runner_label(old)
        {
            if new != old {
                self.warn(became(at, old, new));
            }
            return Some(new.to_owned());
        }
        let shown = runs_on.map_or_else(|| "nothing".to_owned(), show);
        self.error(format!(
            "{at} has `runs-on: {shown}`, which names no runner template; use one of {}",
            template_labels().join(", ")
        ));
        None
    }

    /// Resolves `runs-on: ${{ matrix.KEY }}`, keeping in `values` only
    /// the legs that run on the one template found, and of those only
    /// the first value for each template.
    fn matrix_runs_on(
        &mut self,
        at: &str,
        text: &str,
        key: &str,
        values: &mut Vec<Value>,
    ) -> Option<String> {
        let mut found: Vec<&'static str> = Vec::new();
        let mut kept: Vec<String> = Vec::new();
        let mut dropped = Vec::new();
        let mut duplicates = Vec::new();
        values.retain(|value| {
            let Some(new) = value.as_str().and_then(runner_label) else {
                dropped.push(format!("`{}`", show(value)));
                return false;
            };
            // Two labels of one template, such as `ubuntu-latest` and
            // `ubuntu-24.04`, are one runner: the second leg would run
            // the same job again.
            if let Some(i) = found.iter().position(|f| template(f) == template(new)) {
                duplicates.push(format!("`{}` (same runner as `{}`)", show(value), kept[i]));
                return false;
            }
            found.push(new);
            kept.push(show(value));
            true
        });
        let new = match found.as_slice() {
            [] => {
                self.error(format!(
                    "{at} has `runs-on: {text}`, and matrix key `{key}` names no runner template; use one of {}",
                    template_labels().join(", ")
                ));
                return None;
            }
            [one] => *one,
            many => {
                let shown: Vec<String> = many.iter().map(|l| format!("`{l}`")).collect();
                self.error(format!(
                    "{at} has `runs-on: {text}` over legs that run on {}; a job runs on one runner template",
                    shown.join(", ")
                ));
                return None;
            }
        };
        if !dropped.is_empty() {
            self.warn(format!(
                "{at}: matrix key `{key}` values {} name no runner template and were dropped; hatchrun runs Linux only",
                dropped.join(", ")
            ));
        }
        if !duplicates.is_empty() {
            self.warn(format!(
                "{at}: matrix key `{key}` values {} were dropped: each runs on the same runner template as an earlier value, so its legs would repeat that value's jobs",
                duplicates.join(", ")
            ));
        }
        for value in values.iter().filter_map(Value::as_str) {
            if value != new {
                self.warn(became(at, value, new));
            }
        }
        Some(new.to_owned())
    }

    /// Reads `strategy.matrix` as plain value lists, dropping the rest.
    fn strategy(
        &mut self,
        at: &str,
        strategy: Option<BTreeMap<String, Value>>,
    ) -> BTreeMap<String, Vec<Value>> {
        let mut matrix = BTreeMap::new();
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
                    Value::Array(values) if name != "include" && name != "exclude" => {
                        if values.is_empty() {
                            self.warn(format!(
                                "{at}: matrix key `{name}` has no values and was dropped"
                            ));
                        } else {
                            matrix.insert(name, values);
                        }
                    }
                    _ if name == "include" || name == "exclude" => self.warn(format!(
                        "{at}: `strategy.matrix.{name}` is not supported and was dropped; only plain value lists convert"
                    )),
                    other => self.warn(format!(
                        "{at}: matrix key `{name}` is `{}`, not a list, and was dropped",
                        show(&other)
                    )),
                }
            }
        }
        matrix
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
                Some(Step::Uses(UsesStep {
                    name: gh.name,
                    id: gh.id,
                    uses,
                    with: gh.with.unwrap_or_default(),
                    env,
                    if_,
                    continue_on_error,
                }))
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

/// The template label a GitHub `runs-on` label becomes, if any.
fn runner_label(label: &str) -> Option<&'static str> {
    if let Some((_, new)) = OLD_UBUNTU.iter().find(|(old, _)| *old == label) {
        return Some(new);
    }
    template_labels().into_iter().find(|l| *l == label)
}

/// The warning for a retired label that became a template label.
fn became(at: &str, old: &str, new: &str) -> String {
    format!("{at}: `runs-on: {old}` became `{new}`, which runs Ubuntu 26.04")
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
