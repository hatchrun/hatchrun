// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starts the runs of an event. The webhook, the schedules and manual
//! dispatch all go through [`create`], so the tenant check holds for
//! each of them.

use std::fmt::Write as _;

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::{Diagnostic, Event, JobKind, Plan, PlannedJob, is_expression, run_title};
use serde_json::Value;
use worker::{Env, Method, RequestInit, Result};

use crate::env::{Settings, run_stub};
use crate::github::{self, CONFIG_PATH, CheckRun, GhAnnotation, Output};
use crate::intake::{PullRequest, Repo};
use crate::planning::{self, Failed, Selected};
use crate::run::{RunInfo, Start, github_context};
use crate::store::{self, CiBilling, GateStatus, JobRow, NewJob, NewRun, RunRow};
use crate::trust::Trust;

/// The name of the check run that reports an unusable config.
pub const CONFIG_CHECK: &str = "hatchrun / config";

/// The name of the check run that asks a lapsed tenant for a card.
pub const CARD_CHECK: &str = "hatchrun / billing";

/// GitHub takes at most this many annotations in one request.
const MAX_ANNOTATIONS: usize = 50;

/// Diagnostics beyond this many are left out of the summary.
const MAX_LISTED: usize = 100;

/// Whose code a request runs and who wrote it: what the runs record as
/// their trust, and the approval a fork pull request's runs wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin<'a> {
    /// Whether the code is the repository's own or a fork's.
    pub trust: Trust,
    /// The head repository of a fork pull request, by id.
    pub head_repo_id: Option<u64>,
    /// The head repository of a fork pull request, `owner/name`.
    pub head_repo_name: Option<&'a str>,
    /// The login of the pull request's author; the actor otherwise.
    pub author: &'a str,
    /// GitHub's `author_association` of the author; empty otherwise.
    pub association: &'a str,
    /// Whether the runs wait for an approval.
    pub gate: GateStatus,
}

impl<'a> Origin<'a> {
    /// The origin of a run of the repository's own code started by
    /// `actor`: trusted, and it waits for nobody.
    #[must_use]
    pub const fn own(actor: &'a str) -> Self {
        Self {
            trust: Trust::Trusted,
            head_repo_id: None,
            head_repo_name: None,
            author: actor,
            association: "",
            gate: GateStatus::NotGated,
        }
    }
}

/// What starts runs.
pub struct Request<'a> {
    /// The tenant that owns the repository.
    pub tenant_id: &'a str,
    /// The App installation that reaches the repository.
    pub installation_id: u64,
    /// The repository.
    pub repo: &'a Repo,
    /// The delivery id, or a `schedule:` or `dispatch:` key. A run of
    /// one delivery and workflow is made once.
    pub delivery: &'a str,
    /// `push`, `pull_request`, `schedule` or `workflow_dispatch`.
    pub event_name: &'a str,
    /// The webhook payload.
    pub event: &'a Value,
    /// What workflows match their triggers against.
    pub trigger: &'a Event,
    /// The ref the config is compiled against.
    pub compile_ref: &'a str,
    /// The ref the run shows.
    pub git_ref: &'a str,
    /// The commit the config is read at and the checks are on.
    pub sha: &'a str,
    /// The login that caused the event.
    pub actor: &'a str,
    /// The pull request's head branch, empty otherwise.
    pub head_ref: &'a str,
    /// The pull request's base branch, empty otherwise.
    pub base_ref: &'a str,
    /// Start only this workflow's plan. A schedule names its workflow;
    /// the other workflows that share its cron have their own entries.
    pub workflow: Option<&'a str>,
    /// Whose code the runs execute.
    pub origin: Origin<'a>,
}

fn fail(message: impl Into<String>) -> worker::Error {
    worker::Error::RustError(message.into())
}

/// The ref a pull request's workflows compile against: its base branch.
#[must_use]
pub fn compile_ref_for(pr: &PullRequest) -> String {
    format!("refs/heads/{}", pr.base_ref)
}

/// The config at a commit.
pub enum Config {
    /// The repository has no config file there.
    Missing,
    /// The file, or the reason it cannot be used.
    Text(std::result::Result<String, Diagnostic>),
}

/// Reads the config of `repo` at `sha`.
///
/// # Errors
///
/// No token, GitHub unreachable, or GitHub did not answer with the file
/// or a 404.
pub async fn read_config(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    installation_id: u64,
    repo: &str,
    sha: &str,
) -> Result<Config> {
    let response = tokens
        .send(installation_id, |token| {
            github::config_file(&settings.github_api, repo, sha, token)
        })
        .await
        .map_err(|err| fail(err.to_string()))?;
    match github::config_text(&response) {
        Ok(None) => Ok(Config::Missing),
        Ok(Some(text)) => Ok(Config::Text(Ok(text))),
        // A file GitHub gave us that we cannot use is the author's to fix.
        Err(message) if response.ok() => Ok(Config::Text(Err(Diagnostic::new(message)))),
        Err(message) => Err(fail(message)),
    }
}

