// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Compiles one workflow of a config into a plan: the jobs to run, in
//! order, with commands expanded, `${{ parameters.x }}` substituted and
//! matrices and parallel nodes spread out.
//!
//! Every other expression is left in place for the runner to evaluate.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::config::{
    CacheStep, Config, Job, Parameter, ParameterKind, RunStep, Step, UsesStep, Workflow,
    WorkflowJob, WorkspaceStep,
};
use crate::diagnostic::Diagnostic;
use crate::templates::{Template, template, template_labels};
use crate::trigger::{Filter, Triggers};

/// The timeout of a job that sets none.
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 360;

/// The most parallel nodes one job can run.
const MAX_PARALLELISM: u32 = 64;

/// The most entries one matrix can expand to.
const MAX_MATRIX: usize = 256;

/// The work one `compile` or `validate` call may do, in units. Every
/// step and command call visited costs one, and so does every step
/// written into the plan, counted once per matrix entry and parallel
/// node. With `BYTE_BUDGET` it bounds both time and memory, whatever the
/// config.
const BUDGET: usize = 50_000;

/// The bytes one `compile` or `validate` call may produce. Every
/// substituted string is charged its length as it is made, including
/// values passed into nested commands, and every planned job is charged
/// the bytes of its steps, `env`, `outputs`, `if`, id and matrix once per
/// copy. Charges are made before the memory is allocated.
const BYTE_BUDGET: usize = 8 << 20;

/// The longest string one substitution can make.
const MAX_VALUE: usize = 64 << 10;

/// The most jobs one plan can hold, as GitHub caps a run's matrix.
const MAX_JOBS: usize = 256;

/// The deepest commands can nest.
const MAX_NESTING: usize = 64;

/// The ref `validate` compiles every workflow against.
const VALIDATE_REF: &str = "refs/heads/main";

/// Keys of a step map. A command with one of these names could not be
/// told apart from a step.
const STEP_KEYS: &[&str] = &[
    "name",
    "id",
    "env",
    "if",
    "with",
    "shell",
    "run",
    "uses",
    "cache",
    "workspace",
    "test-results",
    "working-directory",
    "continue-on-error",
    "timeout-minutes",
];

/// The jobs one workflow runs for one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The workflow's name.
    pub workflow: String,
    /// The jobs. Every job comes after the jobs it requires.
    pub jobs: Vec<PlannedJob>,
}

/// One job of a plan: one matrix entry and one parallel node of a
/// workflow job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJob {
    /// Unique in the plan: the entry's id, then `(<v1>, <v2>)` for a
    /// matrix entry and `[<i+1>/<n>]` for a parallel node.
    pub id: String,
    /// The workflow entry's id: its `name`, else its key. Shared by all
    /// expansions of the entry.
    pub name: String,
    /// Ids of the jobs that must finish first.
    pub requires: Vec<String>,
    /// What the job does.
    pub kind: JobKind,
    /// This job's matrix entry, read as `${{ matrix.x }}` at run time.
    pub matrix: BTreeMap<String, Value>,
    /// `(index, total)` of a parallel node, 0-based. `None` without
    /// `parallelism` or with `parallelism: 1`.
    pub node: Option<(u32, u32)>,
    /// Environment variables for every step.
    pub env: BTreeMap<String, String>,
    /// The job runs only when this expression is true.
    pub if_: Option<String>,
    /// The job is cancelled after this many minutes.
    pub timeout_minutes: u32,
    /// Values later jobs can read.
    pub outputs: BTreeMap<String, String>,
    /// The steps, with commands expanded.
    pub steps: Vec<PlannedStep>,
}

/// What a planned job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Runs steps on a runner made from the template.
    Run {
        /// The runner template.
        template: &'static Template,
    },
    /// Waits for a person to approve.
    Approval,
}

/// One step of a planned job. Commands are expanded, so there is no
/// command step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedStep {
    /// A shell command.
    Run(RunStep),
    /// A GitHub action.
    Uses(UsesStep),
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

/// Compiles `workflow` for a push of `git_ref` (`refs/heads/main`,
/// `refs/tags/v1`). `branches:` and `tags:` on workflow jobs are matched
/// against the ref; a job that does not match is dropped, and so is
/// every job that requires it.
///
/// # Errors
///
/// Returns every problem found in the config and the workflow. Problems
/// in jobs the ref drops are reported too.
pub fn compile(config: &Config, workflow: &str, git_ref: &str) -> Result<Plan, Vec<Diagnostic>> {
    let mut errors = config_errors(config);
    let Some(wf) = config.workflows.get(workflow) else {
        errors.push(Diagnostic::new(format!(
            "there is no workflow `{workflow}`"
        )));
        return Err(dedup(errors));
    };
    let mut budget = Budget::new();
    let plan = compile_workflow(config, workflow, wf, git_ref, &mut budget, &mut errors);
    match plan {
        Some(plan) if errors.is_empty() => Ok(plan),
        _ => Err(dedup(errors)),
    }
}

