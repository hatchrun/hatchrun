// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reusable workflows: a workflow entry that runs another workflow.
//!
//! A workflow with `on: call:` can be called by an entry with
//! `call: <workflow>` (a workflow of the same config) or
//! `call: owner/repo/path.yml@ref` (a workflow file of another
//! repository). The callee declares what it takes and gives:
//!
//! ```yaml
//! workflows:
//!   deploy:
//!     on:
//!       call:
//!         inputs:
//!           env: { type: string, required: true }
//!         secrets:
//!           TOKEN: { required: true }
//!         outputs:
//!           url: ${{ jobs.ship.outputs.url }}
//!     jobs: [ship]
//!   release:
//!     on: { push: { tags: ['v*'] } }
//!     jobs:
//!       - build
//!       - prod:
//!           call: deploy
//!           requires: [build]
//!           with: { env: prod }
//!           secrets: { TOKEN: '${{ secrets.PROD_TOKEN }}' }
//! ```
//!
//! The planner puts the callee's jobs in the caller's plan as
//! `<call> / <job>`, each with a [`CallFrame`] that says what the call
//! passed. Jobs outside the call see the call as one entry: `needs.prod`
//! reads the callee's `outputs`, and `requires: [prod]` waits for every
//! job of the callee.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::expr::{self, Functions, JobStatus, evaluate, interpolate};
use crate::plan::PlannedJob;
use crate::secrets::DEBUG_SECRETS;

/// How many reusable workflows may be nested: a workflow calls one,
/// which calls one, and so on up to this many calls deep.
pub const MAX_CALL_DEPTH: usize = 4;

/// The most different reusable workflows one plan may call.
pub const MAX_UNIQUE_CALLEES: usize = 20;

/// The type of a workflow input, as GitHub names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    /// Text; the default type.
    #[default]
    String,
    /// `true` or `false`.
    Boolean,
    /// A number.
    Number,
}

impl InputType {
    /// The word a message uses for the type.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Boolean => "a boolean",
            Self::Number => "a number",
        }
    }

    /// Why `value` is not of this type, `None` when it is, or when it
    /// is an expression that run time reads.
    #[must_use]
    pub fn mismatch(self, value: &Value) -> Option<String> {
        if let Value::String(text) = value
            && text.contains("${{")
        {
            return None;
        }
        let fits = match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
            Self::Number => value.is_number(),
        };
        (!fits).then(|| format!("must be {}, found {value}", self.word()))
    }

    /// `value` as this type, the way GitHub passes inputs on: a string
    /// input takes any scalar as its text, a boolean input takes
    /// `"true"` and `"false"`, a number input takes a numeric text.
    ///
    /// # Errors
    ///
    /// A message when `value` cannot be read as the type.
    pub fn coerce(self, value: &Value) -> Result<Value, String> {
        match (self, value) {
            (Self::String, Value::Null) => Ok(Value::String(String::new())),
            (Self::String, Value::Number(n)) => Ok(Value::String(n.to_string())),
            (Self::String, Value::Bool(b)) => Ok(Value::String(b.to_string())),
            (Self::String, Value::String(_))
            | (Self::Boolean, Value::Bool(_))
            | (Self::Number, Value::Number(_)) => Ok(value.clone()),
            (Self::Boolean, Value::String(text)) if text == "true" => Ok(Value::Bool(true)),
            (Self::Boolean, Value::String(text)) if text == "false" => Ok(Value::Bool(false)),
            (Self::Number, Value::String(text)) => text
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .or_else(|_| text.trim().parse::<f64>().map(Value::from))
                .map_err(|_| format!("`{text}` is not a number")),
            _ => Err(format!("{value} is not {}", self.word())),
        }
    }
}

/// An input of a reusable workflow.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallInput {
    /// The value type.
    #[serde(rename = "type", skip_serializing_if = "is_string")]
    pub kind: InputType,
    /// The value used when the call gives none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// The call must give a value, unless there is a default.
    #[serde(skip_serializing_if = "is_false")]
    pub required: bool,
    /// Shown to people reading the workflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_string(kind: &InputType) -> bool {
    *kind == InputType::String
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// A secret a reusable workflow takes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallSecret {
    /// The call must pass it, unless it passes `inherit`.
    #[serde(skip_serializing_if = "is_false")]
    pub required: bool,
    /// Shown to people reading the workflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `on: call`: what a workflow takes and gives when another workflow
