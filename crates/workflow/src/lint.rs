// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Warnings about workflows that run attacker-influenced input with the
//! base repository's secrets.
//!
//! A `pull_request_target`, `issues`, `issue_comment` or `on.command`
//! workflow runs trusted (secrets, a write token) on an event that
//! anyone can cause. Two mistakes turn that into remote code execution,
//! and both are found by reading the plan:
//!
//! - [`LintKind::HeadCheckout`]: the workflow gets the pull request's
//!   head (the fork's code) onto the runner and runs it. A job is
//!   *tainted* when anything in it reads the head: a path of the
//!   `github` context at or above `github.event.pull_request.head`, the
//!   head ref, a `refs/pull/<n>/head` ref, `GITHUB_HEAD_REF`. The read can
//!   be anywhere in the job (its env, a step's env, `with:`, a script),
//!   so passing the value through `env:` to a script does not hide it;
//!   an `if:`, a `concurrency:` or a `name:` only compares or labels, and
//!   is not a read. A job that reads an output of a tainted job through
//!   `needs` is tainted too. A tainted job that checks out code or runs
//!   a command that fetches it is a head checkout, and so is a script
//!   that fetches code and looks the pull request up at run time (its
//!   number, `GITHUB_EVENT_PATH`, `gh api` of `pulls`), names a
//!   `refs/pull/` refspec, or downloads a tarball or zipball.
//!   [`LintKind::UnverifiedCheckout`] is a checkout whose `ref` or
//!   `repository` is an expression that is not provably the base.
//! - [`LintKind::ScriptInjection`]: a field the pull request's author
//!   writes (title, body, branch name) is interpolated with `${{ }}`
//!   straight into a `run:` script, where the shell reads it as code.
//!   The text of an issue or comment and the line and arguments of a
//!   command are such fields.
//!
//! Two more are about who may start an `issue_comment` workflow, and
//! are found in the plan's `if:` expressions:
//!
//! - [`LintKind::UnguardedComment`]: a job that nothing in its `if:`
//!   (or the `if:` of a job it needs) ties to the commenter.
//! - [`LintKind::BodyContains`]: an `if:` that finds a command in the
//!   comment with `contains`, which matches a quote and a code block.
//!
//! A command's checkout of `github.event.command.pull_request.head_sha`
//! is the one head read that is accepted, when every command of the
//! workflow needs `write` or higher: the commenter named the sha and
//! the control plane compared it with the pull request's head.
//!
//! Expressions are read from their parse, so `github.event.x`,
//! `github['event']['x']` and `GitHub.Event.X` are the same read, a
//! computed index (`github.event.pull_request[format(...)]`) can be any
//! name, and reading a parent (`toJSON(github.event.pull_request)`) reads
//! what is under it. The detectors still work on the plan and not on what
//! a script computes at run time, so a workflow can be unsafe without a
//! warning, and a warning can be a false alarm. A warning never stops
//! `validate` or `convert` from succeeding, and the host never refuses a
//! run for one: the findings are for the people who approve and read the
//! run (see the repository's `pull_request_target_head_checkout`
//! setting).

use serde_json::Value;

use crate::config::{CheckoutStep, UsesStep};
use crate::expr::{reads, reads_expression};
use crate::plan::{Plan, PlannedJob, PlannedStep};
use crate::trigger::{CommandPermission, ShaPolicy};

/// What a lint found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintKind {
    /// The workflow gets the pull request's head onto the runner.
    HeadCheckout,
    /// The workflow checks out a ref or repository that is an
    /// expression not provably the base.
    UnverifiedCheckout,
    /// An untrusted event field is interpolated into a `run:` script.
    ScriptInjection,
    /// A job of an `issue_comment` workflow that nothing ties to who
    /// commented.
    UnguardedComment,
    /// An `if:` that finds a command in the comment with `contains`.
    BodyContains,
}

impl LintKind {
    /// Whether a repository that blocks head checkouts blocks this.
    #[must_use]
    pub const fn is_head_checkout(self) -> bool {
        matches!(self, Self::HeadCheckout | Self::UnverifiedCheckout)
    }
}

/// One finding, with where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    /// What was found.
    pub kind: LintKind,
    /// The workflow's name.
    pub workflow: String,
    /// The planned job's id.
    pub job: String,
    /// The step's position in the job, from 1; 0 for a finding about the
    /// job as a whole.
    pub step: usize,
    /// What is wrong and how to fix it, in one sentence or two.
    pub message: String,
}

impl Lint {
    /// The finding as one line that names its place.
    #[must_use]
    pub fn text(&self) -> String {
        if self.step == 0 {
            return format!(
                "workflow `{}`, job `{}`: {}",
                self.workflow, self.job, self.message
            );
        }
        format!(
            "workflow `{}`, job `{}`, step {}: {}",
            self.workflow, self.job, self.step, self.message
        )
    }
}

/// The text of the head checkout finding. The head checkout is what the
/// repository setting `pull_request_target_head_checkout` blocks.
const HEAD_CHECKOUT: &str = "checks out the pull request head in a workflow that runs with the base \
     repository's secrets (pull_request_target, issues, issue_comment or on.command), \
     so the code of the pull request runs with them and the write token; \
     check out the base and run the pull request's code in a pull_request workflow instead";

/// The text of the unverified checkout finding.
const UNVERIFIED_CHECKOUT: &str = "checks out a ref or repository that is an expression not \
     provably the base branch, so in a workflow that runs with the base repository's secrets it \
     may be the pull request head, which runs the author's code with them and the write token; \
     use `github.sha`, `github.base_ref` or a literal";

/// Paths of an event that the author of an issue, comment or pull
/// request writes.
const UNTRUSTED: &[&str] = &[
    "github.event.issue.title",
    "github.event.issue.body",
    "github.event.pull_request.title",
    "github.event.pull_request.body",
    "github.event.pull_request.head.ref",
    "github.event.pull_request.head.label",
    "github.event.pull_request.head.repo.default_branch",
    "github.event.comment.body",
    "github.event.command.line",
    "github.event.command.args",
    "github.event.command.named",
    "github.event.review.body",
    "github.event.review_comment.body",
    "github.event.pages",
    "github.event.commits",
    "github.event.head_commit.message",
    "github.event.head_commit.author.email",
    "github.event.head_commit.author.name",
    "github.event.workflow_run.head_branch",
    "github.event.workflow_run.display_title",
    "github.event.workflow_run.head_commit.message",
    "github.head_ref",
];

/// Paths that name the pull request's head, or its merge commit, which
/// contains it.
const HEAD_PATHS: &[&str] = &[
    "github.event.pull_request.head",
    "github.event.pull_request.merge_commit_sha",
    "github.head_ref",
    COMMAND_HEAD_SHA,
    "github.event.command.pull_request.head_repo",
];

/// The head sha the commenter named, which the control plane compared
/// with the pull request's head before the run was made.
const COMMAND_HEAD_SHA: &str = "github.event.command.pull_request.head_sha";

/// Paths a `ref` input may read and still be the base.
const BASE_REFS: &[&str] = &[
    "github.base_ref",
    "github.sha",
    "github.ref",
    "github.ref_name",
    "github.event.pull_request.base",
    "github.event.repository.default_branch",
];

/// Paths a `repository` input may read and still be the base.
const BASE_REPOSITORIES: &[&str] = &["github.repository", "github.event.pull_request.base"];

