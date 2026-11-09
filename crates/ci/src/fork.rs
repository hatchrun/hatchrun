// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Pull requests from forks: the approval check run a maintainer
//! clicks, and what follows a decision on a run that waited for one.
//!
//! The text of the check run is built here from the run rows, so the
//! approver sees what the pull request changes before its workflows
//! run: above all whether it changes the config, since the pull
//! request's own workflow is what would run. The functions that talk
//! to GitHub and to the run's object are best effort after the
//! decision is saved: a decision is one row change, and the
//! reconciler's backstops wake a job or cancel a run a failed call
//! left behind.

use std::fmt::Write as _;

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::{JobKind, Plan};
use worker::{Env, Method, RequestInit, console_error};

use crate::checks::plain;
use crate::env::{Settings, run_stub};
use crate::github::{self, CONFIG_PATH, CheckAction, CheckRun, Output};
use crate::launches;
use crate::policy::{APPROVAL_CHECK, APPROVE_ACTION, FORK_MAX_VCPU, ForkApproval, RepoPolicy};
use crate::store::{self, GateStatus, RepoRow, RunRow};
use crate::trust::Trust;

/// Pages of a pull request's files the card reads, 100 files each.
const FILE_PAGES: u32 = 3;
/// Files the card names.
const LISTED_FILES: usize = 20;
/// `len` as a count for the reconciler's tally: all it can hold when it
/// does not fit.
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// What a run that a newer commit replaced is told.
pub const SUPERSEDED: &str = "Superseded by a newer commit";
/// What a run whose approval nobody gave in time is told.
pub const EXPIRED: &str = "Approval expired";
/// What a run of a pull request that closed is told.
pub const CLOSED: &str = "The pull request was closed";

/// One job the approval would start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardJob {
    /// The workflow.
    pub workflow: String,
    /// The job's name.
    pub job: String,
    /// The runner template's first label.
    pub template: Option<String>,
    /// The job's timeout, in minutes, as it will run.
    pub timeout_minutes: u32,
}

/// The name of the approval check run of a run: one card per run, named
/// for its workflow and number, so two runs of one workflow at one
/// commit show as two cards.
#[must_use]
pub fn approval_check_name(workflow: &str, run_number: u64) -> String {
    format!("{APPROVAL_CHECK}: {workflow} #{run_number}")
}

/// The `external_id` of a run's approval check run: the run's id, which
/// is what ties a card to its run when an earlier attempt's is adopted.
#[must_use]
pub fn approval_external_id(run_id: u64) -> String {
    format!("hatchrun-ci-approval:{run_id}")
}

/// What the approver reads: one run's card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The id of the run the card is for.
    pub run_id: u64,
    /// The run's number among those of its workflow.
    pub run_number: u64,
    /// The workflow of the run the card is for.
    pub workflow: String,
    /// The commit the check run is on.
    pub sha: String,
    /// The pull request's number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// The author's login.
    pub author: String,
    /// The author's association with the repository.
    pub association: String,
    /// The head repository, `owner/name`; `None` for a deleted fork.
    pub head_repo: Option<String>,
    /// The base branch.
    pub base_ref: String,
    /// The approval level that asked for this approval.
    pub level: ForkApproval,
    /// The changed files; `None` when they could not be read.
    pub files: Option<Vec<String>>,
    /// The jobs that would start.
    pub jobs: Vec<CardJob>,
    /// The app's base URL.
    pub app_url: String,
    /// The repository, `owner/name`.
    pub repo: String,
}

/// Whether the changed `files` include the config: the line that
/// matters most to an approver, since the pull request's own workflow
/// is what would run.
#[must_use]
pub fn changes_config(files: &[String]) -> bool {
    files.iter().any(|name| name == CONFIG_PATH)
}

fn level_text(level: ForkApproval) -> &'static str {
    match level {
        ForkApproval::FirstTimers => "the author is new to GitHub",
        ForkApproval::FirstTimeContributors => {
            "the author has not contributed to this repository before"
        }
        ForkApproval::AllOutside => "the author is not a member or collaborator",
    }
}

/// The summary of the approval check run.
fn summary(card: &Card) -> String {
    let mut text = format!(
        "Pull request #{} {} from {} wants to run workflows.\n\n",
        card.number,
        plain(&card.title),
        plain(&card.author)
    );
    let _ = writeln!(
        text,
        "- Author: {} ({}), asked because {}",
        plain(&card.author),
        plain(&card.association.to_lowercase()),
        level_text(card.level)
    );
    let _ = writeln!(
        text,
        "- Head: {} at `{}`, into `{}`",
        card.head_repo
            .as_deref()
            .map_or_else(|| "a deleted fork".to_owned(), plain),
        card.sha,
        plain(&card.base_ref)
    );
    let files_url = format!(
        "https://github.com/{}/pull/{}/files",
        card.repo, card.number
    );
    match &card.files {
        Some(files) => {
            let _ = writeln!(text, "- Files changed: {}", files.len());
            if changes_config(files) {
                let _ = writeln!(
                    text,
                    "\n**The pull request changes `{CONFIG_PATH}`.** Its own workflow is what \
                     will run: [read the diff]({files_url})."
                );
            }
        }
        None => {
            let _ = writeln!(
                text,
                "- Files changed: not read, see [the pull request]({files_url})"
            );
        }
    }
    text.push_str("\nJobs that would start:\n\n");
    for job in &card.jobs {
        let _ = writeln!(
            text,
            "- `{}` / `{}` on `{}`, {} min",
            plain(&job.workflow),
            plain(&job.job),
            job.template.as_deref().unwrap_or("-"),
            job.timeout_minutes
        );
    }
    text.push_str(
        "\nThe run gets no secrets, no write token, no OIDC token and no environment. \
         Approving it runs this commit only: a new push asks again.",
    );
    text
}

