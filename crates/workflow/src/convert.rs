// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converts GitHub Actions workflows into a `.hatchrun/config.yml`.
//!
//! Each workflow file becomes one entry of `workflows:`, keyed by the file
//! stem, and each of its jobs becomes `jobs.<stem>-<job>`. Keys hatchrun has
//! no place for are dropped with a warning. Docker steps and runner labels
//! that name no template are errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

use crate::config::{
    Config, Job, Parameter, ParameterKind, RunStep, Step, UsesStep, Workflow, WorkflowJob,
};
use crate::diagnostic::Diagnostic;
use crate::templates::{template, template_labels};
use crate::trigger::{Dispatch, PullRequestTrigger, PushTrigger, Schedule, Triggers};

/// The result of a conversion: the config and what was dropped on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// The converted config.
    pub config: Config,
    /// Keys that were dropped or changed, naming the file and job.
    pub warnings: Vec<Diagnostic>,
}

/// Converts GitHub workflow files into one config.
///
/// Each pair is a file name and its contents. The workflow is keyed by the
/// file stem, so `.github/workflows/ci.yml` becomes workflow `ci`.
///
/// # Errors
///
/// Returns every problem that stops the conversion: YAML that does not
/// parse, a `runs-on` that names no runner template, a `docker://` step,
/// or two files or jobs that end up with one name.
pub fn convert(files: &[(String, String)]) -> Result<Conversion, Vec<Diagnostic>> {
    let mut cx = Cx::default();
    let mut stems: BTreeMap<String, &str> = BTreeMap::new();
    for (file, source) in files {
        let stem = stem(file);
        if let Some(other) = stems.insert(stem.to_owned(), file) {
            cx.errors.push(Diagnostic::new(format!(
                "`{other}` and `{file}` both become workflow `{stem}`; rename one of them"
            )));
            continue;
        }
        let options = serde_saphyr::options! { strict_booleans: true };
        match serde_saphyr::from_str_with_options::<GhWorkflow>(source, options) {
            Ok(gh) => cx.workflow(file, stem, gh),
            Err(err) => {
                let mut diag = Diagnostic::from(&err);
                diag.message = format!("`{file}`: {}", diag.message);
                cx.errors.push(diag);
            }
        }
    }
    if cx.errors.is_empty() {
        Ok(Conversion {
            config: cx.config,
            warnings: cx.warnings,
        })
    } else {
        Err(cx.errors)
    }
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

/// The file name without its directories and its `.yml` or `.yaml`.
fn stem(file: &str) -> &str {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    base.strip_suffix(".yml")
        .or_else(|| base.strip_suffix(".yaml"))
        .unwrap_or(base)
}

/// Labels GitHub retires that become `ubuntu-latest`.
const OLD_UBUNTU: &[&str] = &["ubuntu-24.04", "ubuntu-22.04"];

/// The events hatchrun can start a workflow on.
const EVENTS: &str = "push, pull_request, schedule and workflow_dispatch";

/// The part of a GitHub workflow file the converter reads.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
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
    jobs: Ordered<GhJob>,
}

/// A GitHub job.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
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
    steps: Option<Vec<GhStep>>,
    timeout_minutes: Option<Value>,
    strategy: Option<BTreeMap<String, Value>>,
    continue_on_error: Option<Value>,
    container: Option<Value>,
    services: Option<Value>,
    uses: Option<String>,
    #[serde(rename = "with")]
    _with: Option<Value>,
    #[serde(rename = "secrets")]
    _secrets: Option<Value>,
}

