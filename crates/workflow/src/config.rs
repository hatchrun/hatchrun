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

use crate::concurrency::Concurrency;
use crate::diagnostic::Diagnostic;
use crate::permissions::Permissions;
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

fn later_docker<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(de::Error::custom(
        "docker is not supported in an executor; set `container` on the job",
    ))
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
    /// A failed job counts as a success for the jobs that require it and
    /// for the run, when this is `true` or an expression that is true
    /// when the job ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<ContinueOnError>,
    /// Values later jobs can read.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
    /// What the job's token may do; replaces the workflow's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// The environment the job deploys to. It can read
    /// `${{ parameters.x }}`, and the `hatchrun`, `github`, `needs`,
    /// `matrix`, `strategy`, `vars` and `inputs` contexts, which are
    /// evaluated when the job is ready to start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Where the deployment can be seen. Evaluated when the job ends,
    /// with the job's own contexts (`steps`, `job`, `env`), and shown on
    /// the job's check run and in the app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_url: Option<String>,
    /// At most one job of a group runs in the repository, one waits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Concurrency>,
    /// Runs the steps inside this container instead of on the runner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    /// Containers that run beside the job, by the hostname the steps
    /// reach them under.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, Container>,
    /// The steps.
    pub steps: Vec<Step>,
}

/// A container image the job runs in (`container`) or beside
/// (`services`), with GitHub's keys. In YAML a bare string is the
/// image. Every string may hold `${{ }}` over the `github`, `needs`,
/// `matrix`, `strategy`, `vars`, `secrets` and `inputs` contexts, read
/// when the job starts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Container {
    /// The image, such as `postgres:16` or `ghcr.io/acme/build:1`.
    pub image: String,
    /// The registry login for a private image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,
    /// Environment variables of the container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Ports to publish, as `docker create -p` reads them: `5432`,
    /// `8080:80`, `127.0.0.1:8080:80/tcp`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    /// Volumes to mount, as `docker create -v` reads them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// More `docker create` options, such as `--cpus 1` or
    /// `--health-cmd "pg_isready"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
}

/// The login `docker login` gets for a container's image.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// The user name.
    pub username: String,
    /// The password or token, usually `${{ secrets.X }}`.
    pub password: String,
}