/// The commit the full ref `reference` (`refs/heads/main`,
/// `refs/tags/v1`) names, `None` when the repository has no such ref:
/// GitHub answers 404 for a ref that is gone, 409 for an empty
/// repository and 422 for a ref it cannot resolve. None of them gets
/// better by asking again. The commits endpoint does not take
/// `tags/v1` (it answers 422), so the ref is always given in full.
///
/// # Errors
///
/// No token, GitHub unreachable, or an answer that is neither a commit
/// nor one of those.
pub async fn commit(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    installation_id: u64,
    repo: &str,
    reference: &str,
) -> Result<Option<String>> {
    let response = tokens
        .send(installation_id, |token| {
            github::commit_sha(&settings.github_api, repo, reference, token)
        })
        .await
        .map_err(|err| fail(err.to_string()))?;
    commit_of(&response).map_err(fail)
}

/// The sha in GitHub's answer to a commit request, `None` for an answer
/// that says the ref is not there (see [`commit`]).
///
/// # Errors
///
/// GitHub's answer when it is not a commit.
pub fn commit_of(
    response: &hatchrun_cloud::http::Response,
) -> std::result::Result<Option<String>, String> {
    if response.status == 422 && is_throttled(response) {
        // A secondary rate limit answers 422 too, and it passes.
        return Err(format!(
            "commit: GitHub throttled the request: {}",
            response.text()
        ));
    }
    if matches!(response.status, 404 | 409 | 422) {
        return Ok(None);
    }
    github::sha_of(response).map(Some)
}

