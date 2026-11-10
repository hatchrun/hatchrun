// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs.

use hatchrun_cloud::db::{Db, params};
use serde::{Deserialize, Serialize};
use worker::Result;

use super::with_columns;
use crate::trust::Trust;

/// Where a run stands with the approval its pull request needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    /// The run never waited for an approval.
    #[serde(rename = "none")]
    NotGated,
    /// It waits for a maintainer.
    Pending,
    /// It waits for a free approval slot, and no card is shown yet.
    Deferred,
    /// A maintainer approved it.
    Approved,
    /// Nobody decided in time.
    Expired,
    /// Rejected, superseded by a newer commit, or its pull request
    /// closed.
    Cancelled,
}

impl GateStatus {
    /// The word the `gate_status` column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotGated => "none",
            Self::Pending => "pending",
            Self::Deferred => "deferred",
            Self::Approved => "approved",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the run still waits for a decision, so its jobs do not
    /// launch.
    #[must_use]
    pub const fn waits(self) -> bool {
        matches!(self, Self::Pending | Self::Deferred)
    }
}

/// A run to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    /// The owning tenant.
    pub tenant_id: String,
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The workflow's file name.
    pub workflow: String,
    /// The delivery that triggered it, or a `schedule:` or `dispatch:`
    /// key.
    pub delivery: String,
    /// `push`, `pull_request`, `schedule` and so on.
    pub event: String,
    /// The git ref.
    pub git_ref: String,
    /// The commit.
    pub sha: String,
    /// The login that caused the event.
    pub actor: String,
    /// The base branch of a pull request, as a short name.
    pub base_ref: Option<String>,
    /// The head commit's subject or the pull request's title, cut to
    /// 200 characters.
    pub message: Option<String>,
    /// The pull request's number, for a run of a pull request.
    pub pull_number: Option<u64>,
    /// The head branch, as a short name.
    pub head_ref: Option<String>,
    /// Whether the run is of the repository's own code or a fork's.
    pub trust: Trust,
    /// The head repository of a fork pull request, by id. `None` for
    /// other runs and for a deleted fork.
    pub head_repo_id: Option<u64>,
    /// The head repository of a fork pull request, `owner/name`.
    pub head_repo_name: Option<String>,
    /// The login of the pull request's author; the actor for other
    /// runs.
    pub author_login: String,
    /// GitHub's `author_association` of the pull request's author;
    /// empty for other runs.
    pub author_association: String,
    /// `NotGated`, or `Pending` or `Deferred` for a fork run that needs
    /// an approval.
    pub gate_status: GateStatus,
    /// The pull request's head commit, for a `pull_request_target` run,
    /// whose own sha is the base branch's. An approval is by this.
    pub head_sha: Option<String>,
    /// What the planner found in the run's workflow, one finding to a
    /// line.
    pub lints: Option<String>,
    /// The issue or comment the run is for, for the runs of the
    /// `issues` and `issue_comment` events.
    pub chat: Option<Chat>,
    /// When the run is recorded.
    pub now: u64,
}

/// What ties a run to an issue, a pull request or a chat command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    /// The issue or pull request the event is about.
    pub issue_number: u64,
    /// The command the run answers, for a run started by one.
    pub command: Option<CommentCommand>,
}

/// The chat command a run was started by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentCommand {
    /// The comment the command was written in.
    pub comment_id: u64,
    /// The command's name, without the slash.
    pub name: String,
    /// Whether the run's end reacts with `rocket` and `confused`
    /// instead of `+1` and `-1`.
    pub rocket: bool,
}