/// calls it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallTrigger {
    /// The values the call may give, read as `${{ inputs.NAME }}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, CallInput>,
    /// The secrets the call may pass, read as `${{ secrets.NAME }}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, CallSecret>,
    /// What the call gives back, each an expression over
    /// `${{ jobs.ENTRY.outputs.NAME }}`; the caller reads it as
    /// `needs.CALL.outputs.NAME`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
}

/// The secrets a call passes to its workflow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SecretsPassed {
    /// None: the callee reads only `GITHUB_TOKEN`.
    #[default]
    None,
    /// `inherit`: every secret the caller can read.
    Inherit,
    /// Each callee secret name and the whole expression
    /// `${{ secrets.NAME }}` of the caller's secret it takes.
    Named(BTreeMap<String, String>),
}

impl SecretsPassed {
    /// Whether nothing is passed.
    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl Serialize for SecretsPassed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::None => serializer.serialize_map(Some(0))?.end(),
            Self::Inherit => serializer.serialize_str("inherit"),
            Self::Named(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for (name, value) in map {
                    out.serialize_entry(name, value)?;
                }
                out.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SecretsPassed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Passed;

        impl<'de> Visitor<'de> for Passed {
            type Value = SecretsPassed;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`inherit` or a map of secret names to `${{ secrets.NAME }}`")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<SecretsPassed, E> {
                if text == "inherit" {
                    Ok(SecretsPassed::Inherit)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(text), &self))
                }
            }

            fn visit_unit<E: de::Error>(self) -> Result<SecretsPassed, E> {
                Ok(SecretsPassed::None)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SecretsPassed, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((name, value)) = map.next_entry::<String, String>()? {
                    out.insert(name.to_ascii_uppercase(), value);
                }
                Ok(if out.is_empty() {
                    SecretsPassed::None
                } else {
                    SecretsPassed::Named(out)
                })
            }
        }

        deserializer.deserialize_any(Passed)
    }
}

/// The secret a value passes on, when it is one whole
/// `${{ secrets.NAME }}`: the name, uppercased.
#[must_use]
pub fn passed_secret(value: &str) -> Option<String> {
    let inner = value.trim().strip_prefix("${{")?.strip_suffix("}}")?.trim();
    let name = inner.strip_prefix("secrets.")?;
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| name.to_ascii_uppercase())
}

/// Where a call finds its workflow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallTarget {
    /// A workflow of the same config, by its key.
    Local(String),
    /// A workflow file of a repository.
    Remote(RemoteCall),
}

/// `owner/repo/path.yml@ref`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteCall {
    /// The repository owner.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// The path of the workflow file in the repository.
    pub path: String,
    /// The branch, tag or commit.
    pub git_ref: String,
}

impl RemoteCall {
    /// `owner/repo`.
    #[must_use]
    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

impl fmt::Display for RemoteCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}@{}",
            self.owner, self.repo, self.path, self.git_ref
        )
    }
}