/// Whether an answer is a rate or abuse limit, which GitHub marks with
/// a `Retry-After` header or in its message.
fn is_throttled(response: &hatchrun_cloud::http::Response) -> bool {
    if response.header("retry-after").is_some() {
        return true;
    }
    let text = response.text().to_ascii_lowercase();
    // A missing ref is the answer whatever its name says: a tag called
    // `secondary-v1` is not a rate limit.
    if text.contains("no commit found") {
        return false;
    }
    [
        "rate limit",
        "abuse detection",
        "secondary rate",
        "exceeded a secondary",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// The head of a repository's default branch and its config there.
/// `None` when the repository has no commit on that branch.
///
/// # Errors
///
/// See [`commit`] and [`read_config`].
pub async fn default_head(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    installation_id: u64,
    repo: &Repo,
) -> Result<Option<(String, Config)>> {
    let reference = format!("refs/heads/{}", repo.default_branch);
    let Some(sha) = commit(
        tokens,
        settings,
        installation_id,
        &repo.full_name,
        &reference,
    )
    .await?
    else {
        return Ok(None);
    };
    let config = read_config(tokens, settings, installation_id, &repo.full_name, &sha).await?;
    Ok(Some((sha, config)))
}

/// The completed check run that reports a config that does not parse or
/// compile: the diagnostics are its summary and annotations on the
/// config file.
#[must_use]
pub fn invalid_config_check_run(diagnostics: &[Diagnostic], sha: &str) -> CheckRun {
    failed_check_run(
        CONFIG_CHECK,
        "Invalid hatchrun config",
        "so no workflow started",
        diagnostics,
        sha,
    )
}

/// The failed check run of a workflow that did not compile, named
/// `<workflow> / config`. The other workflows of the event still run.
#[must_use]
pub fn invalid_workflow_check_run(
    workflow: &str,
    diagnostics: &[Diagnostic],
    sha: &str,
) -> CheckRun {
    failed_check_run(
        &format!("{workflow} / config"),
        "Invalid workflow",
        &format!("so workflow `{workflow}` did not start"),
        diagnostics,
        sha,
    )
}

/// A completed, failed check run `name` that lists `diagnostics` in its
/// summary, `consequence` ending the first line, and annotates the
/// config file with them.
fn failed_check_run(
    name: &str,
    title: &str,
    consequence: &str,
    diagnostics: &[Diagnostic],
    sha: &str,
) -> CheckRun {
    let line = |d: &Diagnostic| match (d.line, d.column) {
        (Some(line), Some(column)) => format!("- line {line}, column {column}: {}", d.message),
        (Some(line), None) => format!("- line {line}: {}", d.message),
        _ => format!("- {}", d.message),
    };
    let mut summary = format!(
        "`{CONFIG_PATH}` has {} problem{}, {consequence}:\n\n",
        diagnostics.len(),
        if diagnostics.len() == 1 { "" } else { "s" }
    );
    for d in diagnostics.iter().take(MAX_LISTED) {
        summary.push_str(&line(d));
        summary.push('\n');
    }
    if diagnostics.len() > MAX_LISTED {
        let _ = writeln!(summary, "- and {} more", diagnostics.len() - MAX_LISTED);
    }
    let annotations = diagnostics
        .iter()
        .take(MAX_ANNOTATIONS)
        .map(|d| {
            let start = u32::try_from(d.line.unwrap_or(1))
                .unwrap_or(u32::MAX)
                .max(1);
            let column = d
                .line
                .and(d.column)
                .map(|c| u32::try_from(c).unwrap_or(u32::MAX).max(1));
            GhAnnotation {
                path: CONFIG_PATH.to_owned(),
                start_line: start,
                end_line: start,
                start_column: column,
                end_column: column,
                annotation_level: "failure".to_owned(),
                message: d.message.clone(),
                title: None,
            }
        })
        .collect();
    CheckRun {
        name: name.to_owned(),
        head_sha: sha.to_owned(),
        external_id: String::new(),
        details_url: String::new(),
        status: "completed".to_owned(),
        started_at: None,
        conclusion: Some("failure".to_owned()),
        completed_at: None,
        output: Some(Output {
            title: title.to_owned(),
            summary,
            text: None,
            annotations,
        }),
    }
}

/// The completed check run that asks `org` for a card: its workflows
/// would have run, and a tenant without a card runs none.
#[must_use]
pub fn no_card_check_run(sha: &str, org: &str, app_url: &str) -> CheckRun {
    let page = format!("{app_url}/o/{org}/ci");
    CheckRun {
        name: CARD_CHECK.to_owned(),
        head_sha: sha.to_owned(),
        external_id: String::new(),
        details_url: page.clone(),
        status: "completed".to_owned(),
        started_at: None,
        conclusion: Some("action_required".to_owned()),
        completed_at: None,
        output: Some(Output {
            title: "hatchrun needs a card".to_owned(),
            summary: format!("Add a card for {org} at {page} to run this commit's workflows."),
            text: None,
            annotations: Vec::new(),
        }),
    }
}

/// What a tenant's billing state does with the workflows of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Billing {
    /// Start the runs.
    Run,
    /// Start nothing and ask for a card.
    AskForCard,
    /// Start nothing and say nothing.
    Nothing,
}

/// The tenant with a card runs its `plans`; the one without a card (never
/// subscribed, or lapsed) is asked for one when a workflow would have
/// run.
fn billing_decision(billing: CiBilling, plans: usize) -> Billing {
    match billing {
        CiBilling::Active => Billing::Run,
        CiBilling::Lapsed | CiBilling::Never if plans > 0 => Billing::AskForCard,
        CiBilling::Lapsed | CiBilling::Never => Billing::Nothing,
    }
}

/// Starts the runs `req` calls for and returns their ids. `config` is
/// the config at `req.sha`, which the caller read with [`read_config`]
/// (the webhook also needs it for schedules and path filters).
///
/// Nothing starts for a commit without a config or a missing tenant. A
/// tenant without a card (a beta tenant has one) gets one `action_required` card check run when a workflow
/// would run, and nothing otherwise. For a tenant with a card, a config
/// that does not parse or compile gets one failed check run. Each planned workflow becomes a run with
/// its jobs waiting, and its object is told to start. The same
/// delivery again finds the runs it made and starts those that did
/// not start.
///
/// # Errors
///
/// Postgres, GitHub or the run's object failed; the caller answers 500
/// and GitHub delivers again.
pub async fn create(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    req: &Request<'_>,
    config: &Config,
    now: u64,
) -> Result<Vec<u64>> {
    if matches!(config, Config::Missing) {
        return Ok(Vec::new());
    }
    let Some(tenant) = store::tenant(db, req.tenant_id).await? else {
        worker::console_log!("no tenant {} for a CI run", req.tenant_id);
        return Ok(Vec::new());
    };
    let card = tenant.ci_billing == CiBilling::Active;
    let source = match config {
        Config::Missing => return Ok(Vec::new()),
        Config::Text(Ok(source)) => source,
        Config::Text(Err(diagnostic)) => {
            if card {
                report_invalid(tokens, settings, req, std::slice::from_ref(diagnostic)).await?;
            }
            return Ok(Vec::new());
        }
    };
    let mut wanted = planning::remote_workflows(source, req.trigger);
    if let Some(only) = req.workflow {
        wanted.retain(|name| name == only);
    }
    let library = match hatchrun_workflow::parse(source) {
        Ok(parsed) if !wanted.is_empty() => {
            let github = crate::remote::Github {
                db,
                tokens,
                settings,
                installation_id: req.installation_id,
                tenant_id: req.tenant_id,
            };
            let names: Vec<&str> = wanted.iter().map(String::as_str).collect();
            crate::remote::read_library(&github, &parsed, &names)
                .await
                .map_err(fail)?
        }
        _ => hatchrun_workflow::Library::default(),
    };
    let selected = planning::select_with(source, req.trigger, req.compile_ref, &library);
    let (plans, failed) = match selected {
        Selected::Plans { plans, failed } => (
            only_workflow(plans, req.workflow),
            only_failed(failed, req.workflow),
        ),
        Selected::Invalid(diagnostics) => {
            if card {
                report_invalid(tokens, settings, req, &diagnostics).await?;
            }
            return Ok(Vec::new());
        }
    };
    match billing_decision(tenant.ci_billing, plans.len()) {
        Billing::Run => {}
        Billing::AskForCard => {
            let check = no_card_check_run(req.sha, &tenant.org_login, &settings.app_url);
            report_once(tokens, settings, req, &check).await?;
            return Ok(Vec::new());
        }
        Billing::Nothing => return Ok(Vec::new()),
    }
    let mut ids = Vec::new();
    for plan in &plans {
        ids.push(start_plan(env, db, settings, req, plan, now).await?);
    }
    // The good workflows run first: a GitHub error on a failed
    // workflow's check run must not keep them from starting.
    for workflow in &failed {
        let check = invalid_workflow_check_run(&workflow.workflow, &workflow.diagnostics, req.sha);
        if let Err(err) = report_once(tokens, settings, req, &check).await {
            worker::console_error!(
                "{}: check run for the invalid workflow {}: {err}",
                req.repo.full_name,
                workflow.workflow
            );
        }
    }
    Ok(ids)
}

/// The plans of `workflow` when one is named, all of them otherwise.
fn only_workflow(mut plans: Vec<Plan>, workflow: Option<&str>) -> Vec<Plan> {
    if let Some(workflow) = workflow {
        plans.retain(|plan| plan.workflow == workflow);
    }
    plans
}

/// The failed workflows of `workflow` when one is named, all of them
/// otherwise.
fn only_failed(mut failed: Vec<Failed>, workflow: Option<&str>) -> Vec<Failed> {
    if let Some(workflow) = workflow {
        failed.retain(|failed| failed.workflow == workflow);
    }
    failed
}

async fn report_invalid(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    req: &Request<'_>,
    diagnostics: &[Diagnostic],
) -> Result<()> {
    let check = invalid_config_check_run(diagnostics, req.sha);
    report_once(tokens, settings, req, &check).await
}

/// Creates `check` on the commit unless a check run of its name is
/// there already.
async fn report_once(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    req: &Request<'_>,
    check: &CheckRun,
) -> Result<()> {
    // A delivery repeated after a later failure finds its report.
    let listed = tokens
        .send(req.installation_id, |token| {
            github::check_runs_named(
                &settings.github_api,
                &req.repo.full_name,
                req.sha,
                &check.name,
                token,
            )
        })
        .await
        .map_err(|err| fail(err.to_string()))?;
    if github::check_run_total(&listed).map_err(fail)? > 0 {
        return Ok(());
    }
    let response = tokens
        .send(req.installation_id, |token| {
            github::create_check_run(&settings.github_api, &req.repo.full_name, token, check)
        })
        .await
        .map_err(|err| fail(err.to_string()))?;
    github::check_run_id(&response).map_err(fail)?;
    Ok(())
}

/// The longest message a run keeps, in characters.
const MAX_MESSAGE: usize = 200;

/// What a run shows next to its number: the workflow's `run-name` when
/// it has one, else the first line of the head commit's message of a
/// push, or the title of a pull request.
fn run_message(event_name: &str, event: &Value) -> Option<String> {
    let text = match event_name {
        "push" => event.pointer("/head_commit/message"),
        "pull_request" => event.pointer("/pull_request/title"),
        _ => None,
    }?
    .as_str()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.chars().take(MAX_MESSAGE).collect())
}