/// The text of the approval check run's second part: the files.
fn files_text(card: &Card) -> Option<String> {
    let files = card.files.as_ref()?;
    if files.is_empty() {
        return None;
    }
    let mut text = String::from("Files changed:\n\n");
    for name in files.iter().take(LISTED_FILES) {
        let _ = writeln!(text, "- `{}`", name.replace('`', ""));
    }
    if files.len() > LISTED_FILES {
        let _ = writeln!(text, "- and {} more", files.len() - LISTED_FILES);
    }
    Some(text)
}

fn card_check(
    card: &Card,
    conclusion: &str,
    actions: Vec<CheckAction>,
    output: Output,
) -> CheckRun {
    CheckRun {
        name: approval_check_name(&card.workflow, card.run_number),
        head_sha: card.sha.clone(),
        external_id: approval_external_id(card.run_id),
        details_url: format!(
            "{}/ci/{}/runs?pull={}",
            card.app_url, card.repo, card.number
        ),
        status: "completed".to_owned(),
        started_at: None,
        conclusion: Some(conclusion.to_owned()),
        completed_at: None,
        actions,
        output: Some(output),
    }
}

/// The check run that asks a maintainer to approve: `action_required`
/// with one button, `Approve and run`.
#[must_use]
pub fn approval_check_run(card: &Card) -> CheckRun {
    card_check(
        card,
        "action_required",
        vec![CheckAction {
            label: "Approve and run".to_owned(),
            description: "Run this commit's workflows".to_owned(),
            identifier: APPROVE_ACTION.to_owned(),
        }],
        Output {
            title: "Approval needed to run this pull request".to_owned(),
            summary: summary(card),
            text: files_text(card),
            annotations: Vec::new(),
        },
    )
}

/// The check run of a run that waits for a free approval slot: no
/// button yet.
#[must_use]
pub fn waiting_check_run(card: &Card) -> CheckRun {
    card_check(
        card,
        "neutral",
        Vec::new(),
        Output {
            title: "Waiting for a free approval slot".to_owned(),
            summary: "Waiting for a free approval slot. This repository has too many pull \
                      requests waiting for an approval; this one asks when a slot frees."
                .to_owned(),
            text: None,
            annotations: Vec::new(),
        },
    )
}

/// The check run that replaces the card once the run is decided: no
/// button, `neutral`, saying `message`.
#[must_use]
pub fn decided_check_run(workflow: &str, run_number: u64, message: &str) -> CheckRun {
    CheckRun {
        name: approval_check_name(workflow, run_number),
        head_sha: String::new(),
        external_id: String::new(),
        details_url: String::new(),
        status: "completed".to_owned(),
        started_at: None,
        conclusion: Some("neutral".to_owned()),
        completed_at: None,
        actions: Vec::new(),
        output: Some(Output {
            title: message.to_owned(),
            summary: message.to_owned(),
            text: None,
            annotations: Vec::new(),
        }),
    }
}

/// `Approved by <login>`.
#[must_use]
pub fn approved_message(login: &str) -> String {
    format!("Approved by {login}")
}

/// `Rejected by <login>`.
#[must_use]
pub fn rejected_message(login: &str) -> String {
    format!("Rejected by {login}")
}

/// The jobs of `plan` as the card lists them, with the timeout cut to
/// `limit` minutes as the run will have it.
#[must_use]
pub fn card_jobs(plan: &Plan, limit: u32) -> Vec<CardJob> {
    plan.jobs
        .iter()
        .map(|job| CardJob {
            workflow: plan.workflow.clone(),
            job: job.name.clone(),
            template: match job.kind {
                JobKind::Run { template } => Some(template.labels[0].to_owned()),
                JobKind::Approval => None,
            },
            timeout_minutes: job.timeout_minutes.min(limit),
        })
        .collect()
}

/// Why `plan` cannot run for a fork: its first job on a template over
/// [`FORK_MAX_VCPU`] vCPUs.
#[must_use]
pub fn oversized_job(plan: &Plan) -> Option<String> {
    plan.jobs.iter().find_map(|job| match job.kind {
        JobKind::Run { template } if template.vcpu > FORK_MAX_VCPU => Some(format!(
            "job `{}` runs on `{}` ({} vCPUs); pull requests from forks run on at most \
             {FORK_MAX_VCPU} vCPUs",
            job.name, template.labels[0], template.vcpu
        )),
        _ => None,
    })
}

