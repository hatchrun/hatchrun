// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::with_columns;

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
    /// When the run is recorded.
    pub now: u64,
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
}

impl RunRow {
    /// The branch the run is on: that of a push, the base branch of a
    /// pull request, none for a tag. The same as [`BRANCH`].
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        if let Some(name) = self.git_ref.strip_prefix("refs/heads/") {
            Some(name)
        } else if self.git_ref.starts_with("refs/pull/") {
            self.base_ref.as_deref()
        } else {
            None
        }
    }
}

/// The columns of a [`RunRow`].
const RUN_COLUMNS: &str = "id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, \
     event, ref AS git_ref, sha, actor, base_ref, status, conclusion, created_at, \
     completed_at, (expires_at - created_at) / 86400000 AS retention_days";

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
/// now and `$10` the pull request's base branch. The run expires after its tenant's retention tier (30 or 90 days), read
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
       actor, base_ref, status, created_at, updated_at, expires_at) \
     SELECT $1, $2, $3, n.last, $4, $5, $6, $7, $8, $10::text, 'queued', $9, $9, \
       $9 + (SELECT CASE WHEN retention_days > 30 THEN 90 ELSE 30 END * 86400000::bigint \
         FROM tenants WHERE id = $1) FROM n \
     RETURNING id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, event, \
       ref AS git_ref, sha, actor, base_ref, status, conclusion, created_at, \
       completed_at, (expires_at - created_at) / 86400000 AS retention_days";

/// Run `$1`, where `{columns}` is [`RUN_COLUMNS`].
const RUN: &str = "SELECT {columns} FROM ci_runs WHERE id = $1";

/// Moves run `$1` from queued to in progress at `$2`.
const RUN_STARTED: &str = "UPDATE ci_runs SET status = 'in_progress', updated_at = $2 WHERE id = $1 AND status = 'queued'";

/// Opens run `$1` for attempt `$2` at `$3`.
const REOPEN_RUN_AT: &str = "UPDATE ci_runs SET status = 'in_progress', conclusion = NULL, \
     completed_at = NULL, run_attempt = $2, updated_at = $3 WHERE id = $1";

/// Completes attempt `$4` of run `$1` with conclusion `$2` at `$3`.
const FINISH_RUN: &str = "UPDATE ci_runs SET status = 'completed', conclusion = $2, \
     completed_at = $3, updated_at = $3 WHERE id = $1 AND run_attempt = $4";

/// Records `run` with the next number of its workflow in its repository.
/// Returns `None` when a run of this delivery and workflow exists,
/// which then has not used up a number.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn insert_run(db: &Db, run: &NewRun) -> Result<Option<RunRow>> {
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
            &run.base_ref
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

/// Run `run_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn run(db: &Db, run_id: u64) -> Result<Option<RunRow>> {
    db.first(&with_columns(RUN, RUN_COLUMNS), &params![run_id])
        .await
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

/// The branch a run is on: the branch of a push, the base branch of a
/// pull request, nothing for a tag or other refs. For a query that
/// aliases `ci_runs` as `r`.
const BRANCH: &str = "CASE WHEN r.ref LIKE 'refs/heads/%' THEN substr(r.ref, 12) \
     WHEN r.ref LIKE 'refs/pull/%' THEN r.base_ref END";

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
fn like(text: &str) -> String {
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
/// its parameters `$1` to `$9`.
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
           OR r.sha ILIKE $8::text)",
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
        sha
    ];
    (sql, bound)
}

/// The columns of a [`ListedRun`] over `ci_runs r` and `ci_repos p`.
pub(super) const LISTED_COLUMNS: &str = "r.id, r.run_number, r.run_attempt, r.tenant_id, r.repo_id, \
     r.workflow, r.delivery, r.event, r.ref AS git_ref, r.sha, r.actor, r.base_ref, r.status, \
     r.conclusion, r.created_at, r.completed_at, \
     (r.expires_at - r.created_at) / 86400000 AS retention_days, \
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
         AND ($9::bigint IS NULL OR (r.created_at, r.id) < ($9::bigint, $10::bigint)) \
         ORDER BY r.created_at DESC, r.id DESC LIMIT $11"
    );
    let rows = db.all(&page, &bound).await?;
    Ok((rows, total))
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