/// A string that may be written as a number in YAML, such as a port.
#[derive(Debug)]
struct Text(String);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Text;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a number")
            }

            fn visit_str<E>(self, v: &str) -> Result<Text, E> {
                Ok(Text(v.to_owned()))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Text, E> {
                Ok(Text(v.to_string()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Text, E> {
                Ok(Text(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[derive(Default)]
struct ContainerMap {
    image: String,
    credentials: Option<Credentials>,
    env: BTreeMap<String, String>,
    ports: Vec<Text>,
    volumes: Vec<Text>,
    options: Option<String>,
}

impl<'de> Deserialize<'de> for Container {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Container;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an image or a map with `image`")
            }

            fn visit_str<E: de::Error>(self, image: &str) -> Result<Container, E> {
                if image.trim().is_empty() {
                    return Err(E::custom("a container needs an `image`"));
                }
                Ok(Container {
                    image: image.to_owned(),
                    ..Container::default()
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Container, A::Error> {
                let map = ContainerMap::deserialize(de::value::MapAccessDeserializer::new(map))?;
                if map.image.trim().is_empty() {
                    return Err(de::Error::custom("a container needs an `image`"));
                }
                Ok(Container {
                    image: map.image,
                    credentials: map.credentials,
                    env: map.env,
                    ports: map.ports.into_iter().map(|t| t.0).collect(),
                    volumes: map.volumes.into_iter().map(|t| t.0).collect(),
                    options: map.options,
                })
            }
        }
        d.deserialize_any(V)
    }
}

impl Container {
    /// Every string the container holds, with the key it came from as
    /// it reads in a message.
    #[must_use]
    pub fn texts(&self) -> Vec<(String, &str)> {
        let mut out = vec![("image".to_owned(), self.image.as_str())];
        if let Some(c) = &self.credentials {
            out.push(("credentials.username".to_owned(), &c.username));
            out.push(("credentials.password".to_owned(), &c.password));
        }
        out.extend(
            self.env
                .iter()
                .map(|(k, v)| (format!("env.{k}"), v.as_str())),
        );
        out.extend(self.ports.iter().map(|p| ("ports".to_owned(), p.as_str())));
        out.extend(
            self.volumes
                .iter()
                .map(|v| ("volumes".to_owned(), v.as_str())),
        );
        if let Some(o) = &self.options {
            out.push(("options".to_owned(), o));
        }
        out
    }

    /// Replaces every string the container holds with what `f` makes
    /// of it.
    pub fn map_texts(&mut self, f: &mut dyn FnMut(&str) -> String) {
        self.image = f(&self.image);
        if let Some(c) = &mut self.credentials {
            c.username = f(&c.username);
            c.password = f(&c.password);
        }
        for v in self.env.values_mut() {
            *v = f(v);
        }
        for v in self.ports.iter_mut().chain(self.volumes.iter_mut()) {
            *v = f(v);
        }
        if let Some(o) = &mut self.options {
            *o = f(o);
        }
    }
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
    continue_on_error: Option<ContinueOnError>,
    outputs: BTreeMap<String, String>,
    permissions: Option<Permissions>,
    environment: Option<String>,
    environment_url: Option<String>,
    concurrency: Option<Concurrency>,
    container: Option<Container>,
    services: BTreeMap<String, Container>,
    steps: Vec<Step>,
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
            continue_on_error: job.continue_on_error,
            outputs: job.outputs,
            permissions: job.permissions,
            environment: job.environment,
            environment_url: job.environment_url,
            concurrency: job.concurrency,
            container: job.container,
            services: job.services,
            steps: job.steps,
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
            continue_on_error: de.continue_on_error,
            outputs: de.outputs,
            permissions: de.permissions,
            environment: de.environment,
            environment_url: de.environment_url,
            concurrency: de.concurrency,
            container: de.container,
            services: de.services,
            steps: de.steps,
        }
    }
}

/// The `continue-on-error` of a job: `true`, `false`, or an expression
/// read when the job ends, such as `${{ matrix.experimental }}`. A bare
/// expression (`matrix.experimental`) works as it does in `if:`. The
/// expression may read the `hatchrun`, `github`, `needs`, `matrix`,
/// `strategy`, `vars` and `inputs` contexts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ContinueOnError {
    /// Always, or never.
    Always(bool),
    /// When the expression is true.
    When(String),
}

/// How the copies of a workflow entry's matrix run together.
///
/// `fail-fast` (default `true`) cancels the other copies that are
/// waiting, queued or running when one fails, unless its job has
/// `continue-on-error`. `max-parallel` caps how many copies run at once.
/// Entries that share a `group` count as one matrix for both, and for
/// the `strategy` context. The entry's own name is its group unless
/// this names another; a converted GitHub job whose `include` entries
/// became several workflow entries uses it to stay one matrix.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Strategy {
    /// A failed copy cancels the others.
    #[serde(skip_serializing_if = "is_true")]
    pub fail_fast: bool,
    /// The most copies that run at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
    /// The name entries share to count as one matrix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl Default for Strategy {
    fn default() -> Self {
        Self {
            fail_fast: true,
            max_parallel: None,
            group: None,
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

/// One step of a job or a command.
///
/// In YAML a bare string (`- setup`) or a map with one key that names a
/// command (`- setup: { version: 2 }`) is a [`Step::Command`], except
/// `- checkout`, which is a [`Step::Checkout`] with no options. The
/// other kinds are maps keyed by `run`, `uses`, `checkout`, `cache`,
/// `workspace` or `test-results`.
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
    /// Check out the repository at the event's commit.
    Checkout(CheckoutStep),
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

/// A `checkout:` step. The host adapter gives the agent the clone URL
/// and a token for the repository the event came from; no other
/// repository can be checked out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct CheckoutStep {
    /// The branch, tag or commit to check out; the event's commit when
    /// absent.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// How many commits of history to fetch; 0 fetches all of it.
    #[serde(skip_serializing_if = "is_one")]
    pub fetch_depth: u32,
    /// Which submodules to check out too.
    #[serde(skip_serializing_if = "Submodules::is_none")]
    pub submodules: Submodules,
    /// Download LFS files.
    #[serde(skip_serializing_if = "is_false")]
    pub lfs: bool,
    /// Where to check out, relative to the job's working directory; the
    /// working directory itself when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Default for CheckoutStep {
    fn default() -> Self {
        Self {
            r#ref: None,
            fetch_depth: 1,
            submodules: Submodules::None,
            lfs: false,
            path: None,
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_one(value: &u32) -> bool {
    *value == 1
}

/// The `submodules` option of a checkout step: `false`, `true` or
/// `recursive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Submodules {
    /// None of them.
    #[default]
    None,
    /// The repository's own submodules.
    Top,
    /// Submodules and theirs, all the way down.
    Recursive,
}

impl Submodules {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_none(&self) -> bool {
        *self == Submodules::None
    }
}

impl<'de> Deserialize<'de> for Submodules {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(false) => Ok(Submodules::None),
            Raw::Bool(true) => Ok(Submodules::Top),
            Raw::Text(text) if text == "recursive" => Ok(Submodules::Recursive),
            Raw::Text(text) => Err(de::Error::invalid_value(
                de::Unexpected::Str(&text),
                &"true, false or recursive",
            )),
        }
    }
}

impl Serialize for Submodules {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Submodules::None => serializer.serialize_bool(false),
            Submodules::Top => serializer.serialize_bool(true),
            Submodules::Recursive => serializer.serialize_str("recursive"),
        }
    }
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
    /// The step is cancelled after this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
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
    checkout: Option<CheckoutStep>,
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
        if let Some(checkout) = self.checkout {
            only("checkout")?;
            return Ok(Step::Checkout(checkout));
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
                    timeout_minutes: self.timeout_minutes,
                }))
            }
            (None, None) => Err(
                "a step needs run, uses, checkout, cache, workspace, test-results or a command name"
                    .into(),
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
                if name == "checkout" {
                    return Ok(Step::Checkout(CheckoutStep::default()));
                }
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
                        "checkout" => {
                            let checkout: Option<CheckoutStep> = map.next_value()?;
                            f.checkout = Some(checkout.unwrap_or_default());
                        }
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
            Step::Checkout(checkout) if *checkout == CheckoutStep::default() => {
                serializer.serialize_str("checkout")
            }
            Step::Checkout(checkout) => single_entry(serializer, "checkout", checkout),
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
    /// The title of each run, such as `Deploy ${{ inputs.target }}`. It
    /// reads the `hatchrun`, `github`, `inputs` and `vars` contexts and
    /// is evaluated when the run is made.
    #[serde(default, rename = "run-name", skip_serializing_if = "Option::is_none")]
    pub run_name: Option<String>,
    /// What the token of every job without its own `permissions` may do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// At most one run of a group runs in the repository, one waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Concurrency>,
    /// The jobs, wired together with `requires`.
    pub jobs: Vec<WorkflowJob>,
}

/// One entry of a workflow's `jobs:` list.
///
/// In YAML a bare job name or a map with one key, the job name, whose
/// value holds `requires`, `type: approval`, `branches`, `tags`,
/// `matrix`, `strategy`, `name` and parameter values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowJob {
    /// The job from `jobs:`, or the approval's own name.
    pub job: String,
    /// A display name, needed when one job appears twice.
    pub name: Option<String>,
    /// Entries that must end first, each with the end states it must
    /// reach for this entry to start.
    pub requires: Vec<Require>,
    /// The entry waits for a person to approve; it names no job.
    pub approval: bool,
    /// Branches the entry runs on.
    pub branches: Option<Filter>,
    /// Tags the entry runs on.
    pub tags: Option<Filter>,
    /// Parameter values to run the job with, one copy per combination.
    pub matrix: Matrix,
    /// How the copies run together; `None` is the defaults.
    pub strategy: Option<Strategy>,
    /// Parameter values for the job.
    pub with: BTreeMap<String, Value>,
}