/// The number of the pull request a run is for: the one in the ref of a
/// pull request event.
fn pull_number(req: &Request<'_>) -> Option<u64> {
    if req.event_name != "pull_request" {
        return None;
    }
    req.git_ref
        .strip_prefix("refs/pull/")?
        .strip_suffix("/merge")?
        .parse()
        .ok()
}

/// The title `plan`'s `run-name` gives the run, cut to [`MAX_MESSAGE`]
/// characters. `github` holds the run's id, number and attempt too. A
/// template that fails to evaluate, or gives nothing, leaves the run
/// with its usual message.
fn title_of(
    req: &Request<'_>,
    plan: &Plan,
    api: &str,
    vars: &std::collections::BTreeMap<String, String>,
    run: &RunRow,
) -> Result<Option<String>, String> {
    let Some(template) = plan.run_name.as_deref() else {
        return Ok(None);
    };
    let info = RunInfo {
        run_id: run.id,
        run_number: run.run_number,
        run_attempt: run.run_attempt,
        tenant_id: req.tenant_id.to_owned(),
        retention_days: run.retention_days,
        installation_id: req.installation_id,
        repo: req.repo.clone(),
        workflow: plan.workflow.clone(),
        event_name: req.event_name.to_owned(),
        git_ref: req.git_ref.to_owned(),
        sha: req.sha.to_owned(),
        actor: req.actor.to_owned(),
        head_ref: req.head_ref.to_owned(),
        base_ref: req.base_ref.to_owned(),
        concurrency: None,
        title: None,
    };
    let mut github = github_context(&info, req.event, api);
    if let Some(fields) = github.as_object_mut() {
        fields.insert("run_id".to_owned(), info.run_id.to_string().into());
        fields.insert("run_number".to_owned(), info.run_number.to_string().into());
        fields.insert(
            "run_attempt".to_owned(),
            info.run_attempt.to_string().into(),
        );
    }
    let ctx = crate::concurrency::context(&github, vars);
    let title = run_title(template, &ctx)?;
    Ok((!title.is_empty()).then(|| title.chars().take(MAX_MESSAGE).collect()))
}