/// The git commands that fetch code or move a checkout to it.
const GIT_FETCHERS: &[&str] = &[
    "fetch",
    "checkout",
    "clone",
    "pull",
    "switch",
    "worktree",
    "merge",
    "reset",
    "cherry-pick",
    "restore",
];

/// What a workflow's events make of the paths it reads.
struct Rules {
    /// Paths that name the pull request head.
    head: Vec<&'static str>,
    /// Paths a `ref` input may read and still be the base.
    base_refs: Vec<&'static str>,
}

impl Rules {
    /// The rules for a workflow whose event is not known: the commenter's
    /// sha pin counts as a head.
    fn strict() -> Self {
        Self {
            head: HEAD_PATHS.to_vec(),
            base_refs: BASE_REFS.to_vec(),
        }
    }

    /// The rules for the workflow with triggers `on`. A command's sha pin
    /// is accepted when every command of the workflow needs `write` or
    /// higher and has a sha rule of `required` or `auto`: with
    /// `optional` the sha can be left out, and then the run is on the
    /// head the pull request has when the command is read.
    fn for_triggers(on: &crate::Triggers) -> Self {
        let mut rules = Self::strict();
        let pinned = on.command.as_ref().is_some_and(|commands| {
            !commands.is_empty()
                && commands.iter().all(|(_, options)| {
                    options.permission >= CommandPermission::Write
                        && options.sha != ShaPolicy::Optional
                })
        });
        if pinned {
            rules.head.retain(|path| *path != COMMAND_HEAD_SHA);
            rules.base_refs.push(COMMAND_HEAD_SHA);
        }
        rules
    }
}

/// Whether `on` names an event this module warns about: the workflows
/// that run attacker-influenced events with the base repository's
/// secrets (`pull_request_target`, `issues`, `issue_comment` and
/// `command`).
#[must_use]
pub fn guarded(on: &crate::Triggers) -> bool {
    on.pull_request_target.is_some()
        || on.issues.is_some()
        || on.issue_comment.is_some()
        || on.command.is_some()
}

/// The findings in the jobs of `plan`, which is the plan of a workflow
/// with triggers `on`: those of [`plan_lints`], and for an
/// `issue_comment` workflow the findings about who may start it. A
/// command's checkout of its pinned head sha is accepted when every
/// command of `on` needs `write` or higher and has a sha rule of
/// `required` or `auto`. The caller decides whether
/// the workflow is one to warn about (see [`guarded`]).
#[must_use]
pub fn workflow_lints(plan: &Plan, on: &crate::Triggers) -> Vec<Lint> {
    let mut found = lints(plan, &Rules::for_triggers(on));
    if on.issue_comment.is_some() {
        found.extend(comment_lints(plan));
    }
    found
}

/// The findings about who may start an `issue_comment` workflow: jobs
/// whose `if:` does not look at the commenter, and `if:` expressions
/// that find a command with `contains`.
fn comment_lints(plan: &Plan) -> Vec<Lint> {
    let mut found = Vec::new();
    // The jobs that something ties to the commenter, by id.
    let mut checked: Vec<&str> = Vec::new();
    for job in &plan.jobs {
        // A job that needs a checked job does not run when that one is
        // skipped.
        let ties = job.if_.as_deref().is_some_and(checks_commenter)
            || job.requires.iter().any(|id| checked.contains(&id.as_str()));
        if ties {
            checked.push(&job.id);
        } else {
            found.push(at(
                plan,
                job,
                0,
                LintKind::UnguardedComment,
                UNGUARDED_COMMENT,
            ));
        }
        for (step, cond) in conditions(job) {
            if searches_comment_body(&cond) {
                found.push(at(plan, job, step, LintKind::BodyContains, BODY_CONTAINS));
            }
        }
    }
    found
}

/// The text of the unguarded comment finding.
const UNGUARDED_COMMENT: &str = "runs on issue_comment and no `if:` of it or of a job it needs \
     checks who commented (`author_association`, `github.actor` or a permission), so anyone who \
     can comment can run it with the base repository's secrets; check the commenter, or use \
     `on.command`, which checks the repository permission before the run exists";

/// The text of the `contains` finding.
const BODY_CONTAINS: &str = "finds a command with `contains` on the comment text, which also \
     matches a quote of an earlier comment and text in a code block; use \
     `startsWith(github.event.comment.body, '/name')` or `on.command`";

/// Whether an `if:` looks at who commented: it names `author_association`,
/// an actor or a permission.
fn checks_commenter(cond: &str) -> bool {
    let lower = cond.to_ascii_lowercase();
    ["author_association", "actor", "permission"]
        .iter()
        .any(|word| lower.contains(word))
}

/// The `if:` of a job and of its steps, with the step's position in the
/// job from 1, and 0 for the job's own.
fn conditions(job: &PlannedJob) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    if let Some(cond) = &job.if_ {
        out.push((0, cond.clone()));
    }
    let Ok(Value::Object(fields)) = serde_json::to_value(job) else {
        return out;
    };
    // A step is `{ "<kind>": { fields } }`.
    for (i, step) in fields
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let cond = step
            .as_object()
            .into_iter()
            .flat_map(|kinds| kinds.values())
            .find_map(|inner| inner.get("if")?.as_str());
        if let Some(cond) = cond {
            out.push((i + 1, cond.to_owned()));
        }
    }
    out
}

/// Whether an `if:` calls `contains` on the comment's text.
fn searches_comment_body(cond: &str) -> bool {
    const BODY: &str = "github.event.comment.body";
    let found = if cond.contains("${{") {
        reads(cond)
    } else {
        reads_expression(cond).map(|one| vec![one])
    };
    let Ok(list) = found else {
        let flat = squashed(cond);
        return flat.contains("contains(") && flat.contains(BODY);
    };
    list.iter()
        .any(|read| read.searched.iter().any(|path| touches(path, BODY)))
}

/// The findings in the jobs of `plan`, in job and step order, whatever
/// its event: a sha a command pins counts as a head here, and nothing is
/// said about who may start the workflow (see [`workflow_lints`], which
/// reads the workflow's triggers too). The caller decides whether the
/// plan's event is one to warn about (see [`guarded`]). Commands and
/// called workflows are already expanded in a plan, so their steps are
/// read too.
#[must_use]
pub fn plan_lints(plan: &Plan) -> Vec<Lint> {
    lints(plan, &Rules::strict())
}