/// A recorded run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunRow {
    /// The run's identity.
    pub id: u64,
    /// Counts runs of one workflow in one repository, from 1.
    pub run_number: u64,
    /// Starts at 1; a re-run adds one.
    pub run_attempt: u32,
    /// The owning tenant.
    pub tenant_id: String,
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The workflow's file name.
    pub workflow: String,
    /// The delivery that triggered it.
    pub delivery: String,
    /// The event.
    pub event: String,
    /// The git ref.
    pub git_ref: String,
    /// The commit.
    pub sha: String,
    /// The login that caused the event.
    pub actor: String,
    /// The base branch of a pull request, as a short name.
    pub base_ref: Option<String>,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success`, `failure`, `cancelled` or `skipped`, once completed.
    pub conclusion: Option<String>,
    /// When the run was recorded.
    pub created_at: u64,
    /// When the run finished.
    pub completed_at: Option<u64>,
    /// How many days the run is kept, as its tenant's retention was when
    /// the run was recorded. It names the tier of the run's R2 keys.
    pub retention_days: u32,
    /// The head commit's subject or the pull request's title.
    pub message: Option<String>,
    /// The pull request's number. Runs recorded before pull requests
    /// were kept have none.
    pub pull_number: Option<u64>,
    /// The pull request's head branch, with the number.
    pub head_ref: Option<String>,
    /// The pull request's title, as the `pull_request` webhook last
    /// said it.
    pub pull_title: Option<String>,
    /// Whose code the run executes.
    pub trust: Trust,
    /// The head repository of a fork pull request, `owner/name`.
    pub head_repo_name: Option<String>,
    /// The login of the pull request's author; the actor otherwise.
    pub author_login: String,
    /// GitHub's `author_association` of the pull request's author.
    pub author_association: String,
    /// Where the run stands with the approval it needed.
    pub gate_status: GateStatus,
    /// Who approved it.
    pub gate_by: Option<String>,
    /// When it was decided.
    pub gate_at: Option<u64>,
    /// The check run that carries the approve button.
    pub gate_check_run_id: Option<u64>,
    /// Whether that check run could not be brought up to date and the
    /// reconciler has to update it again.
    #[serde(default)]
    pub gate_card_dirty: bool,
    /// The pull request's head commit, for a `pull_request_target` run.
    pub head_sha: Option<String>,
    /// What the planner found in the run's workflow, one finding to a
    /// line.
    pub lints: Option<String>,
}

impl RunRow {
    /// The commit an approval of the run is about: the pull request's
    /// head for a `pull_request_target` run, the run's own commit for
    /// others.
    #[must_use]
    pub fn pr_sha(&self) -> &str {
        self.head_sha.as_deref().unwrap_or(&self.sha)
    }

    /// Whether the run is of a `pull_request_target` event.
    #[must_use]
    pub fn event_name_is_target(&self) -> bool {
        self.event == "pull_request_target"
    }

    /// The branch the run is on: that of a push, the head branch of a
    /// pull request, none for a tag. The same as [`BRANCH`].
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        if let Some(name) = self.git_ref.strip_prefix("refs/heads/") {
            Some(name)
        } else if self.git_ref.starts_with("refs/pull/") {
            self.head_ref.as_deref()
        } else {
            None
        }
    }
}

/// The columns of a [`RunRow`].
pub(super) const RUN_COLUMNS: &str = "id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, \
     event, ref AS git_ref, sha, actor, base_ref, status, conclusion, created_at, \
     completed_at, (expires_at - created_at) / 86400000 AS retention_days, message, \
     pull_number, head_ref, \
     (SELECT title FROM ci_pulls WHERE repo_id = ci_runs.repo_id \
      AND number = ci_runs.pull_number) AS pull_title, \
     trust, head_repo_name, author_login, author_association, gate_status, gate_by, gate_at, \
     gate_check_run_id, gate_card_dirty, head_sha, lints";

/// The run of delivery `$1` and workflow `$2`, where `{columns}` is
/// [`RUN_COLUMNS`].
const RUN_FOR_DELIVERY: &str =
    "SELECT {columns} FROM ci_runs WHERE delivery = $1 AND workflow = $2";

/// Locks the row of repository `$1`, so runs of one repository are
/// numbered one at a time. The statement after it in the transaction
/// reads what the holder before it committed.
const LOCK_REPO: &str = "SELECT id FROM ci_repos WHERE id = $1 FOR NO KEY UPDATE";

/// Records a run and takes its number in one statement, run after
/// [`LOCK_REPO`]. Binds `$1` tenant, `$2` repository, `$3` workflow,
/// `$4` delivery, `$5` event, `$6` ref, `$7` sha, `$8` actor, `$9`
/// now, `$10` the pull request's base branch, `$11` the message, `$12` the
/// pull request's number, `$13` its head branch, `$14` the trust, `$15` and
/// `$16` the head repository's id and name, `$17` the author, `$18` the
/// author's association, `$19` the gate, `$20` its status, `$21` the
/// pull request's head commit, `$22` the planner's findings, `$23` the
/// issue number, `$24` the comment id, `$25` the command's name and
/// `$26` its reaction set. The run expires after its tenant's retention tier (30 or 90 days), read
/// in the same statement. The number is taken only when no run of this
/// delivery and workflow exists, so a duplicate leaves the counter
/// alone, and the statement returns no row for it.
const INSERT_RUN: &str = "WITH n AS (\
       INSERT INTO ci_run_numbers (repo_id, workflow, last) \
       SELECT $2, $3, 1 \
       WHERE NOT EXISTS (SELECT 1 FROM ci_runs WHERE delivery = $4 AND workflow = $3) \
       ON CONFLICT (repo_id, workflow) DO UPDATE SET last = ci_run_numbers.last + 1 \
       RETURNING last) \
     INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, event, ref, sha, \
       actor, base_ref, message, pull_number, head_ref, status, created_at, updated_at, \
       expires_at, trust, head_repo_id, head_repo_name, author_login, author_association, \
       gate, gate_status, head_sha, lints, issue_number, comment_id, command_name, \
       command_reaction) \
     SELECT $1, $2, $3, n.last, $4, $5, $6, $7, $8, $10::text, $11::text, $12::bigint, $13::text, 'queued', $9, $9, \
       $9 + (SELECT CASE WHEN retention_days > 30 THEN 90 ELSE 30 END * 86400000::bigint \
         FROM tenants WHERE id = $1), \
       $14::text, $15::bigint, $16::text, $17::text, $18::text, $19::text, $20::text, $21::text, $22::text, $23::bigint, $24::bigint, $25::text, $26::text FROM n \
     RETURNING id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, event, \
       ref AS git_ref, sha, actor, base_ref, status, conclusion, created_at, \
       completed_at, (expires_at - created_at) / 86400000 AS retention_days, message, \
       pull_number, head_ref, \
       (SELECT title FROM ci_pulls WHERE repo_id = ci_runs.repo_id \
        AND number = ci_runs.pull_number) AS pull_title, \
       trust, head_repo_name, author_login, author_association, gate_status, gate_by, gate_at, \
       gate_check_run_id, gate_card_dirty, head_sha, lints";

/// Run `$1`, where `{columns}` is [`RUN_COLUMNS`].
const RUN: &str = "SELECT {columns} FROM ci_runs WHERE id = $1";

/// Gives run `$1` the title `$2`.
const SET_RUN_MESSAGE: &str = "UPDATE ci_runs SET message = $2 WHERE id = $1";

/// Moves run `$1` from queued to in progress at `$2`.
const RUN_STARTED: &str = "UPDATE ci_runs SET status = 'in_progress', updated_at = $2 WHERE id = $1 AND status = 'queued'";

/// Opens run `$1` for attempt `$2` at `$3`.
const REOPEN_RUN_AT: &str = "UPDATE ci_runs SET status = 'in_progress', conclusion = NULL, \
     completed_at = NULL, run_attempt = $2, updated_at = $3 WHERE id = $1";

/// Completes attempt `$4` of run `$1` with conclusion `$2` at `$3`. A fork
/// run that still waits for its approval (it was cancelled, or ended
/// some other way) has its gate ended with it: it frees its approval
/// slot, its approve button stops working and its card is marked to be
/// settled.
const FINISH_RUN: &str = "UPDATE ci_runs SET status = 'completed', conclusion = $2, \
     completed_at = $3, updated_at = $3, \
     gate_card_dirty = CASE WHEN gate_status IN ('pending', 'deferred') \
       THEN gate_check_run_id IS NOT NULL ELSE gate_card_dirty END, \
     gate_at = CASE WHEN gate_status IN ('pending', 'deferred') THEN $3 ELSE gate_at END, \
     gate_status = CASE WHEN gate_status IN ('pending', 'deferred') \
       THEN 'cancelled' ELSE gate_status END \
     WHERE id = $1 AND run_attempt = $4";

/// Records `run` with the next number of its workflow in its repository.
/// Returns `None` when a run of this delivery and workflow exists,
/// which then has not used up a number.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn insert_run(db: &Db, run: &NewRun) -> Result<Option<RunRow>> {
    let command = run.chat.as_ref().and_then(|chat| chat.command.as_ref());
    db.first_locked(
        (LOCK_REPO, &params![run.repo_id]),
        INSERT_RUN,
        &params![
            &run.tenant_id,
            run.repo_id,
            &run.workflow,
            &run.delivery,
            &run.event,
            &run.git_ref,
            &run.sha,
            &run.actor,
            run.now,
            &run.base_ref,
            &run.message,
            run.pull_number,
            &run.head_ref,
            run.trust.as_str(),
            run.head_repo_id,
            &run.head_repo_name,
            &run.author_login,
            &run.author_association,
            if run.gate_status == GateStatus::NotGated {
                "none"
            } else {
                "fork_approval"
            },
            run.gate_status.as_str(),
            &run.head_sha,
            &run.lints,
            run.chat.as_ref().map(|chat| chat.issue_number),
            command.map(|command| command.comment_id),
            command.map(|command| &command.name),
            command.map(|command| if command.rocket { "rocket" } else { "default" })
        ],
    )
    .await
}

/// The run that `delivery` made for `workflow`, which a redelivery
/// finds again when the first one stopped before the run started.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run_for_delivery(db: &Db, delivery: &str, workflow: &str) -> Result<Option<RunRow>> {
    db.first(
        &with_columns(RUN_FOR_DELIVERY, RUN_COLUMNS),
        &params![delivery, workflow],
    )
    .await
}

/// Gives run `run_id` the title its workflow's `run-name` made.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_run_message(db: &Db, run_id: u64, message: &str) -> Result<()> {
    db.run(SET_RUN_MESSAGE, &params![run_id, message]).await?;
    Ok(())
}

/// Run `run_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run(db: &Db, run_id: u64) -> Result<Option<RunRow>> {
    db.first(&with_columns(RUN, RUN_COLUMNS), &params![run_id])
        .await
}

