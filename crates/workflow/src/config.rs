// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The data model of `.hatchrun/config.yml` and its parser.
//!
//! Parsing only checks shape. Cross references (an executor that does
//! not exist, a job with both `executor` and `runs-on`) are checked later.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diagnostic::Diagnostic;
use crate::trigger::{Filter, Triggers};

/// Parses a `.hatchrun/config.yml` file.
///
/// # Errors
///
/// Returns the problems found, each with its line and column when known,
/// or one problem when the file is over [`MAX_SOURCE`] bytes.
pub fn parse(source: &str) -> Result<Config, Vec<Diagnostic>> {
    if let Some(too_big) = too_big("the config", source) {
        return Err(vec![too_big]);
    }
    serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|err| vec![Diagnostic::from(&err)])
}

/// The largest file, in bytes, that [`parse`] and
/// [`convert`](crate::convert) read. It keeps a parsed config, and the
/// work done on each webhook, small.
pub const MAX_SOURCE: usize = 1 << 20;

/// An error when `source` is over [`MAX_SOURCE`]; `what` names it.
pub(crate) fn too_big(what: &str, source: &str) -> Option<Diagnostic> {
    (source.len() > MAX_SOURCE).then(|| {
        Diagnostic::new(format!(
            "{what} is {} bytes; the limit is {MAX_SOURCE}",
            source.len()
        ))
    })
}

/// How workflow YAML is read. Only YAML 1.2 booleans count: `y`, `yes`,
/// `on` and `off` stay strings. Comments are skipped rather than kept:
/// nothing reads them, and kept comments are capped at 32 in a row, so
/// a block commented out would fail the file.
pub(crate) fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! { strict_booleans: true, emit_comments: false }
}

/// A whole `.hatchrun/config.yml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Named runner choices jobs can share.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub executors: BTreeMap<String, Executor>,
    /// Named, parameterised step lists.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, Command>,
    /// Jobs workflows can run.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, Job>,
    /// Workflows, each with its own triggers.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, Workflow>,
}

/// A named runner choice.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "ExecutorDe", rename_all = "kebab-case")]
pub struct Executor {
    /// The runner template label.
    pub runs_on: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct ExecutorDe {
    #[serde(default, rename = "docker", deserialize_with = "later_docker")]
    _docker: (),
    runs_on: String,
}

impl From<ExecutorDe> for Executor {
    fn from(de: ExecutorDe) -> Self {
        Self {
            runs_on: de.runs_on,
        }
    }
}

/// Fails on a key that belongs to the Docker phase.
fn later<E: de::Error>(key: &str) -> E {
    E::custom(format!(
        "{key} is not supported yet; Docker services and containers come in a later release"
    ))
}

fn later_docker<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(later("docker"))
}

fn later_services<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(later("services"))
}

fn later_container<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(later("container"))
}

/// A reusable list of steps.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Command {
    /// Parameters the steps read as `${{ parameters.x }}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Parameter>,
    /// The steps.
    pub steps: Vec<Step>,
}

/// A declared parameter of a command, a job or a manual trigger.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "ParameterDe", into = "ParameterDe")]
pub struct Parameter {
    /// The value type.
    pub kind: ParameterKind,
    /// The value used when none is given.
    pub default: Option<Value>,
    /// Shown to people filling the parameter in.
    pub description: Option<String>,
}

