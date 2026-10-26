// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Compiles one workflow of a config into a plan: the jobs to run, in
//! order, with commands expanded, `${{ parameters.x }}` substituted and
//! matrices and parallel nodes spread out.
//!
//! Every other expression is left in place for the runner to evaluate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    CacheStep, CheckoutStep, Config, Job, Matrix, Outcome, Parameter, ParameterKind, RunStep,
    Statuses, Step, UsesStep, Workflow, WorkflowJob, WorkspaceStep,
};
use crate::cron::Cron;
use crate::diagnostic::Diagnostic;
use crate::environment::valid_environment_name;
use crate::expr::{self, ExprError};
use crate::permissions::{Level, token_scopes};
use crate::templates::{Template, template, template_labels};
use crate::trigger::{
    Filter, MAX_GLOBS, MAX_PATTERNS, RELEASE_TYPES, Triggers, WORKFLOW_RUN_TYPES,
};

/// The timeout of a job that sets none.
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 360;

/// The most parallel nodes one job can run.
const MAX_PARALLELISM: u32 = 64;

/// The most entries one matrix can expand to.
const MAX_MATRIX: usize = 256;

/// The most combinations a matrix with `exclude` may have before the
/// excluded ones are taken out. Each is checked against every `exclude`
/// entry.
pub(crate) const MAX_COMBINATIONS: usize = 4096;

/// The most `exclude` entries one matrix may have.
pub(crate) const MAX_EXCLUDE: usize = 256;

/// The work one `compile` or `validate` call may do, in units. Every
/// step and command call visited costs one, and so does every step
/// written into the plan, counted once per matrix entry and parallel
/// node. With `BYTE_BUDGET` it bounds both time and memory, whatever the
/// config.
const BUDGET: usize = 50_000;

/// The bytes one `compile` or `validate` call may produce. Everything
/// the call stores is charged before it is allocated: each string, each
/// map entry and list item (plus `ENTRY_OVERHEAD`), each value passed
/// into a command, each step and job struct, each matrix entry, each
/// reported problem, and each extra copy of a job for parallel nodes.
const BYTE_BUDGET: usize = 8 << 20;

/// The bytes charged for each stored map entry or list string on top of
/// its text: its `String` headers, the key's heap block and the map node
/// share. Without it a config of many short entries costs far more
/// memory than it is charged.
const ENTRY_OVERHEAD: usize = 64;

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
    "checkout",
    "cache",
    "workspace",
    "test-results",
    "working-directory",
    "continue-on-error",
    "timeout-minutes",
];

/// Keys of a workflow entry. A job parameter with one of these names
/// could never be set from a workflow: the entry reads the key itself.
pub(crate) const ENTRY_KEYS: &[&str] = &["name", "requires", "type", "branches", "tags", "matrix"];

/// The jobs one workflow runs for one ref.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Plan {
    /// The workflow's name.
    pub workflow: String,
    /// The jobs. Every job comes after the jobs it requires.
    pub jobs: Vec<PlannedJob>,
}

/// One job of a plan: one matrix entry and one parallel node of a
/// workflow job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlannedJob {
    /// Unique in the plan: the entry's id, then `(<v1>, <v2>)` for a
    /// matrix entry and `[<i+1>/<n>]` for a parallel node.
    pub id: String,
    /// The workflow entry's id: its `name`, else its key. Shared by all
    /// expansions of the entry.
    pub name: String,
    /// Ids of the jobs that must finish first.
    pub requires: Vec<String>,
    /// For each workflow entry required, by its id (the `name` of its
    /// planned jobs), the end states its ancestry must reach for this
    /// job to start. [`ready`] reads it; an entry left out asks for
    /// [`Statuses::SUCCESS`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub statuses: BTreeMap<String, Statuses>,
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
    /// GitHub App permissions for the job's token, from its
    /// `permissions:`, else its workflow's, else the read default.
    pub token_permissions: BTreeMap<String, Level>,
    /// The environment the job deploys to, after parameter substitution.
    /// `None` for approval jobs and jobs without one.
    pub environment: Option<String>,
    /// The steps, with commands expanded.
    pub steps: Vec<PlannedStep>,
}

/// What a planned job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Runs steps on a runner made from the template.
    Run {
        /// The runner template. Serialized as its first label.
        #[serde(serialize_with = "ser_template", deserialize_with = "de_template")]
        template: &'static Template,
    },
    /// Waits for a person to approve.
    Approval,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn ser_template<S: Serializer>(template: &&'static Template, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(template.labels[0])
}

fn de_template<'de, D: Deserializer<'de>>(d: D) -> Result<&'static Template, D::Error> {
    let label = String::deserialize(d)?;
    template(&label).ok_or_else(|| de::Error::custom(format!("unknown template `{label}`")))
}

/// One step of a planned job. Commands are expanded, so there is no
/// command step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedStep {
    /// A shell command.
    Run(RunStep),
    /// A GitHub action.
    Uses(UsesStep),
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

/// Compiles `workflow` for one event. `branches:` and `tags:` on
/// workflow jobs are matched against `git_ref`; a job that does not
/// match is dropped, and so is every job that requires it, whatever
/// statuses it lists.
///
/// `git_ref` is the pushed ref for a push (`refs/heads/main`,
/// `refs/tags/v1`), the base branch for a pull request
/// (`refs/heads/<base>`), the default branch for a schedule or a
/// finished workflow run, the release's tag (`refs/tags/<tag>`) for a
/// release, and the dispatched ref for a manual run.
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

/// Checks every job and command on its own, then compiles every
/// workflow against `refs/heads/main`, and returns every problem found.
/// Empty when the config is valid.
///
/// On its own, each job and command must name a runner and the commands
/// it calls, give parameter defaults of the declared kind, and write
/// only expressions that parse and read known contexts. A `parameters`
/// read is valid only as a whole `${{ parameters.NAME }}`, which compile
/// substitutes; the plan carries no parameter values to run time.
#[must_use]
pub fn validate(config: &Config) -> Vec<Diagnostic> {
    let mut errors = config_errors(config);
    for (name, executor) in &config.executors {
        if template(&executor.runs_on).is_none() {
            errors.push(unknown_template(
                &format!("executor `{name}`"),
                &executor.runs_on,
            ));
        }
    }
    standalone_errors(config, &mut errors);
    let mut budget = Budget::new();
    for (name, wf) in &config.workflows {
        compile_workflow(config, name, wf, VALIDATE_REF, &mut budget, &mut errors);
        if budget.spent {
            break;
        }
    }
    dedup(errors)
}

/// Whether a planned job can start, as [`ready`] decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// A job it requires, or a job before that one, has not ended yet.
    Wait,
    /// Every required entry reached one of its statuses. The job's `if:`
    /// decides next whether it runs.
    Start,
    /// A required entry ended in a state its statuses do not list. The
    /// job does not run: it ends as [`Outcome::NotRun`].
    NotRun,
}