/// How many runs of the `issue_comment` event of issue or pull request
/// `$2` of repository `$1` are not over.
const ACTIVE_COMMENT_RUNS: &str = "SELECT count(*)::bigint AS n FROM ci_runs \
     WHERE repo_id = $1 AND issue_number = $2 AND event = 'issue_comment' \
     AND status <> 'completed'";

/// The comment and reaction set of run `$1`, when a chat command started
/// it.
const RUN_COMMAND: &str = "SELECT comment_id, command_reaction = 'rocket' AS rocket \
     FROM ci_runs WHERE id = $1 AND comment_id IS NOT NULL";

/// How many runs of the `issue_comment` event of issue or pull request
/// `number` are not over. The cap on them is
/// [`crate::chatops::MAX_ACTIVE_PER_ISSUE`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn active_comment_runs(db: &Db, repo_id: u64, number: u64) -> Result<u64> {
    #[derive(Deserialize)]
    struct Count {
        n: u64,
    }
    let count: Option<Count> = db
        .first(ACTIVE_COMMENT_RUNS, &params![repo_id, number])
        .await?;
    Ok(count.map_or(0, |count| count.n))
}

/// The comment a run's chat command was written in and whether it
/// reacts with `rocket`, `None` for a run no command started.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run_command(db: &Db, run_id: u64) -> Result<Option<(u64, bool)>> {
    #[derive(Deserialize)]
    struct Command {
        comment_id: u64,
        rocket: bool,
    }
    let found: Option<Command> = db.first(RUN_COMMAND, &params![run_id]).await?;
    Ok(found.map(|found| (found.comment_id, found.rocket)))
}