/// A GitHub step.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct GhStep {
    id: Option<String>,
    #[serde(rename = "if")]
    if_: Option<Value>,
    name: Option<String>,
    uses: Option<String>,
    run: Option<String>,
    working_directory: Option<String>,
    shell: Option<String>,
    with: Option<BTreeMap<String, Value>>,
    env: Option<BTreeMap<String, Value>>,
    continue_on_error: Option<Value>,
    timeout_minutes: Option<Value>,
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
            ("defaults", gh.defaults.is_some()),
        ] {
            if set {
                self.warn(format!(
                    "`{file}`: `{key}` is not supported and was dropped"
                ));
            }
        }
        let on = self.triggers(file, &gh.on);
        let shared_env = self.env(&format!("`{file}`"), "env", gh.env.as_ref());

        let mut dropped = BTreeSet::new();
        let mut entries = Vec::new();
        for (id, job) in gh.jobs.0 {
            let at = format!("`{file}` job `{id}`");
            if job.uses.is_some() {
                self.warn(format!(
                    "{at} calls a reusable workflow, which is not supported; the job was dropped"
                ));
                dropped.insert(id);
                continue;
            }
            let key = format!("{stem}-{id}");
            if self.config.jobs.contains_key(&key) {
                self.error(format!(
                    "{at} becomes job `{key}`, which another workflow already uses; rename one of them"
                ));
                continue;
            }
            let Some((mut converted, entry)) = self.job(&at, &id, &key, job) else {
                continue;
            };
            for (name, value) in &shared_env {
                converted
                    .env
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
            self.config.jobs.insert(key, converted);
            entries.push(entry);
        }
        for entry in &mut entries {
            entry.requires.retain(|r| !dropped.contains(r));
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
            let default = input.default.map(|value| match (&kind, value) {
                (ParameterKind::Boolean, Value::String(s)) if s == "true" || s == "false" => {
                    Value::Bool(s == "true")
                }
                (ParameterKind::String | ParameterKind::Enum(_), value) => {
                    scalar(&value).map_or(value, Value::String)
                }
                (_, value) => value,
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

    /// Converts one job into the job and its workflow entry. `None` when
    /// the job has errors, which are recorded.
    fn job(&mut self, at: &str, id: &str, key: &str, gh: GhJob) -> Option<(Job, WorkflowJob)> {
        let before = self.errors.len();
        for (name, set, why) in [
            ("services", gh.services.is_some(), LATER),
            ("container", gh.container.is_some(), LATER),
            ("concurrency", gh.concurrency.is_some(), ""),
            ("permissions", gh.permissions.is_some(), ""),
            ("environment", gh.environment.is_some(), ""),
            ("defaults", gh.defaults.is_some(), ""),
            ("continue-on-error", gh.continue_on_error.is_some(), ""),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{name}` is not supported and was dropped{why}"
                ));
            }
        }
        let runs_on = self.runs_on(at, gh.runs_on.as_ref());
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
        let matrix = self.strategy(at, gh.strategy);
        let timeout_minutes = self.minutes(at, "timeout-minutes", gh.timeout_minutes.as_ref());
        let outputs = self.env(at, "outputs", gh.outputs.as_ref());
        let env = self.env(at, "env", gh.env.as_ref());
        let steps: Vec<Step> = gh
            .steps
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, step)| self.step(at, i, step))
            .collect();
        if self.errors.len() > before {
            return None;
        }
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

    /// Maps `runs-on` to a runner template label.
    fn runs_on(&mut self, at: &str, runs_on: Option<&Value>) -> Option<String> {
        let label = match runs_on {
            Some(Value::String(label)) => Some(label.as_str()),
            Some(Value::Array(labels)) if labels.len() == 1 => labels[0].as_str(),
            _ => None,
        };
        let templates = template_labels().join(", ");
        match label {
            Some(old) if OLD_UBUNTU.contains(&old) => {
                self.warn(format!(
                    "{at}: `runs-on: {old}` became `ubuntu-latest`, which runs Ubuntu 26.04"
                ));
                Some("ubuntu-latest".to_owned())
            }
            Some(label) if template(label).is_some() => Some(label.to_owned()),
            _ => {
                let shown = runs_on.map_or_else(|| "nothing".to_owned(), show);
                self.error(format!(
                    "{at} has `runs-on: {shown}`, which names no runner template; use one of {templates}"
                ));
                None
            }
        }
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
    fn step(&mut self, job_at: &str, index: usize, gh: GhStep) -> Option<Step> {
        let label = gh
            .name
            .as_ref()
            .or(gh.id.as_ref())
            .map_or_else(|| format!("{}", index + 1), |n| format!("`{n}`"));
        let at = format!("{job_at} step {label}");
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