/// Records the run of `plan`, with its jobs, and has its object start.
async fn start_plan(
    env: &Env,
    db: &Db,
    settings: &Settings,
    req: &Request<'_>,
    plan: &Plan,
    now: u64,
) -> Result<u64> {
    let new = NewRun {
        tenant_id: req.tenant_id.to_owned(),
        repo_id: req.repo.id,
        workflow: plan.workflow.clone(),
        delivery: req.delivery.to_owned(),
        event: req.event_name.to_owned(),
        git_ref: req.git_ref.to_owned(),
        sha: req.sha.to_owned(),
        actor: req.actor.to_owned(),
        base_ref: (req.event_name == "pull_request").then(|| req.base_ref.to_owned()),
        message: run_message(req.event_name, req.event),
        pull_number: pull_number(req),
        head_ref: pull_number(req).map(|_| req.head_ref.to_owned()),
        trust: req.origin.trust,
        head_repo_id: req.origin.head_repo_id,
        head_repo_name: req.origin.head_repo_name.map(str::to_owned),
        author_login: req.origin.author.to_owned(),
        author_association: req.origin.association.to_owned(),
        gate_status: req.origin.gate,
        now,
    };
    let (run, fresh) = match store::insert_run(db, &new).await? {
        Some(run) => (run, true),
        // An earlier delivery stopped after it recorded the run.
        None => (
            store::run_for_delivery(db, req.delivery, &plan.workflow)
                .await?
                .ok_or_else(|| fail("a run of this delivery vanished"))?,
            false,
        ),
    };
    if run.tenant_id != req.tenant_id || run.repo_id != req.repo.id {
        return Err(fail(format!(
            "delivery {} already made run {} for another repository",
            req.delivery, run.id
        )));
    }
    // The title reads the run's id and number, so it follows the insert.
    let title = if plan.run_name.is_some() {
        let vars = store::job_vars(db, req.tenant_id, req.repo.id, None).await?;
        title_of(req, plan, &settings.github_api, &vars, &run).unwrap_or_else(|err| {
            worker::console_error!("{}: workflow {}: {err}", req.repo.full_name, plan.workflow);
            None
        })
    } else {
        None
    };
    if let (true, Some(title)) = (fresh, &title) {
        store::set_run_message(db, run.id, title).await?;
    }
    let mut rows = store::jobs_for_run(db, run.id).await?;
    if rows.is_empty() {
        let jobs: Vec<NewJob> = plan.jobs.iter().map(new_job).collect();
        store::insert_jobs(db, run.id, &jobs, now).await?;
        rows = store::jobs_for_run(db, run.id).await?;
    } else if !fresh && !unstarted(&run, &rows) {
        // The run went on after its first delivery; it may even have
        // finished and been purged, and a second start would run it again.
        return Ok(run.id);
    }
    let start = start_body(req, &run, &rows, plan, title)?;
    post_start(env, run.id, &start).await?;
    Ok(run.id)
}

/// The row of a planned job.
pub(crate) fn new_job(job: &PlannedJob) -> NewJob {
    NewJob {
        plan_id: job.id.clone(),
        name: job.name.clone(),
        timeout_minutes: job.timeout_minutes,
        template: match job.kind {
            JobKind::Run { template } => Some(template.labels[0].to_owned()),
            JobKind::Approval => None,
        },
        token_permissions: job.token_permissions.clone(),
        secret_refs: job.secret_refs(),
        // A name that is an expression is known when the job is queued,
        // and the row is given it then.
        environment: job.environment.clone().filter(|name| !is_expression(name)),
        strategy_group: job.strategy.as_ref().map(|s| s.group.clone()),
        max_parallel: job.strategy.as_ref().and_then(|s| s.max_parallel),
        needs: job.requires.clone(),
    }
}

/// Whether a recorded run has not started: it is queued and no job has
/// moved past waiting, so no launch or start happened.
#[must_use]
pub fn unstarted(run: &RunRow, rows: &[JobRow]) -> bool {
    run.status == "queued" && rows.iter().all(|row| row.status == "waiting")
}