/// What a run's status is for a list: the filter's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunStatus {
    /// Recorded, nothing started.
    Queued,
    /// In progress, and no job waits for an approval.
    Running,
    /// In progress, and a job waits for an approval.
    Waiting,
    /// Completed with `success`.
    Success,
    /// Completed with `failure`.
    Failure,
    /// Completed with `cancelled`.
    Cancelled,
}

impl RunStatus {
    /// The state the filter calls `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "success" => Self::Success,
            "failure" => Self::Failure,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// The condition on `ci_runs r` that holds for this state. Fixed
    /// text: nothing the caller sent is in it.
    const fn condition(self) -> &'static str {
        match self {
            Self::Queued => "r.status = 'queued'",
            Self::Running => {
                "(r.status = 'in_progress' AND NOT EXISTS (SELECT 1 FROM ci_jobs j \
                 WHERE j.run_id = r.id AND j.status <> 'completed' AND j.held = 'approval'))"
            }
            Self::Waiting => {
                "(r.status = 'in_progress' AND EXISTS (SELECT 1 FROM ci_jobs j \
                 WHERE j.run_id = r.id AND j.status <> 'completed' AND j.held = 'approval'))"
            }
            Self::Success => "(r.status = 'completed' AND r.conclusion = 'success')",
            Self::Failure => "(r.status = 'completed' AND r.conclusion = 'failure')",
            Self::Cancelled => "(r.status = 'completed' AND r.conclusion = 'cancelled')",
        }
    }
}

