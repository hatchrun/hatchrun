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

/// What the approver reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
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
        name: APPROVAL_CHECK.to_owned(),
        head_sha: card.sha.clone(),
        external_id: format!("hatchrun-ci-approval:{}", card.sha),
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
pub fn decided_check_run(message: &str) -> CheckRun {
    CheckRun {
        name: APPROVAL_CHECK.to_owned(),
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

/// The card of the waiting `runs` of one pull request and commit at their
/// repository `repo`, with the jobs of every one of them and `files` as
/// read: what the card lists is exactly what a click approves.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn card_for(
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
    runs: &[RunRow],
    level: ForkApproval,
    files: Option<Vec<String>>,
) -> worker::Result<Card> {
    let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
    let jobs = store::jobs_of_runs(db, &ids)
        .await?
        .into_iter()
        .filter_map(|row| {
            let run = runs.iter().find(|run| run.id == row.run_id)?;
            Some(CardJob {
                workflow: run.workflow.clone(),
                job: row.name,
                template: row.template,
                timeout_minutes: row.timeout_minutes,
            })
        })
        .collect();
    let Some(run) = runs.first() else {
        return Err(worker::Error::RustError("a card of no runs".to_owned()));
    };
    Ok(Card {
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

/// Updates the approval check run of `run`, when it has one. Best
/// effort: the decision is saved already.
pub async fn update_card(
    tokens: &Tokens<'_, Kv, App, Counted>,
    settings: &Settings,
    repo: &RepoRow,
    run: &RunRow,
    check: &CheckRun,
) {
    let Some(id) = run.gate_check_run_id else {
        return;
    };
    let sent = tokens
        .send(repo.installation_id, |token| {
            github::update_check_run(&settings.github_api, &repo.full_name, id, token, check)
        })
        .await;
    match sent {
        Ok(response) if response.ok() => {}
        Ok(response) => console_error!(
            "run {}: update approval check run {id}: GitHub answered {}",
            run.id,
            response.status
        ),
        Err(err) => console_error!("run {}: update approval check run {id}: {err}", run.id),
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
    let mut carded: Vec<u64> = Vec::new();
    for run in runs {
        match store::fork_approval_held(db, run.id).await {
            Ok(woken) => {
                if let Err(err) = launches::send_wakes(env, &woken).await {
                    console_error!("run {}: wake after approval: {err}", run.id);
                }
            }
            Err(err) => console_error!("run {}: find held jobs: {err}", run.id),
        }
        if let Some(id) = run.gate_check_run_id
            && !carded.contains(&id)
        {
            carded.push(id);
            let check = decided_check_run(&approved_message(by));
            update_card(tokens, settings, repo, run, &check).await;
        }
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
    let mut carded: Vec<u64> = Vec::new();
    for run in runs {
        if let Err(err) = cancel_run(env, run, message).await {
            console_error!("run {}: cancel after {message}: {err}", run.id);
        }
        if let Some(id) = run.gate_check_run_id
            && !carded.contains(&id)
        {
            carded.push(id);
            update_card(tokens, settings, repo, run, &decided_check_run(message)).await;
        }
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
    // The oldest group of each author in each repository: a group the
    // author's cap blocks is skipped and the next author's is taken.
    for run in store::deferred_runs(db, PASS, PER_AUTHOR)
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

/// The most runs of one repository a reconciler pass looks at in the
/// queue of cards that were never made.
const PER_REPO: u32 = 5;

/// The most groups of one author in one repository a reconciler pass
/// looks at in the queue of deferred runs.
const PER_AUTHOR: u32 = 2;

/// How long a waiting run may go without its check run before the
/// reconciler makes it: the webhook is likely still at it before that.
const CARD_GRACE_MS: u64 = 60_000;

/// What making the approval check run of a waiting run comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStep {
    /// Every run of the group is on the recorded check run.
    Done,
    /// The group has a check run, and a run that joined after it is not
    /// on it: put the run on it and list its jobs.
    Join(u64),
    /// A check run of the right name is on the commit already, made by
    /// an earlier attempt that never recorded it: record its id.
    Adopt(u64),
    /// Make the check run.
    Create,
}

/// Decides how to get the approval check run: the id recorded on the
/// group (a run that joined later is put on it), else one an earlier
/// attempt left on the commit, else a new one.
#[must_use]
pub const fn card_step(recorded: Option<u64>, on_commit: Option<u64>, all_on_it: bool) -> CardStep {
    match (recorded, on_commit) {
        (Some(id), _) if !all_on_it => CardStep::Join(id),
        (Some(_), _) => CardStep::Done,
        (None, Some(id)) => CardStep::Adopt(id),
        (None, None) => CardStep::Create,
    }
}

/// Makes sure the waiting runs of `run`'s pull request and commit have
/// one approval check run, with its id recorded on all of them, so the
/// Approve button exists, lists every run it will approve, and its click
/// finds them. Safe to repeat, from the webhook and from the
/// reconciler: it adopts a card of ours an earlier attempt made and did
/// not record, and only an undecided one (see [`github::adoptable_card`]).
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
    let Some(number) = run.pull_number else {
        return Ok(());
    };
    let group = store::gate_group(db, repo.id, number, &run.sha)
        .await
        .map_err(|err| err.to_string())?;
    let recorded = group.iter().find_map(|member| member.gate_check_run_id);
    let all_on_it = group
        .iter()
        .all(|member| member.gate_check_run_id == recorded);
    if group.is_empty() || card_step(recorded, None, all_on_it) == CardStep::Done {
        return Ok(());
    }
    // A run that joined a card that exists: on it, and listed on it, so
    // a click never approves a run the card did not show.
    if let CardStep::Join(id) = card_step(recorded, None, all_on_it) {
        store::set_gate_check(db, repo.id, number, &run.sha, id)
            .await
            .map_err(|err| err.to_string())?;
        let group = store::gate_group(db, repo.id, number, &run.sha)
            .await
            .map_err(|err| err.to_string())?;
        let files = changed_files(tokens, settings, repo, number).await;
        let card = card_for(db, settings, repo, &group, policy.fork_approval, files)
            .await
            .map_err(|err| err.to_string())?;
        let check = if group
            .iter()
            .any(|member| member.gate_status == GateStatus::Pending)
        {
            approval_check_run(&card)
        } else {
            waiting_check_run(&card)
        };
        let mut carded = run.clone();
        carded.gate_check_run_id = Some(id);
        update_card(tokens, settings, repo, &carded, &check).await;
        return Ok(());
    }
    let listed = tokens
        .send(repo.installation_id, |token| {
            github::check_runs_named(
                &settings.github_api,
                &repo.full_name,
                &run.sha,
                APPROVAL_CHECK,
                token,
            )
        })
        .await
        .map_err(|err| err.to_string())?;
    let on_commit = github::adoptable_card(&listed, &settings.app_id)?;
    match card_step(recorded, on_commit, all_on_it) {
        CardStep::Done | CardStep::Join(_) => {}
        CardStep::Adopt(id) => store::set_gate_check(db, repo.id, number, &run.sha, id)
            .await
            .map_err(|err| err.to_string())?,
        CardStep::Create => {
            let pending = group
                .iter()
                .any(|member| member.gate_status == GateStatus::Pending);
            let files = if pending {
                changed_files(tokens, settings, repo, number).await
            } else {
                None
            };
            let card = card_for(db, settings, repo, &group, policy.fork_approval, files)
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
            store::set_gate_check(db, repo.id, number, &run.sha, id)
                .await
                .map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

/// Decides what the deferred runs of `run`'s pull request and commit
/// become now, all together (one card covers them): asks for their
/// approval when a slot is free and the author is under the cap, runs
/// them when no approval is needed, ends them when the repository
/// turned fork runs off, and leaves them otherwise. Returns how many
/// changed.
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
    let group: Vec<RunRow> = store::gate_group(db, repo.id, number, &run.sha)
        .await
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|member| member.gate_status == GateStatus::Deferred)
        .collect();
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
            let off = "Fork runs are off for this repository";
            after_end(env, settings, tokens, &repo, &ended, off).await;
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
    let changed = count(admitted.len());
    if let Some(by) = by {
        after_approval(env, db, settings, tokens, &repo, &admitted, by).await;
        return Ok(changed);
    }
    let Some(first) = admitted.first() else {
        return Ok(0);
    };
    if first.gate_check_run_id.is_some() {
        // The waiting card becomes the approval card, listing every run
        // the click will approve.
        let everyone = store::gate_group(db, repo.id, number, &run.sha)
            .await
            .map_err(|err| err.to_string())?;
        let files = changed_files(tokens, settings, &repo, number).await;
        let card = card_for(db, settings, &repo, &everyone, policy.fork_approval, files)
            .await
            .map_err(|err| err.to_string())?;
        update_card(tokens, settings, &repo, first, &approval_check_run(&card)).await;
    } else if let Err(err) = ensure_card(tokens, db, settings, &repo, first, &policy).await {
        console_error!("run {}: make the approval check run: {err}", first.id);
    }
    Ok(changed)
}

/// What a rerun of a fork run whose approval ended is told.
pub const RERUN_REFUSED: &str = "This fork run was not approved. Push a new commit to ask \
     again, or a maintainer can use /approve-run once available.";

/// Whether a rerun of `run` is refused: a fork run whose approval ended
/// without being given (expired, rejected, superseded or its pull
/// request closed). Its jobs never ran, and resetting them would leave
/// jobs nothing ends. A rerun of an approved run keeps its approval,
/// since the code is the same and a rerun already needs write access.
#[must_use]
pub const fn rerun_refused(run: &RunRow) -> bool {
    matches!(run.trust, Trust::Fork)
        && matches!(run.gate_status, GateStatus::Expired | GateStatus::Cancelled)
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
        assert_eq!(check.name, "hatchrun / approval");
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
    fn a_rerun_of_a_fork_run_that_was_never_approved_is_refused() {
        for gate in ["expired", "cancelled"] {
            assert!(rerun_refused(&row("fork", gate)), "{gate}");
        }
        // An approved run keeps working, as does one that never waited,
        // and a waiting one has no job to rerun.
        for gate in ["approved", "none", "pending", "deferred"] {
            assert!(!rerun_refused(&row("fork", gate)), "{gate}");
        }
        // The repository's own code has no approval to lack.
        assert!(!rerun_refused(&row("trusted", "none")));
        assert!(RERUN_REFUSED.contains("not approved"));
        assert!(RERUN_REFUSED.contains("/approve-run"));
    }

    #[test]
    fn a_count_saturates() {
        assert_eq!(count(3), 3);
        assert_eq!(count(usize::MAX), u32::MAX);
    }

    #[test]
    fn a_card_is_recorded_adopted_or_made() {
        assert_eq!(card_step(Some(5), None, true), CardStep::Done);
        assert_eq!(card_step(Some(5), Some(9), true), CardStep::Done);
        // A run that joined the group after the card was made is put on
        // the existing card, not left off it.
        assert_eq!(card_step(Some(5), None, false), CardStep::Join(5));
        assert_eq!(card_step(Some(5), Some(9), false), CardStep::Join(5));
        // A delivery that made the check run and stopped before it
        // recorded the id: the retry adopts it instead of making a
        // second one whose button no run knows.
        assert_eq!(card_step(None, Some(9), true), CardStep::Adopt(9));
        assert_eq!(card_step(None, None, true), CardStep::Create);
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
        let check = decided_check_run(&approved_message("maintainer"));
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