/// Compiles every workflow against `refs/heads/main` and returns every
/// problem found. Empty when the config is valid.
#[must_use]
pub fn validate(config: &Config) -> Vec<Diagnostic> {
    let mut errors = config_errors(config);
    let mut budget = Budget::new();
    for (name, wf) in &config.workflows {
        compile_workflow(config, name, wf, VALIDATE_REF, &mut budget, &mut errors);
        if budget.spent {
            break;
        }
    }
    dedup(errors)
}

/// Problems that belong to the config as a whole, not one workflow.
fn config_errors(config: &Config) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    for name in config.commands.keys() {
        if STEP_KEYS.contains(&name.as_str()) {
            errors.push(Diagnostic::new(format!(
                "command `{name}` has the name of a step key; rename it"
            )));
        }
    }
    for (name, executor) in &config.executors {
        if template(&executor.runs_on).is_none() {
            errors.push(unknown_template(
                &format!("executor `{name}`"),
                &executor.runs_on,
            ));
        }
    }
    errors
}

fn unknown_template(owner: &str, label: &str) -> Diagnostic {
    Diagnostic::new(format!(
        "{owner} has `runs-on: {label}`, which names no runner template; use one of {}",
        template_labels().join(", ")
    ))
}

/// Drops repeated messages, keeping the first of each.
fn dedup(errors: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = BTreeSet::new();
    errors
        .into_iter()
        .filter(|e| seen.insert(e.to_string()))
        .collect()
}

/// Compiles one workflow, adding its problems to `errors`. Returns the
/// plan only when the workflow had no problems.
fn compile_workflow(
    config: &Config,
    name: &str,
    wf: &Workflow,
    git_ref: &str,
    budget: &mut Budget,
    errors: &mut Vec<Diagnostic>,
) -> Option<Plan> {
    let before = errors.len();
    trigger_errors(name, &wf.on, errors);

    let ids: Vec<String> = wf
        .jobs
        .iter()
        .map(|e| match &e.name {
            Some(name) if !e.approval => name.clone(),
            _ => e.job.clone(),
        })
        .collect();
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, id) in ids.iter().enumerate() {
        if index.insert(id, i).is_some() {
            errors.push(Diagnostic::new(format!(
                "workflow `{name}` has a duplicate job id `{id}`; give one of them a `name`"
            )));
        }
    }

    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(ids.len());
    for (entry, id) in wf.jobs.iter().zip(&ids) {
        let mut own = Vec::new();
        for req in &entry.requires {
            match index.get(req.as_str()) {
                Some(&d) if !own.contains(&d) => own.push(d),
                Some(_) => {}
                None => errors.push(Diagnostic::new(format!(
                    "job `{id}` in workflow `{name}` requires `{req}`, which is not a job of the workflow"
                ))),
            }
        }
        deps.push(own);
    }

    let mut planned = 0;
    let expansions: Vec<Vec<PlannedJob>> = wf
        .jobs
        .iter()
        .zip(&ids)
        .map(|(entry, id)| expand_entry(config, name, entry, id, budget, &mut planned, errors))
        .collect();
    if budget.spent {
        return None;
    }
    id_collisions(name, &ids, &expansions, errors);

    let order = topological(&deps);
    if order.len() < ids.len() {
        let cycle: Vec<String> = cycle_members(&deps, &order)
            .into_iter()
            .map(|i| format!("`{}`", ids[i]))
            .collect();
        errors.push(Diagnostic::new(format!(
            "workflow `{name}` has a requires cycle between {}",
            cycle.join(", ")
        )));
    }
    if errors.len() > before {
        return None;
    }

    let expanded_ids: Vec<Vec<String>> = expansions
        .iter()
        .map(|jobs| jobs.iter().map(|j| j.id.clone()).collect())
        .collect();
    let mut expansions = expansions;
    let mut kept = vec![false; ids.len()];
    let mut jobs = Vec::new();
    for i in order {
        kept[i] = ref_allows(&wf.jobs[i], git_ref) && deps[i].iter().all(|&d| kept[d]);
        if !kept[i] {
            continue;
        }
        let requires_bytes: usize = deps[i]
            .iter()
            .flat_map(|&d| expanded_ids[d].iter().map(String::len))
            .sum();
        if !budget.charge_bytes(
            requires_bytes.saturating_mul(expansions[i].len()),
            name,
            errors,
        ) {
            return None;
        }
        let requires: Vec<String> = deps[i]
            .iter()
            .flat_map(|&d| expanded_ids[d].iter().cloned())
            .collect();
        for mut job in std::mem::take(&mut expansions[i]) {
            job.requires.clone_from(&requires);
            jobs.push(job);
        }
    }
    Some(Plan {
        workflow: name.to_owned(),
        jobs,
    })
}

