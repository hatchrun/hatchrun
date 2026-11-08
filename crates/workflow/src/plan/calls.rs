// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Plans a workflow entry that calls another workflow: the callee is
//! compiled on its own and its jobs join the caller's plan.

use std::collections::BTreeSet;

use super::{
    BTreeMap, Budget, Config, Diagnostic, ENTRY_OVERHEAD, EntryAt, PlannedJob, Value, Walk,
    WorkflowJob, combination, compile_workflow, entry_value_errors, expression_problems, job_bytes,
    kept_combinations, matrix_entries, matrix_id, reads_parameters, reserve_jobs,
};
use crate::call::{
    CallFrame, CallTarget, CallTrigger, InputType, Library, MAX_CALL_DEPTH, MAX_UNIQUE_CALLEES,
    RemoteCall, SecretsPassed, passed_secret,
};
use crate::config::Workflow;
use crate::expr;
use crate::permissions::{Level, token_scopes};

/// The contexts a call's `with` and `if` read: what the host knows
/// when the call's jobs can start.
const CALL_CONTEXTS: &[&str] = &[
    "hatchrun", "github", "needs", "strategy", "matrix", "inputs", "vars",
];

/// The contexts a callee's `on.call.outputs` read.
const OUTPUT_CONTEXTS: &[&str] = &["jobs"];

/// The calls on the way to the entry being planned, and the workflows
/// called so far.
pub(super) struct Calls<'a> {
    /// The files of other repositories. `None` reads no file: a call of
    /// one plans nothing.
    library: Option<&'a Library>,
    /// The workflows being planned, the one the plan is for first.
    stack: Vec<String>,
    /// Every workflow called, once each, for the limit.
    unique: BTreeSet<String>,
    /// The file the workflows being planned come from: empty for the
    /// config, the call that named it for a file of another repository.
    origin: String,
    /// What the token of the workflow being planned may do when it and
    /// its jobs set no permissions: the caller's, for a called workflow.
    inherited: Option<BTreeMap<String, Level>>,
}

impl<'a> Calls<'a> {
    /// The permissions a job of the workflow being planned gets when
    /// neither the job nor its workflow sets any, `None` for a workflow
    /// that was not called.
    pub(super) fn inherited(&self) -> Option<&BTreeMap<String, Level>> {
        self.inherited.as_ref()
    }

    pub(super) fn new(library: Option<&'a Library>, top: &str) -> Self {
        Self {
            library,
            stack: vec![top.to_owned()],
            unique: BTreeSet::new(),
            origin: String::new(),
            inherited: None,
        }
    }
}

/// A workflow a call resolved to.
struct Target<'a> {
    config: &'a Config,
    wf: &'a Workflow,
    /// How messages name the workflow.
    shown: String,
    /// The key that tells it from every other workflow of the plan.
    unique: String,
    /// The file it came from, empty for the config.
    origin: String,
    /// `owner/repo/path.yml@<sha>` for a file of another repository.
    source: Option<String>,
}

fn problem(errors: &mut Vec<Diagnostic>, owner: &str, message: impl std::fmt::Display) {
    errors.push(Diagnostic::new(format!("{owner}: {message}")));
}