/// One entry of a workflow job's `requires:` list.
///
/// In YAML a bare entry id (`- build`), which asks for
/// [`Statuses::SUCCESS`], or a map with one key, the entry id, whose
/// value is one status or a list of them: `- test: failed`,
/// `- lint: [failed, not_run]`, `- deploy: terminal`. It is written back
/// in the shortest of these forms.
///
/// The statuses are checked over the required entry and every entry
/// before it, not only the entry itself: see [`Statuses`] and
/// [`plan::ready`](crate::plan::ready).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Require {
    /// The id of the required entry: its `name`, else its key.
    pub job: String,
    /// The end states that let the requiring entry start.
    pub statuses: Statuses,
}

impl Require {
    /// Requires `job` to succeed, as a bare id does.
    pub fn new(job: impl Into<String>) -> Self {
        Self {
            job: job.into(),
            statuses: Statuses::SUCCESS,
        }
    }
}

/// How a job ended, as a `requires` entry sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// Every step passed, or an approval was given.
    Success,
    /// The job never started: its `requires` statuses or its `if:` kept
    /// it from running.
    NotRun,
    /// A step failed, or an approval was refused.
    Failed,
    /// The run was cancelled, or the job ran out of time.
    Canceled,
}

impl Outcome {
    /// Every outcome, weakest first, in the order an ancestry
    /// aggregates them: see [`Statuses`].
    pub const ALL: [Self; 4] = [Self::Success, Self::NotRun, Self::Failed, Self::Canceled];