/// The type of a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterKind {
    /// Any string.
    String,
    /// `true` or `false`.
    Boolean,
    /// A whole number.
    Integer,
    /// One of the listed strings.
    Enum(Vec<String>),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ParameterDe {
    #[serde(rename = "type")]
    kind: KindName,
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    values: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum KindName {
    String,
    Boolean,
    Integer,
    Enum,
}

impl TryFrom<ParameterDe> for Parameter {
    type Error = String;

    fn try_from(de: ParameterDe) -> Result<Self, String> {
        let kind = match (de.kind, de.values) {
            (KindName::Enum, Some(values)) => ParameterKind::Enum(values),
            (KindName::Enum, None) => return Err("an enum parameter needs `enum:` values".into()),
            (_, Some(_)) => return Err("`enum:` values need `type: enum`".into()),
            (KindName::String, None) => ParameterKind::String,
            (KindName::Boolean, None) => ParameterKind::Boolean,
            (KindName::Integer, None) => ParameterKind::Integer,
        };
        Ok(Self {
            kind,
            default: de.default,
            description: de.description,
        })
    }
}

impl From<Parameter> for ParameterDe {
    fn from(p: Parameter) -> Self {
        let (kind, values) = match p.kind {
            ParameterKind::String => (KindName::String, None),
            ParameterKind::Boolean => (KindName::Boolean, None),
            ParameterKind::Integer => (KindName::Integer, None),
            ParameterKind::Enum(values) => (KindName::Enum, Some(values)),
        };
        Self {
            kind,
            values,
            default: p.default,
            description: p.description,
        }
    }
}

/// A job: steps that run on one runner.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(from = "JobDe", rename_all = "kebab-case")]
pub struct Job {
    /// The executor to run on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// A runner template label, shorthand for an executor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<String>,
    /// Parameters the steps read as `${{ parameters.x }}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Parameter>,
    /// How many copies of the job run, splitting tests between them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    /// Environment variables for every step.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The job runs only when this expression is true.
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    /// The job is cancelled after this many minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
    /// Values later jobs can read.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
    /// The steps.
    pub steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct JobDe {
    executor: Option<String>,
    runs_on: Option<String>,
    parameters: BTreeMap<String, Parameter>,
    parallelism: Option<u32>,
    env: BTreeMap<String, String>,
    #[serde(rename = "if")]
    if_: Option<String>,
    timeout_minutes: Option<u32>,
    outputs: BTreeMap<String, String>,
    steps: Vec<Step>,
    #[serde(rename = "services", deserialize_with = "later_services")]
    _services: (),
    #[serde(rename = "container", deserialize_with = "later_container")]
    _container: (),
}

impl Default for JobDe {
    fn default() -> Self {
        let job = Job::default();
        Self {
            executor: job.executor,
            runs_on: job.runs_on,
            parameters: job.parameters,
            parallelism: job.parallelism,
            env: job.env,
            if_: job.if_,
            timeout_minutes: job.timeout_minutes,
            outputs: job.outputs,
            steps: job.steps,
            _services: (),
            _container: (),
        }
    }
}

impl From<JobDe> for Job {
    fn from(de: JobDe) -> Self {
        Self {
            executor: de.executor,
            runs_on: de.runs_on,
            parameters: de.parameters,
            parallelism: de.parallelism,
            env: de.env,
            if_: de.if_,
            timeout_minutes: de.timeout_minutes,
            outputs: de.outputs,
            steps: de.steps,
        }
    }
}

/// One step of a job or a command.
///
/// In YAML a bare string (`- setup`) or a map with one key that names a
/// command (`- setup: { version: 2 }`) is a [`Step::Command`]. The other
/// kinds are maps keyed by `run`, `uses`, `cache`, `workspace` or
/// `test-results`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A shell command.
    Run(RunStep),
    /// A GitHub action.
    Uses(UsesStep),
    /// A command from `commands:`, with parameter values.
    Command {
        /// The command's name.
        name: String,
        /// Parameter values.
        with: BTreeMap<String, Value>,
    },
    /// Restore a cache now, save it when the job ends.
    Cache(CacheStep),
    /// Pass files to later jobs of the workflow.
    Workspace(WorkspaceStep),
    /// Upload `JUnit` results from a path.
    TestResults {
        /// A file or directory of `JUnit` XML.
        path: String,
    },
}

/// A `run:` step.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RunStep {
    /// Shown in logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Lets later steps read this step's outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The script.
    pub run: String,
    /// The shell that runs the script.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// The directory the script runs in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Environment variables for this step.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The step runs only when this expression is true.
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    /// A failure does not fail the job.
    #[serde(default, skip_serializing_if = "is_false")]
    pub continue_on_error: bool,
    /// The step is cancelled after this many minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
}

/// A `uses:` step.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct UsesStep {
    /// Shown in logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Lets later steps read this step's outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The action, as `owner/repo@ref` or a local path.
    pub uses: String,
    /// The action's inputs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub with: BTreeMap<String, Value>,
    /// Environment variables for this step.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The step runs only when this expression is true.
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    /// A failure does not fail the job.
    #[serde(default, skip_serializing_if = "is_false")]
    pub continue_on_error: bool,
}

/// A `cache:` step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CacheStep {
    /// The exact key to restore and save under.
    pub key: String,
    /// Key prefixes to fall back to, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore_keys: Vec<String>,
    /// The paths cached.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A `workspace:` step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum WorkspaceStep {
    /// Save paths under `root` for later jobs.
    Persist {
        /// The directory paths are relative to.
        root: String,
        /// The paths saved.
        #[serde(default)]
        paths: Vec<String>,
    },
    /// Restore what earlier jobs saved.
    Attach {
        /// The directory to restore into.
        at: String,
    },
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// What a step map held, before deciding which kind of step it is.
#[derive(Default)]
struct StepFields {
    keys: Vec<String>,
    name: Option<String>,
    id: Option<String>,
    run: Option<String>,
    uses: Option<String>,
    with: Option<BTreeMap<String, Value>>,
    shell: Option<String>,
    working_directory: Option<String>,
    env: Option<BTreeMap<String, String>>,
    if_: Option<String>,
    continue_on_error: Option<bool>,
    timeout_minutes: Option<u32>,
    cache: Option<CacheStep>,
    workspace: Option<WorkspaceStep>,
    test_results: Option<String>,
    command: Option<(String, BTreeMap<String, Value>)>,
}