/// Reports planned jobs of different entries, or of one entry, that
/// ended up with the same id.
fn id_collisions(
    name: &str,
    ids: &[String],
    expansions: &[Vec<PlannedJob>],
    errors: &mut Vec<Diagnostic>,
) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, jobs) in expansions.iter().enumerate() {
        for job in jobs {
            match seen.get(job.id.as_str()) {
                // Two entries with one id are reported above.
                Some(&other) if other != i && ids[other] == ids[i] => {}
                Some(&other) => errors.push(Diagnostic::new(format!(
                    "workflow `{name}` has two jobs with the id `{}`, from `{}` and `{}`",
                    job.id, ids[other], ids[i]
                ))),
                None => {
                    seen.insert(&job.id, i);
                }
            }
        }
    }
}

/// Reports a trigger that has both an include and an ignore filter of
/// one kind, which GitHub rejects.
fn trigger_errors(name: &str, on: &Triggers, errors: &mut Vec<Diagnostic>) {
    let mut both = |event: &str, key: &str, include: Option<&Filter>, ignore: Option<&Filter>| {
        if include.is_some() && ignore.is_some() {
            errors.push(Diagnostic::new(format!(
                "workflow `{name}`: `on.{event}` has both `{key}` and `{key}-ignore`; use one"
            )));
        }
    };
    if let Some(push) = &on.push {
        both(
            "push",
            "branches",
            push.branches.as_ref(),
            push.branches_ignore.as_ref(),
        );
        both(
            "push",
            "tags",
            push.tags.as_ref(),
            push.tags_ignore.as_ref(),
        );
        both(
            "push",
            "paths",
            push.paths.as_ref(),
            push.paths_ignore.as_ref(),
        );
    }
    if let Some(pr) = &on.pull_request {
        both(
            "pull_request",
            "branches",
            pr.branches.as_ref(),
            pr.branches_ignore.as_ref(),
        );
        both(
            "pull_request",
            "paths",
            pr.paths.as_ref(),
            pr.paths_ignore.as_ref(),
        );
    }
}

/// Kahn's algorithm. Among the jobs that are ready, the one declared
/// first goes first. Jobs on or after a cycle are left out.
fn topological(deps: &[Vec<usize>]) -> Vec<usize> {
    let mut waiting: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); deps.len()];
    for (i, own) in deps.iter().enumerate() {
        for &d in own {
            dependents[d].push(i);
        }
    }
    let mut ready: BTreeSet<usize> = (0..deps.len()).filter(|&i| waiting[i] == 0).collect();
    let mut order = Vec::with_capacity(deps.len());
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &next in &dependents[i] {
            waiting[next] -= 1;
            if waiting[next] == 0 {
                ready.insert(next);
            }
        }
    }
    order
}

/// The jobs Kahn's algorithm left out, without those that only wait on
/// a cycle and that no cycle waits on.
fn cycle_members(deps: &[Vec<usize>], order: &[usize]) -> Vec<usize> {
    let mut left: BTreeSet<usize> = (0..deps.len()).collect();
    for i in order {
        left.remove(i);
    }
    loop {
        let needed: BTreeSet<usize> = left.iter().flat_map(|&i| deps[i].iter().copied()).collect();
        let before = left.len();
        left.retain(|i| needed.contains(i));
        if left.len() == before {
            return left.into_iter().collect();
        }
    }
}

/// Whether a workflow job's `branches:` and `tags:` let it run for
/// `git_ref`. A job with only a tag filter runs only for tags, and one
/// with only a branch filter only for branches.
fn ref_allows(entry: &WorkflowJob, git_ref: &str) -> bool {
    if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
        match &entry.branches {
            Some(filter) => filter.matches(branch),
            None => entry.tags.is_none(),
        }
    } else if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
        match &entry.tags {
            Some(filter) => filter.matches(tag),
            None => entry.branches.is_none(),
        }
    } else {
        entry.branches.is_none() && entry.tags.is_none()
    }
}