    /// The name a `requires` status uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotRun => "not_run",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The end states a `requires` entry accepts: a set of [`Outcome`]s.
///
/// A required entry is judged by the aggregate outcome of its ancestry:
/// the entry itself and every entry before it, transitively through
/// their `requires`, counting each planned job (matrix leg, parallel
/// node) of those entries. The aggregate is the strongest outcome among
/// them, in the order `canceled` > `failed` > `not_run` > `success`:
///
/// - `canceled` when any of them was cancelled;
/// - else `failed` when any of them failed;
/// - else `not_run` when any of them did not run;
/// - else `success`: all of them succeeded.
///
/// This is how GitHub's job-level status functions read the jobs
/// before a job. So when A fails and B, which requires A, does not run,
/// an entry requiring `B: failed` runs: B's aggregate is `failed`. The
/// default, `success`, means every ancestor succeeded; `terminal`
/// accepts any end state.
///
/// An entry with several `requires` starts only when each of them,
/// judged on its own ancestry, reaches one of its own statuses.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Statuses(u8);

impl Statuses {
    /// No end state: the entry never starts.
    pub const NONE: Self = Self(0);
    /// Every ancestor succeeded. The default.
    pub const SUCCESS: Self = Self(1 << Outcome::Success as u8);
    /// The aggregate is `not_run`: an ancestor did not run, and none
    /// failed or was cancelled.
    pub const NOT_RUN: Self = Self(1 << Outcome::NotRun as u8);
    /// The aggregate is `failed`: an ancestor failed, and none was
    /// cancelled.
    pub const FAILED: Self = Self(1 << Outcome::Failed as u8);
    /// The aggregate is `canceled`: an ancestor was cancelled.
    pub const CANCELED: Self = Self(1 << Outcome::Canceled as u8);
    /// Any end state.
    pub const TERMINAL: Self = Self(0b1111);

    /// Whether `outcome` is one of the statuses.
    #[must_use]
    pub fn contains(self, outcome: Outcome) -> bool {
        self.0 & outcome.bit() != 0
    }

    /// The statuses in both sets.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The statuses in either set.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether no end state is accepted.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The outcomes in the set, weakest first.
    pub fn outcomes(self) -> impl Iterator<Item = Outcome> {
        Outcome::ALL.into_iter().filter(move |o| self.contains(*o))
    }

    /// Parses one status name.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "success" => Self::SUCCESS,
            "failed" => Self::FAILED,
            "canceled" => Self::CANCELED,
            "not_run" => Self::NOT_RUN,
            "terminal" => Self::TERMINAL,
            _ => return None,
        })
    }

    /// The names written for the set: `terminal`, else each outcome.
    fn names(self) -> Vec<&'static str> {
        if self == Self::TERMINAL {
            vec!["terminal"]
        } else {
            self.outcomes().map(Outcome::name).collect()
        }
    }
}

impl Default for Statuses {
    fn default() -> Self {
        Self::SUCCESS
    }
}