/// The body of `POST /start`: each planned job with its row id.
fn start_body(
    req: &Request<'_>,
    run: &RunRow,
    rows: &[JobRow],
    plan: &Plan,
    title: Option<String>,
) -> Result<Start> {
    let jobs = plan
        .jobs
        .iter()
        .map(|job| {
            rows.iter()
                .find(|row| row.plan_id == job.id)
                .map(|row| (row.id, job.clone()))
                .ok_or_else(|| fail(format!("run {} has no row for job {}", run.id, job.id)))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Start {
        run: RunInfo {
            run_id: run.id,
            run_number: run.run_number,
            run_attempt: run.run_attempt,
            tenant_id: run.tenant_id.clone(),
            retention_days: run.retention_days,
            installation_id: req.installation_id,
            repo: req.repo.clone(),
            workflow: run.workflow.clone(),
            event_name: req.event_name.to_owned(),
            git_ref: run.git_ref.clone(),
            sha: run.sha.clone(),
            actor: run.actor.clone(),
            head_ref: req.head_ref.to_owned(),
            base_ref: req.base_ref.to_owned(),
            concurrency: plan.concurrency.clone(),
            title,
        },
        jobs,
        event: req.event.clone(),
    })
}

async fn post_start(env: &Env, run_id: u64, start: &Start) -> Result<()> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(serde_json::to_string(start)?.into()));
    let request = worker::Request::new_with_init("https://run/start", &init)?;
    let response = run_stub(env, run_id)?.fetch_with_request(request).await?;
    match response.status_code() {
        200..=299 => Ok(()),
        status => Err(fail(format!("run {run_id}: start answered {status}"))),
    }
}

#[cfg(test)]
mod tests {
    use hatchrun_cloud::http::Response as HttpResponse;

    use super::{Request, run_message, title_of};
    use crate::intake::Repo;

    fn title(run_name: &str, event: &serde_json::Value) -> Result<Option<String>, String> {
        let yaml = format!(
            "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}\nworkflows: {{ ci: {{ run-name: \"{run_name}\", on: {{ workflow_dispatch: {{}} }}, jobs: [t] }} }}\n"
        );
        let config = hatchrun_workflow::parse(&yaml).unwrap();
        let plan = hatchrun_workflow::compile(&config, "ci", "refs/heads/main").unwrap();
        let repo = Repo {
            id: 4242,
            full_name: "octo/widgets".into(),
            default_branch: "main".into(),
            private: false,
        };
        let trigger = hatchrun_workflow::Event::Dispatch;
        let req = Request {
            tenant_id: "t",
            installation_id: 7,
            repo: &repo,
            delivery: "d",
            event_name: "workflow_dispatch",
            event,
            trigger: &trigger,
            compile_ref: "refs/heads/main",
            git_ref: "refs/heads/main",
            sha: "abc",
            actor: "octocat",
            head_ref: "",
            base_ref: "",
            workflow: None,
            origin: Origin::own("octocat"),
        };
        let vars = std::collections::BTreeMap::from([("REGION".to_owned(), "eu".to_owned())]);
        let run = crate::store::RunRow {
            id: 9,
            run_number: 12,
            run_attempt: 1,
            tenant_id: "t".into(),
            repo_id: 4242,
            workflow: "ci".into(),
            delivery: "d".into(),
            event: "workflow_dispatch".into(),
            git_ref: "refs/heads/main".into(),
            sha: "abc".into(),
            actor: "octocat".into(),
            base_ref: None,
            status: "queued".into(),
            conclusion: None,
            created_at: 0,
            completed_at: None,
            retention_days: 30,
            message: None,
            pull_number: None,
            head_ref: None,
            pull_title: None,
            trust: Trust::Trusted,
            head_repo_name: None,
            author_login: "octocat".into(),
            author_association: String::new(),
            gate_status: GateStatus::NotGated,
            gate_by: None,
            gate_at: None,
            gate_check_run_id: None,
        };
        title_of(&req, &plan, "https://api.github.com", &vars, &run)
    }

    fn titled(run_name: &str, event: &serde_json::Value) -> Option<String> {
        title(run_name, event).unwrap()
    }

    #[test]
    fn run_name_reads_github_inputs_and_vars() {
        let event = serde_json::json!({ "inputs": { "target": "staging" } });
        assert_eq!(
            titled(
                "Deploy ${{ inputs.target }} to ${{ vars.REGION }} by ${{ github.actor }}",
                &event
            )
            .as_deref(),
            Some("Deploy staging to eu by octocat")
        );
    }

    #[test]
    fn run_name_reads_the_run_number() {
        let event = serde_json::json!({});
        assert_eq!(
            titled(
                "Deploy #${{ github.run_number }} (${{ github.run_id }})",
                &event
            )
            .as_deref(),
            Some("Deploy #12 (9)")
        );
    }

    #[test]
    fn a_run_name_that_fails_or_comes_out_empty_gives_no_title() {
        let event = serde_json::json!({});
        assert!(title("${{ fromJSON('{') }}", &event).is_err());
        assert_eq!(titled("${{ inputs.missing }}", &event), None);
        let long = "x".repeat(300);
        assert_eq!(titled(&long, &event).unwrap().chars().count(), 200);
    }