impl CallTarget {
    /// Reads the value of an entry's `call:`: a workflow key, or
    /// `owner/repo/path@ref`.
    ///
    /// # Errors
    ///
    /// A message that says what is wrong with the value.
    pub fn parse(call: &str) -> Result<Self, String> {
        let call = call.trim();
        if call.is_empty() {
            return Err("`call` is empty".to_owned());
        }
        if call.starts_with("./") {
            return Err(format!(
                "`call: {call}` is a path; name a workflow of this config (`call: deploy`), or a file of another repository (`call: owner/repo/.github/workflows/x.yml@ref`)"
            ));
        }
        if !call.contains('/') && !call.contains('@') {
            return Ok(Self::Local(call.to_owned()));
        }
        let Some((path, git_ref)) = call.rsplit_once('@') else {
            return Err(format!(
                "`call: {call}` names a file of another repository without a ref; write `owner/repo/path.yml@ref`"
            ));
        };
        let mut parts = path.splitn(3, '/');
        let (Some(owner), Some(repo), Some(file)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("`call: {call}` is not `owner/repo/path.yml@ref`"));
        };
        let plain = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !plain(owner) || !plain(repo) || file.is_empty() || git_ref.is_empty() {
            return Err(format!("`call: {call}` is not `owner/repo/path.yml@ref`"));
        }
        // The ref goes into an API path, so it follows git's ref rules
        // where they keep it inside that path.
        let bad_ref = git_ref
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            || git_ref.contains("..")
            || git_ref.contains("@{")
            || git_ref.to_ascii_lowercase().ends_with(".lock")
            || git_ref.chars().any(|c| {
                c.is_control()
                    || c.is_whitespace()
                    || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
            });
        if bad_ref {
            return Err(format!(
                "`call: {call}` has the ref `{git_ref}`, which is not a valid git ref: it may not have an empty, `.` or `..` part, `..`, spaces, control characters or any of `~^:?*[\\`"
            ));
        }
        if file.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(format!(
                "`call: {call}` has a path with `..` or an empty part"
            ));
        }
        Ok(Self::Remote(RemoteCall {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            path: file.to_owned(),
            git_ref: git_ref.to_owned(),
        }))
    }
}

/// A workflow file of another repository, read and converted by the
/// host before planning.
#[derive(Debug, Clone)]
pub struct Callee {
    /// The file as a config: the workflow in [`Callee::workflow`], and
    /// what it calls and runs.
    pub config: Config,
    /// The key of the workflow the file made in `config`.
    pub workflow: String,
    /// The commit the ref resolved to.
    pub sha: String,
}

/// The workflow files of other repositories a plan may call, by the
/// call as written. The planner does no I/O: the host reads each file
/// with [`remote_calls`], resolves its ref to a commit, and adds it here.
#[derive(Debug, Clone, Default)]
pub struct Library {
    files: BTreeMap<RemoteCall, Callee>,
    refused: BTreeMap<RemoteCall, String>,
    offline: bool,
    limit: bool,
}

impl Library {
    /// A library for a host that cannot read other repositories, such
    /// as the command line: a call of a file it does not have plans no
    /// jobs, where [`Library::default`] makes it an error.
    #[must_use]
    pub fn offline() -> Self {
        Self {
            offline: true,
            ..Self::default()
        }
    }

    /// Whether a call of a missing file plans nothing.
    pub(crate) fn is_offline(&self) -> bool {
        self.offline
    }

    /// Adds the file `call` names.
    pub fn insert(&mut self, call: RemoteCall, callee: Callee) {
        self.files.insert(call, callee);
    }

    /// Records that the host stopped reading files for a workflow that
    /// reaches more than [`MAX_UNIQUE_CALLEES`] of them.
    pub fn mark_limit(&mut self) {
        self.limit = true;
    }

    /// Whether the host stopped reading files at the limit.
    #[must_use]
    pub fn limit_reached(&self) -> bool {
        self.limit
    }

    /// Records why the host did not read the file `call` names, which a
    /// plan that calls it reports.
    pub fn refuse(&mut self, call: RemoteCall, reason: impl Into<String>) {
        self.refused.insert(call, reason.into());
    }

    /// Why the file `call` names was not read, when the host said.
    #[must_use]
    pub fn refusal(&self, call: &RemoteCall) -> Option<&str> {
        self.refused.get(call).map(String::as_str)
    }

    /// The file `call` names, when it was added.
    #[must_use]
    pub fn get(&self, call: &RemoteCall) -> Option<&Callee> {
        self.files.get(call)
    }
}

/// The files of other repositories `config` calls that `library` does
/// not have yet, in call order, once each. A file in `library` can call
/// more; ask again after adding what this returns, until it is empty.
#[must_use]
pub fn remote_calls(config: &Config, library: &Library) -> Vec<RemoteCall> {
    let all: Vec<&str> = config.workflows.keys().map(String::as_str).collect();
    remote_calls_from(config, &all, library)
}