/// The vCPUs of the runner template named `label`, when it is one.
#[must_use]
pub fn template_vcpu(label: &str) -> Option<u32> {
    hatchrun_workflow::template(label).map(|template| template.vcpu)
}

/// The card of `run` at its repository `repo`, with the jobs of that
/// run and `files` as read.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn card_for(
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    level: ForkApproval,
    files: Option<Vec<String>>,
) -> worker::Result<Card> {
    let jobs = store::jobs_for_run(db, run.id)
        .await?
        .into_iter()
        .map(|row| CardJob {
            workflow: run.workflow.clone(),
            job: row.name,
            template: row.template,
            timeout_minutes: row.timeout_minutes,
        })
        .collect();
    Ok(Card {
        run_id: run.id,
        run_number: run.run_number,
        workflow: run.workflow.clone(),
        sha: run.sha.clone(),
        number: run.pull_number.unwrap_or_default(),
        title: run
            .pull_title
            .clone()
            .or_else(|| run.message.clone())
            .unwrap_or_default(),
        author: run.author_login.clone(),
        association: run.author_association.clone(),
        head_repo: run.head_repo_name.clone(),
        base_ref: run.base_ref.clone().unwrap_or_default(),
        level,
        files,
        jobs,
        app_url: settings.app_url.clone(),
        repo: repo.full_name.clone(),
    })
}

/// The names of the files pull request `number` changes, up to three
/// pages; `None` when GitHub would not say.
pub async fn changed_files(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    repo: &RepoRow,
    number: u64,
) -> Option<Vec<String>> {
    let mut all = Vec::new();
    for page in 1..=FILE_PAGES {
        let response = tokens
            .send(repo.installation_id, |token| {
                github::pull_files(&settings.github_api, &repo.full_name, number, page, token)
            })
            .await
            .ok()?;
        let names = github::pull_file_names(&response).ok()?;
        let full = names.len() >= 100;
        all.extend(names);
        if !full {
            break;
        }
    }
    Some(all)
}

/// Updates check run `check_id` of run `run_id`. Best effort: the
/// decision is saved already. Returns whether GitHub took it.
pub async fn update_check(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    repo: &RepoRow,
    run_id: u64,
    check_id: u64,
    check: &CheckRun,
) -> bool {
    let sent = tokens
        .send(repo.installation_id, |token| {
            github::update_check_run(
                &settings.github_api,
                &repo.full_name,
                check_id,
                token,
                check,
            )
        })
        .await;
    match sent {
        Ok(response) if response.ok() => true,
        Ok(response) => {
            console_error!(
                "run {run_id}: update approval check run {check_id}: GitHub answered {}",
                response.status
            );
            false
        }
        Err(err) => {
            console_error!("run {run_id}: update approval check run {check_id}: {err}");
            false
        }
    }
}

/// Updates the approval check run of `run`, when it has one. Returns
/// whether it is up to date: a run with no card has nothing to update.
pub async fn update_card(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    check: &CheckRun,
) -> bool {
    match run.gate_check_run_id {
        Some(id) => update_check(tokens, settings, repo, run.id, id, check).await,
        None => true,
    }
}

/// Asks the object of `run` to cancel it with `message`, whatever its
/// jobs' `if:` (nothing of it ever ran).
///
/// # Errors
///
/// The object could not be reached or refused.
pub async fn cancel_run(env: &Env, run: &RunRow, message: &str) -> Result<(), String> {
    let body = serde_json::json!({ "message": message, "outright": true });
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(body.to_string().into()));
    let url = format!(
        "https://run/cancel?run={}&tenant={}&days={}&attempt={}",
        run.id, run.tenant_id, run.retention_days, run.run_attempt
    );
    let request = worker::Request::new_with_init(&url, &init).map_err(|e| e.to_string())?;
    let response = run_stub(env, run.id)
        .map_err(|e| e.to_string())?
        .fetch_with_request(request)
        .await
        .map_err(|e| e.to_string())?;
    match response.status_code() {
        200..=299 | 404 | 409 => Ok(()),
        other => Err(format!("the run's object answered {other}")),
    }
}

/// After `runs` were approved by `by`: wakes the jobs held for the
/// approval and marks the card. A failed wake is left to the
/// reconciler.
pub async fn after_approval(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    repo: &RepoRow,
    runs: &[RunRow],
    by: &str,
) {
    for run in runs {
        match store::fork_approval_held(db, run.id).await {
            Ok(woken) => {
                if let Err(err) = launches::send_wakes(env, &woken).await {
                    console_error!("run {}: wake after approval: {err}", run.id);
                }
            }
            Err(err) => console_error!("run {}: find held jobs: {err}", run.id),
        }
        let check = decided_check_run(&run.workflow, run.run_number, &approved_message(by));
        update_card(tokens, settings, repo, run, &check).await;
    }
}

/// After `runs` ended without an approval (rejected, superseded,
/// expired, closed): cancels them and marks the card with `message`.
pub async fn after_end(
    env: &Env,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    repo: &RepoRow,
    runs: &[RunRow],
    message: &str,
) {
    for run in runs {
        if let Err(err) = cancel_run(env, run, message).await {
            console_error!("run {}: cancel after {message}: {err}", run.id);
        }
        let check = decided_check_run(&run.workflow, run.run_number, message);
        update_card(tokens, settings, repo, run, &check).await;
    }
}