/// The branch a run is on: the branch of a push, the head branch of a
/// pull request, nothing for a tag or other refs. For a query that
/// aliases `ci_runs` as `r`.
pub(super) const BRANCH: &str = "CASE WHEN r.ref LIKE 'refs/heads/%' THEN substr(r.ref, 12) \
     WHEN r.ref LIKE 'refs/pull/%' THEN r.head_ref END";

/// Which runs a list holds. Every field narrows the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// The repositories the list may show, by id. An empty list shows
    /// nothing.
    pub repo_ids: Vec<u64>,
    /// The states; empty for all.
    pub states: Vec<RunStatus>,
    /// Repository names, `owner/name`; empty for all.
    pub repos: Vec<String>,
    /// Branches; empty for all.
    pub branches: Vec<String>,
    /// Actor logins; empty for all.
    pub actors: Vec<String>,
    /// Workflow file names; empty for all.
    pub workflows: Vec<String>,
    /// The pull request number, for the runs of that pull request.
    pub pull: Option<u64>,
    /// Text in the workflow, ref, actor or the start of the commit.
    pub text: Option<String>,
    /// Runs recorded at or after this, in Unix milliseconds.
    pub since: u64,
    /// Runs recorded before this, in Unix milliseconds.
    pub until: Option<u64>,
}

/// A run of a list, with the name of its repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListedRun {
    /// The run.
    #[serde(flatten)]
    pub run: RunRow,
    /// The repository, `owner/name`.
    pub repository: String,
}

/// The place after which the next page of a list starts: the sort key
/// of its last run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct After {
    /// The run's creation time.
    pub created_at: u64,
    /// The run's id.
    pub id: u64,
}