/// [`remote_calls`] for the calls the workflows `workflows` of `config`
/// reach: their own, those of the workflows of the config they call, and
/// those of the files already in `library`. A workflow or file is
/// walked once, so a file that calls itself ends the walk; the planner
/// reports the cycle.
#[must_use]
pub fn remote_calls_from(
    config: &Config,
    workflows: &[&str],
    library: &Library,
) -> Vec<RemoteCall> {
    let mut out: Vec<RemoteCall> = Vec::new();
    let mut walked: BTreeSet<(String, String)> = BTreeSet::new();
    let mut todo: Vec<(&Config, &str, String)> = workflows
        .iter()
        .map(|name| (config, *name, String::new()))
        .collect();
    while let Some((current, name, origin)) = todo.pop() {
        if !walked.insert((origin.clone(), name.to_owned())) {
            continue;
        }
        let Some(wf) = current.workflows.get(name) else {
            continue;
        };
        for entry in &wf.jobs {
            match entry.call.as_deref().map(CallTarget::parse) {
                Some(Ok(CallTarget::Local(key))) => {
                    if let Some((key, _)) = current.workflows.get_key_value(&key) {
                        todo.push((current, key.as_str(), origin.clone()));
                    }
                }
                Some(Ok(CallTarget::Remote(call))) => match library.get(&call) {
                    Some(callee) => {
                        todo.push((&callee.config, callee.workflow.as_str(), call.to_string()));
                    }
                    None if library.refusal(&call).is_some() => {}
                    None if !out.contains(&call) => out.push(call),
                    None => {}
                },
                _ => {}
            }
        }
    }
    out
}

/// What a call passed to the workflow it runs, kept on every job of
/// that workflow. The outermost call comes first in
/// [`PlannedJob::scope`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CallFrame {
    /// The planned id of this call, unique in the plan: the caller's
    /// frame id and ` / ` in front of the entry id, then `(<values>)`
    /// for a matrix entry.
    pub id: String,
    /// The calling entry's id in its own workflow: what `needs` and
    /// `requires` there name.
    pub name: String,
    /// The workflow that runs: its key, or `owner/repo/path.yml` for a
    /// file of another repository.
    pub workflow: String,
    /// Where a workflow of another repository came from, with the ref
    /// resolved to a commit: `owner/repo/path.yml@<sha>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The values given to the inputs, expressions of the caller's
    /// contexts included, with the inputs' defaults for those left out.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub with: BTreeMap<String, Value>,
    /// The type of each input, for reading `with`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kinds: BTreeMap<String, InputType>,
    /// The secrets the call passes.
    #[serde(default, skip_serializing_if = "SecretsPassed::is_none")]
    pub secrets: SecretsPassed,
    /// This call's matrix entry, read as `matrix.X` by `with`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub matrix: BTreeMap<String, Value>,
    /// The call's `if`: the jobs of the workflow run only when it holds.
    #[serde(rename = "if", default, skip_serializing_if = "Option::is_none")]
    pub if_: Option<String>,
    /// What the call gives back, from the callee's `on.call.outputs`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
}

impl CallFrame {
    /// The length of the string fields, for the planner's byte budget.
    pub(crate) fn bytes(&self) -> usize {
        let map = |m: &BTreeMap<String, String>| {
            m.iter().map(|(k, v)| k.len() + v.len() + 64).sum::<usize>()
        };
        self.id.len()
            + self.name.len()
            + self.workflow.len()
            + self.source.as_ref().map_or(0, String::len)
            + self.if_.as_ref().map_or(0, String::len)
            + map(&self.outputs)
            + self
                .with
                .iter()
                .map(|(k, v)| k.len() + 64 + v.to_string().len())
                .sum::<usize>()
            + self.matrix.len() * 96
            + match &self.secrets {
                SecretsPassed::Named(m) => map(m),
                _ => 0,
            }
            + 256
    }
}