/// Shown as YAML writes it: one name, or a list of names.
impl fmt::Display for Statuses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.names().as_slice() {
            [one] => f.write_str(one),
            many => write!(f, "[{}]", many.join(", ")),
        }
    }
}

impl fmt::Debug for Statuses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The status names a `requires` entry accepts.
const STATUS_NAMES: &str = "success, failed, canceled, not_run or terminal";

impl<'de> Deserialize<'de> for Statuses {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StatusesVisitor;

        impl<'de> Visitor<'de> for StatusesVisitor {
            type Value = Statuses;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a status ({STATUS_NAMES}) or a list of them")
            }

            fn visit_str<E: de::Error>(self, name: &str) -> Result<Statuses, E> {
                Statuses::from_name(name).ok_or_else(|| {
                    E::custom(format!("unknown status `{name}`; use {STATUS_NAMES}"))
                })
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Statuses, A::Error> {
                let mut all = Statuses::NONE;
                while let Some(name) = seq.next_element::<String>()? {
                    all = all.union(StatusesVisitor.visit_str::<A::Error>(&name)?);
                }
                if all.is_empty() {
                    return Err(de::Error::custom(
                        "a requires entry lists no statuses; give at least one",
                    ));
                }
                Ok(all)
            }
        }

        deserializer.deserialize_any(StatusesVisitor)
    }
}

impl Serialize for Statuses {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.names().as_slice() {
            [one] => serializer.serialize_str(one),
            many => many.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Require {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RequireVisitor;

        impl<'de> Visitor<'de> for RequireVisitor {
            type Value = Require;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an entry id or a map of one entry id to its statuses")
            }

            fn visit_str<E: de::Error>(self, job: &str) -> Result<Require, E> {
                Ok(Require::new(job))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Require, A::Error> {
                let Some(job) = map.next_key::<String>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                let statuses = map.next_value()?;
                if let Some(other) = map.next_key::<String>()? {
                    return Err(de::Error::custom(format!(
                        "a requires entry is a map with one key, the entry id; found `{other}` after `{job}`"
                    )));
                }
                Ok(Require { job, statuses })
            }
        }

        deserializer.deserialize_any(RequireVisitor)
    }
}

impl Serialize for Require {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.statuses == Statuses::SUCCESS {
            serializer.serialize_str(&self.job)
        } else {
            single_entry(serializer, &self.job, &self.statuses)
        }
    }
}

/// The `matrix` of a workflow job entry.
///
/// In YAML a map of keys to value lists, with `exclude` holding the
/// partial combinations left out. A combination is left out when it
/// matches every key of one `exclude` entry.
///
/// A matrix can also be read when the entry becomes ready, after the
/// jobs it requires have ended: the whole `matrix` as one `${{ }}`
/// expression, a key's list or the `include` list as one. Such a matrix
/// is dynamic. `include` adds combinations and keys to the ones the
/// other parts make, as on GitHub Actions, in either kind of matrix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Matrix {
    /// Each key and its values; every combination of them runs.
    pub values: BTreeMap<String, Vec<Value>>,
    /// Partial combinations that do not run.
    pub exclude: Vec<BTreeMap<String, Value>>,
    /// Combinations `include` adds, or keys it adds to combinations.
    /// Applied after `exclude`.
    pub include: Vec<BTreeMap<String, Value>>,
    /// The whole matrix as an expression that reads as a map of value
    /// lists, with `include` and `exclude` allowed in it.
    pub from: Option<String>,
    /// Keys whose value list is an expression read when the entry
    /// becomes ready.
    pub lists: BTreeMap<String, String>,
    /// `include` as an expression that reads as a list of maps.
    pub include_from: Option<String>,
}

impl Matrix {
    /// Whether the matrix has nothing in it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
            && self.exclude.is_empty()
            && self.include.is_empty()
            && !self.dynamic()
    }

    /// Whether some of the matrix is read when the entry becomes ready,
    /// so the entry cannot be expanded when the workflow is planned.
    #[must_use]
    pub fn dynamic(&self) -> bool {
        self.from.is_some() || !self.lists.is_empty() || self.include_from.is_some()
    }

    /// Every expression the matrix reads when the entry becomes ready,
    /// with the part it belongs to as it reads in a message.
    pub fn expressions(&self) -> impl Iterator<Item = (String, &str)> {
        self.from
            .iter()
            .map(|e| ("`matrix`".to_owned(), e.as_str()))
            .chain(
                self.lists
                    .iter()
                    .map(|(k, e)| (format!("matrix key `{k}`"), e.as_str())),
            )
            .chain(
                self.include_from
                    .iter()
                    .map(|e| ("matrix `include`".to_owned(), e.as_str())),
            )
    }
}

