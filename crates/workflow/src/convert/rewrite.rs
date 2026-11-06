// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Expression checks and rewriting of `${{ }}` text inside jobs.

use super::{
    BTreeSet, ContinueOnError, ExprError, Job, Step, UNSUPPORTED_CONTEXTS, Value, WorkflowJob,
    expr, plan,
};

/// Why a job that had errors is missing from those that need it.
/// What happened to a job that had errors, after "which".
pub(super) const FAILED: &str = "did not convert";

/// What happened to a reusable workflow call, after "which".
pub(super) const REUSABLE: &str = "calls a reusable workflow";

/// What in a converted job's expressions `validate` would reject: the
/// GitHub contexts it reads that hatchrun does not provide, in
/// [`UNSUPPORTED_CONTEXTS`] order, and, as error messages, each
/// expression that does not parse, reads an unknown context or reads
/// `parameters`.
pub(super) fn expression_problems(job: &Job) -> (Vec<&'static str>, Vec<String>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut conds: Vec<&str> = job.if_.iter().map(String::as_str).collect();
    if let Some(ContinueOnError::When(text)) = &job.continue_on_error {
        conds.push(text);
    }
    texts.extend(job.env.values().map(String::as_str));
    texts.extend(job.outputs.values().map(String::as_str));
    texts.extend(job.environment.as_deref());
    texts.extend(job.environment_url.as_deref());
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

/// What `validate` would reject in what a converted job reads when it
/// becomes ready: a `runs-on` that is an expression, and the
/// expressions of its entries' matrices. They can read only the
/// contexts the host has then.
pub(super) fn dynamic_problems(job: &Job, entries: &[WorkflowJob]) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(text) = &job.runs_on
        && text.contains("${{")
        && !text.trim().starts_with("${{ parameters.")
    {
        problems.extend(plan::expression_problems(
            "`runs-on`",
            text,
            plan::RUNS_ON_CONTEXTS,
        ));
    }
    for entry in entries {
        for (field, text) in entry.matrix.expressions() {
            problems.extend(plan::expression_problems(
                &field,
                text,
                plan::MATRIX_CONTEXTS,
            ));
        }
    }
    problems
}

/// Adds every string inside `value` to `out`.
pub(super) fn strings<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// The reason Docker-phase keys are dropped.
pub(super) const LATER: &str = "; Docker services and containers come in a later release";

/// What `on.workflow_call` is in an `on:` value, when it has one: its
/// map, or null.
pub(super) fn workflow_call(on: &Value) -> Option<Value> {
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
pub(super) fn calls_workflow(on: &Value) -> bool {
    workflow_call(on).is_some()
}
