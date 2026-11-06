// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reusable workflows: a job with `uses:` becomes a call entry, and
//! `on.workflow_call` becomes `on: call`.

use serde::Deserialize;

use super::rewrite::workflow_call;
use super::{
    BTreeMap, BTreeSet, Call, Cx, GhJob, Matrix, Require, Strategy, Value, WORKFLOW_DIRS,
    WorkflowJob, stem,
};
use crate::call::{
    CallInput, CallSecret, CallTarget, CallTrigger, InputType, SecretsPassed, passed_secret,
};
use crate::plan::{call_expression, output_expression};

/// An input of `on.workflow_call.inputs`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GhCallInput {
    description: Option<String>,
    default: Option<Value>,
    required: Option<bool>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// A secret of `on.workflow_call.secrets`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GhCallSecret {
    description: Option<String>,
    required: Option<bool>,
}

/// An output of `on.workflow_call.outputs`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GhCallOutput {
    value: Option<Value>,
}

impl Cx {
    /// The file `uses: ./path` names, among the files with
    /// `on.workflow_call`. The path may be under `.github`, `.forgejo` or
    /// `.gitea`; a file is found under any of them.
    pub(super) fn callee(&self, uses: &str) -> Option<String> {
        let want = uses.strip_prefix("./")?.trim_start_matches("./");
        let name = WORKFLOW_DIRS
            .iter()
            .find_map(|dir| want.strip_prefix(dir))
            .unwrap_or(want);
        let at = |file: &str, path: &str| file == path || file.ends_with(&format!("/{path}"));
        self.library
            .iter()
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

    /// Converts a job that calls a reusable workflow into an entry with
    /// `call:`. The call is the stem of a file among the converted ones
    /// for `uses: ./...`, and the `uses` itself for
    /// `owner/repo/path.yml@ref`. `with`, `secrets`, `needs`, `if`,
    /// `permissions` and a matrix written out come along. A call of a
    /// file that was not given, a matrix read at run time, `strategy`
    /// options and `concurrency` are dropped with a warning.
    pub(super) fn call(&mut self, at: &str, id: &str, ids: &BTreeSet<String>, gh: &GhJob) -> Call {
        let before = self.errors.len();
        let Some(uses) = gh.uses.as_ref().and_then(Value::as_str) else {
            self.warn(format!(
                "{at} calls a reusable workflow with a `uses` that is not text; the job was dropped"
            ));
            return Call::Dropped;
        };
        let target = if uses.starts_with("./") {
            let Some(file) = self.callee(uses) else {
                self.warn(format!(
                    "{at} calls `{uses}`, which is not among the converted files or has no `on: workflow_call`; the job was dropped"
                ));
                return Call::Dropped;
            };
            stem(&file).to_owned()
        } else if matches!(CallTarget::parse(uses), Ok(CallTarget::Remote(_))) {
            uses.to_owned()
        } else {
            self.warn(format!(
                "{at} calls `{uses}`, which is neither a file of this repository (`./.github/workflows/x.yml`) nor `owner/repo/path.yml@ref`; the job was dropped"
            ));
            return Call::Dropped;
        };
        if gh.concurrency.is_some() {
            self.warn(format!(
                "{at}: `concurrency` on a job that calls a reusable workflow is not supported and was dropped"
            ));
        }
        let (matrix, settings) = self.strategy(at, gh.strategy.clone());
        if matrix.dynamic() {
            self.warn(format!(
                "{at} calls `{uses}` with a matrix that is read at run time, which a call does not support yet; the job was dropped"
            ));
            return Call::Dropped;
        }
        if settings != Strategy::default() {
            self.warn(format!(
                "{at}: `strategy.fail-fast` and `strategy.max-parallel` on a call are not supported and were dropped"
            ));
        }
        let matrix = Matrix {
            values: matrix.values,
            exclude: matrix.exclude,
            include: matrix.include,
            ..Matrix::default()
        };
        let requires = self.needs(at, gh.needs.clone(), ids);
        let permissions = self.permissions(at, gh.permissions.as_ref(), true);
        let with = self.call_with(at, gh);
        let secrets = self.call_secrets(at, gh);
        let if_ = gh
            .if_
            .as_ref()
            .and_then(super::calls::scalar)
            .filter(|c| !c.trim().is_empty());
        let mut problems = Vec::new();
        if let Some(cond) = &if_ {
            problems.extend(call_expression("`if`", cond, true));
        }
        for (name, value) in &with {
            if let Value::String(text) = value
                && text.contains("${{")
            {
                problems.extend(call_expression(&format!("`with.{name}`"), text, false));
            }
        }
        for problem in problems {
            self.error(format!("{at} {problem}; the job was dropped"));
        }
        if self.errors.len() > before {
            return Call::Failed;
        }
        Call::Done(Box::new(WorkflowJob {
            job: id.to_owned(),
            requires: requires
                .iter()
                .map(String::as_str)
                .map(Require::new)
                .collect(),
            matrix,
            with,
            call: Some(target),
            secrets,
            permissions,
            if_,
            ..WorkflowJob::default()
        }))
    }

    /// The `with` of a call: scalars stay as they are, expressions
    /// included.
    fn call_with(&mut self, at: &str, gh: &GhJob) -> BTreeMap<String, Value> {
        let mut with = BTreeMap::new();
        let given = match &gh.with_ {
            None | Some(Value::Null) => return with,
            Some(Value::Object(map)) => map,
            Some(other) => {
                self.warn(format!(
                    "{at}: `with: {}` is not a map and was dropped",
                    show(other)
                ));
                return with;
            }
        };
        for (name, value) in given {
            match value {
                Value::Array(_) | Value::Object(_) => self.warn(format!(
                    "{at}: `with.{name}` is a list or a map, which an input cannot be, and was dropped"
                )),
                Value::Null => {
                    with.insert(name.clone(), Value::String(String::new()));
                }
                other => {
                    with.insert(name.clone(), other.clone());
                }
            }
        }
        with
    }

    /// The `secrets` of a call: `inherit`, or the secrets it passes by
    /// name. A value that is not one whole `${{ secrets.NAME }}` is
    /// dropped with a warning, as hatchrun hands over only a secret.
    fn call_secrets(&mut self, at: &str, gh: &GhJob) -> SecretsPassed {
        match &gh.secrets {
            None | Some(Value::Null) => SecretsPassed::None,
            Some(Value::String(text)) if text == "inherit" => SecretsPassed::Inherit,
            Some(Value::Object(map)) => {
                let mut named = BTreeMap::new();
                for (name, value) in map {
                    let text = super::calls::scalar(value).unwrap_or_default();
                    if passed_secret(&text).is_some() {
                        named.insert(name.to_ascii_uppercase(), text);
                    } else {
                        self.warn(format!(
                            "{at}: `secrets.{name}` is `{text}`, not one whole `${{{{ secrets.NAME }}}}`, and was dropped"
                        ));
                    }
                }
                if named.is_empty() {
                    SecretsPassed::None
                } else {
                    SecretsPassed::Named(named)
                }
            }
            Some(other) => {
                self.warn(format!(
                    "{at}: `secrets: {}` is not `inherit` or a map and was dropped",
                    show(other)
                ));
                SecretsPassed::None
            }
        }
    }

    /// Converts `on.workflow_call` into `on: call`: its inputs, with
    /// GitHub's types, its secrets and its outputs. An output whose
    /// value is not an expression over `jobs.<job>.outputs.<name>` is
    /// dropped with a warning.
    pub(super) fn call_trigger(&mut self, file: &str, on: &Value) -> CallTrigger {
        let mut trigger = CallTrigger::default();
        let body = workflow_call(on).unwrap_or(Value::Null);
        if let Some(Value::Object(inputs)) = body.get("inputs") {
            for (name, input) in inputs {
                let at = format!("`{file}`: input `{name}`");
                let input = match GhCallInput::deserialize(input) {
                    Ok(input) => input,
                    Err(err) => {
                        self.error(format!("{at}: {err}"));
                        continue;
                    }
                };
                let kind = match input.kind.as_deref().unwrap_or("string") {
                    "string" => InputType::String,
                    "boolean" => InputType::Boolean,
                    "number" => InputType::Number,
                    other => {
                        self.error(format!(
                            "{at} has type `{other}`; a reusable workflow takes string, boolean and number inputs"
                        ));
                        continue;
                    }
                };
                let default = input.default.and_then(|value| {
                    let fitted = kind.coerce(&value).ok();
                    if fitted.is_none() {
                        self.warn(format!(
                            "{at}: `default: {}` does not fit the input type and was dropped",
                            show(&value)
                        ));
                    }
                    fitted
                });
                trigger.inputs.insert(
                    name.clone(),
                    CallInput {
                        kind,
                        default,
                        required: input.required.unwrap_or(false),
                        description: input.description,
                    },
                );
            }
        }
        if let Some(Value::Object(secrets)) = body.get("secrets") {
            for (name, secret) in secrets {
                let secret = GhCallSecret::deserialize(secret).unwrap_or_default();
                trigger.secrets.insert(
                    name.clone(),
                    CallSecret {
                        required: secret.required.unwrap_or(false),
                        description: secret.description,
                    },
                );
            }
        }
        if let Some(Value::Object(outputs)) = body.get("outputs") {
            for (name, output) in outputs {
                let output = GhCallOutput::deserialize(output).unwrap_or_default();
                let text = output.value.as_ref().and_then(scalar).unwrap_or_default();
                let problems =
                    output_expression(&format!("`on.workflow_call.outputs.{name}`"), &text);
                if problems.is_empty() {
                    trigger.outputs.insert(name.clone(), text);
                } else {
                    self.warn(format!(
                        "`{file}`: output `{name}` of the called workflow {}; it was dropped",
                        problems.join("; ")
                    ));
                }
            }
        }
        trigger
    }
}

/// A string, number or boolean as text. `None` for lists and maps; null
/// is the empty string.
pub(super) fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// A value as it appears in a message.
pub(super) fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