/// Plans a call entry: one copy of the callee's jobs for each matrix
/// entry of the call, each as `<call> / <job>`.
#[allow(clippy::too_many_lines)] // Checks, then one pass over the matrix.
pub(super) fn expand_call(
    at: &EntryAt<'_>,
    entry: &WorkflowJob,
    id: &str,
    budget: &mut Budget,
    planned: &mut usize,
    calls: &mut Calls<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Vec<PlannedJob> {
    let EntryAt {
        config,
        wf,
        workflow,
        git_ref,
    } = *at;
    let owner = format!("workflow `{workflow}` entry `{id}`");
    let before = errors.len();
    let call = entry.call.as_deref().unwrap_or_default();
    if entry.approval {
        problem(errors, &owner, "an approval cannot also `call` a workflow");
        return Vec::new();
    }
    if entry.matrix.dynamic() {
        problem(
            errors,
            &owner,
            "a matrix read at run time cannot run a called workflow yet; write the matrix out, or run the workflow once and put the matrix on its jobs",
        );
        return Vec::new();
    }
    if entry.strategy.is_some() {
        problem(
            errors,
            &owner,
            "`strategy` is not supported on a call yet; `fail-fast` and `max-parallel` work on the jobs of the called workflow",
        );
        return Vec::new();
    }
    let target = match CallTarget::parse(call) {
        Ok(target) => target,
        Err(message) => {
            problem(errors, &owner, message);
            return Vec::new();
        }
    };
    let library = calls.library;
    let resolved = match &target {
        CallTarget::Local(key) => {
            let Some(callee) = config.workflows.get(key) else {
                let keys: Vec<String> = config.workflows.keys().map(|k| format!("`{k}`")).collect();
                problem(
                    errors,
                    &owner,
                    format!(
                        "calls workflow `{key}`, which is not a workflow of the config; it has {}",
                        keys.join(", ")
                    ),
                );
                return Vec::new();
            };
            Target {
                config,
                wf: callee,
                shown: key.clone(),
                unique: format!("{}#{key}", calls.origin),
                origin: calls.origin.clone(),
                source: None,
            }
        }
        CallTarget::Remote(remote) => {
            // `validate` reads no file of another repository.
            let Some(library) = library else {
                return Vec::new();
            };
            if library.is_offline() && library.get(remote).is_none() {
                return Vec::new();
            }
            match remote_target(library, remote) {
                Ok(target) => target,
                Err(message) => {
                    problem(errors, &owner, message);
                    return Vec::new();
                }
            }
        }
    };
    let target = resolved;
    let Some(trigger) = &target.wf.on.call else {
        problem(
            errors,
            &owner,
            format!(
                "calls `{}`, which has no `on: call`; add `call:` to its `on:` to let other workflows call it",
                target.shown
            ),
        );
        return Vec::new();
    };
    if calls.stack.contains(&target.unique) {
        let chain: Vec<String> = calls
            .stack
            .iter()
            .chain([&target.unique])
            .map(|k| format!("`{}`", k.rsplit('#').next().unwrap_or(k)))
            .collect();
        problem(
            errors,
            &owner,
            format!(
                "calls `{}`, which makes a cycle ({})",
                target.shown,
                chain.join(" calls ")
            ),
        );
        return Vec::new();
    }
    if calls.stack.len() > MAX_CALL_DEPTH {
        problem(
            errors,
            &owner,
            format!(
                "calls `{}`, which is {} workflows deep; workflows may call each other {MAX_CALL_DEPTH} levels deep",
                target.shown,
                calls.stack.len()
            ),
        );
        return Vec::new();
    }
    calls.unique.insert(target.unique.clone());
    if calls.unique.len() > MAX_UNIQUE_CALLEES {
        problem(
            errors,
            &owner,
            format!(
                "calls `{}`, which makes {} different workflows called in one run; the limit is {MAX_UNIQUE_CALLEES}",
                target.shown,
                calls.unique.len()
            ),
        );
        return Vec::new();
    }

    let (with, kinds) = inputs(&owner, entry, &target, trigger, errors);
    secrets(&owner, entry, &target, trigger, errors);
    outputs(&owner, &target, trigger, errors);
    if let Some(cond) = &entry.if_ {
        for message in call_expression("`if`", cond, true) {
            problem(errors, &owner, message);
        }
    }
    // A matrix key is not a parameter of the call: `with` may read it
    // as `matrix.KEY` under any name.
    let mut shadow = entry.clone();
    shadow.with.clear();
    let combinations = entry_value_errors(&shadow, id, &BTreeMap::new(), errors);
    if errors.len() > before || budget.spent {
        return Vec::new();
    }

    // What the caller's token may do, which a callee that sets no
    // permissions of its own inherits, and none may raise.
    let caller_scopes = match entry.permissions.as_ref().or(wf.permissions.as_ref()) {
        Some(p) => token_scopes(Some(p)),
        None => calls
            .inherited
            .clone()
            .unwrap_or_else(|| token_scopes(None)),
    };
    let outer_inherited = calls.inherited.replace(caller_scopes.clone());
    calls.stack.push(target.unique.clone());
    let saved = std::mem::replace(&mut calls.origin, target.origin.clone());
    let plan = compile_workflow(
        target.config,
        &target.shown,
        target.wf,
        git_ref,
        budget,
        calls,
        errors,
    );
    calls.origin = saved;
    calls.inherited = outer_inherited;
    calls.stack.pop();
    let Some(plan) = plan else {
        return Vec::new();
    };
    if errors.len() > before {
        return Vec::new();
    }

    let mut walk = Walk {
        workflow,
        stack: Vec::new(),
        budget,
    };
    let listed = if entry.matrix.include.is_empty() {
        None
    } else {
        match matrix_entries(&entry.matrix, &Value::Null) {
            Ok(listed) => Some(listed),
            Err(message) => {
                errors.push(Diagnostic::new(format!("`{id}`: {message}")));
                return Vec::new();
            }
        }
    };
    let kept: Vec<usize> = if let Some(listed) = &listed {
        if listed.is_empty() {
            errors.push(Diagnostic::new(format!(
                "`{id}` has a matrix whose `exclude` leaves no combination"
            )));
            return Vec::new();
        }
        (0..listed.len()).collect()
    } else {
        let Some(kept) = kept_combinations(&entry.matrix, combinations, id, &mut walk, errors)
        else {
            return Vec::new();
        };
        kept
    };
    if plan.jobs.is_empty() {
        return Vec::new();
    }
    if !reserve_jobs(workflow, planned, plan.jobs.len() * kept.len(), errors) {
        return Vec::new();
    }
    let cap = caller_scopes;
    let one = plan.jobs.iter().map(job_bytes).sum::<usize>();
    let mut out = Vec::with_capacity(plan.jobs.len() * kept.len());
    for index in kept {
        let combo = if let Some(listed) = &listed {
            listed[index].values.clone()
        } else {
            let Some(combo) = combination(&entry.matrix.values, index, &mut walk, errors) else {
                return Vec::new();
            };
            combo
        };
        let instance = match listed.as_ref().map(|l| l[index].shown.as_slice()) {
            Some(shown) if !shown.is_empty() => format!("{id} ({})", shown.join(", ")),
            _ => matrix_id(id, &combo),
        };
        let frame = CallFrame {
            id: instance.clone(),
            name: id.to_owned(),
            workflow: target.shown.clone(),
            source: target.source.clone(),
            with: with.clone(),
            kinds: kinds.clone(),
            secrets: entry.secrets.clone(),
            matrix: combo,
            if_: entry.if_.clone(),
            outputs: trigger.outputs.clone(),
        };
        let bytes = one
            + plan
                .jobs
                .iter()
                .map(|j| (j.scope.len() + 1) * (frame.bytes() + ENTRY_OVERHEAD) + j.id.len() * 2)
                .sum::<usize>();
        let steps: usize = plan.jobs.iter().map(|j| j.steps.len() + 1).sum();
        if !walk.bytes(bytes, errors) || !walk.budget.charge(steps, workflow, errors) {
            return Vec::new();
        }
        let prefix = format!("{instance} / ");
        for job in &plan.jobs {
            let mut job = job.clone();
            job.id = format!("{prefix}{}", job.id);
            job.requires = job
                .requires
                .iter()
                .map(|r| format!("{prefix}{r}"))
                .collect();
            for outer in &mut job.scope {
                outer.id = format!("{prefix}{}", outer.id);
            }
            job.scope.insert(0, frame.clone());
            // Each instance of a call has its own strategy groups, so a
            // failure in one does not cancel the jobs of another.
            if let Some(strategy) = &mut job.strategy {
                strategy.group = format!("{prefix}{}", strategy.group);
            }
            job.token_permissions = capped(&job.token_permissions, &cap);
            out.push(job);
        }
    }
    out
}

/// A job's token permissions, kept to what `cap` allows: a called
/// workflow can lower what its caller grants and cannot raise it.
fn capped(
    permissions: &BTreeMap<String, Level>,
    cap: &BTreeMap<String, Level>,
) -> BTreeMap<String, Level> {
    permissions
        .iter()
        .filter_map(|(scope, level)| {
            let allowed = cap.get(scope).copied().unwrap_or(Level::None);
            let level = (*level).min(allowed);
            (level != Level::None).then(|| (scope.clone(), level))
        })
        .collect()
}

/// The workflow a file of another repository holds.
fn remote_target<'a>(library: &'a Library, remote: &RemoteCall) -> Result<Target<'a>, String> {
    let Some(callee) = library.get(remote) else {
        return Err(match library.refusal(remote) {
            Some(reason) => format!("calls `{remote}`, which cannot be used: {reason}"),
            None if library.limit_reached() => format!(
                "calls `{remote}`, which was not read: a run may call at most {MAX_UNIQUE_CALLEES} different workflows"
            ),
            None => format!("calls `{remote}`, which was not read by the host that planned this"),
        });
    };
    let Some(wf) = callee.config.workflows.get(&callee.workflow) else {
        return Err(format!(
            "calls `{remote}`, which converted to no workflow `{}`",
            callee.workflow
        ));
    };
    let shown = format!("{}/{}/{}", remote.owner, remote.repo, remote.path);
    Ok(Target {
        config: &callee.config,
        wf,
        unique: format!("{remote}#{}", callee.workflow),
        origin: remote.to_string(),
        source: Some(format!("{shown}@{}", callee.sha)),
        shown,
    })
}