/// Expands one workflow entry into its matrix entries and parallel
/// nodes, with `requires` left empty.
fn expand_entry(
    config: &Config,
    workflow: &str,
    entry: &WorkflowJob,
    id: &str,
    budget: &mut Budget,
    planned: &mut usize,
    errors: &mut Vec<Diagnostic>,
) -> Vec<PlannedJob> {
    if budget.spent {
        return Vec::new();
    }
    if entry.approval {
        if !reserve_jobs(workflow, planned, 1, errors) {
            return Vec::new();
        }
        return approval(entry, id, errors);
    }

    let Some(job) = config.jobs.get(&entry.job) else {
        errors.push(Diagnostic::new(format!(
            "workflow `{workflow}` runs job `{}`, which is not defined in `jobs:`",
            entry.job
        )));
        return Vec::new();
    };
    let before = errors.len();
    let owner = format!("job `{}`", entry.job);
    let template = resolve_template(config, &owner, job, errors);

    let nodes = job.parallelism.unwrap_or(1);
    if !(1..=MAX_PARALLELISM).contains(&nodes) {
        errors.push(Diagnostic::new(format!(
            "{owner} has parallelism {nodes}; it must be 1 to {MAX_PARALLELISM}"
        )));
    }
    let size = entry_value_errors(entry, id, errors);
    if errors.len() > before {
        return Vec::new();
    }
    let Some(template) = template else {
        return Vec::new();
    };
    let copies = nodes as usize;
    if !reserve_jobs(workflow, planned, size * copies, errors) {
        return Vec::new();
    }

    let mut walk = Walk {
        workflow,
        stack: Vec::new(),
        budget,
    };
    let mut out = Vec::new();
    for combo in combinations(&entry.matrix) {
        let mut args = entry.with.clone();
        for (key, value) in &combo {
            if job.parameters.contains_key(key) {
                args.insert(key.clone(), value.clone());
            }
        }
        let Some(values) = bind(&job.parameters, &args, &owner, errors) else {
            continue;
        };
        // Steps fail the same way for every combination; stop at the first.
        let Some(base) = expand_job(config, job, &owner, &values, &mut walk, errors) else {
            break;
        };
        // Pay for the copies before making them: an id is at most the
        // entry id, the matrix values and a node suffix.
        let matrix_bytes: usize = combo.iter().map(|(k, v)| k.len() + 2 * value_len(v)).sum();
        let copy_bytes = job_bytes(&base) + 2 * id.len() + matrix_bytes + 32;
        if !walk
            .budget
            .charge(base.steps.len() * copies, workflow, errors)
            || !walk.bytes(copy_bytes.saturating_mul(copies), errors)
        {
            break;
        }
        let matrix_id = if combo.is_empty() {
            id.to_owned()
        } else {
            let shown: Vec<String> = combo.values().map(render).collect();
            format!("{id} ({})", shown.join(", "))
        };
        for node in 0..nodes {
            let (job_id, node_of) = if nodes > 1 {
                (
                    format!("{matrix_id} [{}/{nodes}]", node + 1),
                    Some((node, nodes)),
                )
            } else {
                (matrix_id.clone(), None)
            };
            out.push(PlannedJob {
                id: job_id,
                name: id.to_owned(),
                kind: JobKind::Run { template },
                matrix: combo.clone(),
                node: node_of,
                ..base.clone()
            });
        }
    }
    out
}

/// Counts `count` more jobs into the plan, or reports that the plan
/// would hold more than `MAX_JOBS`.
fn reserve_jobs(
    workflow: &str,
    planned: &mut usize,
    count: usize,
    errors: &mut Vec<Diagnostic>,
) -> bool {
    *planned = planned.saturating_add(count);
    if *planned > MAX_JOBS {
        errors.push(Diagnostic::new(format!(
            "workflow `{workflow}` plans more than {MAX_JOBS} jobs; the limit is {MAX_JOBS}"
        )));
        return false;
    }
    true
}

/// Checks the values a workflow entry sets: none may read `parameters`,
/// and the matrix must have values, list none twice, not repeat a `with`
/// key and stay within `MAX_MATRIX` entries. Returns the matrix size.
fn entry_value_errors(entry: &WorkflowJob, id: &str, errors: &mut Vec<Diagnostic>) -> usize {
    for (key, value) in &entry.with {
        if reads_parameters(value) {
            errors.push(no_parameters_here(id, key));
        }
    }
    let mut size: usize = 1;
    for (key, values) in &entry.matrix {
        if values.is_empty() {
            errors.push(Diagnostic::new(format!(
                "`{id}` has matrix key `{key}` with no values"
            )));
        }
        if entry.with.contains_key(key) {
            errors.push(Diagnostic::new(format!(
                "`{id}` sets `{key}` both in its matrix and as a parameter"
            )));
        }
        size = size.saturating_mul(values.len());
        let mut shown = BTreeSet::new();
        for value in values {
            if reads_parameters(value) {
                errors.push(no_parameters_here(id, key));
            }
            let text = render(value);
            if !shown.insert(text.clone()) {
                errors.push(Diagnostic::new(format!(
                    "`{id}` lists `{text}` twice in matrix key `{key}`"
                )));
            }
        }
    }
    if size > MAX_MATRIX {
        errors.push(Diagnostic::new(format!(
            "`{id}` has a matrix of {size} entries; the limit is {MAX_MATRIX}"
        )));
    }
    size
}