impl StepFields {
    fn into_step(self) -> Result<Step, String> {
        let only = |kind: &str| match self.keys.iter().find(|k| *k != kind) {
            Some(other) => Err(format!(
                "a {kind} step takes no other keys, found `{other}`"
            )),
            None => Ok(()),
        };
        if let Some((name, with)) = self.command {
            only(&name).map_err(|_| {
                format!("unknown step key `{name}`; a command step is a map with one key")
            })?;
            return Ok(Step::Command { name, with });
        }
        if let Some(cache) = self.cache {
            only("cache")?;
            return Ok(Step::Cache(cache));
        }
        if let Some(workspace) = self.workspace {
            only("workspace")?;
            return Ok(Step::Workspace(workspace));
        }
        if let Some(path) = self.test_results {
            only("test-results")?;
            return Ok(Step::TestResults { path });
        }
        match (self.run, self.uses) {
            (Some(_), Some(_)) => Err("a step cannot have both run and uses".into()),
            (Some(run), None) => {
                if self.with.is_some() {
                    return Err("`with` is valid only on uses steps".into());
                }
                Ok(Step::Run(RunStep {
                    name: self.name,
                    id: self.id,
                    run,
                    shell: self.shell,
                    working_directory: self.working_directory,
                    env: self.env.unwrap_or_default(),
                    if_: self.if_,
                    continue_on_error: self.continue_on_error.unwrap_or_default(),
                    timeout_minutes: self.timeout_minutes,
                }))
            }
            (None, Some(uses)) => {
                for (key, set) in [
                    ("shell", self.shell.is_some()),
                    ("working-directory", self.working_directory.is_some()),
                    ("timeout-minutes", self.timeout_minutes.is_some()),
                ] {
                    if set {
                        return Err(format!("`{key}` is valid only on run steps"));
                    }
                }
                Ok(Step::Uses(UsesStep {
                    name: self.name,
                    id: self.id,
                    uses,
                    with: self.with.unwrap_or_default(),
                    env: self.env.unwrap_or_default(),
                    if_: self.if_,
                    continue_on_error: self.continue_on_error.unwrap_or_default(),
                }))
            }
            (None, None) => Err(
                "a step needs run, uses, cache, workspace, test-results or a command name".into(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StepVisitor;

        impl<'de> Visitor<'de> for StepVisitor {
            type Value = Step;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a command name or a step map")
            }

            fn visit_str<E: de::Error>(self, name: &str) -> Result<Step, E> {
                Ok(Step::Command {
                    name: name.to_owned(),
                    with: BTreeMap::new(),
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Step, A::Error> {
                let mut f = StepFields::default();
                while let Some(key) = map.next_key::<String>()? {
                    if f.keys.contains(&key) {
                        return Err(de::Error::custom(format!("duplicate step key `{key}`")));
                    }
                    match key.as_str() {
                        "name" => f.name = Some(map.next_value()?),
                        "id" => f.id = Some(map.next_value()?),
                        "run" => f.run = Some(map.next_value()?),
                        "uses" => f.uses = Some(map.next_value()?),
                        "with" => f.with = Some(map.next_value()?),
                        "shell" => f.shell = Some(map.next_value()?),
                        "working-directory" => f.working_directory = Some(map.next_value()?),
                        "env" => f.env = Some(map.next_value()?),
                        "if" => f.if_ = Some(map.next_value()?),
                        "continue-on-error" => f.continue_on_error = Some(map.next_value()?),
                        "timeout-minutes" => f.timeout_minutes = Some(map.next_value()?),
                        "cache" => f.cache = Some(map.next_value()?),
                        "workspace" => f.workspace = Some(map.next_value()?),
                        "test-results" => f.test_results = Some(map.next_value()?),
                        _ => {
                            let with: Option<BTreeMap<String, Value>> = map.next_value()?;
                            f.command = Some((key.clone(), with.unwrap_or_default()));
                        }
                    }
                    f.keys.push(key);
                }
                f.into_step().map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(StepVisitor)
    }
}

impl Serialize for Step {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Step::Run(step) => step.serialize(serializer),
            Step::Uses(step) => step.serialize(serializer),
            Step::Command { name, with } if with.is_empty() => serializer.serialize_str(name),
            Step::Command { name, with } => single_entry(serializer, name, with),
            Step::Cache(cache) => single_entry(serializer, "cache", cache),
            Step::Workspace(workspace) => single_entry(serializer, "workspace", workspace),
            Step::TestResults { path } => single_entry(serializer, "test-results", path),
        }
    }
}

fn single_entry<S: Serializer, V: Serialize + ?Sized>(
    serializer: S,
    key: &str,
    value: &V,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(key, value)?;
    map.end()
}

/// A workflow: triggers and the jobs they run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// The events that start the workflow.
    pub on: Triggers,
    /// The jobs, wired together with `requires`.
    pub jobs: Vec<WorkflowJob>,
}

/// One entry of a workflow's `jobs:` list.
///
/// In YAML a bare job name or a map with one key, the job name, whose
/// value holds `requires`, `type: approval`, `branches`, `tags`,
/// `matrix`, `name` and parameter values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowJob {
    /// The job from `jobs:`, or the approval's own name.
    pub job: String,
    /// A display name, needed when one job appears twice.
    pub name: Option<String>,
    /// Entries that must finish first.
    pub requires: Vec<String>,
    /// The entry waits for a person to approve; it names no job.
    pub approval: bool,
    /// Branches the entry runs on.
    pub branches: Option<Filter>,
    /// Tags the entry runs on.
    pub tags: Option<Filter>,
    /// Parameter values to run the job with, one copy per combination.
    pub matrix: BTreeMap<String, Vec<Value>>,
    /// Parameter values for the job.
    pub with: BTreeMap<String, Value>,
}

/// The body of a workflow job entry: everything but the job name.
struct WorkflowJobBody(WorkflowJob);

impl<'de> Deserialize<'de> for WorkflowJobBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BodyVisitor;

        impl<'de> Visitor<'de> for BodyVisitor {
            type Value = WorkflowJobBody;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of requires, type, branches, tags, matrix, name and parameters")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut job = WorkflowJob::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => job.name = Some(map.next_value()?),
                        "requires" => job.requires = map.next_value()?,
                        "type" => {
                            let kind: String = map.next_value()?;
                            if kind != "approval" {
                                return Err(de::Error::invalid_value(
                                    de::Unexpected::Str(&kind),
                                    &"approval",
                                ));
                            }
                            job.approval = true;
                        }
                        "branches" => job.branches = Some(map.next_value()?),
                        "tags" => job.tags = Some(map.next_value()?),
                        "matrix" => job.matrix = map.next_value()?,
                        _ => {
                            let value: Value = map.next_value()?;
                            job.with.insert(key, value);
                        }
                    }
                }
                Ok(WorkflowJobBody(job))
            }
        }

        deserializer.deserialize_map(BodyVisitor)
    }
}