/// The values the call gives the callee's inputs, with the defaults of
/// the inputs it leaves out, and each input's type. Reports a value of
/// an input the callee does not take, one of the wrong type, and an
/// input that is required and not given.
fn inputs(
    owner: &str,
    entry: &WorkflowJob,
    target: &Target<'_>,
    trigger: &CallTrigger,
    errors: &mut Vec<Diagnostic>,
) -> (BTreeMap<String, Value>, BTreeMap<String, InputType>) {
    let mut with = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for key in entry.with.keys() {
        if !trigger.inputs.contains_key(key) {
            let takes: Vec<String> = trigger.inputs.keys().map(|k| format!("`{k}`")).collect();
            let takes = if takes.is_empty() {
                "no inputs".to_owned()
            } else {
                format!("the inputs {}", takes.join(", "))
            };
            problem(
                errors,
                owner,
                format!(
                    "passes `{key}`, which is not an input of `{}`; it takes {takes}",
                    target.shown
                ),
            );
        }
    }
    for (name, input) in &trigger.inputs {
        kinds.insert(name.clone(), input.kind);
        if let Some(default) = &input.default
            && let Some(want) = input.kind.mismatch(default)
        {
            problem(
                errors,
                owner,
                format!(
                    "input `{name}` of `{}` has a default that {want}",
                    target.shown
                ),
            );
        }
        match entry.with.get(name) {
            Some(value) => {
                if matches!(value, Value::Array(_) | Value::Object(_)) {
                    problem(
                        errors,
                        owner,
                        format!(
                            "input `{name}` must be a string, number or boolean, found {value}"
                        ),
                    );
                    continue;
                }
                if reads_parameters(value) {
                    problem(
                        errors,
                        owner,
                        format!(
                            "input `{name}` reads `parameters`, which a workflow does not have"
                        ),
                    );
                }
                if let Some(want) = input.kind.mismatch(value) {
                    problem(
                        errors,
                        owner,
                        format!("input `{name}` of `{}` {want}", target.shown),
                    );
                }
                if let Value::String(text) = value
                    && text.contains("${{")
                {
                    for message in call_expression(&format!("input `{name}`"), text, false) {
                        problem(errors, owner, message);
                    }
                }
                with.insert(name.clone(), value.clone());
            }
            None => match (&input.default, input.required) {
                (Some(default), _) => {
                    with.insert(name.clone(), default.clone());
                }
                (None, true) => problem(
                    errors,
                    owner,
                    format!(
                        "does not pass the input `{name}` of `{}`, which is required and has no default",
                        target.shown
                    ),
                ),
                // GitHub reads an input nobody gave as the empty value of
                // its type.
                (None, false) => {
                    let empty = match input.kind {
                        InputType::String => Value::String(String::new()),
                        InputType::Boolean => Value::Bool(false),
                        InputType::Number => Value::from(0),
                    };
                    with.insert(name.clone(), empty);
                }
            },
        }
    }
    (with, kinds)
}