fn lints(plan: &Plan, rules: &Rules) -> Vec<Lint> {
    let mut found = Vec::new();
    // The jobs that read the head, by id, and why.
    let mut tainted_jobs: Vec<(&str, String)> = Vec::new();
    for job in &plan.jobs {
        let taint = job_taint(job, rules).or_else(|| {
            // A job that reads what a tainted job it needs produced.
            let from = job
                .requires
                .iter()
                .find_map(|id| tainted_jobs.iter().find(|(known, _)| known == id))?;
            reads_needs(job).then(|| format!("an output of the job `{}`", from.0))
        });
        if let Some(reason) = &taint {
            tainted_jobs.push((job.id.as_str(), reason.clone()));
        }
        let tainted = |what: &str| {
            taint.as_ref().map(|reason| {
                format!(
                    "{what} in a job that reads the pull request head ({reason}) in a \
                     workflow that runs with the base repository's secrets, so the code of the \
                     pull request can run with them and the write token; keep the head out of \
                     this job and run the pull request's code in a pull_request workflow instead"
                )
            })
        };
        for (i, step) in job.steps.iter().enumerate() {
            let (kind, message) = match step {
                PlannedStep::Checkout(native) => {
                    match checkout_finding(
                        native_checkout(native, rules),
                        tainted("checks out code"),
                    ) {
                        Some(hit) => hit,
                        None => continue,
                    }
                }
                PlannedStep::Uses(uses) => {
                    if let Some(field) = script_input(uses).and_then(injected) {
                        found.push(lint(
                            plan,
                            job,
                            i,
                            LintKind::ScriptInjection,
                            injection(&field),
                        ));
                    }
                    if is_action(uses, "actions/checkout") {
                        match checkout_finding(
                            action_checkout(uses, rules),
                            tainted("checks out code"),
                        ) {
                            Some(hit) => hit,
                            None => continue,
                        }
                    } else {
                        continue;
                    }
                }
                PlannedStep::Run(run) => {
                    if let Some(field) = injected(&run.run) {
                        found.push(lint(
                            plan,
                            job,
                            i,
                            LintKind::ScriptInjection,
                            injection(&field),
                        ));
                    }
                    if has_word_run(&run.run, &["gh", "pr", "checkout"]) {
                        (LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned())
                    } else if fetches_code(&run.run)
                        && let Some(how) = looks_up_pull_request(&run.run)
                    {
                        (
                            LintKind::HeadCheckout,
                            format!(
                                "runs a command that fetches code and finds the pull request at \
                                 run time ({how}), so the code of the pull request can run with \
                                 the base repository's secrets and write token; keep the head out \
                                 of this job and run the pull request's code in a pull_request \
                                 workflow instead"
                            ),
                        )
                    } else if fetches_code(&run.run)
                        && let Some(message) = tainted("runs a command that fetches code")
                    {
                        (LintKind::HeadCheckout, message)
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            found.push(lint(plan, job, i, kind, message));
        }
    }
    found
}

fn lint(plan: &Plan, job: &PlannedJob, step: usize, kind: LintKind, message: String) -> Lint {
    Lint {
        kind,
        workflow: plan.workflow.clone(),
        job: job.id.clone(),
        step: step + 1,
        message,
    }
}

/// A finding at `step` of `job`, counted from 1, or 0 for the job.
fn at(plan: &Plan, job: &PlannedJob, step: usize, kind: LintKind, message: &str) -> Lint {
    Lint {
        kind,
        workflow: plan.workflow.clone(),
        job: job.id.clone(),
        step,
        message: message.to_owned(),
    }
}

/// What a checkout step is found to be: what its own inputs rate, else
/// a head checkout when its job is tainted.
fn checkout_finding(
    rated: Option<LintKind>,
    tainted: Option<String>,
) -> Option<(LintKind, String)> {
    match rated {
        Some(LintKind::HeadCheckout) => Some((LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned())),
        Some(kind) => Some((kind, UNVERIFIED_CHECKOUT.to_owned())),
        None => tainted.map(|message| (LintKind::HeadCheckout, message)),
    }
}

fn injection(field: &str) -> String {
    format!(
        "interpolates `{field}` into a script, which lets whoever wrote it run commands; \
         pass it through `env:` and read it as a shell variable"
    )
}

/// Whether `path` reads `entry`: the same place, a place below it, or a
/// parent of it (`github.event` reads everything under it). A step of
/// `*` (a computed index) can be any name.
fn touches(path: &[String], entry: &str) -> bool {
    path.iter()
        .zip(entry.split('.'))
        .all(|(step, want)| step == "*" || step == want)
}

/// Whether `path` is `under` the dotted `entry`, or the entry itself: a
/// stricter match than [`touches`], for what must be provably the base.
fn under(path: &[String], entry: &str) -> bool {
    let want: Vec<&str> = entry.split('.').collect();
    path.len() >= want.len() && path.iter().zip(&want).all(|(a, b)| a == b)
}

/// The text with case, spaces and quotes removed, for a substring match
/// on an expression that does not parse.
fn squashed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        let c = if matches!(c, '\'' | '"' | '[' | ']') {
            '.'
        } else {
            c.to_ascii_lowercase()
        };
        if c != '.' || !out.ends_with('.') {
            out.push(c);
        }
    }
    out
}

/// What reading `text` says about the pull request head, `None` when it
/// says nothing: the path it reads, or the ref it names. Text with an
/// expression that does not parse counts as reading it.
fn head_read(text: &str, rules: &Rules) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("github_head_ref") {
        return Some("GITHUB_HEAD_REF".to_owned());
    }
    if pull_ref(&lower) {
        return Some("a refs/pull ref".to_owned());
    }
    let Ok(list) = reads(text) else {
        let flat = squashed(text);
        return rules
            .head
            .iter()
            .any(|entry| flat.contains(entry))
            .then(|| "an expression that cannot be read".to_owned());
    };
    for read in &list {
        for path in &read.paths {
            if rules.head.iter().any(|entry| touches(path, entry)) {
                return Some(path.join("."));
            }
        }
        if read
            .strings
            .iter()
            .any(|s| pull_ref(&s.to_ascii_lowercase()))
        {
            return Some("a refs/pull ref".to_owned());
        }
    }
    None
}

/// Whether `text` names the pull request head.
fn names_head(text: &str, rules: &Rules) -> bool {
    head_read(text, rules).is_some()
}

/// Why the job is tainted: the first read of the pull request head found
/// in it (its env, its matrix, a step's env, `with:` or script, its
/// container and services), else `None`. What only compares or labels
/// is not a read: the job's own `if:`, `continue-on-error`,
/// `concurrency:` and `name:`, and a step's. A job that cannot be read
/// counts as tainted.
fn job_taint(job: &PlannedJob, rules: &Rules) -> Option<String> {
    let Some(texts) = job_texts(job) else {
        return Some("a job that could not be read".to_owned());
    };
    texts.iter().find_map(|text| head_read(text, rules))
}

/// Whether anything in the job reads the `needs` context.
fn reads_needs(job: &PlannedJob) -> bool {
    let Some(texts) = job_texts(job) else {
        return true;
    };
    texts.iter().any(|text| {
        reads(text).map_or(true, |list| {
            list.iter().any(|read| {
                read.paths
                    .iter()
                    .any(|path| path.first().is_some_and(|c| c == "needs"))
            })
        })
    })
}

/// Keys of a planned job that only compare or label, at the job's own
/// level. What is nested in the other keys is read whatever it is called:
/// a `matrix` value, an env variable or a `with:` input named `name`.
const JOB_LABELS: &[&str] = &[
    "if_",
    "continue_on_error",
    "concurrency",
    "name",
    "id",
    "requires",
    "strategy",
];

/// Keys of a planned step that only compare or label, at the step's own
/// level.
const STEP_LABELS: &[&str] = &["if", "continue_on_error", "name", "id", "timeout_minutes"];