impl std::ops::Index<&str> for Matrix {
    type Output = Vec<Value>;

    fn index(&self, key: &str) -> &Vec<Value> {
        &self.values[key]
    }
}

/// The text of a matrix part that is an expression.
fn matrix_expression(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|text| text.trim_start().starts_with("${{"))
}

fn matrix_list<E: de::Error>(key: &str, value: Value) -> Result<Vec<Value>, E> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(E::custom(format!(
            "matrix `{key}` must be a list or a `${{{{ }}}}` expression, found {other}"
        ))),
    }
}

fn matrix_maps<E: de::Error>(key: &str, value: Value) -> Result<Vec<BTreeMap<String, Value>>, E> {
    let Value::Array(items) = value else {
        return Err(E::custom(format!(
            "matrix `{key}` must be a list of maps or a `${{{{ }}}}` expression, found {value}"
        )));
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Object(map) => Ok(map.into_iter().collect()),
            other => Err(E::custom(format!(
                "matrix `{key}` entries must be maps, found {other}"
            ))),
        })
        .collect()
}

impl<'de> Deserialize<'de> for Matrix {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MatrixVisitor;

        impl<'de> Visitor<'de> for MatrixVisitor {
            type Value = Matrix;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of value lists and `exclude`, or a `${{ }}` expression")
            }

            fn visit_unit<E>(self) -> Result<Matrix, E> {
                Ok(Matrix::default())
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Matrix, E> {
                if !text.trim_start().starts_with("${{") {
                    return Err(E::custom(
                        "a matrix that is text must be one `${{ }}` expression",
                    ));
                }
                Ok(Matrix {
                    from: Some(text.to_owned()),
                    ..Matrix::default()
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Matrix, A::Error> {
                let mut matrix = Matrix::default();
                while let Some(key) = map.next_key::<String>()? {
                    let value: Value = map.next_value()?;
                    match (key.as_str(), matrix_expression(&value)) {
                        ("include", Some(text)) => matrix.include_from = Some(text.to_owned()),
                        ("include", None) => matrix.include = matrix_maps(&key, value)?,
                        ("exclude", _) => matrix.exclude = matrix_maps(&key, value)?,
                        (_, Some(text)) => {
                            matrix.lists.insert(key, text.to_owned());
                        }
                        (_, None) => {
                            let values = matrix_list(&key, value)?;
                            matrix.values.insert(key, values);
                        }
                    }
                }
                Ok(matrix)
            }
        }

        deserializer.deserialize_any(MatrixVisitor)
    }
}

impl Serialize for Matrix {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(from) = &self.from {
            return serializer.serialize_str(from);
        }
        let mut map = serializer.serialize_map(None)?;
        for (key, values) in &self.values {
            map.serialize_entry(key, values)?;
        }
        for (key, expr) in &self.lists {
            map.serialize_entry(key, expr)?;
        }
        if let Some(expr) = &self.include_from {
            map.serialize_entry("include", expr)?;
        } else if !self.include.is_empty() {
            map.serialize_entry("include", &self.include)?;
        }
        if !self.exclude.is_empty() {
            map.serialize_entry("exclude", &self.exclude)?;
        }
        map.end()
    }
}

/// The body of a workflow job entry: everything but the job name.
struct WorkflowJobBody(WorkflowJob);

impl<'de> Deserialize<'de> for WorkflowJobBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BodyVisitor;

        impl<'de> Visitor<'de> for BodyVisitor {
            type Value = WorkflowJobBody;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a map of requires, type, branches, tags, matrix, strategy, name and parameters",
                )
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
                        "strategy" => job.strategy = Some(map.next_value()?),
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
            && j.strategy.is_none()
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
        if let Some(strategy) = &j.strategy {
            map.serialize_entry("strategy", strategy)?;
        }
        for (key, value) in &j.with {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