/// How many of `scope`'s frames, from the outermost, `other` runs in
/// too.
#[must_use]
pub fn shared_depth(scope: &[CallFrame], other: &PlannedJob) -> usize {
    scope
        .iter()
        .zip(&other.scope)
        .take_while(|(a, b)| a.id == b.id)
        .count()
}

/// The name of the entry `required` is, read from a job that runs in
/// `scope`: see [`PlannedJob::entry_of`].
#[must_use]
pub fn entry_in<'a>(scope: &[CallFrame], required: &'a PlannedJob) -> &'a str {
    match required.scope.get(shared_depth(scope, required)) {
        Some(frame) => &frame.name,
        None => &required.name,
    }
}

impl PlannedJob {
    /// The id of the call this job runs in, `None` for a job of the
    /// top workflow.
    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        self.scope.last().map(|f| f.id.as_str())
    }

    /// The text before this job's own id in the plan: the ids of the
    /// calls it runs in, each followed by ` / `.
    #[must_use]
    pub fn scope_prefix(&self) -> String {
        self.scope
            .last()
            .map_or_else(String::new, |f| format!("{} / ", f.id))
    }

    /// How many frames this job and `other` share, from the outermost.
    #[must_use]
    pub fn shared_frames(&self, other: &Self) -> usize {
        shared_depth(&self.scope, other)
    }

    /// The name of the entry `required` is, as this job reads it in
    /// `needs` and `requires`: the job's own name when both run in the
    /// same call, else the name of the outermost call of `required`
    /// that this job is not in.
    #[must_use]
    pub fn entry_of<'a>(&self, required: &'a Self) -> &'a str {
        entry_in(&self.scope, required)
    }

    /// The secrets `names` (what the job reads, uppercased) are in the
    /// outermost workflow, which is where the stored secrets are. A
    /// name a call does not pass is left out. `all` stands for every
    /// secret the job can see.
    #[must_use]
    pub fn outer_secrets(&self, names: &[String], all: bool) -> (Vec<String>, bool) {
        let mut names: Vec<String> = names.to_vec();
        let mut all = all;
        for frame in self.scope.iter().rev() {
            match &frame.secrets {
                SecretsPassed::Inherit => {}
                SecretsPassed::None => return (Vec::new(), false),
                SecretsPassed::Named(map) => {
                    let passed = |name: &String| map.get(name).and_then(|v| passed_secret(v));
                    names = if all {
                        map.values().filter_map(|v| passed_secret(v)).collect()
                    } else {
                        names.iter().filter_map(passed).collect()
                    };
                    all = false;
                }
            }
        }
        (names, all)
    }

    /// The secrets this job may read, by the names it reads them as,
    /// out of `opened`, the outermost workflow's secrets by name: the
    /// ones every call on the way passed, and no others.
    /// [`DEBUG_SECRETS`] reach every job.
    #[must_use]
    pub fn visible_secrets(&self, opened: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut seen = opened.clone();
        // The job token is not passed: every workflow of a plan has it,
        // so a call further in can pass it on under another name.
        let token = opened.get("GITHUB_TOKEN");
        for frame in &self.scope {
            seen = match &frame.secrets {
                SecretsPassed::Inherit => seen,
                SecretsPassed::None => BTreeMap::new(),
                SecretsPassed::Named(map) => map
                    .iter()
                    .filter_map(|(name, value)| {
                        let from = passed_secret(value)?;
                        Some((name.clone(), seen.get(&from)?.clone()))
                    })
                    .collect(),
            };
            if let Some(token) = token {
                seen.entry("GITHUB_TOKEN".to_owned())
                    .or_insert_with(|| token.clone());
            }
        }
        for name in DEBUG_SECRETS {
            if let Some(value) = opened.get(name) {
                seen.insert(name.to_owned(), value.clone());
            }
        }
        seen
    }
}

/// What an expression of a call reads on the control plane: no files, a
/// job that has not failed.
struct Plain(JobStatus);

impl Functions for Plain {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, expr::ExprError> {
        Err(expr::ExprError::new("hashFiles runs on the runner", 0))
    }

    fn status(&self) -> JobStatus {
        self.0
    }
}

