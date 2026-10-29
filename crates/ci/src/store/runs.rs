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

/// The columns of a [`RunRow`].
const RUN_COLUMNS: &str = "id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, \
     event, ref AS git_ref, sha, actor, status, conclusion, created_at, completed_at, \
     (expires_at - created_at) / 86400000 AS retention_days";

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
/// `$4` delivery, `$5` event, `$6` ref, `$7` sha, `$8` actor and `$9`
/// now. The run expires after its tenant's retention tier (30 or 90 days), read
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
       actor, status, created_at, updated_at, expires_at) \
     SELECT $1, $2, $3, n.last, $4, $5, $6, $7, $8, 'queued', $9, $9, \
       $9 + (SELECT CASE WHEN retention_days > 30 THEN 90 ELSE 30 END * 86400000::bigint \
         FROM tenants WHERE id = $1) FROM n \
     RETURNING id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, event, \
       ref AS git_ref, sha, actor, status, conclusion, created_at, completed_at, \
       (expires_at - created_at) / 86400000 AS retention_days";

/// Run `$1`, where `{columns}` is [`RUN_COLUMNS`].
const RUN: &str = "SELECT {columns} FROM ci_runs WHERE id = $1";

/// Runs of repository `$1` newest first, those with an id below `$2`
/// when it is set, at most `$3`. Only runs of the tenant that owns the
/// repository now: after a transfer, the history stays with the tenant
/// that owned the repository when the run happened.
const RUNS_FOR_REPO: &str = "SELECT {columns} FROM ci_runs \
     WHERE repo_id = $1 \
     AND tenant_id = (SELECT tenant_id FROM ci_repos WHERE id = $1) \
     AND ($2::bigint IS NULL OR id < $2::bigint) \
     ORDER BY id DESC LIMIT $3";

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
            run.now
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

/// Runs of `repo_id`, newest first: those with an id below `before`
/// when it is set, at most `limit`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn runs_for_repo(
    db: &Db,
    repo_id: u64,
    before: Option<u64>,
    limit: u32,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(RUNS_FOR_REPO, RUN_COLUMNS),
        &params![repo_id, before, limit],
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