/// Decides whether `job`, a job of `plan`, can start. `outcome` tells
/// how a job of the plan ended, by id, or `None` while it has not.
///
/// The job waits until every job before it has ended. It then starts
/// when, for each workflow entry it requires, the aggregate outcome of
/// that entry's ancestry (all of the entry's planned jobs and every job
/// before them) is one of the statuses `job.statuses` lists for the
/// entry, or [`Statuses::SUCCESS`] when it lists none. [`Statuses`]
/// describes how outcomes aggregate. A job that requires nothing starts
/// at once. Ids the plan does not hold count only through `outcome`.
pub fn ready<'a, F>(plan: &'a Plan, job: &'a PlannedJob, outcome: F) -> Ready
where
    F: Fn(&str) -> Option<Outcome>,
{
    let by_id: BTreeMap<&str, &PlannedJob> = plan.jobs.iter().map(|j| (j.id.as_str(), j)).collect();
    let entry = |id: &'a str| -> &'a str { by_id.get(id).map_or(id, |j| j.name.as_str()) };
    let mut entries: Vec<&str> = Vec::new();
    for id in &job.requires {
        let name = entry(id);
        if !entries.contains(&name) {
            entries.push(name);
        }
    }
    let mut start = true;
    for name in entries {
        let roots = job.requires.iter().filter(|id| entry(id) == name);
        let Some(state) = ancestry(roots, &by_id, &outcome) else {
            return Ready::Wait;
        };
        let wanted = job.statuses.get(name).copied().unwrap_or_default();
        start &= wanted.contains(state);
    }
    if start { Ready::Start } else { Ready::NotRun }
}

/// The aggregate outcome of `roots` and every job before them: the
/// strongest of their outcomes. `None` while one of them has not ended.
fn ancestry<'a, F>(
    roots: impl Iterator<Item = &'a String>,
    by_id: &BTreeMap<&str, &'a PlannedJob>,
    outcome: &F,
) -> Option<Outcome>
where
    F: Fn(&str) -> Option<Outcome>,
{
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&str> = roots.map(String::as_str).collect();
    let mut strongest = Outcome::Success;
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        strongest = strongest.max(outcome(id)?);
        if let Some(job) = by_id.get(id) {
            todo.extend(job.requires.iter().map(String::as_str));
        }
    }
    Some(strongest)
}

/// Problems of the config as a whole (command names, filters, crons).
/// An executor's unknown label is no longer one of them: compile reports
/// it for a workflow whose job uses the executor, and `validate` still
/// reports it for every executor.
#[must_use]
pub fn config_errors(config: &Config) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    for name in config.commands.keys() {
        if STEP_KEYS.contains(&name.as_str()) {
            errors.push(Diagnostic::new(format!(
                "command `{name}` has the name of a step key; rename it"
            )));
        }
    }
    filter_errors(config, &mut errors);
    cron_errors(config, &mut errors);
    errors
}

/// Reports the `on.schedule` entries that are not cron expressions.
fn cron_errors(config: &Config, errors: &mut Vec<Diagnostic>) {
    for wf in config.workflows.values() {
        for schedule in &wf.on.schedule {
            if let Err(reason) = Cron::parse(&schedule.cron) {
                errors.push(Diagnostic::new(format!(
                    "invalid cron \"{}\": {reason}",
                    schedule.cron
                )));
            }
        }
    }
}

/// Reports a filter with more than `MAX_PATTERNS` patterns, and a config
/// whose filters hold more than `MAX_GLOBS` glob patterns in all. Glob
/// patterns are compiled each time they are matched, so these bound the
/// work of one webhook.
fn filter_errors(config: &Config, errors: &mut Vec<Diagnostic>) {
    let mut globs = 0usize;
    let mut check = |owner: &str, key: &str, filter: Option<&Filter>| {
        let Some(filter) = filter else { return };
        globs = globs.saturating_add(filter.globs());
        let n = filter.patterns().len();
        if n > MAX_PATTERNS {
            errors.push(Diagnostic::new(format!(
                "{owner}: `{key}` has {n} patterns; the limit is {MAX_PATTERNS}"
            )));
        }
    };
    for (name, wf) in &config.workflows {
        let owner = format!("workflow `{name}`");
        if let Some(push) = &wf.on.push {
            check(&owner, "on.push.branches", push.branches.as_ref());
            check(
                &owner,
                "on.push.branches-ignore",
                push.branches_ignore.as_ref(),
            );
            check(&owner, "on.push.tags", push.tags.as_ref());
            check(&owner, "on.push.tags-ignore", push.tags_ignore.as_ref());
            check(&owner, "on.push.paths", push.paths.as_ref());
            check(&owner, "on.push.paths-ignore", push.paths_ignore.as_ref());
        }
        if let Some(pr) = &wf.on.pull_request {
            check(&owner, "on.pull_request.branches", pr.branches.as_ref());
            check(
                &owner,
                "on.pull_request.branches-ignore",
                pr.branches_ignore.as_ref(),
            );
            check(&owner, "on.pull_request.paths", pr.paths.as_ref());
            check(
                &owner,
                "on.pull_request.paths-ignore",
                pr.paths_ignore.as_ref(),
            );
        }
        if let Some(run) = &wf.on.workflow_run {
            check(&owner, "on.workflow_run.branches", run.branches.as_ref());
            check(
                &owner,
                "on.workflow_run.branches-ignore",
                run.branches_ignore.as_ref(),
            );
        }
        if let Some(release) = &wf.on.release {
            check(&owner, "on.release.tags", release.tags.as_ref());
            check(
                &owner,
                "on.release.tags-ignore",
                release.tags_ignore.as_ref(),
            );
        }
        for entry in &wf.jobs {
            let id = entry.name.as_deref().unwrap_or(&entry.job);
            let owner = format!("workflow `{name}` entry `{id}`");
            check(&owner, "branches", entry.branches.as_ref());
            check(&owner, "tags", entry.tags.as_ref());
        }
    }
    if globs > MAX_GLOBS {
        errors.push(Diagnostic::new(format!(
            "the config's filters have {globs} glob patterns; the limit is {MAX_GLOBS}"
        )));
    }
}

fn unknown_template(owner: &str, label: &str) -> Diagnostic {
    Diagnostic::new(format!(
        "{owner} has `runs-on: {label}`, which names no runner template; use one of {}",
        template_labels().join(", ")
    ))
}

/// Problems in each job, command and workflow entry on its own, whether
/// or not a workflow reaches it.
fn standalone_errors(config: &Config, errors: &mut Vec<Diagnostic>) {
    for (name, job) in &config.jobs {
        let owner = format!("job `{name}`");
        for key in job.parameters.keys() {
            if ENTRY_KEYS.contains(&key.as_str()) {
                errors.push(Diagnostic::new(format!(
                    "{owner} has parameter `{key}`, which a workflow entry reads as its own key; rename it"
                )));
            }
        }
        resolve_runner(config, &owner, job, errors);
        let nodes = job.parallelism.unwrap_or(1);
        if !(1..=MAX_PARALLELISM).contains(&nodes) {
            errors.push(Diagnostic::new(format!(
                "{owner} has parallelism {nodes}; it must be 1 to {MAX_PARALLELISM}"
            )));
        }
        default_errors(&owner, &job.parameters, errors);
        let mut check = Check {
            config,
            owner: &owner,
            params: Some(&job.parameters),
            errors,
        };
        for (key, value) in &job.env {
            check.text(&format_args!("`env.{key}`"), value);
        }
        if let Some(cond) = &job.if_ {
            check.cond(&"`if`", cond);
        }
        for (key, value) in &job.outputs {
            check.text(&format_args!("`outputs.{key}`"), value);
        }
        check.steps(&job.steps);
    }
    for (name, command) in &config.commands {
        let owner = format!("command `{name}`");
        default_errors(&owner, &command.parameters, errors);
        Check {
            config,
            owner: &owner,
            params: Some(&command.parameters),
            errors,
        }
        .steps(&command.steps);
    }
    for (workflow, wf) in &config.workflows {
        for entry in &wf.jobs {
            let id = entry.name.as_deref().unwrap_or(&entry.job);
            let owner = format!("workflow `{workflow}` entry `{id}`");
            let mut check = Check {
                config,
                owner: &owner,
                params: None,
                errors,
            };
            for (key, value) in &entry.with {
                check.value(&format_args!("`{key}`"), value, &mut |c, field, text| {
                    if c.text(field, text) {
                        c.errors.push(no_parameters_here(id, key));
                    }
                });
            }
            let excluded = entry.matrix.exclude.iter().flatten();
            for (key, value) in entry
                .matrix
                .values
                .iter()
                .flat_map(|(key, values)| values.iter().map(move |v| (key, v)))
                .chain(excluded)
            {
                check.value(&format_args!("`{key}`"), value, &mut |c, field, text| {
                    if c.text(field, text) {
                        c.errors.push(no_parameters_here(id, key));
                    }
                });
            }
        }
    }
}