    #[test]
    fn messages_are_the_first_line_cut_short() {
        let push = serde_json::json!({"head_commit": {"message": " fix it\n\nlong body"}});
        assert_eq!(run_message("push", &push).as_deref(), Some("fix it"));
        let pr = serde_json::json!({"pull_request": {"title": "x".repeat(300)}});
        assert_eq!(
            run_message("pull_request", &pr).unwrap().chars().count(),
            200
        );
        assert_eq!(run_message("schedule", &push), None);
        assert_eq!(run_message("push", &serde_json::json!({})), None);
        let empty = serde_json::json!({"head_commit": {"message": ""}});
        assert_eq!(run_message("push", &empty), None);
    }

    fn answer(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_missing_ref_is_none_and_not_an_error() {
        for status in [404, 409, 422] {
            assert_eq!(commit_of(&answer(status, "{}")), Ok(None), "{status}");
        }
        assert_eq!(
            commit_of(&answer(200, r#"{"sha":"abc"}"#)),
            Ok(Some("abc".to_owned()))
        );
        assert!(commit_of(&answer(500, "boom")).is_err());
        let limited = answer(
            422,
            r#"{"message":"You have exceeded a secondary rate limit."}"#,
        );
        assert!(commit_of(&limited).is_err());
        let mut waiting = answer(422, "{}");
        waiting.headers.push(("retry-after".into(), "60".into()));
        assert!(commit_of(&waiting).is_err());
        let unresolvable = answer(422, r#"{"message":"No commit found for SHA: x"}"#);
        assert_eq!(commit_of(&unresolvable), Ok(None));
        let named = answer(
            422,
            r#"{"message":"No commit found for SHA: tags/secondary-v1"}"#,
        );
        assert_eq!(commit_of(&named), Ok(None));
        let abuse = answer(
            422,
            r#"{"message":"You have triggered an abuse detection mechanism."}"#,
        );
        assert!(commit_of(&abuse).is_err());
        assert!(commit_of(&answer(403, "rate limited")).is_err());
    }

    use std::collections::BTreeMap;

    use hatchrun_workflow::SecretRefs;

    use super::*;
    use crate::intake::{Trigger, parse};

    #[test]
    fn invalid_workflow_check_run_names_the_workflow() {
        let diagnostics = [Diagnostic::new(
            "job `t` has `runs-on: ubuntu-24.04`, which names no runner template",
        )];
        let run = invalid_workflow_check_run("bad", &diagnostics, "abc");
        assert_eq!(run.name, "bad / config");
        assert_eq!(run.head_sha, "abc");
        assert_eq!(run.status, "completed");
        assert_eq!(run.conclusion.as_deref(), Some("failure"));
        let output = run.output.unwrap();
        assert_eq!(output.title, "Invalid workflow");
        assert!(output.summary.contains("workflow `bad` did not start"));
        assert!(output.summary.contains("names no runner template"));
        assert_eq!(output.annotations.len(), 1);
    }

    #[test]
    fn no_card_check_run_asks_for_a_card() {
        let run = no_card_check_run("abc", "acme", "https://app.example");
        assert_eq!(run.name, "hatchrun / billing");
        assert_eq!(CARD_CHECK, run.name);
        assert_eq!(run.head_sha, "abc");
        assert_eq!(run.status, "completed");
        assert_eq!(run.conclusion.as_deref(), Some("action_required"));
        assert_eq!(run.details_url, "https://app.example/o/acme/ci");
        let output = run.output.unwrap();
        assert_eq!(output.title, "hatchrun needs a card");
        assert_eq!(
            output.summary,
            "Add a card for acme at https://app.example/o/acme/ci to run this commit's workflows."
        );
        assert_eq!(output.annotations.len(), 0);
    }

    #[test]
    fn no_card_without_a_triggered_workflow_reports_nothing() {
        assert_eq!(billing_decision(CiBilling::Lapsed, 0), Billing::Nothing);
        assert_eq!(billing_decision(CiBilling::Never, 0), Billing::Nothing);
        assert_eq!(billing_decision(CiBilling::Lapsed, 2), Billing::AskForCard);
        assert_eq!(billing_decision(CiBilling::Never, 2), Billing::AskForCard);
        assert_eq!(billing_decision(CiBilling::Active, 0), Billing::Run);
        assert_eq!(billing_decision(CiBilling::Active, 2), Billing::Run);
    }

    #[test]
    fn config_failure_check_run_lists_diagnostics() {
        let diagnostics = [
            Diagnostic {
                message: "unknown job `x`".into(),
                file: None,
                line: Some(3),
                column: Some(5),
            },
            Diagnostic::new("no workflows"),
        ];
        let run = invalid_config_check_run(&diagnostics, "abc");
        assert_eq!(run.name, "hatchrun / config");
        assert_eq!(run.head_sha, "abc");
        assert_eq!(run.status, "completed");
        assert_eq!(run.conclusion.as_deref(), Some("failure"));
        let output = run.output.unwrap();
        assert!(output.summary.contains("line 3, column 5: unknown job `x`"));
        assert!(output.summary.contains("- no workflows"));
        assert_eq!(output.annotations.len(), 2);
        let first = &output.annotations[0];
        assert_eq!(first.path, ".hatchrun/config.yml");
        assert_eq!((first.start_line, first.end_line), (3, 3));
        assert_eq!((first.start_column, first.end_column), (Some(5), Some(5)));
        assert_eq!(first.annotation_level, "failure");
        let second = &output.annotations[1];
        assert_eq!((second.start_line, second.start_column), (1, None));
    }

    #[test]
    fn config_failure_annotations_stay_within_githubs_limit() {
        let many: Vec<Diagnostic> = (0..120).map(|i| Diagnostic::new(format!("p{i}"))).collect();
        let output = invalid_config_check_run(&many, "abc").output.unwrap();
        assert_eq!(output.annotations.len(), 50);
        assert!(output.summary.contains("and 20 more"));
    }

    #[test]
    fn only_a_run_that_has_not_started_is_started_again() {
        use hatchrun_cloud::testdb::Db as TestDb;
        let db = TestDb::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, \
             updated_at) VALUES ('t1', 1, 'acme', 99, 0, 0)",
            &[],
        );
        let repo = store::RepoRow {
            id: 7,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "acme/app".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(store::upsert_repo(db.db(), &repo, 1)).unwrap();
        let run = db
            .wait(store::insert_run(
                db.db(),
                &NewRun {
                    tenant_id: "t1".into(),
                    repo_id: 7,
                    workflow: "ci".into(),
                    delivery: "d".into(),
                    event: "push".into(),
                    git_ref: "refs/heads/main".into(),
                    sha: "abc".into(),
                    actor: "octocat".into(),
                    base_ref: None,
                    message: None,
                    pull_number: None,
                    head_ref: None,
                    trust: Trust::Trusted,
                    head_repo_id: None,
                    head_repo_name: None,
                    author_login: "octocat".into(),
                    author_association: String::new(),
                    gate_status: GateStatus::NotGated,
                    now: 1,
                },
            ))
            .unwrap()
            .unwrap();
        let jobs = [NewJob {
            plan_id: "build".into(),
            name: "build".into(),
            timeout_minutes: 10,
            template: Some("ubuntu-latest".into()),
            token_permissions: BTreeMap::new(),
            secret_refs: SecretRefs::default(),
            environment: None,
            strategy_group: None,
            max_parallel: None,
            needs: Vec::new(),
        }];
        let ids = db
            .wait(store::insert_jobs(db.db(), run.id, &jobs, 1))
            .unwrap();
        let load = |db: &TestDb| {
            let run = db.wait(store::run(db.db(), run.id)).unwrap().unwrap();
            let rows = db.wait(store::jobs_for_run(db.db(), run.id)).unwrap();
            (run, rows)
        };
        let (r, rows) = load(&db);
        assert!(unstarted(&r, &rows));
        db.wait(store::run_started(db.db(), run.id, 2)).unwrap();
        let (r, rows) = load(&db);
        assert!(!unstarted(&r, &rows), "run is in progress");
        db.run(
            "UPDATE ci_runs SET status = 'queued' WHERE id = $1",
            &hatchrun_cloud::db::params![run.id],
        );
        db.wait(store::set_job(db.db(), ids[0].1, "queued", None, 3))
            .unwrap();
        let (r, rows) = load(&db);
        assert!(!unstarted(&r, &rows), "a job was queued");
    }

    #[test]
    fn workflows_sharing_a_cron_each_start_once() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  a: { on: { schedule: [{ cron: '0 3 * * *' }] }, jobs: [t] }
  b: { on: { schedule: [{ cron: '0 3 * * *' }] }, jobs: [t] }
  c: { on: { schedule: [{ cron: '5 3 * * *' }] }, jobs: [t] }
";
        let event = Event::Schedule {
            cron: "0 3 * * *".into(),
        };
        let Selected::Plans { plans, .. } = planning::select(source, &event, "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans.len(), 2, "both share the cron");
        let names = |workflow| {
            only_workflow(plans.clone(), workflow)
                .into_iter()
                .map(|plan| plan.workflow)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Some("a")), ["a"]);
        assert_eq!(names(Some("b")), ["b"]);
        assert_eq!(names(Some("c")), [] as [String; 0]);
        assert_eq!(names(None), ["a", "b"]);
    }

    #[test]
    fn pull_request_compiles_against_the_base_branch() {
        let body = include_str!("../testdata/pull_request.json");
        let Trigger::PullRequest(pr) = parse("pull_request", body.as_bytes()).unwrap() else {
            panic!("not a pull request");
        };
        assert_eq!(compile_ref_for(&pr), "refs/heads/main");
    }
}