/// Escapes `text` for a `LIKE` pattern.
pub(super) fn like(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The `WHERE` of a run list over `ci_runs r` and `ci_repos p`, and
/// its parameters `$1` to `$10`.
fn run_where(filter: &RunFilter) -> (String, Vec<hatchrun_cloud::db::Param>) {
    let mut sql = String::from(
        "p.id = r.repo_id AND p.tenant_id = r.tenant_id \
         AND r.repo_id = ANY (string_to_array($1, ',')::bigint[]) \
         AND r.created_at >= $2 AND ($3::bigint IS NULL OR r.created_at < $3::bigint) \
         AND ($4::text IS NULL OR lower(p.full_name) = ANY (string_to_array(lower($4), ','))) \
         AND ($5::text IS NULL OR ",
    );
    sql.push_str(BRANCH);
    sql.push_str(
        " = ANY (string_to_array($5, ','))) \
         AND ($6::text IS NULL OR lower(r.actor) = ANY (string_to_array(lower($6), ','))) \
         AND ($7::text IS NULL OR r.workflow ILIKE $7 OR r.ref ILIKE $7 OR r.actor ILIKE $7 \
           OR r.sha ILIKE $8::text) \
         AND ($9::text IS NULL OR r.workflow = ANY (string_to_array($9, ','))) \
         AND ($10::bigint IS NULL OR r.pull_number = $10::bigint)",
    );
    if !filter.states.is_empty() {
        let any: Vec<&str> = filter.states.iter().map(|s| s.condition()).collect();
        sql.push_str(" AND (");
        sql.push_str(&any.join(" OR "));
        sql.push(')');
    }
    let ids: Vec<String> = filter.repo_ids.iter().map(u64::to_string).collect();
    let list = |items: &[String]| (!items.is_empty()).then(|| items.join(","));
    let text = filter.text.as_deref().map(like);
    let sha = filter
        .text
        .as_deref()
        .map(|t| like(t).trim_start_matches('%').to_owned());
    let bound = params![
        ids.join(","),
        filter.since,
        filter.until,
        list(&filter.repos),
        list(&filter.branches),
        list(&filter.actors),
        text,
        sha,
        list(&filter.workflows),
        filter.pull
    ];
    (sql, bound)
}

/// The columns of a [`ListedRun`] over `ci_runs r` and `ci_repos p`.
pub(super) const LISTED_COLUMNS: &str = "r.id, r.run_number, r.run_attempt, r.tenant_id, r.repo_id, \
     r.workflow, r.delivery, r.event, r.ref AS git_ref, r.sha, r.actor, r.base_ref, r.status, \
     r.conclusion, r.created_at, r.completed_at, \
     (r.expires_at - r.created_at) / 86400000 AS retention_days, r.message, \
     r.pull_number, r.head_ref, \
     (SELECT title FROM ci_pulls WHERE repo_id = r.repo_id AND number = r.pull_number) \
     AS pull_title, r.trust, r.head_repo_name, r.author_login, r.author_association, \
     r.gate_status, r.gate_by, r.gate_at, r.gate_check_run_id, r.gate_card_dirty, \
     p.full_name AS repository";

/// One page of the runs `filter` holds, newest first, those after
/// `after` when it is set, at most `limit`; and how many runs the
/// filter holds in all.
///
/// Only runs of the tenant that owns the repository now are shown:
/// after a transfer, the history stays with the tenant that owned the
/// repository when the run happened.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn runs_page(
    db: &Db,
    filter: &RunFilter,
    after: Option<After>,
    limit: u32,
) -> Result<(Vec<ListedRun>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let (conditions, bound) = run_where(filter);
    let count = format!("SELECT count(*) AS total FROM ci_runs r, ci_repos p WHERE {conditions}");
    let total = db
        .first::<Total>(&count, &bound)
        .await?
        .map_or(0, |row| row.total);
    let mut bound = bound;
    bound.extend(params![
        after.map(|a| a.created_at),
        after.map(|a| a.id),
        limit
    ]);
    let page = format!(
        "SELECT {LISTED_COLUMNS} FROM ci_runs r, ci_repos p \
         WHERE {conditions} \
         AND ($11::bigint IS NULL OR (r.created_at, r.id) < ($11::bigint, $12::bigint)) \
         ORDER BY r.created_at DESC, r.id DESC LIMIT $13"
    );
    let rows = db.all(&page, &bound).await?;
    Ok((rows, total))
}