/// The reconciler's pass over fork approvals: expires the runs nobody
/// decided on in seven days, then asks for the approval of deferred runs
/// while their repository has slots. Returns how many runs it changed.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn sweep(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    now: u64,
) -> Result<u32, String> {
    const PASS: u32 = 20;
    let before = now.saturating_sub(crate::policy::APPROVAL_EXPIRES_MS);
    let expired = store::expire_gates(db, before, now, PASS)
        .await
        .map_err(|err| err.to_string())?;
    let mut changed = count(expired.len());
    for run in &expired {
        let found = store::repo(db, run.repo_id)
            .await
            .map_err(|err| err.to_string())?;
        if let Some(repo) = found {
            after_end(
                env,
                settings,
                tokens,
                &repo,
                std::slice::from_ref(run),
                EXPIRED,
            )
            .await;
        }
    }
    // The oldest groups of each repository and of each author in it: a
    // group the author's cap blocks is skipped and the next is taken.
    for run in store::deferred_runs(db, PASS, PER_REPO, PER_AUTHOR)
        .await
        .map_err(|err| err.to_string())?
    {
        changed += admit(env, db, settings, tokens, &run, now).await?;
    }
    // A card a delivery never finished is made here, so the approve
    // button always exists.
    let stale = now.saturating_sub(CARD_GRACE_MS);
    for run in store::runs_without_card(db, stale, PASS, PER_REPO)
        .await
        .map_err(|err| err.to_string())?
    {
        let Some(repo) = store::repo(db, run.repo_id)
            .await
            .map_err(|err| err.to_string())?
        else {
            continue;
        };
        let policy = store::repo_policy(db, repo.id, repo.private)
            .await
            .map_err(|err| err.to_string())?;
        if let Err(err) = ensure_card(tokens, db, settings, &repo, &run, &policy).await {
            console_error!("run {}: make the approval check run: {err}", run.id);
        }
    }
    Ok(changed)
}

/// The most runs of one repository a reconciler pass looks at in each
/// of its fork queues.
const PER_REPO: u32 = 5;

/// The most groups of one author in one repository a reconciler pass
/// looks at in the queue of deferred runs.
const PER_AUTHOR: u32 = 2;

/// How long a waiting run may go without its check run before the
/// reconciler makes it: the webhook is likely still at it before that.
const CARD_GRACE_MS: u64 = 60_000;

/// Whether a run needs its approval check run made: a pull request's
/// run that waits and has none recorded. A run that was decided, or has
/// no pull request, needs none.
#[must_use]
pub const fn needs_card(run: &RunRow) -> bool {
    run.pull_number.is_some() && waits(run) && run.gate_check_run_id.is_none()
}

/// What a decided run's card says, `None` for a run that still waits.
#[must_use]
pub fn decided_message(run: &RunRow) -> Option<String> {
    match run.gate_status {
        GateStatus::Approved => Some(approved_message(run.gate_by.as_deref().unwrap_or("policy"))),
        GateStatus::Cancelled => Some(
            run.gate_by
                .as_deref()
                .map_or_else(|| "Cancelled".to_owned(), rejected_message),
        ),
        GateStatus::Expired => Some(EXPIRED.to_owned()),
        GateStatus::NotGated | GateStatus::Pending | GateStatus::Deferred => None,
    }
}

/// Records card `id` for `run`. When the run was decided while the card
/// was being made, nothing is recorded and the new card is settled to
/// the run's decided state at once, so it never offers a button for a
/// run that no longer waits.
async fn record_card(
    tokens: &Tokens<'_, Kv, App, Counted>,
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    id: u64,
) -> Result<(), String> {
    let recorded = store::set_gate_check(db, run.id, id)
        .await
        .map_err(|err| err.to_string())?;
    if recorded {
        return Ok(());
    }
    let current = store::run(db, run.id)
        .await
        .map_err(|err| err.to_string())?;
    if let Some(message) = current.as_ref().and_then(decided_message) {
        let check = decided_check_run(&run.workflow, run.run_number, &message);
        update_check(tokens, settings, repo, run.id, id, &check).await;
    }
    Ok(())
}