/// The planned job of an approval entry, which takes no parameters and
/// no matrix.
fn approval(entry: &WorkflowJob, id: &str, errors: &mut Vec<Diagnostic>) -> Vec<PlannedJob> {
    let before = errors.len();
    if entry.name.is_some() {
        errors.push(Diagnostic::new(format!(
            "approval `{id}` cannot have a `name`; its id is its key"
        )));
    }
    for key in entry.with.keys() {
        errors.push(Diagnostic::new(format!(
            "approval `{id}` takes no parameters, found `{key}`"
        )));
    }
    if !entry.matrix.is_empty() {
        errors.push(Diagnostic::new(format!(
            "approval `{id}` cannot have a matrix"
        )));
    }
    if errors.len() > before {
        return Vec::new();
    }
    vec![PlannedJob {
        id: id.to_owned(),
        name: id.to_owned(),
        requires: Vec::new(),
        kind: JobKind::Approval,
        matrix: BTreeMap::new(),
        node: None,
        env: BTreeMap::new(),
        if_: None,
        timeout_minutes: DEFAULT_TIMEOUT_MINUTES,
        outputs: BTreeMap::new(),
        steps: Vec::new(),
    }]
}

/// Every combination of matrix values. The first key changes slowest.
fn combinations(matrix: &BTreeMap<String, Vec<Value>>) -> Vec<BTreeMap<String, Value>> {
    let mut combos = vec![BTreeMap::new()];
    for (key, values) in matrix {
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for combo in &combos {
            for value in values {
                let mut c = combo.clone();
                c.insert(key.clone(), value.clone());
                next.push(c);
            }
        }
        combos = next;
    }
    combos
}

/// Finds the template a job runs on.
fn resolve_template(
    config: &Config,
    owner: &str,
    job: &Job,
    errors: &mut Vec<Diagnostic>,
) -> Option<&'static Template> {
    match (&job.executor, &job.runs_on) {
        (Some(_), Some(_)) => {
            errors.push(Diagnostic::new(format!(
                "{owner} has both `executor` and `runs-on`; use one"
            )));
            None
        }
        (None, None) => {
            errors.push(Diagnostic::new(format!(
                "{owner} needs `executor` or `runs-on`"
            )));
            None
        }
        (Some(name), None) => {
            // An executor's label is checked once, in `config_errors`.
            let found = config.executors.get(name).map(|e| template(&e.runs_on));
            if found.is_none() {
                errors.push(Diagnostic::new(format!(
                    "{owner} uses executor `{name}`, which is not defined in `executors:`"
                )));
            }
            found.flatten()
        }
        (None, Some(label)) => {
            let found = template(label);
            if found.is_none() {
                errors.push(unknown_template(owner, label));
            }
            found
        }
    }
}

/// The job-level fields and steps of a planned job, with parameters
/// substituted. Id, name, kind, matrix and node are filled in by the
/// caller.
fn expand_job(
    config: &Config,
    job: &Job,
    owner: &str,
    values: &BTreeMap<String, Value>,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<PlannedJob> {
    let before = errors.len();
    let mut steps = Vec::new();
    expand_steps(config, &job.steps, values, owner, walk, &mut steps, errors);
    if walk.budget.spent {
        return None;
    }
    let mut sub = |text: &str| substitute(text, values, owner, walk, errors);
    let env = job.env.iter().map(|(k, v)| (k.clone(), sub(v))).collect();
    let if_ = job.if_.as_deref().map(&mut sub);
    let outputs = job
        .outputs
        .iter()
        .map(|(k, v)| (k.clone(), sub(v)))
        .collect();
    if errors.len() > before || walk.budget.spent {
        return None;
    }
    Some(PlannedJob {
        id: String::new(),
        name: String::new(),
        requires: Vec::new(),
        kind: JobKind::Approval,
        matrix: BTreeMap::new(),
        node: None,
        env,
        if_,
        timeout_minutes: job.timeout_minutes.unwrap_or(DEFAULT_TIMEOUT_MINUTES),
        outputs,
        steps,
    })
}

/// What is left of the work one `compile` or `validate` call may do.
struct Budget {
    /// Units still allowed.
    left: usize,
    /// Bytes still allowed.
    bytes_left: usize,
    /// The budget ran out and was reported; all work stops.
    spent: bool,
}

impl Budget {
    fn new() -> Self {
        Self {
            left: BUDGET,
            bytes_left: BYTE_BUDGET,
            spent: false,
        }
    }

    /// Takes `bytes`. False once the budget is spent, after reporting it
    /// once.
    fn charge_bytes(&mut self, bytes: usize, workflow: &str, errors: &mut Vec<Diagnostic>) -> bool {
        if self.spent {
            return false;
        }
        if bytes > self.bytes_left {
            self.spent = true;
            self.bytes_left = 0;
            errors.push(Diagnostic::new(format!(
                "workflow `{workflow}` expands to more than {BYTE_BUDGET} bytes of steps and values, counting each matrix entry and parallel node; the limit is {BYTE_BUDGET} bytes"
            )));
            return false;
        }
        self.bytes_left -= bytes;
        true
    }

    /// Takes `units`. False once the budget is spent, after reporting it
    /// once.
    fn charge(&mut self, units: usize, workflow: &str, errors: &mut Vec<Diagnostic>) -> bool {
        if self.spent {
            return false;
        }
        if units > self.left {
            self.spent = true;
            self.left = 0;
            errors.push(Diagnostic::new(format!(
                "workflow `{workflow}` expands to more than {BUDGET} steps and command calls, counting each matrix entry and parallel node; the limit is {BUDGET}"
            )));
            return false;
        }
        self.left -= units;
        true
    }
}

/// The state of expanding one workflow entry's steps.
struct Walk<'a> {
    /// The workflow, as named in errors.
    workflow: &'a str,
    /// The commands being expanded, so one that reaches itself is caught.
    stack: Vec<String>,
    /// The work left for the whole call.
    budget: &'a mut Budget,
}