impl<'de> Deserialize<'de> for WorkflowJob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = WorkflowJob;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a job name or a map with one key, the job name")
            }

            fn visit_str<E: de::Error>(self, job: &str) -> Result<WorkflowJob, E> {
                Ok(WorkflowJob {
                    job: job.to_owned(),
                    ..WorkflowJob::default()
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WorkflowJob, A::Error> {
                let Some(job) = map.next_key::<String>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                let body: Option<WorkflowJobBody> = map.next_value()?;
                if let Some(other) = map.next_key::<String>()? {
                    return Err(de::Error::custom(format!(
                        "a workflow job entry is a map with one key, the job name; found `{other}` after `{job}`"
                    )));
                }
                let mut entry = body.map(|b| b.0).unwrap_or_default();
                entry.job = job;
                Ok(entry)
            }
        }

        deserializer.deserialize_any(EntryVisitor)
    }
}

impl Serialize for WorkflowJob {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let body = WorkflowJobBodyRef(self);
        if body.is_empty() {
            serializer.serialize_str(&self.job)
        } else {
            single_entry(serializer, &self.job, &body)
        }
    }
}

struct WorkflowJobBodyRef<'a>(&'a WorkflowJob);

impl WorkflowJobBodyRef<'_> {
    fn is_empty(&self) -> bool {
        let j = self.0;
        j.name.is_none()
            && j.requires.is_empty()
            && !j.approval
            && j.branches.is_none()
            && j.tags.is_none()
            && j.matrix.is_empty()
            && j.with.is_empty()
    }
}

impl Serialize for WorkflowJobBodyRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let j = self.0;
        let mut map = serializer.serialize_map(None)?;
        if let Some(name) = &j.name {
            map.serialize_entry("name", name)?;
        }
        if j.approval {
            map.serialize_entry("type", "approval")?;
        }
        if !j.requires.is_empty() {
            map.serialize_entry("requires", &j.requires)?;
        }
        if let Some(branches) = &j.branches {
            map.serialize_entry("branches", branches)?;
        }
        if let Some(tags) = &j.tags {
            map.serialize_entry("tags", tags)?;
        }
        if !j.matrix.is_empty() {
            map.serialize_entry("matrix", &j.matrix)?;
        }
        for (key, value) in &j.with {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