/// Makes sure the waiting `run` has its own approval check run, named
/// for its workflow and number and tied to the run by its `external_id`,
/// and that its id is recorded, only after the check run exists: the
/// Approve button then approves exactly this run. Safe to repeat, from
/// the webhook and from the reconciler: it adopts this run's card of
/// ours that an earlier attempt made and did not record, and only an
/// undecided one (see [`github::adoptable_card`]).
///
/// # Errors
///
/// GitHub or Postgres failed; the caller tries again.
pub async fn ensure_card(
    tokens: &Tokens<'_, Kv, App, Counted>,
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    policy: &RepoPolicy,
) -> Result<(), String> {
    if !needs_card(run) {
        return Ok(());
    }
    let name = approval_check_name(&run.workflow, run.run_number);
    let listed = tokens
        .send(repo.installation_id, |token| {
            github::check_runs_named(
                &settings.github_api,
                &repo.full_name,
                &run.sha,
                &name,
                token,
            )
        })
        .await
        .map_err(|err| err.to_string())?;
    let external_id = approval_external_id(run.id);
    if let Some(id) = github::adoptable_card(&listed, &settings.app_id, &external_id)? {
        return record_card(tokens, db, settings, repo, run, id).await;
    }
    // The file list is read only for a card that asks; a run waiting for
    // a slot has no use for it yet.
    let pending = run.gate_status == GateStatus::Pending;
    let files = match run.pull_number {
        Some(number) if pending => changed_files(tokens, settings, repo, number).await,
        _ => None,
    };
    let card = card_for(db, settings, repo, run, policy.fork_approval, files)
        .await
        .map_err(|err| err.to_string())?;
    let check = if pending {
        approval_check_run(&card)
    } else {
        waiting_check_run(&card)
    };
    let response = tokens
        .send(repo.installation_id, |token| {
            github::create_check_run(&settings.github_api, &repo.full_name, token, &check)
        })
        .await
        .map_err(|err| err.to_string())?;
    let id = github::check_run_id(&response)?;
    record_card(tokens, db, settings, repo, run, id).await
}

/// Decides what the deferred runs of `run`'s pull request and commit
/// (a group, counted once against the limits) become now, together:
/// asks for their approval when a slot is free and the author is under
/// the cap, runs them when no approval is needed, ends them when the
/// repository turned fork runs off, and leaves them otherwise. Each
/// keeps its own card. Returns how many changed.
async fn admit(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    run: &RunRow,
    now: u64,
) -> Result<u32, String> {
    let (Some(repo), Some(number)) = (
        store::repo(db, run.repo_id)
            .await
            .map_err(|err| err.to_string())?,
        run.pull_number,
    ) else {
        return Ok(0);
    };
    let policy = store::repo_policy(db, repo.id, repo.private)
        .await
        .map_err(|err| err.to_string())?;
    let mut waiting = store::fork_waiting(db, repo.id, &run.author_login, Some(number))
        .await
        .map_err(|err| err.to_string())?;
    // The runs asking are deferred; they must not count against
    // themselves.
    waiting.deferred = 0;
    let group = store::deferred_group(db, repo.id, number, &run.sha)
        .await
        .map_err(|err| err.to_string())?;
    let slot = crate::policy::admission(&policy, &run.author_association, waiting);
    let (status, by) = match slot {
        crate::policy::Admission::Pending => (GateStatus::Pending, None),
        crate::policy::Admission::Run => (GateStatus::Approved, Some("policy")),
        crate::policy::Admission::Off => {
            let mut ended = Vec::new();
            for member in &group {
                ended.extend(
                    store::end_gate(db, member.id, "cancelled", None, now)
                        .await
                        .map_err(|err| err.to_string())?,
                );
            }
            after_end(env, settings, tokens, &repo, &ended, FORK_RUNS_OFF).await;
            return Ok(count(ended.len()));
        }
        crate::policy::Admission::Deferred | crate::policy::Admission::Drop => return Ok(0),
    };
    let mut admitted = Vec::new();
    for member in &group {
        admitted.extend(
            store::admit_run(db, member.id, status, by, now)
                .await
                .map_err(|err| err.to_string())?,
        );
    }
    if let Some(by) = by {
        after_approval(env, db, settings, tokens, &repo, &admitted, by).await;
        return Ok(count(admitted.len()));
    }
    // The file list is the same for the whole group: read it once.
    let files = changed_files(tokens, settings, &repo, number).await;
    for member in &admitted {
        let made = async {
            if member.gate_check_run_id.is_none() {
                return ensure_card(tokens, db, settings, &repo, member, &policy)
                    .await
                    .map(|()| true);
            }
            // The waiting card becomes the approval card.
            let card = card_for(
                db,
                settings,
                &repo,
                member,
                policy.fork_approval,
                files.clone(),
            )
            .await
            .map_err(|err| err.to_string())?;
            Ok(update_card(tokens, settings, &repo, member, &approval_check_run(&card)).await)
        }
        .await;
        // A card that could not be made or updated is forgotten, so the
        // reconciler makes it again; the other members go on.
        if !matches!(made, Ok(true)) {
            if let Err(err) = &made {
                console_error!("run {}: the approval check run: {err}", member.id);
            }
            if let Err(err) = store::clear_gate_check(db, member.id).await {
                console_error!("run {}: forget the approval check run: {err}", member.id);
            }
        }
    }
    Ok(count(admitted.len()))
}

/// What a rerun of a fork run is told when the repository turned fork
/// runs off, and what admission ends a deferred run with.
pub const FORK_RUNS_OFF: &str = "Fork runs are off for this repository";

/// What a rerun of a fork run whose approval ended is told.
pub const NOT_APPROVED: &str =
    "This fork run was not approved. Push a new commit to ask for approval again.";

/// The name of the check run that says a rerun was refused, one per
/// commit: a later refusal updates it.
pub const RERUN_CHECK: &str = "hatchrun / rerun";