/// The runs `ids` name, with their repositories' names, for the runs of
/// the repositories' present tenants only. The order is unspecified.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn listed_runs(db: &Db, ids: &[u64]) -> Result<Vec<ListedRun>> {
    let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
    db.all(
        &format!(
            "SELECT {LISTED_COLUMNS} FROM ci_runs r, ci_repos p \
             WHERE p.id = r.repo_id AND p.tenant_id = r.tenant_id \
             AND r.id = ANY (string_to_array($1, ',')::bigint[])"
        ),
        &params![ids.join(",")],
    )
    .await
}

/// What one bucket of the activity chart holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Bucket {
    /// Runs recorded in it.
    pub started: u64,
    /// Runs that completed in it with `success`.
    pub passed: u64,
    /// Runs that completed in it with `failure`.
    pub failed: u64,
}

/// The runs of `repo_ids` (and of the repository `repo`, `owner/name`,
/// when given) per bucket of `bucket_ms` milliseconds, `count` buckets
/// from `start`. A run counts as started at its creation and as passed
/// or failed at its completion.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn activity(
    db: &Db,
    repo_ids: &[u64],
    repo: Option<&str>,
    start: u64,
    bucket_ms: u64,
    count: u32,
) -> Result<Vec<Bucket>> {
    #[derive(Deserialize)]
    struct Row {
        kind: String,
        bucket: u64,
        n: u64,
    }
    let scope = "r.repo_id = ANY (string_to_array($1, ',')::bigint[]) \
         AND ($2::text IS NULL OR lower(p.full_name) = lower($2::text)) \
         AND p.id = r.repo_id AND p.tenant_id = r.tenant_id";
    let sql = format!(
        "SELECT 'started' AS kind, ((r.created_at - $3) / $4)::bigint AS bucket, \
           count(*)::bigint AS n \
         FROM ci_runs r, ci_repos p WHERE {scope} \
           AND r.created_at >= $3 AND r.created_at < $3 + $4 * $5 GROUP BY 2 \
         UNION ALL \
         SELECT r.conclusion AS kind, ((r.completed_at - $3) / $4)::bigint AS bucket, \
           count(*)::bigint AS n \
         FROM ci_runs r, ci_repos p WHERE {scope} \
           AND r.conclusion IN ('success', 'failure') \
           AND r.completed_at >= $3 AND r.completed_at < $3 + $4 * $5 GROUP BY 1, 2"
    );
    let ids: Vec<String> = repo_ids.iter().map(u64::to_string).collect();
    let rows: Vec<Row> = db
        .all(&sql, &params![ids.join(","), repo, start, bucket_ms, count])
        .await?;
    let mut buckets = vec![Bucket::default(); count as usize];
    for row in rows {
        let Some(bucket) = usize::try_from(row.bucket)
            .ok()
            .and_then(|at| buckets.get_mut(at))
        else {
            continue;
        };
        match row.kind.as_str() {
            "started" => bucket.started = row.n,
            "success" => bucket.passed = row.n,
            _ => bucket.failed = row.n,
        }
    }
    Ok(buckets)
}