/// The texts of a job that can read something: every string in it but
/// the labels, `None` when the job does not serialize.
fn job_texts(job: &PlannedJob) -> Option<Vec<String>> {
    fn strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
            Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
            _ => {}
        }
    }
    let Value::Object(fields) = serde_json::to_value(job).ok()? else {
        return None;
    };
    let mut out = Vec::new();
    for (key, value) in &fields {
        if JOB_LABELS.contains(&key.as_str()) {
            continue;
        }
        if key != "steps" {
            strings(value, &mut out);
            continue;
        }
        // A step is `{ "<kind>": { fields } }`.
        for step in value.as_array()? {
            let Value::Object(kinds) = step else {
                strings(step, &mut out);
                continue;
            };
            for inner in kinds.values() {
                let Value::Object(fields) = inner else {
                    strings(inner, &mut out);
                    continue;
                };
                for (key, value) in fields {
                    if !STEP_LABELS.contains(&key.as_str()) {
                        strings(value, &mut out);
                    }
                }
            }
        }
    }
    Some(out)
}

/// How a script finds the pull request at run time, when it does: its
/// number from the event, the event file, `gh api` of the pull
/// requests, or a `refs/pull/` refspec (a split or wildcard one too).
fn looks_up_pull_request(script: &str) -> Option<&'static str> {
    let lower = script.to_ascii_lowercase();
    if lower.contains("github_event_path") {
        return Some("GITHUB_EVENT_PATH");
    }
    if lower.contains("gh api") && (lower.contains("pulls") || lower.contains("/pull/")) {
        return Some("gh api");
    }
    if lower.contains("refs/pull/") {
        return Some("a refs/pull refspec");
    }
    if (lower.contains("curl") || lower.contains("wget"))
        && ["tarball", "zipball", "/archive/"]
            .iter()
            .any(|word| lower.contains(word))
    {
        return Some("a download of a repository archive");
    }
    let numbered = reads(script).is_ok_and(|list| {
        list.iter().any(|read| {
            read.paths.iter().any(|path| {
                [
                    "github.event.number",
                    "github.event.pull_request.number",
                    "github.event.issue.number",
                ]
                .iter()
                .any(|entry| touches(path, entry))
            })
        })
    });
    numbered.then_some("the pull request number")
}

/// Whether the text of a `ref` or `repository` input is provably the
/// base: no expression, or only expressions that are one read of a path
/// in `allowed` (or one literal).
fn provably_base(text: &str, allowed: &[&str]) -> bool {
    let Ok(list) = reads(text) else {
        return false;
    };
    list.iter().all(|read| {
        read.plain
            && read
                .paths
                .iter()
                .all(|path| allowed.iter().any(|entry| under(path, entry)))
    })
}

/// How a checkout's `ref` and `repository` rate: the head, an
/// expression that may be, or fine.
fn rate_checkout(inputs: &[(&str, &[&str])], rules: &Rules) -> Option<LintKind> {
    if inputs.iter().any(|(text, _)| names_head(text, rules)) {
        return Some(LintKind::HeadCheckout);
    }
    inputs
        .iter()
        .any(|(text, allowed)| !provably_base(text, allowed))
        .then_some(LintKind::UnverifiedCheckout)
}

fn native_checkout(step: &CheckoutStep, rules: &Rules) -> Option<LintKind> {
    let text = step.r#ref.as_deref()?;
    rate_checkout(&[(text, &rules.base_refs)], rules)
}

fn is_action(step: &UsesStep, name: &str) -> bool {
    step.uses.to_ascii_lowercase().split('@').next() == Some(name)
}

fn action_checkout(step: &UsesStep, rules: &Rules) -> Option<LintKind> {
    let mut inputs: Vec<(&str, &[&str])> = Vec::new();
    for (name, allowed) in [
        ("ref", &rules.base_refs[..]),
        ("repository", BASE_REPOSITORIES),
    ] {
        if let Some(text) = step.with.get(name).and_then(Value::as_str) {
            inputs.push((text, allowed));
        }
    }
    rate_checkout(&inputs, rules)
}