/// Reports parameter defaults that do not fit their declared kind.
fn default_errors(owner: &str, params: &BTreeMap<String, Parameter>, errors: &mut Vec<Diagnostic>) {
    for (key, param) in params {
        if let Some(default) = &param.default
            && let Some(want) = kind_mismatch(&param.kind, default)
        {
            errors.push(Diagnostic::new(format!(
                "{owner} parameter `{key}` must be {want}, found {default}"
            )));
        }
    }
}

/// Checks the strings of one job, command or workflow entry. Each
/// method takes the field it checks as it reads in a message, with its
/// key in backticks, such as "`env.X`" or "step 2 `if`".
struct Check<'a> {
    config: &'a Config,
    /// The job or command, as named in errors.
    owner: &'a str,
    /// The declared parameters; `None` where nothing can be substituted.
    params: Option<&'a BTreeMap<String, Parameter>>,
    errors: &'a mut Vec<Diagnostic>,
}

impl Check<'_> {
    fn steps(&mut self, steps: &[Step]) {
        for (i, step) in steps.iter().enumerate() {
            let at = |key| At {
                step: i + 1,
                prefix: "",
                key,
            };
            match step {
                Step::Run(run) => {
                    self.opt(&at("name"), run.name.as_deref());
                    self.text(&at("run"), &run.run);
                    self.opt(&at("shell"), run.shell.as_deref());
                    self.opt(&at("working-directory"), run.working_directory.as_deref());
                    self.env(i + 1, &run.env);
                    if let Some(cond) = &run.if_ {
                        self.cond(&at("if"), cond);
                    }
                }
                Step::Uses(uses) => {
                    self.opt(&at("name"), uses.name.as_deref());
                    self.text(&at("uses"), &uses.uses);
                    if uses.uses.starts_with("docker://") {
                        self.errors.push(Diagnostic::new(format!(
                            "{} has `uses: {}`; `uses: docker://` is not supported yet; Docker services and containers come in a later release",
                            self.owner, uses.uses
                        )));
                    }
                    for (key, value) in &uses.with {
                        let field = At {
                            step: i + 1,
                            prefix: "with.",
                            key,
                        };
                        self.value(&field, value, &mut |c, f, t| {
                            c.text(f, t);
                        });
                    }
                    self.env(i + 1, &uses.env);
                    if let Some(cond) = &uses.if_ {
                        self.cond(&at("if"), cond);
                    }
                }
                Step::Checkout(checkout) => {
                    self.opt(&at("checkout.ref"), checkout.r#ref.as_deref());
                    if let Some(path) = &checkout.path {
                        self.text(&at("checkout.path"), path);
                        if path.starts_with('/') || path.split(['/', '\\']).any(|p| p == "..") {
                            self.errors.push(Diagnostic::new(format!(
                                "{} {} is `{path}`; it must stay inside the working directory",
                                self.owner,
                                at("checkout.path")
                            )));
                        }
                    }
                }
                Step::Cache(cache) => {
                    self.text(&at("cache.key"), &cache.key);
                    for item in &cache.restore_keys {
                        self.text(&at("cache.restore-keys"), item);
                    }
                    for item in &cache.paths {
                        self.text(&at("cache.paths"), item);
                    }
                }
                Step::Workspace(WorkspaceStep::Persist { root, paths }) => {
                    self.text(&at("workspace.persist.root"), root);
                    for item in paths {
                        self.text(&at("workspace.persist.paths"), item);
                    }
                }
                Step::Workspace(WorkspaceStep::Attach { at: dir }) => {
                    self.text(&at("workspace.attach.at"), dir);
                }
                Step::TestResults { path } => {
                    self.text(&at("test-results"), path);
                }
                Step::Command { name, with } => self.call(&at(name), name, with),
            }
        }
    }

    /// Checks a command call: the command exists and the values fit its
    /// parameters.
    fn call(&mut self, field: &dyn fmt::Display, name: &str, with: &BTreeMap<String, Value>) {
        for (key, value) in with {
            self.value(&format_args!("{field} `{key}`"), value, &mut |c, f, t| {
                c.text(f, t);
            });
        }
        let Some(command) = self.config.commands.get(name) else {
            self.errors.push(Diagnostic::new(format!(
                "{} uses command `{name}`, which is not defined in `commands:`",
                self.owner
            )));
            return;
        };
        let inner = format!("command `{name}`");
        for key in with.keys() {
            if !command.parameters.contains_key(key) {
                self.errors
                    .push(Diagnostic::new(format!("{inner} has no parameter `{key}`")));
            }
        }
        for (key, param) in &command.parameters {
            let Some(value) = with.get(key).or(param.default.as_ref()) else {
                self.errors.push(Diagnostic::new(format!(
                    "{inner} needs a value for parameter `{key}`, which is required"
                )));
                continue;
            };
            if let Some(want) = kind_mismatch(&param.kind, value) {
                self.errors.push(Diagnostic::new(format!(
                    "{inner} parameter `{key}` must be {want}, found {value}"
                )));
            }
        }
    }

    fn opt(&mut self, field: &dyn fmt::Display, text: Option<&str>) {
        if let Some(text) = text {
            self.text(field, text);
        }
    }

    /// Checks the `env` of step `step`.
    fn env(&mut self, step: usize, env: &BTreeMap<String, String>) {
        for (key, value) in env {
            let field = At {
                step,
                prefix: "env.",
                key,
            };
            self.text(&field, value);
        }
    }

    /// Calls `f` on every string inside `value`.
    fn value(
        &mut self,
        field: &dyn fmt::Display,
        value: &Value,
        f: &mut dyn FnMut(&mut Self, &dyn fmt::Display, &str),
    ) {
        match value {
            Value::String(text) => f(self, field, text),
            Value::Array(items) => {
                for item in items {
                    self.value(field, item, f);
                }
            }
            Value::Object(map) => {
                for item in map.values() {
                    self.value(field, item, f);
                }
            }
            _ => {}
        }
    }

    /// Checks every `${{ }}` in a string. Returns whether one reads
    /// `parameters` other than as a whole substitution where there are
    /// no parameters, which the caller reports.
    fn text(&mut self, field: &dyn fmt::Display, text: &str) -> bool {
        let found = match expr::expressions(text) {
            Ok(found) => found,
            Err(e) => {
                self.bad(field, &e);
                return false;
            }
        };
        let mut reads = false;
        for (start, inner) in found {
            if let Some(name) = parameter_name(inner) {
                match self.params {
                    Some(params) if !params.contains_key(name) => {
                        self.errors.push(Diagnostic::new(format!(
                            "{} reads `${{{{ parameters.{name} }}}}` but declares no parameter `{name}`",
                            self.owner
                        )));
                    }
                    Some(_) => {}
                    None => reads = true,
                }
                continue;
            }
            reads |= self.expression(field, inner, start);
        }
        reads
    }

    /// Checks an `if:`, which is a bare expression unless it is wrapped
    /// in `${{ }}`.
    fn cond(&mut self, field: &dyn fmt::Display, cond: &str) {
        if expr::find_open(cond).is_some() {
            self.text(field, cond);
        } else {
            self.expression(field, cond, 0);
        }
    }

    /// Checks one expression that is evaluated at run time, where
    /// `parameters` cannot be read. Returns whether it reads
    /// `parameters` where there are no parameters at all.
    fn expression(&mut self, field: &dyn fmt::Display, inner: &str, start: usize) -> bool {
        let names = match expr::checked_contexts(inner) {
            Ok(names) => names,
            Err(e) => {
                self.bad(
                    field,
                    &ExprError {
                        message: e.message,
                        offset: e.offset + start,
                    },
                );
                return false;
            }
        };
        let reads = names.iter().any(|(name, _)| name == "parameters");
        if reads && self.params.is_some() {
            self.errors.push(Diagnostic::new(format!(
                "{} {field} reads `parameters` inside an expression; only a whole `${{{{ parameters.NAME }}}}` is substituted, and run time has no parameters",
                self.owner
            )));
            return false;
        }
        reads
    }

    fn bad(&mut self, field: &dyn fmt::Display, e: &ExprError) {
        self.errors.push(Diagnostic::new(format!(
            "{} {field} has a bad expression: {e}",
            self.owner
        )));
    }
}