/// Reports the secrets a call passes that the callee does not declare,
/// the ones it declares as required and the call leaves out, and a
/// value that is not one whole `${{ secrets.NAME }}`.
fn secrets(
    owner: &str,
    entry: &WorkflowJob,
    target: &Target<'_>,
    trigger: &CallTrigger,
    errors: &mut Vec<Diagnostic>,
) {
    let declared: BTreeMap<String, bool> = trigger
        .secrets
        .iter()
        .map(|(name, secret)| (name.to_ascii_uppercase(), secret.required))
        .collect();
    let given: BTreeSet<&String> = match &entry.secrets {
        SecretsPassed::Inherit => return,
        SecretsPassed::None => BTreeSet::new(),
        SecretsPassed::Named(map) => {
            for (name, value) in map {
                if !declared.contains_key(name) {
                    problem(
                        errors,
                        owner,
                        format!(
                            "passes the secret `{name}`, which `{}` does not declare under `on.call.secrets`",
                            target.shown
                        ),
                    );
                }
                if passed_secret(value).is_none() {
                    problem(
                        errors,
                        owner,
                        format!(
                            "passes the secret `{name}` as `{value}`; pass one whole `${{{{ secrets.NAME }}}}`, as that is all a call can hand over"
                        ),
                    );
                }
            }
            map.keys().collect()
        }
    };
    for (name, required) in &declared {
        if *required && !given.contains(name) {
            problem(
                errors,
                owner,
                format!(
                    "does not pass the secret `{name}` of `{}`, which is required; pass it, or `secrets: inherit`",
                    target.shown
                ),
            );
        }
    }
}