impl Walk<'_> {
    /// Counts one visit. False once the budget is spent.
    fn visit(&mut self, errors: &mut Vec<Diagnostic>) -> bool {
        self.budget.charge(1, self.workflow, errors)
    }

    /// Counts bytes about to be allocated. False once the budget is spent.
    fn bytes(&mut self, bytes: usize, errors: &mut Vec<Diagnostic>) -> bool {
        self.budget.charge_bytes(bytes, self.workflow, errors)
    }
}

/// Expands commands depth first, within the walk's budget.
fn expand_steps(
    config: &Config,
    steps: &[Step],
    values: &BTreeMap<String, Value>,
    scope: &str,
    walk: &mut Walk<'_>,
    out: &mut Vec<PlannedStep>,
    errors: &mut Vec<Diagnostic>,
) {
    for step in steps {
        if !walk.visit(errors) {
            return;
        }
        let Step::Command { name, with } = step else {
            out.push(plan_step(step, values, scope, walk, errors));
            continue;
        };
        let Some(command) = config.commands.get(name) else {
            errors.push(Diagnostic::new(format!(
                "{scope} uses command `{name}`, which is not defined in `commands:`"
            )));
            continue;
        };
        if let Some(pos) = walk.stack.iter().position(|c| c == name) {
            let chain: Vec<String> = walk.stack[pos..]
                .iter()
                .chain(std::iter::once(name))
                .map(|c| format!("`{c}`"))
                .collect();
            errors.push(Diagnostic::new(format!(
                "command `{name}` reaches itself: {}",
                chain.join(" -> ")
            )));
            continue;
        }
        if walk.stack.len() >= MAX_NESTING {
            errors.push(Diagnostic::new(format!(
                "{scope} nests commands deeper than {MAX_NESTING}"
            )));
            continue;
        }
        let args: BTreeMap<String, Value> = with
            .iter()
            .map(|(k, v)| (k.clone(), substitute_value(v, values, scope, walk, errors)))
            .collect();
        if walk.budget.spent {
            return;
        }
        let inner = format!("command `{name}`");
        let Some(bound) = bind(&command.parameters, &args, &inner, errors) else {
            continue;
        };
        walk.stack.push(name.clone());
        expand_steps(config, &command.steps, &bound, &inner, walk, out, errors);
        walk.stack.pop();
        if walk.budget.spent {
            return;
        }
    }
}