/// A key of a step as it reads in a message, such as step 2 `env.X`.
/// It is written out only when a message is made.
struct At<'k> {
    step: usize,
    prefix: &'k str,
    key: &'k str,
}

impl fmt::Display for At<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {} `{}{}`", self.step, self.prefix, self.key)
    }
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
    trigger_errors(config, name, &wf.on, errors);

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

    let (deps, wanted) = dependencies(name, wf, &ids, &index, errors);

    let mut planned = 0;
    let expansions: Vec<Vec<PlannedJob>> = wf
        .jobs
        .iter()
        .zip(&ids)
        .map(|(entry, id)| expand_entry(config, wf, name, entry, id, budget, &mut planned, errors))
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

    let expanded_ids = planned_ids(&expansions, name, budget, errors)?;
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
            .flat_map(|&d| expanded_ids[d].iter().map(|id| id.len() + ENTRY_OVERHEAD))
            .chain(deps[i].iter().map(|&d| ids[d].len() + ENTRY_OVERHEAD))
            .sum();
        // One list, then a clone of it per planned job.
        if !budget.charge_bytes(
            requires_bytes.saturating_mul(expansions[i].len() + 1),
            name,
            errors,
        ) {
            return None;
        }
        let requires: Vec<String> = deps[i]
            .iter()
            .flat_map(|&d| expanded_ids[d].iter().cloned())
            .collect();
        let statuses: BTreeMap<String, Statuses> = deps[i]
            .iter()
            .zip(&wanted[i])
            .map(|(&d, &s)| (ids[d].clone(), s))
            .collect();
        for mut job in std::mem::take(&mut expansions[i]) {
            job.requires.clone_from(&requires);
            job.statuses.clone_from(&statuses);
            jobs.push(job);
        }
    }
    Some(Plan {
        workflow: name.to_owned(),
        jobs,
    })
}

/// The entries each entry requires, as indexes into `ids`, once each,
/// and the statuses each must reach. An entry required twice must meet
/// both lists. Reports a requires that names no entry.
fn dependencies(
    name: &str,
    wf: &Workflow,
    ids: &[String],
    index: &BTreeMap<&str, usize>,
    errors: &mut Vec<Diagnostic>,
) -> (Vec<Vec<usize>>, Vec<Vec<Statuses>>) {
    let mut deps = Vec::with_capacity(ids.len());
    let mut wanted = Vec::with_capacity(ids.len());
    for (entry, id) in wf.jobs.iter().zip(ids) {
        let mut own: Vec<usize> = Vec::new();
        let mut own_wanted: Vec<Statuses> = Vec::new();
        for req in &entry.requires {
            let Some(&d) = index.get(req.job.as_str()) else {
                errors.push(Diagnostic::new(format!(
                    "job `{id}` in workflow `{name}` requires `{}`, which is not a job of the workflow",
                    req.job
                )));
                continue;
            };
            if let Some(at) = own.iter().position(|&o| o == d) {
                own_wanted[at] = own_wanted[at].intersection(req.statuses);
            } else {
                own.push(d);
                own_wanted.push(req.statuses);
            }
        }
        deps.push(own);
        wanted.push(own_wanted);
    }
    (deps, wanted)
}