/// Reads a value a call gives: a text that is one `${{ }}` is the
/// expression's value, any other text is interpolated, and any other
/// value stays.
///
/// # Errors
///
/// A message when an expression cannot be read.
pub fn read_value(value: &Value, ctx: &Value, status: JobStatus) -> Result<Value, String> {
    let Value::String(text) = value else {
        return Ok(value.clone());
    };
    if !text.contains("${{") {
        return Ok(value.clone());
    }
    let trimmed = text.trim();
    let whole = expr::expressions(trimmed)
        .ok()
        .and_then(|found| match found.as_slice() {
            [(3, inner)] if 3 + inner.len() + 2 == trimmed.len() => Some(*inner),
            _ => None,
        });
    match whole {
        Some(inner) => evaluate(inner, ctx, &Plain(status)).map_err(|e| e.to_string()),
        None => interpolate(text, ctx, &Plain(status))
            .map(Value::String)
            .map_err(|e| e.to_string()),
    }
}

/// The `inputs` of the innermost call of `frames`: each call's `with`
/// read in the contexts of the workflow that makes it, with the inputs
/// of the call before as its `inputs`, then read as the type each input
/// has. `context_at(i, inputs)` gives the contexts of the workflow that
/// makes call `i`, given the inputs that workflow was called with
/// (`dispatch` for the outermost). The `with` of a frame has the
/// callee's defaults in it, so every input has a value.
///
/// # Errors
///
/// A message naming the call and the input that cannot be read.
pub fn resolve_inputs(
    frames: &[CallFrame],
    dispatch: &Value,
    mut context_at: impl FnMut(usize, &Value) -> Value,
) -> Result<Value, String> {
    let mut inputs = dispatch.clone();
    for (i, frame) in frames.iter().enumerate() {
        let ctx = context_at(i, &inputs);
        inputs = resolve_frame(frame, &ctx)?;
    }
    Ok(inputs)
}