/// Checks parameter values against their declarations and fills in
/// defaults. `None` when a value is missing, unknown or the wrong kind.
fn bind(
    params: &BTreeMap<String, Parameter>,
    args: &BTreeMap<String, Value>,
    owner: &str,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, Value>> {
    let before = errors.len();
    for key in args.keys() {
        if !params.contains_key(key) {
            errors.push(Diagnostic::new(format!("{owner} has no parameter `{key}`")));
        }
    }
    let mut values = BTreeMap::new();
    for (key, param) in params {
        let Some(value) = args.get(key).or(param.default.as_ref()) else {
            errors.push(Diagnostic::new(format!(
                "{owner} needs a value for parameter `{key}`, which is required"
            )));
            continue;
        };
        if let Some(want) = kind_mismatch(&param.kind, value) {
            errors.push(Diagnostic::new(format!(
                "{owner} parameter `{key}` must be {want}, found {value}"
            )));
            continue;
        }
        values.insert(key.clone(), value.clone());
    }
    (errors.len() == before).then_some(values)
}

/// What a value should have been, when it is not of `kind`. A string
/// holding an expression passes any kind; it is checked at run time.
fn kind_mismatch(kind: &ParameterKind, value: &Value) -> Option<String> {
    if value.as_str().is_some_and(|s| s.contains("${{")) {
        return None;
    }
    let ok = match kind {
        ParameterKind::String => {
            matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
        }
        ParameterKind::Boolean => value.is_boolean(),
        ParameterKind::Integer => value.is_i64() || value.is_u64(),
        ParameterKind::Enum(options) => value
            .as_str()
            .is_some_and(|s| options.iter().any(|o| o == s)),
    };
    if ok {
        return None;
    }
    Some(match kind {
        ParameterKind::String => "a string".into(),
        ParameterKind::Boolean => "a boolean".into(),
        ParameterKind::Integer => "an integer".into(),
        ParameterKind::Enum(options) => format!("one of {}", options.join(", ")),
    })
}

/// Substitutes parameters in every string field of a step.
fn plan_step(
    step: &Step,
    values: &BTreeMap<String, Value>,
    scope: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> PlannedStep {
    let mut sub = |text: &str| substitute(text, values, scope, walk, errors);
    match step {
        Step::Run(run) => {
            let planned = RunStep {
                name: run.name.as_deref().map(&mut sub),
                id: run.id.clone(),
                run: sub(&run.run),
                shell: run.shell.as_deref().map(&mut sub),
                working_directory: run.working_directory.as_deref().map(&mut sub),
                env: run.env.iter().map(|(k, v)| (k.clone(), sub(v))).collect(),
                if_: run.if_.as_deref().map(&mut sub),
                continue_on_error: run.continue_on_error,
                timeout_minutes: run.timeout_minutes,
            };
            PlannedStep::Run(planned)
        }
        Step::Uses(uses) => {
            let planned = UsesStep {
                name: uses.name.as_deref().map(&mut sub),
                id: uses.id.clone(),
                uses: sub(&uses.uses),
                with: BTreeMap::new(),
                env: uses.env.iter().map(|(k, v)| (k.clone(), sub(v))).collect(),
                if_: uses.if_.as_deref().map(&mut sub),
                continue_on_error: uses.continue_on_error,
            };
            let with = uses
                .with
                .iter()
                .map(|(k, v)| (k.clone(), substitute_value(v, values, scope, walk, errors)))
                .collect();
            if planned.uses.starts_with("docker://") {
                errors.push(Diagnostic::new(format!(
                    "{scope} has `uses: {}`; `uses: docker://` is not supported yet; Docker services and containers come in a later release",
                    planned.uses
                )));
            }
            PlannedStep::Uses(UsesStep { with, ..planned })
        }
        Step::Cache(cache) => PlannedStep::Cache(CacheStep {
            key: sub(&cache.key),
            restore_keys: cache.restore_keys.iter().map(|k| sub(k)).collect(),
            paths: cache.paths.iter().map(|p| sub(p)).collect(),
        }),
        Step::Workspace(WorkspaceStep::Persist { root, paths }) => {
            PlannedStep::Workspace(WorkspaceStep::Persist {
                root: sub(root),
                paths: paths.iter().map(|p| sub(p)).collect(),
            })
        }
        Step::Workspace(WorkspaceStep::Attach { at }) => {
            PlannedStep::Workspace(WorkspaceStep::Attach { at: sub(at) })
        }
        Step::TestResults { path } => PlannedStep::TestResults { path: sub(path) },
        Step::Command { .. } => unreachable!("commands are expanded by expand_steps"),
    }
}

/// Substitutes parameters inside a value. A string that is exactly one
/// `${{ parameters.x }}` becomes the parameter's value, keeping its type.
fn substitute_value(
    value: &Value,
    values: &BTreeMap<String, Value>,
    scope: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Value {
    match value {
        Value::String(text) => {
            if let Some(found) = sole_parameter(text).and_then(|name| values.get(name)) {
                if !walk.bytes(value_len(found), errors) {
                    return Value::Null;
                }
                return found.clone();
            }
            Value::String(substitute(text, values, scope, walk, errors))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| substitute_value(v, values, scope, walk, errors))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_value(v, values, scope, walk, errors)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether a value set on a workflow entry reads `${{ parameters... }}`.
/// A workflow has no parameters, so such a value can never be filled in.
fn reads_parameters(value: &Value) -> bool {
    match value {
        Value::String(text) => text.split("${{").skip(1).any(|expr| {
            expr.split("}}")
                .next()
                .is_some_and(|inner| inner.trim_start().starts_with("parameters"))
        }),
        Value::Array(items) => items.iter().any(reads_parameters),
        Value::Object(map) => map.values().any(reads_parameters),
        _ => false,
    }
}

fn no_parameters_here(id: &str, key: &str) -> Diagnostic {
    Diagnostic::new(format!(
        "`{id}` sets `{key}` to a value that reads `parameters`; a workflow has no parameters to read"
    ))
}

/// The parameter name when `text` is one `${{ parameters.x }}` and
/// nothing else.
fn sole_parameter(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("${{")?.strip_suffix("}}")?;
    if inner.contains("}}") {
        return None;
    }
    parameter_name(inner)
}

/// `x` when `expr` is `parameters.x`, surrounded by any whitespace.
fn parameter_name(expr: &str) -> Option<&str> {
    let name = expr.trim().strip_prefix("parameters.")?;
    let ident = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ident.then_some(name)
}

/// One piece of a string being substituted.
enum Piece<'t> {
    /// Text kept as it is, including expressions left for run time.
    Text(&'t str),
    /// A `${{ parameters.x }}`: the name and the whole expression.
    Param(&'t str, &'t str),
}

/// Splits `text` into kept text and parameter reads.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let body = &rest[start + 3..];
        let Some(end) = body.find("}}") else {
            break;
        };
        let whole_end = start + 3 + end + 2;
        match parameter_name(&body[..end]) {
            Some(name) => {
                out.push(Piece::Text(&rest[..start]));
                out.push(Piece::Param(name, &rest[start..whole_end]));
            }
            None => out.push(Piece::Text(&rest[..whole_end])),
        }
        rest = &rest[whole_end..];
    }
    out.push(Piece::Text(rest));
    out
}

/// Replaces each `${{ parameters.x }}` in `text` with the value of `x`.
/// Every other `${{ }}` is left as it is. The result's length is worked
/// out and charged to the budget before it is built; a result over
/// `MAX_VALUE` is an error. Returns an empty string on any error.
fn substitute(
    text: &str,
    values: &BTreeMap<String, Value>,
    scope: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> String {
    if !text.contains("${{") {
        return if walk.bytes(text.len(), errors) {
            text.to_owned()
        } else {
            String::new()
        };
    }
    let pieces = pieces(text);
    let mut len: usize = 0;
    let mut first = None;
    for piece in &pieces {
        len = len.saturating_add(match piece {
            Piece::Text(t) => t.len(),
            Piece::Param(name, whole) => {
                if let Some(value) = values.get(*name) {
                    first.get_or_insert(*name);
                    value_len(value)
                } else {
                    errors.push(Diagnostic::new(format!(
                        "{scope} reads `${{{{ parameters.{name} }}}}` but declares no parameter `{name}`"
                    )));
                    whole.len()
                }
            }
        });
    }
    if let Some(name) = first.filter(|_| len > MAX_VALUE) {
        errors.push(Diagnostic::new(format!(
            "{scope} substitutes `${{{{ parameters.{name} }}}}` into a value of {len} bytes; the limit is {MAX_VALUE}"
        )));
        return String::new();
    }
    if !walk.bytes(len, errors) {
        return String::new();
    }
    let mut out = String::with_capacity(len);
    for piece in pieces {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Param(name, whole) => match values.get(name) {
                Some(Value::String(v)) => out.push_str(v),
                Some(other) => out.push_str(&render(other)),
                None => out.push_str(whole),
            },
        }
    }
    out
}

/// The length of a value rendered as text.
fn value_len(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        other => render(other).len(),
    }
}

/// The bytes of a step's strings.
fn step_bytes(step: &PlannedStep) -> usize {
    let opt = |o: &Option<String>| o.as_ref().map_or(0, String::len);
    let map =
        |m: &BTreeMap<String, String>| m.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    let list = |l: &[String]| l.iter().map(String::len).sum::<usize>();
    match step {
        PlannedStep::Run(r) => {
            opt(&r.name)
                + opt(&r.id)
                + r.run.len()
                + opt(&r.shell)
                + opt(&r.working_directory)
                + map(&r.env)
                + opt(&r.if_)
        }
        PlannedStep::Uses(u) => {
            opt(&u.name)
                + opt(&u.id)
                + u.uses.len()
                + u.with
                    .iter()
                    .map(|(k, v)| k.len() + value_len(v))
                    .sum::<usize>()
                + map(&u.env)
                + opt(&u.if_)
        }
        PlannedStep::Cache(c) => c.key.len() + list(&c.restore_keys) + list(&c.paths),
        PlannedStep::Workspace(WorkspaceStep::Persist { root, paths }) => root.len() + list(paths),
        PlannedStep::Workspace(WorkspaceStep::Attach { at }) => at.len(),
        PlannedStep::TestResults { path } => path.len(),
    }
}

/// The bytes a copy of a planned job holds in steps, `env`, `outputs` and
/// `if`.
fn job_bytes(job: &PlannedJob) -> usize {
    let map =
        |m: &BTreeMap<String, String>| m.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    job.steps.iter().map(step_bytes).sum::<usize>()
        + map(&job.env)
        + map(&job.outputs)
        + job.if_.as_ref().map_or(0, String::len)
}

/// A parameter or matrix value as text: strings as they are, whole
/// numbers without `.0`, booleans as `true` or `false`.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => f.to_string(),
            _ => n.to_string(),
        },
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