/// The ids of each entry's planned jobs, charged before they are copied.
fn planned_ids(
    expansions: &[Vec<PlannedJob>],
    workflow: &str,
    budget: &mut Budget,
    errors: &mut Vec<Diagnostic>,
) -> Option<Vec<Vec<String>>> {
    let bytes = expansions
        .iter()
        .flatten()
        .map(|j| j.id.len() + ENTRY_OVERHEAD)
        .sum();
    if !budget.charge_bytes(bytes, workflow, errors) {
        return None;
    }
    Some(
        expansions
            .iter()
            .map(|jobs| jobs.iter().map(|j| j.id.clone()).collect())
            .collect(),
    )
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
/// one kind, which GitHub rejects, a manual input whose default does
/// not fit its type, an activity type hatchrun does not run on, and a
/// `workflow_run` that names no other workflow of the config.
fn trigger_errors(config: &Config, name: &str, on: &Triggers, errors: &mut Vec<Diagnostic>) {
    if let Some(dispatch) = &on.workflow_dispatch {
        for (input, param) in &dispatch.inputs {
            let Some(default) = &param.default else {
                continue;
            };
            if let Some(want) = kind_mismatch(&param.kind, default) {
                errors.push(Diagnostic::new(format!(
                    "workflow `{name}`: `on.workflow_dispatch` input `{input}` default must be {want}, found {default}"
                )));
            }
        }
    }
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
    if let Some(run) = &on.workflow_run {
        both(
            "workflow_run",
            "branches",
            run.branches.as_ref(),
            run.branches_ignore.as_ref(),
        );
    }
    if let Some(release) = &on.release {
        both(
            "release",
            "tags",
            release.tags.as_ref(),
            release.tags_ignore.as_ref(),
        );
    }
    if let Some(run) = &on.workflow_run {
        types_errors(name, "workflow_run", &run.types, WORKFLOW_RUN_TYPES, errors);
        if run.workflows.is_empty() {
            errors.push(Diagnostic::new(format!(
                "workflow `{name}`: `on.workflow_run` names no workflow"
            )));
        }
        for other in &run.workflows {
            if other == name {
                errors.push(Diagnostic::new(format!(
                    "workflow `{name}`: `on.workflow_run` names the workflow itself"
                )));
            } else if !config.workflows.contains_key(other) {
                errors.push(Diagnostic::new(format!(
                    "workflow `{name}`: `on.workflow_run` names workflow `{other}`, which is not defined in `workflows:`"
                )));
            }
        }
    }
    if let Some(release) = &on.release {
        types_errors(name, "release", &release.types, RELEASE_TYPES, errors);
    }
}

/// Reports an empty `types` list and a type outside `known`.
fn types_errors(
    name: &str,
    event: &str,
    types: &[String],
    known: &[&str],
    errors: &mut Vec<Diagnostic>,
) {
    if types.is_empty() {
        errors.push(Diagnostic::new(format!(
            "workflow `{name}`: `on.{event}.types` is empty"
        )));
    }
    for kind in types {
        if !known.contains(&kind.as_str()) {
            errors.push(Diagnostic::new(format!(
                "workflow `{name}`: `on.{event}` type `{kind}` is not supported; use {}",
                known.join(", ")
            )));
        }
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
#[allow(clippy::too_many_arguments)]
fn expand_entry(
    config: &Config,
    wf: &Workflow,
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
    let runner = resolve_runner(config, &owner, job, errors);

    let nodes = job.parallelism.unwrap_or(1);
    if !(1..=MAX_PARALLELISM).contains(&nodes) {
        errors.push(Diagnostic::new(format!(
            "{owner} has parallelism {nodes}; it must be 1 to {MAX_PARALLELISM}"
        )));
    }
    let combinations = entry_value_errors(entry, id, errors);
    if errors.len() > before {
        return Vec::new();
    }
    let Some(runner) = runner else {
        return Vec::new();
    };
    let mut walk = Walk {
        workflow,
        stack: Vec::new(),
        budget,
    };
    let Some(kept) = kept_combinations(&entry.matrix, combinations, id, &mut walk, errors) else {
        return Vec::new();
    };
    let copies = nodes as usize;
    if !reserve_jobs(workflow, planned, kept.len() * copies, errors) {
        return Vec::new();
    }

    let mut out = Vec::new();
    for index in kept {
        let Some(combo) = combination(&entry.matrix.values, index, &mut walk, errors) else {
            break;
        };
        let Some(args) = entry_args(entry, job, &combo, &mut walk, errors) else {
            break;
        };
        let Some(values) = bind(&job.parameters, &args, &owner, &mut walk, errors) else {
            if walk.budget.spent {
                break;
            }
            continue;
        };
        drop(args);
        let leg = (id, &combo);
        let Some(template) = leg_template(&runner, &values, &owner, leg, &mut walk, errors) else {
            if walk.budget.spent {
                break;
            }
            continue;
        };
        // Steps fail the same way for every combination; stop at the first.
        let Some(mut base) = expand_job(config, job, &owner, &values, &mut walk, errors) else {
            break;
        };
        base.kind = JobKind::Run { template };
        if job.permissions.is_none() {
            base.token_permissions = token_scopes(wf.permissions.as_ref());
        }
        let Some(jobs) = copy_job(base, combo, id, nodes, &mut walk, errors) else {
            break;
        };
        out.extend(jobs);
    }
    out
}

/// The id of one matrix entry's job: the entry id, then `(<v1>, <v2>)`
/// when there is a matrix.
fn matrix_id(id: &str, combo: &BTreeMap<String, Value>) -> String {
    if combo.is_empty() {
        id.to_owned()
    } else {
        let shown: Vec<String> = combo.values().map(render).collect();
        format!("{id} ({})", shown.join(", "))
    }
}

/// The indexes of the combinations `exclude` leaves in, out of
/// `combinations`. Each combination checked costs one unit, and the
/// list is charged before it is made. `None` after reporting a matrix
/// left over `MAX_MATRIX` or with nothing, or a spent budget.
fn kept_combinations(
    matrix: &Matrix,
    combinations: usize,
    id: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<Vec<usize>> {
    if !matrix.exclude.is_empty() && !walk.budget.charge(combinations, walk.workflow, errors) {
        return None;
    }
    let kept: Vec<usize> = (0..combinations)
        .filter(|&i| !excluded(matrix, i))
        .collect();
    if !walk.bytes(kept.len() * size_of::<usize>(), errors) {
        return None;
    }
    if kept.is_empty() {
        errors.push(Diagnostic::new(format!(
            "`{id}` has a matrix whose `exclude` leaves no combination"
        )));
        return None;
    }
    if kept.len() > MAX_MATRIX {
        errors.push(too_big_matrix(id, kept.len()));
        return None;
    }
    Some(kept)
}

/// Whether the combination at `index` matches every key of one
/// `exclude` entry.
pub(crate) fn excluded(matrix: &Matrix, index: usize) -> bool {
    if matrix.exclude.is_empty() {
        return false;
    }
    let mut picked: Vec<(&str, &Value)> = Vec::with_capacity(matrix.values.len());
    let mut rest = index;
    for (key, values) in matrix.values.iter().rev() {
        picked.push((key, &values[rest % values.len()]));
        rest /= values.len();
    }
    matrix.exclude.iter().any(|entry| {
        entry.iter().all(|(key, want)| {
            picked
                .iter()
                .any(|(k, value)| *k == key.as_str() && *value == want)
        })
    })
}

/// The number of combinations a matrix runs: every combination of its
/// values without those `exclude` leaves out. When there are more than
/// `MAX_COMBINATIONS` to check, all of them, which is over the limit
/// either way.
fn matrix_size(matrix: &Matrix) -> usize {
    let all = matrix
        .values
        .values()
        .fold(1usize, |n, values| n.saturating_mul(values.len()));
    if matrix.exclude.is_empty() || all > MAX_COMBINATIONS {
        return all;
    }
    (0..all).filter(|&i| !excluded(matrix, i)).count()
}

fn too_big_matrix(id: &str, size: usize) -> Diagnostic {
    Diagnostic::new(format!(
        "`{id}` has a matrix of {size} entries; the limit is {MAX_MATRIX}"
    ))
}

/// The matrix entry at `index`, charged before it is made. The first key
/// changes slowest.
fn combination(
    matrix: &BTreeMap<String, Vec<Value>>,
    index: usize,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, Value>> {
    let mut picked = Vec::with_capacity(matrix.len());
    let mut rest = index;
    for (key, values) in matrix.iter().rev() {
        picked.push((key, &values[rest % values.len()]));
        rest /= values.len();
    }
    let bytes = picked
        .iter()
        .map(|(k, v)| k.len() + ENTRY_OVERHEAD + value_bytes(v))
        .sum();
    if !walk.bytes(bytes, errors) {
        return None;
    }
    Some(
        picked
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// The values a workflow entry passes to its job: its `with` and the
/// matrix keys the job declares as parameters. Charged before each is
/// copied.
fn entry_args(
    entry: &WorkflowJob,
    job: &Job,
    combo: &BTreeMap<String, Value>,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, Value>> {
    let picked = combo
        .iter()
        .filter(|(k, _)| job.parameters.contains_key(*k));
    let mut args = BTreeMap::new();
    for (key, value) in entry.with.iter().chain(picked) {
        if !walk.bytes(key.len() + ENTRY_OVERHEAD + value_bytes(value), errors) {
            return None;
        }
        args.insert(key.clone(), value.clone());
    }
    Some(args)
}

/// Spreads one matrix entry's job over its parallel nodes. The base job
/// and the matrix entry were charged as they were made; the other copies
/// are charged here, before they are made. `None` once the budget is
/// spent.
fn copy_job(
    base: PlannedJob,
    combo: BTreeMap<String, Value>,
    id: &str,
    nodes: u32,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<Vec<PlannedJob>> {
    let copies = nodes as usize;
    let workflow = walk.workflow;
    // An id is at most the entry id, the matrix values and a node suffix,
    // and the name is the entry id again.
    let combo_bytes: usize = combo
        .iter()
        .map(|(k, v)| k.len() + ENTRY_OVERHEAD + value_bytes(v))
        .sum();
    let combo_text: usize = combo.values().map(|v| rendered_len(v) + 2).sum();
    let per_copy = 2 * (id.len() + combo_text) + combo_bytes + 32;
    let clones = job_bytes(&base).saturating_mul(copies - 1);
    if !walk
        .budget
        .charge(base.steps.len() * copies, workflow, errors)
        || !walk.bytes(
            clones.saturating_add(per_copy.saturating_mul(copies)),
            errors,
        )
    {
        return None;
    }
    let matrix_id = matrix_id(id, &combo);
    let mut base = Some(base);
    let mut combo = Some(combo);
    let mut out = Vec::with_capacity(copies);
    for node in 0..nodes {
        let (job_id, node_of) = if nodes > 1 {
            (
                format!("{matrix_id} [{}/{nodes}]", node + 1),
                Some((node, nodes)),
            )
        } else {
            (matrix_id.clone(), None)
        };
        // The last copy takes the base itself.
        let (from, matrix) = if node + 1 == nodes {
            (base.take()?, combo.take()?)
        } else {
            (base.clone()?, combo.clone()?)
        };
        out.push(PlannedJob {
            id: job_id,
            name: id.to_owned(),
            matrix,
            node: node_of,
            ..from
        });
    }
    Some(out)
}

/// The jobs a workflow plans at most: each entry's matrix size times
/// its job's parallelism, and one per approval. An entry whose job is
/// missing counts as one.
pub(crate) fn planned_jobs(config: &Config, wf: &Workflow) -> usize {
    wf.jobs
        .iter()
        .map(|entry| {
            let size = matrix_size(&entry.matrix);
            let nodes = config
                .jobs
                .get(&entry.job)
                .and_then(|job| job.parallelism)
                .unwrap_or(1);
            if entry.approval {
                1
            } else {
                size.saturating_mul(nodes as usize)
            }
        })
        .fold(0, usize::saturating_add)
}

/// The message for a workflow that plans more than `MAX_JOBS` jobs.
pub(crate) fn too_many_jobs(workflow: &str) -> String {
    format!("workflow `{workflow}` plans more than {MAX_JOBS} jobs; the limit is {MAX_JOBS}")
}

/// Whether a workflow plans more than `MAX_JOBS` jobs.
pub(crate) fn over_job_limit(config: &Config, wf: &Workflow) -> bool {
    planned_jobs(config, wf) > MAX_JOBS
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
        errors.push(Diagnostic::new(too_many_jobs(workflow)));
        return false;
    }
    true
}

/// Checks the values a workflow entry sets: none may read `parameters`,
/// and the matrix must have values, list none twice, not repeat a `with`
/// key and stay within `MAX_MATRIX` entries. Each `exclude` entry must
/// name only matrix keys, and with `exclude` there may be at most
/// `MAX_COMBINATIONS` combinations to check. Returns the number of
/// combinations before `exclude`.
fn entry_value_errors(entry: &WorkflowJob, id: &str, errors: &mut Vec<Diagnostic>) -> usize {
    for (key, value) in &entry.with {
        if reads_parameters(value) {
            errors.push(no_parameters_here(id, key));
        }
    }
    let matrix = &entry.matrix;
    if matrix.exclude.len() > MAX_EXCLUDE {
        errors.push(Diagnostic::new(format!(
            "`{id}` has {} matrix `exclude` entries; the limit is {MAX_EXCLUDE}",
            matrix.exclude.len()
        )));
    }
    for exclude in &matrix.exclude {
        if exclude.is_empty() {
            errors.push(Diagnostic::new(format!(
                "`{id}` has an empty matrix `exclude` entry, which would leave out every combination"
            )));
        }
        for (key, value) in exclude {
            if !matrix.values.contains_key(key) {
                errors.push(Diagnostic::new(format!(
                    "`{id}` has matrix `exclude` key `{key}`, which is not a key of the matrix"
                )));
            }
            if reads_parameters(value) {
                errors.push(no_parameters_here(id, key));
            }
        }
    }
    let mut size: usize = 1;
    for (key, values) in &matrix.values {
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
    if matrix.exclude.is_empty() {
        if size > MAX_MATRIX {
            errors.push(too_big_matrix(id, size));
        }
    } else if size > MAX_COMBINATIONS {
        errors.push(Diagnostic::new(format!(
            "`{id}` has a matrix of {size} combinations before `exclude`; the limit is {MAX_COMBINATIONS}"
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
        statuses: BTreeMap::new(),
        kind: JobKind::Approval,
        matrix: BTreeMap::new(),
        node: None,
        env: BTreeMap::new(),
        if_: None,
        timeout_minutes: DEFAULT_TIMEOUT_MINUTES,
        outputs: BTreeMap::new(),
        token_permissions: BTreeMap::new(),
        environment: None,
        steps: Vec::new(),
    }]
}

/// What a job runs on: one template, or the template a parameter names,
/// which can differ for each matrix entry.
enum Runner<'j> {
    /// Every planned job of the entry runs on this template.
    Fixed(&'static Template),
    /// `runs-on: ${{ parameters.NAME }}`: each planned job runs on the
    /// template its value of `NAME` names.
    Parameter(&'j str),
}

/// Finds what a job runs on. A `runs-on` that is one whole
/// `${{ parameters.NAME }}` must name a declared string or enum
/// parameter, and its default and enum values must name templates; the
/// value each planned job gets is checked when it is planned.
fn resolve_runner<'j>(
    config: &Config,
    owner: &str,
    job: &'j Job,
    errors: &mut Vec<Diagnostic>,
) -> Option<Runner<'j>> {
    let Some(name) = job.runs_on.as_deref().and_then(sole_parameter) else {
        return resolve_template(config, owner, job, errors).map(Runner::Fixed);
    };
    if job.executor.is_some() {
        return resolve_template(config, owner, job, errors).map(Runner::Fixed);
    }
    let Some(param) = job.parameters.get(name) else {
        errors.push(Diagnostic::new(format!(
            "{owner} has `runs-on: ${{{{ parameters.{name} }}}}` but declares no parameter `{name}`"
        )));
        return None;
    };
    let before = errors.len();
    let mut labels: Vec<&str> = Vec::new();
    match &param.kind {
        ParameterKind::String => {}
        ParameterKind::Enum(options) => labels.extend(options.iter().map(String::as_str)),
        _ => errors.push(Diagnostic::new(format!(
            "{owner} runs on parameter `{name}`, which must be a string or an enum"
        ))),
    }
    labels.extend(param.default.as_ref().and_then(Value::as_str));
    for label in labels {
        if template(label).is_none() {
            errors.push(unknown_template(
                &format!("{owner} parameter `{name}`"),
                label,
            ));
        }
    }
    (errors.len() == before).then_some(Runner::Parameter(name))
}

/// The template the planned job of one matrix entry runs on, given the
/// entry id and the combination, or `None` after reporting a parameter
/// value that names none.
fn leg_template(
    runner: &Runner<'_>,
    values: &BTreeMap<String, Value>,
    owner: &str,
    (id, combo): (&str, &BTreeMap<String, Value>),
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<&'static Template> {
    let name = match runner {
        Runner::Fixed(template) => return Some(template),
        Runner::Parameter(name) => *name,
    };
    let found = values.get(name).and_then(Value::as_str).and_then(template);
    if found.is_none() {
        let value = values.get(name).map_or_else(String::new, render);
        walk.report(
            format!(
                "{owner} has `runs-on: ${{{{ parameters.{name} }}}}`, which is `{value}` for `{}` and names no runner template; use one of {}",
                matrix_id(id, combo),
                template_labels().join(", ")
            ),
            errors,
        );
    }
    found
}

/// Finds the template of a job that names one, through `executor` or a
/// `runs-on` label.
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
            let Some(executor) = config.executors.get(name) else {
                errors.push(Diagnostic::new(format!(
                    "{owner} uses executor `{name}`, which is not defined in `executors:`"
                )));
                return None;
            };
            let found = template(&executor.runs_on);
            if found.is_none() {
                errors.push(unknown_template(
                    &format!("executor `{name}`"),
                    &executor.runs_on,
                ));
            }
            found
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
    if !walk.bytes(size_of::<PlannedJob>(), errors) {
        return None;
    }
    let mut steps = Vec::new();
    expand_steps(config, &job.steps, values, owner, walk, &mut steps, errors);
    if walk.budget.spent {
        return None;
    }
    let mut sub = Sub {
        values,
        scope: owner,
        walk,
        errors,
    };
    let env = sub.map(&job.env);
    let if_ = sub.opt(job.if_.as_deref());
    let outputs = sub.map(&job.outputs);
    let environment = sub.opt(job.environment.as_deref());
    if let Some(name) = &environment {
        if name.contains("${{") {
            errors.push(Diagnostic::new(format!(
                "{owner}: `environment` can use only parameters; found `{name}`"
            )));
        } else if let Err(rule) = valid_environment_name(name) {
            errors.push(Diagnostic::new(format!(
                "{owner}: `environment` `{name}`: {rule}"
            )));
        }
    }
    if errors.len() > before || walk.budget.spent {
        return None;
    }
    Some(PlannedJob {
        id: String::new(),
        name: String::new(),
        requires: Vec::new(),
        statuses: BTreeMap::new(),
        kind: JobKind::Approval,
        matrix: BTreeMap::new(),
        node: None,
        env,
        if_,
        timeout_minutes: job.timeout_minutes.unwrap_or(DEFAULT_TIMEOUT_MINUTES),
        outputs,
        token_permissions: token_scopes(job.permissions.as_ref()),
        environment,
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

    /// Reports a problem found while expanding, charging its bytes first:
    /// one config problem can be found once per command call.
    fn report(&mut self, message: String, errors: &mut Vec<Diagnostic>) {
        if self.bytes(size_of::<Diagnostic>() + message.len(), errors) {
            errors.push(Diagnostic::new(message));
        }
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
            if !walk.bytes(size_of::<PlannedStep>(), errors) {
                return;
            }
            let planned = plan_step(step, values, scope, walk, errors);
            if walk.budget.spent {
                return;
            }
            out.push(planned);
            continue;
        };
        let Some(command) = config.commands.get(name) else {
            walk.report(
                format!("{scope} uses command `{name}`, which is not defined in `commands:`"),
                errors,
            );
            continue;
        };
        if let Some(pos) = walk.stack.iter().position(|c| c == name) {
            let chain: Vec<String> = walk.stack[pos..]
                .iter()
                .chain(std::iter::once(name))
                .map(|c| format!("`{c}`"))
                .collect();
            walk.report(
                format!("command `{name}` reaches itself: {}", chain.join(" -> ")),
                errors,
            );
            continue;
        }
        if walk.stack.len() >= MAX_NESTING {
            walk.report(
                format!("{scope} nests commands deeper than {MAX_NESTING}"),
                errors,
            );
            continue;
        }
        let args = Sub {
            values,
            scope,
            walk,
            errors,
        }
        .value_map(with);
        if walk.budget.spent {
            return;
        }
        let inner = format!("command `{name}`");
        let Some(bound) = bind(&command.parameters, &args, &inner, walk, errors) else {
            if walk.budget.spent {
                return;
            }
            continue;
        };
        drop(args);
        walk.stack.push(name.clone());
        expand_steps(config, &command.steps, &bound, &inner, walk, out, errors);
        walk.stack.pop();
        if walk.budget.spent {
            return;
        }
    }
}

/// Checks parameter values against their declarations and fills in
/// defaults. `None` when a value is missing, unknown or the wrong kind,
/// or the budget is spent. Each value is charged before it is copied.
fn bind(
    params: &BTreeMap<String, Parameter>,
    args: &BTreeMap<String, Value>,
    owner: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, Value>> {
    let before = errors.len();
    for key in args.keys() {
        if !params.contains_key(key) {
            walk.report(format!("{owner} has no parameter `{key}`"), errors);
        }
    }
    let mut values = BTreeMap::new();
    for (key, param) in params {
        if walk.budget.spent {
            return None;
        }
        let Some(value) = args.get(key).or(param.default.as_ref()) else {
            walk.report(
                format!("{owner} needs a value for parameter `{key}`, which is required"),
                errors,
            );
            continue;
        };
        if let Some(want) = kind_mismatch(&param.kind, value) {
            walk.report(
                format!("{owner} parameter `{key}` must be {want}, found {value}"),
                errors,
            );
            continue;
        }
        if !walk.bytes(key.len() + ENTRY_OVERHEAD + value_bytes(value), errors) {
            return None;
        }
        values.insert(key.clone(), value.clone());
    }
    (errors.len() == before).then_some(values)
}

/// What a value should have been, when it is not of `kind`. A string
/// holding an expression passes any kind; it is checked at run time.
fn kind_mismatch(kind: &ParameterKind, value: &Value) -> Option<String> {
    if value.as_str().is_some_and(|s| expr::find_open(s).is_some()) {
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

/// Substitutes parameters in every string field of a step. Every string,
/// entry and value is charged before it is made.
fn plan_step(
    step: &Step,
    values: &BTreeMap<String, Value>,
    scope: &str,
    walk: &mut Walk<'_>,
    errors: &mut Vec<Diagnostic>,
) -> PlannedStep {
    let mut sub = Sub {
        values,
        scope,
        walk,
        errors,
    };
    match step {
        Step::Run(run) => PlannedStep::Run(RunStep {
            name: sub.opt(run.name.as_deref()),
            id: sub.copy(run.id.as_deref()),
            run: sub.text(&run.run),
            shell: sub.opt(run.shell.as_deref()),
            working_directory: sub.opt(run.working_directory.as_deref()),
            env: sub.map(&run.env),
            if_: sub.opt(run.if_.as_deref()),
            continue_on_error: run.continue_on_error,
            timeout_minutes: run.timeout_minutes,
        }),
        Step::Uses(uses) => {
            let planned = UsesStep {
                name: sub.opt(uses.name.as_deref()),
                id: sub.copy(uses.id.as_deref()),
                uses: sub.text(&uses.uses),
                with: sub.value_map(&uses.with),
                env: sub.map(&uses.env),
                if_: sub.opt(uses.if_.as_deref()),
                continue_on_error: uses.continue_on_error,
            };
            if planned.uses.starts_with("docker://") {
                sub.walk.report(
                    format!(
                        "{scope} has `uses: {}`; `uses: docker://` is not supported yet; Docker services and containers come in a later release",
                        planned.uses
                    ),
                    sub.errors,
                );
            }
            PlannedStep::Uses(planned)
        }
        Step::Checkout(checkout) => PlannedStep::Checkout(CheckoutStep {
            r#ref: sub.opt(checkout.r#ref.as_deref()),
            fetch_depth: checkout.fetch_depth,
            submodules: checkout.submodules,
            lfs: checkout.lfs,
            path: sub.opt(checkout.path.as_deref()),
        }),
        Step::Cache(cache) => PlannedStep::Cache(CacheStep {
            key: sub.text(&cache.key),
            restore_keys: sub.list(&cache.restore_keys),
            paths: sub.list(&cache.paths),
        }),
        Step::Workspace(WorkspaceStep::Persist { root, paths }) => {
            PlannedStep::Workspace(WorkspaceStep::Persist {
                root: sub.text(root),
                paths: sub.list(paths),
            })
        }
        Step::Workspace(WorkspaceStep::Attach { at }) => {
            PlannedStep::Workspace(WorkspaceStep::Attach { at: sub.text(at) })
        }
        Step::TestResults { path } => PlannedStep::TestResults {
            path: sub.text(path),
        },
        Step::Command { .. } => unreachable!("commands are expanded by expand_steps"),
    }
}

/// Substitutes the parameters of one scope into strings and values,
/// charging the walk's budget for each string, map entry, list item and
/// value before it is made. Once the budget is spent it makes empty
/// strings and stops filling maps and lists.
struct Sub<'s, 'w> {
    /// The parameter values of the scope.
    values: &'s BTreeMap<String, Value>,
    /// The scope, as named in errors.
    scope: &'s str,
    /// The walk whose budget is charged.
    walk: &'s mut Walk<'w>,
    /// Where problems go.
    errors: &'s mut Vec<Diagnostic>,
}

impl Sub<'_, '_> {
    /// `text` with parameters substituted.
    fn text(&mut self, text: &str) -> String {
        substitute(text, self.values, self.scope, self.walk, self.errors)
    }

    /// An optional string with parameters substituted.
    fn opt(&mut self, text: Option<&str>) -> Option<String> {
        text.map(|t| self.text(t))
    }

    /// An optional string copied as it is.
    fn copy(&mut self, text: Option<&str>) -> Option<String> {
        let text = text?;
        self.walk
            .bytes(text.len(), self.errors)
            .then(|| text.to_owned())
    }

    /// A map of strings with parameters substituted in its values.
    /// Collected, not inserted one by one: a map built from sorted
    /// entries packs its nodes full.
    fn map(&mut self, map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        map.iter()
            .map_while(|(key, value)| {
                self.walk
                    .bytes(key.len() + ENTRY_OVERHEAD, self.errors)
                    .then(|| (key.clone(), self.text(value)))
            })
            .collect()
    }

    /// A list of strings with parameters substituted.
    fn list(&mut self, list: &[String]) -> Vec<String> {
        list.iter()
            .map_while(|item| {
                self.walk
                    .bytes(ENTRY_OVERHEAD, self.errors)
                    .then(|| self.text(item))
            })
            .collect()
    }

    /// A map of values with parameters substituted in its values.
    fn value_map(&mut self, map: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        map.iter()
            .map_while(|(key, value)| {
                self.walk
                    .bytes(key.len() + ENTRY_OVERHEAD, self.errors)
                    .then(|| (key.clone(), self.value(value)))
            })
            .collect()
    }

    /// Substitutes parameters inside a value. A string that is exactly
    /// one `${{ parameters.x }}` becomes the parameter's value, keeping
    /// its type.
    fn value(&mut self, value: &Value) -> Value {
        if let Value::String(text) = value
            && let Some(found) = sole_parameter(text).and_then(|name| self.values.get(name))
        {
            if !self.walk.bytes(value_bytes(found), self.errors) {
                return Value::Null;
            }
            return found.clone();
        }
        if !self.walk.bytes(size_of::<Value>(), self.errors) {
            return Value::Null;
        }
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    if self.walk.budget.spent {
                        break;
                    }
                    out.push(self.value(item));
                }
                Value::Array(out)
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map_while(|(key, item)| {
                        self.walk
                            .bytes(key.len() + ENTRY_OVERHEAD, self.errors)
                            .then(|| (key.clone(), self.value(item)))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
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
    if expr::find_close(inner).is_some() {
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
    while let Some(start) = expr::find_open(rest) {
        let body = &rest[start + 3..];
        let Some(end) = expr::find_close(body) else {
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
    if walk.budget.spent {
        return String::new();
    }
    if expr::find_open(text).is_none() {
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
                    rendered_len(value)
                } else {
                    walk.report(
                        format!(
                            "{scope} reads `${{{{ parameters.{name} }}}}` but declares no parameter `{name}`"
                        ),
                        errors,
                    );
                    whole.len()
                }
            }
        });
    }
    if let Some(name) = first.filter(|_| len > MAX_VALUE) {
        walk.report(
            format!(
                "{scope} substitutes `${{{{ parameters.{name} }}}}` into a value of {len} bytes; the limit is {MAX_VALUE}"
            ),
            errors,
        );
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
                // Written in place: the text was charged, a copy was not.
                Some(v @ (Value::Array(_) | Value::Object(_))) => {
                    let _ = write!(out, "{v}");
                }
                Some(other) => out.push_str(&render(other)),
                None => out.push_str(whole),
            },
        }
    }
    out
}

/// The length of a value rendered as text, worked out without
/// rendering a list or map.
fn rendered_len(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Array(_) | Value::Object(_) => {
            let mut count = Count(0);
            let _ = write!(count, "{value}");
            count.0
        }
        other => render(other).len(),
    }
}

/// Counts the bytes written to it.
struct Count(usize);

impl std::fmt::Write for Count {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// The bytes a value holds in memory: a `Value` per node, its strings,
/// and `ENTRY_OVERHEAD` per map entry.
fn value_bytes(value: &Value) -> usize {
    size_of::<Value>()
        + match value {
            Value::String(s) => s.len(),
            Value::Array(items) => items.iter().map(value_bytes).sum(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| k.len() + ENTRY_OVERHEAD + value_bytes(v))
                .sum(),
            _ => 0,
        }
}

/// The bytes of a map of strings: its text and `ENTRY_OVERHEAD` per entry.
fn map_bytes(m: &BTreeMap<String, String>) -> usize {
    m.iter()
        .map(|(k, v)| k.len() + v.len() + ENTRY_OVERHEAD)
        .sum()
}

/// The bytes of a list of strings: its text and `ENTRY_OVERHEAD` per
/// string.
fn list_bytes(l: &[String]) -> usize {
    l.iter().map(|s| s.len() + ENTRY_OVERHEAD).sum()
}

/// The bytes a copy of a step holds: the step struct, its strings and
/// `ENTRY_OVERHEAD` per map entry and list string.
fn step_bytes(step: &PlannedStep) -> usize {
    let opt = |o: &Option<String>| o.as_ref().map_or(0, String::len);
    size_of::<PlannedStep>()
        + match step {
            PlannedStep::Run(r) => {
                opt(&r.name)
                    + opt(&r.id)
                    + r.run.len()
                    + opt(&r.shell)
                    + opt(&r.working_directory)
                    + map_bytes(&r.env)
                    + opt(&r.if_)
            }
            PlannedStep::Uses(u) => {
                opt(&u.name)
                    + opt(&u.id)
                    + u.uses.len()
                    + u.with
                        .iter()
                        .map(|(k, v)| k.len() + value_bytes(v) + ENTRY_OVERHEAD)
                        .sum::<usize>()
                    + map_bytes(&u.env)
                    + opt(&u.if_)
            }
            PlannedStep::Checkout(c) => opt(&c.r#ref) + opt(&c.path),
            PlannedStep::Cache(c) => {
                c.key.len() + list_bytes(&c.restore_keys) + list_bytes(&c.paths)
            }
            PlannedStep::Workspace(WorkspaceStep::Persist { root, paths }) => {
                root.len() + list_bytes(paths)
            }
            PlannedStep::Workspace(WorkspaceStep::Attach { at }) => at.len(),
            PlannedStep::TestResults { path } => path.len(),
        }
}

/// The bytes a copy of a planned job holds: the job struct, its steps,
/// `env`, `outputs` and `if`.
fn job_bytes(job: &PlannedJob) -> usize {
    size_of::<PlannedJob>()
        + job.steps.iter().map(step_bytes).sum::<usize>()
        + map_bytes(&job.env)
        + map_bytes(&job.outputs)
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