/// Reports an `on.call.outputs` value that reads a context a call
/// cannot, or a job the callee does not have.
fn outputs(owner: &str, target: &Target<'_>, trigger: &CallTrigger, errors: &mut Vec<Diagnostic>) {
    let entries: BTreeSet<&str> = target
        .wf
        .jobs
        .iter()
        .map(|e| e.name.as_deref().filter(|_| !e.approval).unwrap_or(&e.job))
        .collect();
    for (name, text) in &trigger.outputs {
        let field = format!("`on.call.outputs.{name}` of `{}`", target.shown);
        for message in output_expression(&field, text) {
            problem(errors, owner, message);
        }
        for inner in expr::expressions(text)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, e)| e)
        {
            for job in expr::jobs_read(inner).unwrap_or_default() {
                if !entries.contains(job.as_str()) {
                    problem(
                        errors,
                        owner,
                        format!(
                            "{field} reads `jobs.{job}`, which is not a job of `{}`",
                            target.shown
                        ),
                    );
                }
            }
        }
    }
}

/// What is wrong with an expression of a call, which the host reads
/// when the call's jobs can start. `bare` reads a text without `${{ }}`
/// as an expression, as `if` does.
pub(crate) fn call_expression(field: &str, text: &str, bare: bool) -> Vec<String> {
    let wrapped;
    let text = if bare && expr::find_open(text).is_none() {
        wrapped = format!("${{{{ {text} }}}}");
        wrapped.as_str()
    } else {
        text
    };
    expression_problems(field, text, CALL_CONTEXTS)
}

/// What is wrong with the value of an `on.call.outputs` entry, each
/// starting with `field`: it may read only `jobs`.
pub(crate) fn output_expression(field: &str, text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![format!("{field} has no value")];
    }
    let mut problems = Vec::new();
    let found = match expr::expressions(text) {
        Ok(found) => found,
        Err(e) => return vec![format!("{field} has a bad expression: {e}")],
    };
    for (_, inner) in found {
        match expr::contexts(inner) {
            Err(e) => problems.push(format!("{field} has a bad expression: {e}")),
            Ok(names) => {
                for (context, _) in names {
                    if !OUTPUT_CONTEXTS.contains(&context.as_ref()) {
                        problems.push(format!(
                            "{field} reads `{context}`; it can read only {}",
                            OUTPUT_CONTEXTS.join(", ")
                        ));
                    }
                }
            }
        }
    }
    problems
}