/// Why a rerun of `run` is refused, `None` when it may go on. A fork
/// run whose approval ended without being given (expired, rejected,
/// superseded or its pull request closed) is refused on its own row: its
/// jobs never ran and would be reset with nothing to end them. A fork
/// run of a repository that turned fork runs off is refused too, which
/// needs the repository's `policy`. A rerun of an approved run keeps its
/// approval, since the code is the same and a rerun already needs write
/// access.
#[must_use]
pub const fn rerun_refusal(run: &RunRow, policy: Option<&RepoPolicy>) -> Option<&'static str> {
    if !matches!(run.trust, Trust::Fork) {
        return None;
    }
    if let Some(policy) = policy
        && matches!(policy.fork_runs, crate::policy::ForkRuns::Off)
    {
        return Some(FORK_RUNS_OFF);
    }
    match run.gate_status {
        GateStatus::Expired | GateStatus::Cancelled => Some(NOT_APPROVED),
        _ => None,
    }
}

/// The check run that says on commit `sha` why a rerun was refused.
#[must_use]
pub fn rerun_check_run(sha: &str, message: &str) -> CheckRun {
    CheckRun {
        name: RERUN_CHECK.to_owned(),
        head_sha: sha.to_owned(),
        ..decided_check_run("", 0, message)
    }
}

/// Says on the commit of `run` why a rerun was refused, updating the
/// rerun check run that is there or making it. Best effort.
pub async fn say_rerun_refused(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    message: &str,
) {
    let check = rerun_check_run(&run.sha, message);
    let sent = async {
        let listed = tokens
            .send(repo.installation_id, |token| {
                github::check_runs_named(
                    &settings.github_api,
                    &repo.full_name,
                    &run.sha,
                    RERUN_CHECK,
                    token,
                )
            })
            .await
            .map_err(|err| err.to_string())?;
        let existing = github::own_check_run(&listed, &settings.app_id)?;
        match existing {
            Some(id) => tokens
                .send(repo.installation_id, |token| {
                    github::update_check_run(
                        &settings.github_api,
                        &repo.full_name,
                        id,
                        token,
                        &check,
                    )
                })
                .await
                .map(|_| ())
                .map_err(|err| err.to_string()),
            None => tokens
                .send(repo.installation_id, |token| {
                    github::create_check_run(&settings.github_api, &repo.full_name, token, &check)
                })
                .await
                .map(|_| ())
                .map_err(|err| err.to_string()),
        }
    }
    .await;
    if let Err(err) = sent {
        console_error!("run {}: say the rerun was refused: {err}", run.id);
    }
}