/// The jobs of `repo_ids` (and of `repo`, when given) that run now and
/// that wait for a launch.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn job_load(db: &Db, repo_ids: &[u64], repo: Option<&str>) -> Result<(u64, u64)> {
    #[derive(Deserialize)]
    struct Row {
        running: u64,
        queued: u64,
    }
    let ids: Vec<String> = repo_ids.iter().map(u64::to_string).collect();
    let row = db
        .first::<Row>(
            "SELECT count(*) FILTER (WHERE j.status IN ('launching', 'launched', 'running'))::bigint \
               AS running, \
             count(*) FILTER (WHERE j.status = 'queued')::bigint AS queued \
             FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id \
             JOIN ci_repos p ON p.id = r.repo_id AND p.tenant_id = r.tenant_id \
             WHERE r.repo_id = ANY (string_to_array($1, ',')::bigint[]) \
               AND ($2::text IS NULL OR lower(p.full_name) = lower($2::text)) \
               AND j.status IN ('queued', 'launching', 'launched', 'running')",
            &params![ids.join(","), repo],
        )
        .await?;
    Ok(row.map_or((0, 0), |row| (row.running, row.queued)))
}

/// How many runs a filter holds, by state, for the tabs of a run list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RunCounts {
    /// Every run the filter holds.
    pub all: u64,
    /// Running.
    pub running: u64,
    /// Waiting for an approval.
    pub waiting: u64,
    /// Completed with `failure`.
    pub failed: u64,
    /// Completed with `success`.
    pub passed: u64,
}

/// The counts of the runs `filter` holds, whatever its states.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run_counts(db: &Db, filter: &RunFilter) -> Result<RunCounts> {
    let mut all_states = filter.clone();
    all_states.states.clear();
    let (conditions, bound) = run_where(&all_states);
    let sql = format!(
        "SELECT count(*) AS all, \
         count(*) FILTER (WHERE {running}) AS running, \
         count(*) FILTER (WHERE {waiting}) AS waiting, \
         count(*) FILTER (WHERE {failed}) AS failed, \
         count(*) FILTER (WHERE {passed}) AS passed \
         FROM ci_runs r, ci_repos p WHERE {conditions}",
        running = RunStatus::Running.condition(),
        waiting = RunStatus::Waiting.condition(),
        failed = RunStatus::Failure.condition(),
        passed = RunStatus::Success.condition(),
    );
    Ok(db
        .first::<RunCounts>(&sql, &bound)
        .await?
        .unwrap_or_default())
}

/// The newest completed run on the default branch of each repository
/// in `repo_ids` of `tenant`, for those that have one.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn latest_on_default(db: &Db, tenant: &str, repo_ids: &[u64]) -> Result<Vec<ListedRun>> {
    let ids: Vec<String> = repo_ids.iter().map(u64::to_string).collect();
    db.all(
        &format!(
            "SELECT DISTINCT ON (r.repo_id) {LISTED_COLUMNS} \
             FROM ci_runs r, ci_repos p \
             WHERE p.id = r.repo_id AND p.tenant_id = r.tenant_id AND p.tenant_id = $1 \
             AND p.id = ANY (string_to_array($2, ',')::bigint[]) \
             AND r.status = 'completed' AND r.ref = 'refs/heads/' || p.default_branch \
             ORDER BY r.repo_id, r.created_at DESC, r.id DESC"
        ),
        &params![tenant, ids.join(",")],
    )
    .await
}

/// Moves run `run_id` from queued to in progress.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run_started(db: &Db, run_id: u64, now: u64) -> Result<()> {
    db.run(RUN_STARTED, &params![run_id, now]).await?;
    Ok(())
}

/// Opens run `run_id` for `attempt`. It can be repeated.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn reopen_run_at(db: &Db, run_id: u64, attempt: u32, now: u64) -> Result<()> {
    db.run(REOPEN_RUN_AT, &params![run_id, attempt, now])
        .await?;
    Ok(())
}

/// Completes attempt `attempt` of run `run_id` with its `conclusion`; a
/// run of another attempt is left alone.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn finish_run(
    db: &Db,
    run_id: u64,
    attempt: u32,
    conclusion: &str,
    now: u64,
) -> Result<()> {
    db.run(FINISH_RUN, &params![run_id, conclusion, now, attempt])
        .await?;
    Ok(())
}