/// Whether a script runs a command that fetches code or moves a
/// checkout to it: a git command such as `fetch` or `checkout` (within
/// a few words of `git`, which covers `git -C dir fetch`), `gh pr
/// checkout`, `gh repo clone`, or `curl` or `wget` of a tarball, zipball
/// or archive.
fn fetches_code(script: &str) -> bool {
    let flat: String = script
        .chars()
        .map(|c| {
            if matches!(c, ';' | '&' | '|' | '(' | ')') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    let lower = flat.to_ascii_lowercase();
    let archive = ["tarball", "zipball", "/archive/"]
        .iter()
        .any(|word| lower.contains(word));
    words.iter().enumerate().any(|(i, word)| match *word {
        "git" => words[i + 1..]
            .iter()
            .take(4)
            .any(|w| GIT_FETCHERS.contains(w)),
        "gh" => matches!(
            words.get(i + 1..i + 3),
            Some(["pr", "checkout"] | ["repo", "clone"])
        ),
        // A download of a repository archive is a fetch of code.
        "curl" | "wget" => archive,
        _ => false,
    })
}

/// Whether `words` appear in `text` one after another.
fn has_word_run(text: &str, words: &[&str]) -> bool {
    let all: Vec<&str> = text.split_whitespace().collect();
    all.windows(words.len()).any(|w| w == words)
}

/// Whether `c` can be in a git ref name.
fn is_ref_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

/// The text after a pull request number at the start of `text`: digits,
/// a `${{ }}`, a `${VAR}`, a `$VAR` or a `{0}` placeholder.
fn after_number(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix("${{") {
        return rest.split_once("}}").map(|(_, rest)| rest);
    }
    if let Some(rest) = text.strip_prefix("${") {
        return rest.split_once('}').map(|(_, rest)| rest);
    }
    if let Some(rest) = text.strip_prefix('{') {
        return rest.split_once('}').map(|(_, rest)| rest);
    }
    let name = if let Some(rest) = text.strip_prefix('$') {
        rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_')
    } else {
        text.trim_start_matches(|c: char| c.is_ascii_digit())
    };
    (name.len() < text.len()).then_some(name)
}

/// Whether `text` (lowercase) has a ref of the form
/// `refs/pull/<n>/head` or `refs/pull/<n>/merge`, with or without the
/// `refs/` before it. It starts where a ref starts and ends where it
/// ends, so `release/pull/2/head-fix` and `pull/2/header` are other
/// branches.
fn pull_ref(text: &str) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find("pull/") {
        let at = from + found;
        from = at + "pull/".len();
        let before = &text[..at];
        let before = before.strip_suffix("refs/").unwrap_or(before);
        if before.chars().next_back().is_some_and(is_ref_char) {
            continue;
        }
        let Some(rest) = after_number(&text[from..]).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        for end in ["head", "merge"] {
            if let Some(more) = rest.strip_prefix(end)
                && !more.chars().next().is_some_and(is_ref_char)
            {
                return true;
            }
        }
    }
    false
}

/// The script of an `actions/github-script` step, which runs the text
/// as JavaScript.
fn script_input(step: &UsesStep) -> Option<&str> {
    if !is_action(step, "actions/github-script") {
        return None;
    }
    step.with.get("script")?.as_str()
}

/// The first untrusted field that `script` interpolates with `${{ }}`,
/// as a dotted path. Reading a parent of one (`toJSON(github.event)`)
/// reads it.
fn injected(script: &str) -> Option<String> {
    let Ok(list) = reads(script) else {
        // An expression that does not parse is read as text.
        let flat = squashed(script);
        return UNTRUSTED
            .iter()
            .find(|entry| flat.contains(*entry))
            .map(|entry| (*entry).to_owned());
    };
    list.iter().find_map(|read| {
        read.paths.iter().find_map(|path| {
            UNTRUSTED
                .iter()
                .find(|entry| touches(path, entry))
                .map(|entry| (*entry).to_owned())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, compile, parse, validate_with_warnings};

    fn config(on: &str, steps: &str) -> Config {
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{steps}\nworkflows:\n  w:\n    on: {on}\n    jobs: [t]\n"
        );
        parse(&yaml).unwrap()
    }

    fn found(steps: &str) -> Vec<LintKind> {
        found_in("", steps)
    }

    /// The findings of a job with `job` (more keys of the job) and `steps`.
    fn found_in(job: &str, steps: &str) -> Vec<LintKind> {
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n{job}    steps:\n{steps}\n\
             workflows:\n  w:\n    on: {{ pull_request_target: {{}} }}\n    jobs: [t]\n"
        );
        let config = parse(&yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        plan_lints(&plan).iter().map(|l| l.kind).collect()
    }

    /// Whether `block` would refuse the job.
    fn blocked(job: &str, steps: &str) -> bool {
        found_in(job, steps).iter().any(|k| k.is_head_checkout())
    }

    #[test]
    fn the_head_passed_through_env_to_a_fetch_is_blocked() {
        let head = "${{ github.event.pull_request.head.sha }}";
        // A step's env.
        let steps = format!(
            "      - run: git fetch origin $PR_SHA && git checkout $PR_SHA\n        env:\n          PR_SHA: {head}"
        );
        assert!(blocked("", &steps));
        // The job's env.
        let job = format!("    env:\n      PR_SHA: {head}\n");
        assert!(blocked(&job, "      - run: git fetch origin $PR_SHA"));
        // A checkout that has nothing to do with the head in its inputs,
        // in a job that reads the head elsewhere.
        assert!(blocked(
            &job,
            "      - uses: actions/checkout@v4\n        with:\n          ref: main"
        ));
        assert!(blocked(&job, "      - checkout"));
        // Read by a `with:` of another action, then a fetch in the job.
        let steps = format!(
            "      - uses: some/action@v1\n        with:\n          sha: {head}\n      - run: gh repo clone o/r"
        );
        assert!(blocked("", &steps));
    }

    #[test]
    fn a_guard_or_a_label_does_not_read_the_head() {
        // `if:`, `concurrency:` and `name:` compare or label.
        let steps = "      - run: git clone https://example.com/x\n        if: github.event.pull_request.head.repo.fork == false\n        name: ${{ github.event.pull_request.head.ref }}";
        assert!(!blocked("", steps));
        let job = "    if: github.event.pull_request.head.repo.full_name == github.repository\n    concurrency:\n      group: ci-${{ github.head_ref }}\n";
        assert!(!blocked(job, "      - checkout"));
        assert!(!blocked(job, "      - run: git fetch origin main"));
    }

    #[test]
    fn a_label_key_nested_in_env_with_or_matrix_still_reads_the_head() {
        let head = "${{ github.event.pull_request.head.sha }}";
        // An env variable named like a label key.
        for key in ["name", "if", "id", "concurrency"] {
            let steps = format!(
                "      - run: git fetch origin \"$V\"\n        env:\n          {key}: {head}\n          V: x"
            );
            assert!(blocked("", &steps), "env {key}");
        }
        // A `with:` input named like a label key.
        for key in ["name", "if", "id"] {
            let steps = format!(
                "      - uses: some/action@v1\n        with:\n          {key}: {head}\n      - run: git fetch origin main"
            );
            assert!(blocked("", &steps), "with {key}");
        }
        // The job's env under a key that is not the job's own label.
        let job = format!("    env:\n      name: {head}\n");
        assert!(blocked(
            &job,
            "      - run: git clone https://example.com/x"
        ));
        // A matrix value.
        let yaml = format!(
            "
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - run: git fetch origin ${{{{ matrix.ref }}}}
workflows:
  w:
    on: {{ pull_request_target: {{}} }}
    jobs:
      - t: {{ matrix: {{ ref: ['{head}'] }} }}
"
        );
        let config = parse(&yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        assert_eq!(plan_lints(&plan).len(), 1, "matrix");
        // The labels at their own level still are not reads.
        let steps = format!(
            "      - run: git fetch origin main\n        name: {head}\n        if: {head} != ''"
        );
        assert!(!blocked("", &steps));
    }

    #[test]
    fn a_run_time_lookup_of_the_pull_request_in_a_fetch_is_found() {
        for script in [
            "git fetch origin pull/${{ github.event.number }}/x",
            "git fetch origin \"$(jq -r .pull_request.head.sha \"$GITHUB_EVENT_PATH\")\"",
            "sha=$(gh api repos/o/r/pulls/12 --jq .head.sha) && git fetch origin $sha",
            "git fetch origin ${{ github.event.pull_request.number }}",
            "git fetch origin refs/pull/*/head:refs/remotes/pr/*",
            "git fetch origin \"refs/pull/${N}\" && git checkout FETCH_HEAD",
            "curl -L https://api.github.com/repos/o/r/tarball/$SHA | tar xz",
            "wget https://github.com/o/r/archive/$SHA.zip",
            "curl -L https://api.github.com/repos/o/r/zipball/$SHA -o x.zip",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(blocked("", &steps), "{script}");
        }
        // The same lookup without a fetch fetches nothing.
        for script in [
            "echo ${{ github.event.number }}",
            "jq .number \"$GITHUB_EVENT_PATH\"",
            "gh api repos/o/r/pulls/12 --jq .title",
            "curl https://example.com/health",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(!blocked("", &steps), "{script}");
        }
    }

    #[test]
    fn the_head_passed_to_a_later_job_through_needs_is_found() {
        let yaml = "
jobs:
  find:
    runs-on: ubuntu-latest
    outputs: { sha: '${{ steps.s.outputs.sha }}' }
    steps:
      - id: s
        run: echo \"sha=${{ github.event.pull_request.head.sha }}\" >> \"$GITHUB_OUTPUT\"
  build:
    runs-on: ubuntu-latest
    steps:
      - run: git fetch origin ${{ needs.find.outputs.sha }}
  unrelated:
    runs-on: ubuntu-latest
    steps:
      - run: git fetch origin main
workflows:
  w:
    on: { pull_request_target: {} }
    jobs:
      - find
      - build: { requires: [find] }
      - unrelated: { requires: [find] }
";
        let config = parse(yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        let found = plan_lints(&plan);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].job, "build");
        assert!(found[0].message.contains("an output of the job `find`"));
    }

    #[test]
    fn computed_indexes_and_parents_of_the_head_are_blocked() {
        for expr in [
            "github.event.pull_request[format('h{0}', 'ead')].sha",
            "github.event.pull_request[inputs.field].sha",
            "github.event[matrix.k].head.sha",
            "toJSON(github.event.pull_request)",
            "toJSON(github.event)",
            "toJSON(github)",
            "github.event.pull_request",
            "format('{0}', github.event.pull_request)",
            "fromJSON(toJSON(github.event.pull_request)).head.sha",
        ] {
            let steps = format!("      - run: git fetch origin \"${{{{ {expr} }}}}\"");
            assert!(blocked("", &steps), "{expr}");
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert!(blocked("", &steps), "{expr}");
        }
    }

    #[test]
    fn a_job_that_never_reads_the_head_is_not_blocked_whatever_it_fetches() {
        for steps in [
            "      - run: git fetch origin main && git checkout release",
            "      - run: git clone https://example.com/x && make",
            "      - run: git fetch origin release/pull/2/head-fix",
            "      - run: git fetch origin refs/heads/pull/1/head",
            "      - run: git fetch origin pull/2/header",
            "      - run: git fetch origin pull/head",
            "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}",
            "      - run: git fetch origin ${{ github.event.pull_request.base.sha }}",
            "      - run: echo ${{ github.event.pull_request.number }} ${{ github.event.pull_request.labels.*.name }}",
        ] {
            assert!(!blocked("", steps), "{steps}");
        }
        // The head read in a job that fetches nothing is only a warning
        // for the job that checks out.
        assert!(!blocked(
            "",
            "      - run: echo ${{ github.event.pull_request.head.sha }}"
        ));
    }

    #[test]
    fn a_pull_ref_is_a_whole_ref_and_nothing_longer() {
        for yes in [
            "pull/12/head",
            "refs/pull/12/merge",
            "+refs/pull/5/head:refs/remotes/pr/5",
            "git fetch origin pull/7/merge",
            "refs/pull/${{ github.event.number }}/head",
            "refs/pull/$PR/head",
            "refs/pull/${pr}/merge",
            "refs/pull/{0}/head",
            "\"refs/pull/3/head\"",
        ] {
            assert!(pull_ref(&yes.to_ascii_lowercase()), "{yes}");
        }
        for no in [
            "release/pull/2/head-fix",
            "refs/heads/pull/1/head",
            "origin/pull/2/head",
            "pull/2/header",
            "pull/2/head_old",
            "pull/head",
            "refs/pull/x/head",
            "pull/2/heads",
            "pull/2/merged",
        ] {
            assert!(!pull_ref(no), "{no}");
        }
    }

    #[test]
    fn a_checkout_of_the_head_is_found() {
        for with in [
            "ref: ${{ github.event.pull_request.head.sha }}",
            "ref: ${{ github.event.pull_request.head.ref }}",
            "ref: ${{ github.head_ref }}",
            "ref: refs/pull/${{ github.event.number }}/merge",
            "ref: ${{ github.event.pull_request.merge_commit_sha }}",
            "repository: ${{ github.event.pull_request.head.repo.full_name }}",
        ] {
            let steps =
                format!("      - uses: actions/checkout@v4\n        with:\n          {with}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{with}");
        }
    }

    #[test]
    fn every_spelling_of_the_head_is_the_head() {
        for expr in [
            "github.event.pull_request.head.sha",
            "github['event']['pull_request']['head']['sha']",
            "github.event.pull_request['head'].sha",
            "GitHub.Event.Pull_Request.Head.SHA",
            "github[\"event\"].pull_request.head.ref",
            "github.event.pull_request.MERGE_COMMIT_SHA",
            "github.HEAD_REF",
            "github['head_ref']",
            "format('{0}', github.event.pull_request.head.sha)",
            "github.head_ref || 'main'",
            "format('refs/pull/{0}/head', github.event.number)",
            "toJSON(github.event.pull_request.head)",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert_eq!(found(&steps), [LintKind::HeadCheckout], "{expr}");
            let steps = format!("      - run: git checkout ${{{{ {expr} }}}}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{expr}");
        }
        for text in ["$GITHUB_HEAD_REF", "$github_head_ref", "${GITHUB_HEAD_REF}"] {
            let steps = format!("      - run: git fetch origin {text}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{text}");
        }
    }

    #[test]
    fn a_ref_that_is_not_provably_the_base_is_unverified() {
        for expr in [
            "matrix.ref",
            "inputs.ref",
            "env.REF",
            "steps.x.outputs.ref",
            "fromJSON('{\"a\":1}').a",
            "github.event.inputs.ref",
            "github.event.pull_request.base.ref == 'x' && matrix.r",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert_eq!(found(&steps), [LintKind::UnverifiedCheckout], "{expr}");
        }
        let steps = "      - uses: actions/checkout@v4\n        with:\n          repository: ${{ matrix.repo }}";
        assert_eq!(found(steps), [LintKind::UnverifiedCheckout]);
        let steps = "      - checkout:\n          ref: ${{ matrix.ref }}";
        assert_eq!(found(steps), [LintKind::UnverifiedCheckout]);
        assert!(LintKind::UnverifiedCheckout.is_head_checkout());
        assert!(!LintKind::ScriptInjection.is_head_checkout());
    }

    #[test]
    fn provably_base_refs_are_clean() {
        for ref_ in [
            "main",
            "refs/heads/release",
            "${{ github.base_ref }}",
            "${{ github.sha }}",
            "${{ github.ref }}",
            "${{ GitHub['Base_Ref'] }}",
            "${{ github.event.pull_request.base.sha }}",
            "${{ github.event.pull_request['base']['ref'] }}",
            "${{ 'main' }}",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: \"{ref_}\""
            );
            assert_eq!(found(&steps), Vec::<LintKind>::new(), "{ref_}");
        }
        let steps = "      - uses: actions/checkout@v4\n        with:\n          repository: ${{ github.repository }}";
        assert_eq!(found(steps), Vec::<LintKind>::new());
    }

    #[test]
    fn injection_is_found_in_every_spelling() {
        for expr in [
            "GitHub.Event.Pull_Request.Title",
            "github['event']['pull_request']['title']",
            "github.event.pull_request['body']",
            "github['head_ref']",
            "github.event.commits[0].message",
            "github.event.commits.*.message",
        ] {
            let steps = format!("      - run: echo \"${{{{ {expr} }}}}\"");
            assert_eq!(found(&steps), [LintKind::ScriptInjection], "{expr}");
        }
    }

    #[test]
    fn a_checkout_of_the_base_is_not_found() {
        assert_eq!(
            found("      - uses: actions/checkout@v4"),
            Vec::<LintKind>::new()
        );
        let steps = "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.event.pull_request.base.sha }}";
        assert_eq!(found(steps), Vec::<LintKind>::new());
        assert_eq!(found("      - checkout"), Vec::<LintKind>::new());
    }

    #[test]
    fn the_native_checkout_step_is_read_too() {
        let steps = "      - checkout:\n          ref: ${{ github.event.pull_request.head.sha }}";
        assert_eq!(found(steps), [LintKind::HeadCheckout]);
    }

    #[test]
    fn scripts_that_fetch_or_check_out_the_head_are_found() {
        for script in [
            "gh pr checkout ${{ github.event.number }}",
            "git fetch origin refs/pull/${{ github.event.number }}/head",
            "git fetch origin pull/12/merge",
            "git checkout $GITHUB_HEAD_REF && git fetch origin ${{ github.head_ref }}",
            "git checkout ${{ github.event.pull_request.head.sha }}",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{script}");
        }
        for script in [
            "git fetch origin main",
            "git log --oneline",
            "echo refs/pull/1/head",
            "git fetch origin pull/12/header",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(!found(&steps).contains(&LintKind::HeadCheckout), "{script}");
        }
    }

    #[test]
    fn untrusted_fields_in_a_script_are_found() {
        for field in [
            "github.event.pull_request.title",
            "github.event.pull_request.body",
            "github.event.pull_request.head.ref",
            "github.event.issue.title",
            "github.event.comment.body",
            "github.head_ref",
            "github.event.commits[0].message",
            "  github.event.pull_request.title  ",
            "contains(github.event.pull_request.title, 'x')",
            "github.event['pull_request']['title']",
        ] {
            let steps = format!("      - run: echo \"${{{{ {field} }}}}\"");
            assert_eq!(found(&steps), [LintKind::ScriptInjection], "{field}");
        }
    }

    #[test]
    fn safe_interpolations_and_env_passing_are_not_found() {
        for steps in [
            "      - run: echo ${{ github.event.pull_request.number }}",
            "      - run: echo ${{ github.sha }} ${{ github.event.pull_request.base.ref }}",
            "      - run: echo \"$TITLE\"\n        env:\n          TITLE: ${{ github.event.pull_request.title }}",
        ] {
            assert!(found(steps).is_empty(), "{steps}");
        }
    }

    #[test]
    fn a_github_script_with_a_title_in_its_script_is_found() {
        let steps = "      - uses: actions/github-script@v7\n        with:\n          script: |\n            core.info('${{ github.event.pull_request.title }}')";
        assert_eq!(found(steps), [LintKind::ScriptInjection]);
    }

    #[test]
    fn a_finding_names_the_workflow_job_and_step() {
        let config = config(
            "{ pull_request_target: {} }",
            "      - run: echo hi\n      - run: echo ${{ github.event.issue.title }}",
        );
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        let lints = plan_lints(&plan);
        assert_eq!(lints.len(), 1);
        assert!(
            lints[0]
                .text()
                .starts_with("workflow `w`, job `t`, step 2: ")
        );
    }

    #[test]
    fn only_a_pull_request_target_workflow_is_linted_in_a_config() {
        let step = "      - run: echo ${{ github.event.pull_request.title }}";
        assert_eq!(
            validate_with_warnings(&config("{ pull_request_target: {} }", step))
                .1
                .len(),
            1
        );
        assert_eq!(
            validate_with_warnings(&config("{ pull_request: {} }", step))
                .1
                .len(),
            0
        );
        assert_eq!(
            validate_with_warnings(&config("{ push: {} }", step))
                .1
                .len(),
            0
        );
    }

    #[test]
    fn a_command_is_expanded_before_it_is_read() {
        let yaml = "
commands:
  grab:
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - grab
workflows:
  w:
    on: { pull_request_target: {} }
    jobs: [t]
";
        let config = parse(yaml).unwrap();
        assert_eq!(validate_with_warnings(&config).1.len(), 1);
    }

    /// The findings of a workflow with triggers `on` (YAML) and a job
    /// with `job` (more keys of the job) and `steps`, as `validate` reads
    /// them.
    fn warned(on: &str, job: &str, steps: &str) -> Vec<Lint> {
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n{job}    steps:\n{steps}\n\
             workflows:\n  w:\n    on: {on}\n    jobs: [t]\n"
        );
        let config = parse(&yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        workflow_lints(&plan, &config.workflows["w"].on)
    }

    fn kinds(found: &[Lint]) -> Vec<LintKind> {
        found.iter().map(|l| l.kind).collect()
    }

    const GUARD: &str =
        "    if: contains(fromJSON('[\"OWNER\"]'), github.event.comment.author_association)\n";

    #[test]
    fn issues_issue_comment_and_command_workflows_are_guarded() {
        let on = |yaml: &str| -> crate::Triggers { serde_saphyr::from_str(yaml).unwrap() };
        assert!(guarded(&on("{ issues: {} }")));
        assert!(guarded(&on("{ issue_comment: {} }")));
        assert!(guarded(&on("{ command: retest }")));
        assert!(guarded(&on("{ pull_request_target: {} }")));
        assert!(!guarded(&on("{ pull_request: {}, push: {} }")));
    }

    #[test]
    fn the_head_is_a_checkout_in_issue_comment_and_command_workflows() {
        let steps = "      - uses: actions/checkout@v4\n        with:\n          ref: refs/pull/${{ github.event.issue.number }}/head";
        for on in [
            "{ issue_comment: {} }",
            "{ issues: {} }",
            "{ command: retest }",
        ] {
            let job = if on.contains("comment") { GUARD } else { "" };
            assert!(
                kinds(&warned(on, job, steps)).contains(&LintKind::HeadCheckout),
                "{on}"
            );
        }
        // A script that finds the pull request from the issue.
        let steps = "      - run: git fetch origin pull/${{ github.event.issue.number }}/head";
        assert!(kinds(&warned("{ issues: {} }", "", steps)).contains(&LintKind::HeadCheckout));
        let steps = "      - run: gh pr checkout ${{ github.event.issue.number }}";
        assert!(kinds(&warned("{ command: retest }", "", steps)).contains(&LintKind::HeadCheckout));
    }

    const PINNED: &str = "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.event.command.pull_request.head_sha }}";

    #[test]
    fn a_command_may_check_out_the_sha_the_commenter_named() {
        for on in [
            "{ command: retest }",
            "{ command: { deploy: { permission: maintain } } }",
            "{ command: { a: { permission: write }, b: { permission: admin } } }",
        ] {
            assert_eq!(warned(on, "", PINNED), [], "{on}");
        }
        let native =
            "      - checkout:\n          ref: ${{ github.event.command.pull_request.head_sha }}";
        assert_eq!(warned("{ command: retest }", "", native), []);
        // In a job that fetches too.
        let steps = format!(
            "{PINNED}\n      - run: git fetch origin ${{{{ github.event.command.pull_request.head_sha }}}}"
        );
        assert_eq!(warned("{ command: retest }", "", &steps), []);
    }

    #[test]
    fn a_command_with_an_optional_sha_warns_on_the_sha() {
        // The pin can be left out, so the checkout is the head as it is.
        for on in [
            "{ command: { deploy: { permission: write, sha: optional } } }",
            "{ command: { a: { permission: admin, sha: required }, b: { permission: write, sha: optional } } }",
        ] {
            assert_eq!(
                kinds(&warned(on, "", PINNED)),
                [LintKind::HeadCheckout],
                "{on}"
            );
        }
        for on in [
            "{ command: { deploy: { permission: write, sha: required } } }",
            "{ command: { deploy: { permission: write, sha: auto } } }",
        ] {
            assert_eq!(warned(on, "", PINNED), [], "{on}");
        }
    }

    #[test]
    fn a_command_below_write_or_mixed_with_one_warns_on_the_sha() {
        for on in [
            "{ command: { look: { permission: triage } } }",
            "{ command: { look: { permission: read } } }",
            "{ command: { a: { permission: triage }, b: { permission: admin } } }",
        ] {
            assert_eq!(
                kinds(&warned(on, "", PINNED)),
                [LintKind::HeadCheckout],
                "{on}"
            );
        }
        // Without the workflow's triggers the pin counts as a head.
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{PINNED}\nworkflows:\n  w:\n    on: {{ command: retest }}\n    jobs: [t]\n"
        );
        let config = parse(&yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        assert_eq!(plan_lints(&plan).len(), 1);
    }

    #[test]
    fn a_command_still_warns_on_the_other_head_forms() {
        for with in [
            "ref: ${{ github.head_ref }}",
            "ref: refs/pull/${{ github.event.command.pull_request.number }}/head",
            "ref: refs/pull/12/merge",
            "repository: ${{ github.event.command.pull_request.head_repo }}",
            "ref: ${{ github.event.pull_request.head.sha }}",
            "ref: ${{ github.event.command.args[0] }}",
            "ref: ${{ github.event.command.pull_request.base_ref }}${{ github.event.command.line }}",
        ] {
            let steps =
                format!("      - uses: actions/checkout@v4\n        with:\n          {with}");
            let kinds = kinds(&warned("{ command: retest }", "", &steps));
            assert!(
                kinds.iter().any(|k| k.is_head_checkout()),
                "{with}: {kinds:?}"
            );
        }
        // Another ref of the command is a base-like value.
        let steps =
            "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}";
        assert_eq!(warned("{ command: retest }", "", steps), []);
    }

    #[test]
    fn script_injection_covers_issue_comment_and_command_text() {
        for field in [
            "github.event.issue.title",
            "github.event.issue.body",
            "github.event.comment.body",
            "github.event.command.line",
            "github.event.command.args",
            "github.event.command.args[0]",
            "github.event.command.named.environment",
            "toJSON(github.event.command)",
        ] {
            let steps = format!("      - run: echo \"${{{{ {field} }}}}\"");
            let on = "{ issues: {}, command: retest }";
            assert!(
                kinds(&warned(on, "", &steps)).contains(&LintKind::ScriptInjection),
                "{field}"
            );
            let script = format!(
                "      - uses: actions/github-script@v7\n        with:\n          script: console.log('${{{{ {field} }}}}')"
            );
            assert!(
                kinds(&warned(on, "", &script)).contains(&LintKind::ScriptInjection),
                "script {field}"
            );
        }
        // Safe fields, and the value passed through env.
        for safe in [
            "github.event.command.name",
            "github.event.command.actor",
            "github.event.command.permission",
            "github.event.command.pull_request.number",
            "github.event.issue.number",
        ] {
            let steps = format!("      - run: echo \"${{{{ {safe} }}}}\"");
            assert!(
                !kinds(&warned("{ command: retest }", "", &steps))
                    .contains(&LintKind::ScriptInjection),
                "{safe}"
            );
        }
        let steps = "      - run: echo \"$BODY\"\n        env:\n          BODY: ${{ github.event.comment.body }}";
        assert!(
            !kinds(&warned("{ issue_comment: {} }", GUARD, steps))
                .contains(&LintKind::ScriptInjection)
        );
    }

    #[test]
    fn an_issue_comment_job_with_no_check_on_the_commenter_warns() {
        let steps = "      - run: ./retest.sh";
        let found = warned("{ issue_comment: {} }", "", steps);
        assert_eq!(kinds(&found), [LintKind::UnguardedComment]);
        assert_eq!(found[0].step, 0);
        assert!(
            found[0].text().starts_with("workflow `w`, job `t`: "),
            "{}",
            found[0].text()
        );
        // An if that has nothing to do with the commenter.
        let job = "    if: github.event.issue.pull_request\n";
        assert_eq!(
            kinds(&warned("{ issue_comment: {} }", job, steps)),
            [LintKind::UnguardedComment]
        );
        // Each of the three checks.
        for cond in [
            "github.event.comment.author_association == 'OWNER'",
            "github.actor == 'alice'",
            "github.triggering_actor == 'alice'",
            "contains(fromJSON(vars.MAINTAINERS), github.actor)",
            "needs_permission(github.actor)",
        ] {
            let job = format!("    if: {cond}\n");
            assert_eq!(warned("{ issue_comment: {} }", &job, steps), [], "{cond}");
        }
        let job = "    if: ${{ github.event.comment.author_association == 'OWNER' }}\n";
        assert_eq!(warned("{ issue_comment: {} }", job, steps), []);
    }

    #[test]
    fn only_issue_comment_workflows_are_asked_who_commented() {
        let steps = "      - run: ./triage.sh";
        for on in [
            "{ issues: {} }",
            "{ command: retest }",
            "{ pull_request_target: {} }",
        ] {
            assert!(
                !kinds(&warned(on, "", steps)).contains(&LintKind::UnguardedComment),
                "{on}"
            );
        }
    }

    #[test]
    fn a_job_that_needs_a_checked_job_is_covered_by_it() {
        let yaml = "jobs:
  gate:
    runs-on: ubuntu-latest
    if: github.actor == 'alice'
    steps:
      - run: ./gate.sh
  work:
    runs-on: ubuntu-latest
    steps:
      - run: ./work.sh
  other:
    runs-on: ubuntu-latest
    steps:
      - run: ./other.sh
workflows:
  w:
    on: { issue_comment: {} }
    jobs:
      - gate
      - work: { requires: [gate] }
      - other
";
        let config = parse(yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        let found = workflow_lints(&plan, &config.workflows["w"].on);
        let unguarded: Vec<&str> = found.iter().map(|l| l.job.as_str()).collect();
        assert_eq!(unguarded, ["other"], "{found:?}");
    }

    #[test]
    fn contains_on_the_comment_text_warns() {
        let steps = "      - run: ./retest.sh";
        for cond in [
            "contains(github.event.comment.body, '/retest')",
            "contains(github.event.comment.body, '/retest') && github.actor == 'alice'",
            "github.actor == 'alice' && contains(github.event.comment.body, '/retest')",
            "contains(github['event']['comment']['body'], '/retest')",
            "contains(toLower(github.event.comment.body), '/retest')",
            "!contains(github.event.comment.body, 'wip')",
            "${{ contains(github.event.comment.body, '/retest') }}",
        ] {
            let job = format!("    if: \"{cond}\"\n");
            let found = warned("{ issue_comment: {} }", &job, steps);
            assert!(
                kinds(&found).contains(&LintKind::BodyContains),
                "{cond}: {found:?}"
            );
        }
        // A step's if counts, and says its step.
        let steps =
            "      - run: ./retest.sh\n        if: contains(github.event.comment.body, '/retest')";
        let found = warned("{ issue_comment: {} }", GUARD, steps);
        assert_eq!(kinds(&found), [LintKind::BodyContains]);
        assert_eq!(found[0].step, 1);
    }

    #[test]
    fn startswith_and_other_contains_do_not_warn() {
        let steps = "      - run: ./retest.sh";
        for cond in [
            "startsWith(github.event.comment.body, '/retest') && github.actor == 'alice'",
            "contains(github.event.issue.labels.*.name, 'bug') && github.actor == 'alice'",
            "contains('/retest', github.event.comment.body) && github.actor == 'alice'",
            "contains(fromJSON('[\"OWNER\"]'), github.event.comment.author_association)",
        ] {
            let job = format!("    if: {cond}\n");
            assert_eq!(warned("{ issue_comment: {} }", &job, steps), [], "{cond}");
        }
    }

    #[test]
    fn validate_reports_the_comment_findings_and_still_passes() {
        let yaml = "
jobs:
  t:
    runs-on: ubuntu-latest
    if: contains(github.event.comment.body, '/retest')
    steps:
      - run: ./retest.sh
workflows:
  w:
    on: { issue_comment: { types: [created] } }
    jobs: [t]
";
        let (errors, warnings) = validate_with_warnings(&parse(yaml).unwrap());
        assert_eq!(errors, Vec::new());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