/// Whether a run still waits for a decision.
#[must_use]
pub const fn waits(run: &RunRow) -> bool {
    matches!(run.gate_status, GateStatus::Pending | GateStatus::Deferred)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(files: Option<Vec<&str>>) -> Card {
        Card {
            run_id: 9,
            run_number: 3,
            workflow: "ci".into(),
            sha: "3333333333333333333333333333333333333333".into(),
            number: 7,
            title: "Fix the `thing` [now](http://evil)".into(),
            author: "mallory".into(),
            association: "FIRST_TIME_CONTRIBUTOR".into(),
            head_repo: Some("mallory/widgets".into()),
            base_ref: "main".into(),
            level: ForkApproval::FirstTimeContributors,
            files: files.map(|f| f.into_iter().map(str::to_owned).collect()),
            jobs: vec![CardJob {
                workflow: "ci".into(),
                job: "test".into(),
                template: Some("ubuntu-latest".into()),
                timeout_minutes: 30,
            }],
            app_url: "https://app.example".into(),
            repo: "octo/widgets".into(),
        }
    }

    #[test]
    fn the_card_asks_with_one_button_on_the_head_sha() {
        let check = approval_check_run(&card(Some(vec!["src/a.rs"])));
        assert_eq!(check.name, "hatchrun / approval: ci #3");
        // Two runs of one workflow at one commit are two cards.
        assert_ne!(approval_check_name("ci", 3), approval_check_name("ci", 4));
        assert_eq!(
            approval_check_name("lint", 5),
            "hatchrun / approval: lint #5"
        );
        // The card is tied to its run by the id, which adoption reads.
        assert_eq!(check.external_id, "hatchrun-ci-approval:9");
        assert_eq!(approval_external_id(9), check.external_id);
        assert_eq!(check.status, "completed");
        assert_eq!(check.conclusion.as_deref(), Some("action_required"));
        assert_eq!(check.head_sha, "3333333333333333333333333333333333333333");
        assert_eq!(check.actions.len(), 1);
        assert_eq!(check.actions[0].label, "Approve and run");
        assert_eq!(check.actions[0].identifier, "approve-fork-run");
        // GitHub's limits on a button.
        assert!(check.actions[0].label.chars().count() <= 20);
        assert!(check.actions[0].identifier.chars().count() <= 20);
        assert!(check.actions[0].description.chars().count() <= 40);
    }

    #[test]
    fn the_card_names_the_pull_request_the_author_the_head_and_the_jobs() {
        let summary = approval_check_run(&card(Some(vec!["src/a.rs"])))
            .output
            .unwrap()
            .summary;
        assert!(summary.contains("#7"));
        assert!(summary.contains("mallory"));
        assert!(summary.contains("first\\_time\\_contributor"));
        assert!(summary.contains("mallory/widgets"));
        assert!(summary.contains("3333333333333333333333333333333333333333"));
        assert!(summary.contains("`ci` / `test` on `ubuntu-latest`, 30 min"));
        assert!(summary.contains("has not contributed to this repository before"));
        assert!(summary.contains("no secrets, no write token, no OIDC token"));
        assert!(!summary.contains("changes `.hatchrun/config.yml`"));
    }

    #[test]
    fn a_change_to_the_config_gets_its_own_line_with_the_diff() {
        let summary = approval_check_run(&card(Some(vec![".hatchrun/config.yml", "a"])))
            .output
            .unwrap()
            .summary;
        assert!(summary.contains("The pull request changes `.hatchrun/config.yml`."));
        assert!(summary.contains("https://github.com/octo/widgets/pull/7/files"));
        assert!(changes_config(&[".hatchrun/config.yml".to_owned()]));
        assert!(!changes_config(&["docs/.hatchrun/config.yml".to_owned()]));
    }

    #[test]
    fn an_unread_file_list_points_at_the_pull_request() {
        let summary = approval_check_run(&card(None)).output.unwrap().summary;
        assert!(summary.contains("not read"));
        assert!(!summary.contains("changes `.hatchrun/config.yml`"));
    }

    #[test]
    fn text_from_the_pull_request_cannot_make_markup() {
        let summary = approval_check_run(&card(None)).output.unwrap().summary;
        assert!(!summary.contains("[now](http://evil)"));
        assert!(summary.contains("\\[now\\]\\(http"));
    }

    #[test]
    fn a_deleted_fork_is_named_as_one() {
        let mut gone = card(None);
        gone.head_repo = None;
        let summary = approval_check_run(&gone).output.unwrap().summary;
        assert!(summary.contains("a deleted fork"));
    }

    fn row(trust: &str, gate: &str) -> RunRow {
        serde_json::from_value(serde_json::json!({
            "id": 9, "run_number": 1, "run_attempt": 1, "tenant_id": "t1",
            "repo_id": 7, "workflow": "ci", "delivery": "d", "event": "pull_request",
            "git_ref": "refs/pull/4/merge", "sha": "abc", "actor": "a",
            "status": "completed", "conclusion": "cancelled", "created_at": 0,
            "completed_at": 1, "retention_days": 30, "pull_number": 4,
            "trust": trust, "author_login": "a", "author_association": "",
            "gate_status": gate
        }))
        .unwrap()
    }

    #[test]
    fn a_rerun_of_a_fork_run_that_was_never_approved_is_refused_with_its_reason() {
        let on = RepoPolicy::default_for(false);
        for gate in ["expired", "cancelled"] {
            let reason = rerun_refusal(&row("fork", gate), Some(&on)).unwrap();
            assert!(reason.contains("not approved"), "{gate}");
            assert!(reason.contains("Push a new commit"), "{gate}");
            assert!(!reason.contains("/approve-run"), "{gate}");
            // It does not depend on the repository row: without the
            // policy the run row alone decides.
            assert_eq!(
                rerun_refusal(&row("fork", gate), None),
                Some(reason),
                "{gate}"
            );
        }
        // An approved run keeps working, as does one that never waited,
        // and a waiting one has no job to rerun.
        for gate in ["approved", "none", "pending", "deferred"] {
            assert_eq!(rerun_refusal(&row("fork", gate), Some(&on)), None, "{gate}");
            assert_eq!(rerun_refusal(&row("fork", gate), None), None, "{gate}");
        }
        // The repository's own code has no approval to lack.
        assert_eq!(rerun_refusal(&row("trusted", "none"), Some(&on)), None);
        assert_eq!(rerun_refusal(&row("trusted", "cancelled"), Some(&on)), None);
    }

    #[test]
    fn a_rerun_of_any_fork_run_is_refused_when_fork_runs_are_off() {
        let off = RepoPolicy::default_for(true);
        for gate in ["approved", "none", "expired"] {
            assert_eq!(
                rerun_refusal(&row("fork", gate), Some(&off)),
                Some("Fork runs are off for this repository"),
                "{gate}"
            );
        }
        assert_eq!(rerun_refusal(&row("trusted", "none"), Some(&off)), None);
    }

    #[test]
    fn a_refusal_is_one_neutral_check_run_without_a_button() {
        let check = rerun_check_run("abc", NOT_APPROVED);
        assert_eq!(check.name, "hatchrun / rerun");
        assert_eq!(check.head_sha, "abc");
        assert_eq!(check.status, "completed");
        assert_eq!(check.conclusion.as_deref(), Some("neutral"));
        assert_eq!(check.output.unwrap().summary, NOT_APPROVED);
        assert_eq!(check.actions.len(), 0);
    }

    fn waiting_row(gate: &str, check: Option<u64>, pull: Option<u64>) -> RunRow {
        let mut value = serde_json::json!({
            "id": 9, "run_number": 3, "run_attempt": 1, "tenant_id": "t1",
            "repo_id": 7, "workflow": "ci", "delivery": "d", "event": "pull_request",
            "git_ref": "refs/pull/4/merge", "sha": "abc", "actor": "a",
            "status": "queued", "conclusion": null, "created_at": 0,
            "completed_at": null, "retention_days": 30, "pull_number": pull,
            "trust": "fork", "author_login": "a", "author_association": "",
            "gate_status": gate, "gate_check_run_id": check, "gate_by": "maintainer"
        });
        value["gate_at"] = 5.into();
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn only_a_waiting_pull_request_run_without_a_card_needs_one() {
        assert!(needs_card(&waiting_row("pending", None, Some(4))));
        assert!(needs_card(&waiting_row("deferred", None, Some(4))));
        // Recorded already, decided, or not a pull request's: no card.
        assert!(!needs_card(&waiting_row("pending", Some(5), Some(4))));
        assert!(!needs_card(&waiting_row("approved", None, Some(4))));
        assert!(!needs_card(&waiting_row("cancelled", None, Some(4))));
        assert!(!needs_card(&waiting_row("pending", None, None)));
    }

    #[test]
    fn a_card_made_for_a_run_decided_meanwhile_is_settled_to_its_state() {
        let said = |gate: &str| decided_message(&waiting_row(gate, None, Some(4)));
        assert_eq!(said("approved").as_deref(), Some("Approved by maintainer"));
        assert_eq!(said("cancelled").as_deref(), Some("Rejected by maintainer"));
        assert_eq!(said("expired").as_deref(), Some("Approval expired"));
        // A run that still waits keeps its card as made.
        assert_eq!(said("pending"), None);
        assert_eq!(said("deferred"), None);
        assert_eq!(said("none"), None);
        let mut superseded = waiting_row("cancelled", None, Some(4));
        superseded.gate_by = None;
        assert_eq!(decided_message(&superseded).as_deref(), Some("Cancelled"));
        // What replaces the card has no button.
        let check = decided_check_run("ci", 3, "Approved by maintainer");
        assert_eq!(check.actions.len(), 0);
        assert_eq!(check.name, "hatchrun / approval: ci #3");
    }

    #[test]
    fn a_count_saturates() {
        assert_eq!(count(3), 3);
        assert_eq!(count(usize::MAX), u32::MAX);
    }

    #[test]
    fn a_waiting_card_has_no_button() {
        let check = waiting_check_run(&card(None));
        assert_eq!(check.actions.len(), 0);
        assert_eq!(check.conclusion.as_deref(), Some("neutral"));
        assert_eq!(
            check.output.unwrap().title,
            "Waiting for a free approval slot"
        );
    }

    #[test]
    fn a_decided_card_says_who_and_loses_its_button() {
        let check = decided_check_run("ci", 3, &approved_message("maintainer"));
        assert_eq!(check.actions.len(), 0);
        assert_eq!(check.conclusion.as_deref(), Some("neutral"));
        assert_eq!(check.output.unwrap().summary, "Approved by maintainer");
        assert_eq!(rejected_message("x"), "Rejected by x");
        // An update leaves the commit to GitHub.
        assert_eq!(check.head_sha, "");
    }

    #[test]
    fn a_long_file_list_is_cut() {
        let names: Vec<String> = (0..30).map(|n| format!("f{n}")).collect();
        let c = Card {
            files: Some(names),
            ..card(None)
        };
        let text = files_text(&c).unwrap();
        assert!(text.contains("- and 10 more"));
        assert_eq!(files_text(&card(Some(vec![]))), None);
        assert_eq!(files_text(&card(None)), None);
    }

    fn plan(runs_on: &str) -> Plan {
        let yaml = format!(
            "jobs: {{ big: {{ runs-on: {runs_on}, timeout-minutes: 120, steps: [] }} }}\n\
             workflows: {{ ci: {{ on: {{ pull_request: {{}} }}, jobs: [big] }} }}\n"
        );
        let config = hatchrun_workflow::parse(&yaml).unwrap();
        hatchrun_workflow::compile(&config, "ci", "refs/heads/main").unwrap()
    }

    #[test]
    fn a_template_over_four_vcpus_is_refused_for_a_fork() {
        assert_eq!(oversized_job(&plan("ubuntu-latest")), None);
        let big = hatchrun_workflow::templates::TEMPLATES
            .iter()
            .find(|template| template.vcpu > FORK_MAX_VCPU)
            .expect("a template over the limit");
        let message = oversized_job(&plan(big.labels[0])).expect("over the limit");
        assert!(message.contains("at most 4 vCPUs"));
        assert!(template_vcpu("ubuntu-latest").is_some());
        assert_eq!(template_vcpu("nonsense"), None);
    }

    #[test]
    fn the_card_shows_the_timeout_the_run_will_have() {
        let jobs = card_jobs(&plan("ubuntu-latest"), 30);
        assert_eq!(jobs[0].timeout_minutes, 30);
        let long = card_jobs(&plan("ubuntu-latest"), 360);
        assert_eq!(long[0].timeout_minutes, 120);
    }
}