/// The inputs one call gives its workflow: each `with` value read in
/// `ctx` and coerced to the type of its input.
///
/// # Errors
///
/// A message naming the call and the input that cannot be read.
pub fn resolve_frame(frame: &CallFrame, ctx: &Value) -> Result<Value, String> {
    let mut next = Map::new();
    for (name, value) in &frame.with {
        let read = read_value(value, ctx, JobStatus::Success)
            .map_err(|e| format!("`with.{name}` of `{}` could not be read: {e}", frame.id))?;
        let kind = frame.kinds.get(name).copied().unwrap_or_default();
        let read = kind.coerce(&read).map_err(|e| {
            format!(
                "`with.{name}` of `{}` must be {}: {e}",
                frame.id,
                kind.word()
            )
        })?;
        next.insert(name.clone(), read);
    }
    Ok(Value::Object(next))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_local_target_is_a_workflow_key() {
        assert_eq!(
            CallTarget::parse("deploy"),
            Ok(CallTarget::Local("deploy".into()))
        );
    }

    #[test]
    fn a_remote_target_has_owner_repo_path_and_ref() {
        let Ok(CallTarget::Remote(call)) =
            CallTarget::parse("acme/shared/.github/workflows/build.yml@v2")
        else {
            panic!("not remote");
        };
        assert_eq!(call.repository(), "acme/shared");
        assert_eq!(call.path, ".github/workflows/build.yml");
        assert_eq!(call.git_ref, "v2");
        assert_eq!(
            call.to_string(),
            "acme/shared/.github/workflows/build.yml@v2"
        );
    }

    #[test]
    fn a_remote_target_with_a_slash_in_the_ref_keeps_it() {
        let Ok(CallTarget::Remote(call)) = CallTarget::parse("a/b/c.yml@feature/x") else {
            panic!("not remote");
        };
        assert_eq!(call.git_ref, "feature/x");
    }

    #[test]
    fn bad_targets_say_why() {
        for (text, want) in [
            ("", "is empty"),
            ("./.github/workflows/x.yml", "is a path"),
            ("a/b/c.yml", "without a ref"),
            ("a/b@main", "is not `owner/repo/path.yml@ref`"),
            ("a/b/c.yml@", "is not `owner/repo/path.yml@ref`"),
            ("a/b/../c.yml@main", "`..`"),
        ] {
            let err = CallTarget::parse(text).unwrap_err();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn a_ref_cannot_steer_the_api_path() {
        for bad in [
            "../../other/repo/commits/main",
            "..",
            ".",
            "a//b",
            "a/./b",
            "a/../b",
            "x..y",
            "a b",
            "a\u{7}b",
            "a:b",
            "x.lock",
        ] {
            let err = CallTarget::parse(&format!("a/b/c.yml@{bad}")).unwrap_err();
            assert!(err.contains("not a valid git ref"), "{bad}: {err}");
        }
        for good in ["main", "v1.2.3", "feature/x-y", "0123abc"] {
            assert!(CallTarget::parse(&format!("a/b/c.yml@{good}")).is_ok());
        }
    }

    #[test]
    fn a_passed_secret_is_one_whole_reference() {
        assert_eq!(
            passed_secret("${{ secrets.npm_token }}").as_deref(),
            Some("NPM_TOKEN")
        );
        assert_eq!(passed_secret("${{ secrets.A }}-x"), None);
        assert_eq!(passed_secret("${{ secrets.A || secrets.B }}"), None);
        assert_eq!(passed_secret("plain"), None);
    }

    #[test]
    fn inputs_coerce_like_github_passes_them() {
        assert_eq!(InputType::String.coerce(&json!(3)), Ok(json!("3")));
        assert_eq!(InputType::Boolean.coerce(&json!("true")), Ok(json!(true)));
        assert_eq!(InputType::Number.coerce(&json!("4")), Ok(json!(4)));
        assert!(InputType::Boolean.coerce(&json!("yes")).is_err());
        assert!(InputType::Number.coerce(&json!("x")).is_err());
        assert!(InputType::Number.coerce(&json!(true)).is_err());
    }

    #[test]
    fn a_literal_of_the_wrong_type_is_a_mismatch_but_an_expression_is_not() {
        assert!(InputType::Boolean.mismatch(&json!("true")).is_some());
        assert!(InputType::Boolean.mismatch(&json!(true)).is_none());
        assert!(
            InputType::Number
                .mismatch(&json!("${{ inputs.n }}"))
                .is_none()
        );
        assert!(InputType::String.mismatch(&json!(1)).is_some());
    }

    fn frame(id: &str, with: &[(&str, Value)], kinds: &[(&str, InputType)]) -> CallFrame {
        CallFrame {
            id: id.to_owned(),
            name: id.to_owned(),
            workflow: "w".to_owned(),
            source: None,
            with: with
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            kinds: kinds.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
            secrets: SecretsPassed::None,
            matrix: BTreeMap::new(),
            if_: None,
            outputs: BTreeMap::new(),
        }
    }

    #[test]
    fn nested_inputs_read_the_calls_before_them() {
        let outer = frame(
            "a",
            &[("env", json!("${{ github.ref_name }}"))],
            &[("env", InputType::String)],
        );
        let inner = frame(
            "a / b",
            &[
                ("target", json!("deploy-${{ inputs.env }}")),
                ("n", json!("2")),
            ],
            &[("target", InputType::String), ("n", InputType::Number)],
        );
        let got = resolve_inputs(
            &[outer, inner],
            &json!({}),
            |i, inputs| json!({ "github": { "ref_name": "main" }, "inputs": inputs, "level": i }),
        )
        .unwrap();
        assert_eq!(got, json!({ "target": "deploy-main", "n": 2 }));
    }

    #[test]
    fn an_input_that_cannot_be_read_names_the_call() {
        let bad = frame(
            "a",
            &[("flag", json!("${{ matrix.x }}"))],
            &[("flag", InputType::Boolean)],
        );
        let err = resolve_inputs(
            &[bad],
            &json!({}),
            |_, _| json!({ "matrix": { "x": "maybe" } }),
        )
        .unwrap_err();
        assert!(err.contains("`with.flag` of `a`"), "{err}");
    }
}
