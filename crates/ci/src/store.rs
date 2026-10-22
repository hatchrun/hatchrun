// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The SQL on the `ci_` tables, and small functions that run it.
//!
//! The webhook records runs and jobs here, the launch claims and fails
//! jobs, the agent's calls move them along, the app lists them, and the
//! reconciler checks, restarts and expires them. Times are Unix
//! milliseconds. A job's `steps` column is `jsonb`, which rows do not
//! carry, so every statement reads it as text and [`JobRow`] parses it.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use serde::de::{Deserializer, Error as _};
use worker::Result;

/// How long runs, their jobs and delivery ids are kept, in
/// milliseconds.
const RETENTION_MS: u64 = 30 * 86_400_000;

/// A tenant, as far as CI needs it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TenantRow {
    /// The tenant id.
    pub id: String,
    /// Whether CI runs for this tenant.
    pub ci_enabled: bool,
}

/// A repository CI has seen.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoRow {
    /// GitHub's repository id, which survives renames.
    pub id: u64,
    /// The owning tenant.
    pub tenant_id: String,
    /// The App installation that reaches the repository.
    pub installation_id: u64,
    /// `owner/name`.
    pub full_name: String,
    /// The branch scheduled workflows run on.
    pub default_branch: String,
    /// Whether the repository is private.
    pub private: bool,
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
}

/// A job to record, from a planned job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// The planned job's id in its workflow.
    pub plan_id: String,
    /// Its display name.
    pub name: String,
    /// The most minutes it may run.
    pub timeout_minutes: u32,
}

/// A recorded job.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct JobRow {
    /// The job's identity.
    pub id: u64,
    /// The run it belongs to.
    pub run_id: u64,
    /// The planned job's id in its workflow.
    pub plan_id: String,
    /// Its display name.
    pub name: String,
    /// `waiting`, `queued`, `launching`, `launched`, `running` or
    /// `completed`.
    pub status: String,
    /// `success`, `failure`, `cancelled` or `skipped`, once completed.
    pub conclusion: Option<String>,
    /// The EC2 instance running it.
    pub instance_id: Option<String>,
    /// Starts at 1; a re-run adds one.
    pub attempt: u32,
    /// How many times a launch was claimed for this attempt.
    pub launch_attempts: u32,
    /// Not before this, a failed launch is tried again.
    pub next_launch_at: Option<u64>,
    /// The most minutes it may run.
    pub timeout_minutes: u32,
    /// When the row last changed.
    pub updated_at: u64,
    /// The agent's last sign of life.
    pub heartbeat_at: Option<u64>,
    /// When the agent started it.
    pub started_at: Option<u64>,
    /// When it finished.
    pub completed_at: Option<u64>,
    /// The step summaries, a JSON array.
    #[serde(deserialize_with = "json_text")]
    pub steps: serde_json::Value,
    /// The R2 key of the finished log.
    pub log_key: Option<String>,
}

/// A schedule of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ScheduleRow {
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The workflow's file name.
    pub workflow: String,
    /// The cron expression.
    pub cron: String,
}

/// Reads a JSON document that the row carries as text.
fn json_text<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<serde_json::Value, D::Error> {
    let text = String::deserialize(d)?;
    serde_json::from_str(&text).map_err(D::Error::custom)
}

/// The columns of a [`RunRow`].
const RUN_COLUMNS: &str = "id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, \
     event, ref AS git_ref, sha, actor, status, conclusion, created_at, completed_at";

/// The columns of a [`JobRow`].
const JOB_COLUMNS: &str = "id, run_id, plan_id, name, status, conclusion, instance_id, attempt, \
     launch_attempts, next_launch_at, timeout_minutes, updated_at, heartbeat_at, started_at, \
     completed_at, steps::text AS steps, log_key";

/// The tenant of installation `$1`.
const TENANT_FOR_INSTALLATION: &str =
    "SELECT id, ci_enabled FROM tenants WHERE installation_id = $1";

/// Tenant `$1`.
const TENANT: &str = "SELECT id, ci_enabled FROM tenants WHERE id = $1";

/// The run of delivery `$1` and workflow `$2`, where `{columns}` is
/// [`RUN_COLUMNS`].
const RUN_FOR_DELIVERY: &str =
    "SELECT {columns} FROM ci_runs WHERE delivery = $1 AND workflow = $2";

/// Records repository `$1` of tenant `$2`, installation `$3`, named
/// `$4` with default branch `$5`, private `$6`, at `$7`; again when it
/// was renamed or changed.
const UPSERT_REPO: &str = "INSERT INTO ci_repos \
     (id, tenant_id, installation_id, full_name, default_branch, private, updated_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7) \
     ON CONFLICT (id) DO UPDATE SET tenant_id = $2, installation_id = $3, full_name = $4, \
     default_branch = $5, private = $6, updated_at = $7";

/// Repository `$1` by GitHub id.
const REPO: &str = "SELECT id, tenant_id, installation_id, full_name, default_branch, private \
     FROM ci_repos WHERE id = $1";

/// The repository named `$1`, ignoring case.
const REPO_BY_NAME: &str = "SELECT id, tenant_id, installation_id, full_name, default_branch, \
     private FROM ci_repos WHERE lower(full_name) = lower($1)";

/// Whether delivery `$1` was recorded.
const DELIVERY_SEEN: &str = "SELECT 1 AS seen FROM ci_deliveries WHERE guid = $1";

/// Records delivery `$1` of event `$2` received at `$3`, once.
const RECORD_DELIVERY: &str = "INSERT INTO ci_deliveries (guid, event, received_at) \
     VALUES ($1, $2, $3) ON CONFLICT (guid) DO NOTHING";

/// Locks the row of repository `$1`, so runs of one repository are
/// numbered one at a time. The statement after it in the transaction
/// reads what the holder before it committed.
const LOCK_REPO: &str = "SELECT id FROM ci_repos WHERE id = $1 FOR NO KEY UPDATE";

/// Records a run and takes its number in one statement, run after
/// [`LOCK_REPO`]. Binds `$1` tenant, `$2` repository, `$3` workflow,
/// `$4` delivery, `$5` event, `$6` ref, `$7` sha, `$8` actor, `$9` now
/// and `$10` the expiry. The number is taken only when no run of this
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
     SELECT $1, $2, $3, n.last, $4, $5, $6, $7, $8, 'queued', $9, $9, $10 FROM n \
     RETURNING id, run_number, run_attempt, tenant_id, repo_id, workflow, delivery, event, \
       ref AS git_ref, sha, actor, status, conclusion, created_at, completed_at";

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

/// The jobs of run `$1` in plan order.
const JOBS_FOR_RUN: &str = "SELECT {columns} FROM ci_jobs WHERE run_id = $1 ORDER BY id";

/// Jobs that are not completed, the least recently changed first, at
/// most `$1`.
const OPEN_JOBS: &str = "SELECT {columns} FROM ci_jobs WHERE status <> 'completed' \
     ORDER BY updated_at, id LIMIT $1";

/// Job `$1`.
const JOB: &str = "SELECT {columns} FROM ci_jobs WHERE id = $1";

/// Starts running job `$1` at `$2`, which is also its first heartbeat.
const START_JOB: &str = "UPDATE ci_jobs SET status = 'running', started_at = $2, \
     heartbeat_at = $2, updated_at = $2 \
     WHERE id = $1 AND status IN ('launching', 'launched')";

/// Sets job `$1` up for attempt `$2` at `$3`: nothing of the earlier
/// attempt is left. A job already at that attempt or later is left
/// alone, so a late repeat does nothing.
const RESET_JOB: &str = "UPDATE ci_jobs SET status = 'waiting', attempt = $2, launch_attempts = 0, \
     conclusion = NULL, instance_id = NULL, launch_error = NULL, next_launch_at = NULL, \
     heartbeat_at = NULL, started_at = NULL, completed_at = NULL, log_key = NULL, \
     steps = '[]', updated_at = $3 WHERE id = $1 AND attempt < $2";

/// Moves run `$1` from queued to in progress at `$2`.
const RUN_STARTED: &str = "UPDATE ci_runs SET status = 'in_progress', updated_at = $2 WHERE id = $1 AND status = 'queued'";

/// Queues attempt `$2` of job `$1` at `$3`. Only a job of that attempt
/// that is waiting, or already queued, changes: one that is launching
/// or launched is left alone, so repeating the call launches nothing
/// twice.
const QUEUE_JOB: &str = "UPDATE ci_jobs SET status = 'queued', updated_at = $3 \
     WHERE id = $1 AND attempt = $2 AND status IN ('waiting', 'queued')";

/// Opens run `$1` for attempt `$2` at `$3`.
const REOPEN_RUN_AT: &str = "UPDATE ci_runs SET status = 'in_progress', conclusion = NULL, \
     completed_at = NULL, run_attempt = $2, updated_at = $3 WHERE id = $1";

/// Sets job `$1` to status `$2` and conclusion `$3` at `$4`. A
/// completed job takes the time as its completion.
const SET_JOB: &str = "UPDATE ci_jobs SET status = $2, conclusion = $3, updated_at = $4, \
     completed_at = CASE WHEN $2 = 'completed' THEN $4 ELSE completed_at END WHERE id = $1";

/// Takes the launch of job `$1` at `$2`, if it is queued.
const CLAIM_LAUNCH: &str = "UPDATE ci_jobs SET status = 'launching', \
     launch_attempts = launch_attempts + 1, updated_at = $2 \
     WHERE id = $1 AND status = 'queued' RETURNING {columns}";

/// Records the instance `$2` of job `$1` at `$3`.
const LAUNCHED: &str = "UPDATE ci_jobs SET status = 'launched', instance_id = $2, \
     launch_error = NULL, next_launch_at = NULL, updated_at = $3 \
     WHERE id = $1 AND status = 'launching'";

/// Queues job `$1` again after a failed launch: error `$2`, not before
/// `$3`, at `$4`.
const LAUNCH_RETRY: &str = "UPDATE ci_jobs SET status = 'queued', launch_error = $2, \
     next_launch_at = $3, updated_at = $4 \
     WHERE id = $1 AND status = 'launching'";

/// Fails job `$1` for good after a failed launch: error `$2`, at `$3`.
const LAUNCH_GAVE_UP: &str = "UPDATE ci_jobs SET status = 'completed', conclusion = 'failure', \
     launch_error = $2, next_launch_at = NULL, completed_at = $3, updated_at = $3 \
     WHERE id = $1 AND status = 'launching'";

/// Records the agent's heartbeat for job `$1` at `$2`.
const HEARTBEAT: &str = "UPDATE ci_jobs SET heartbeat_at = $2, updated_at = $2 \
     WHERE id = $1 AND status <> 'completed'";

/// Completes attempt `$6` of job `$1` with conclusion `$2`, steps `$3`
/// (JSON text) and log key `$4` at `$5`.
const FINISH_JOB: &str = "UPDATE ci_jobs SET status = 'completed', conclusion = $2, \
     steps = $3::jsonb, log_key = $4, completed_at = $5, updated_at = $5 \
     WHERE id = $1 AND status <> 'completed' AND attempt = $6";

/// Completes attempt `$4` of run `$1` with conclusion `$2` at `$3`.
const FINISH_RUN: &str = "UPDATE ci_runs SET status = 'completed', conclusion = $2, \
     completed_at = $3, updated_at = $3 WHERE id = $1 AND run_attempt = $4";

/// Drops the schedules of repository `$1`.
const DELETE_SCHEDULES: &str = "DELETE FROM ci_schedules WHERE repo_id = $1";

/// Adds a schedule of repository `$1`, workflow `$2` and cron `$3`.
const INSERT_SCHEDULE: &str = "INSERT INTO ci_schedules (repo_id, workflow, cron) \
     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING";

/// Schedules, at most `$1`.
const SCHEDULES: &str = "SELECT repo_id, workflow, cron FROM ci_schedules \
     ORDER BY repo_id, workflow, cron LIMIT $1";

/// The jobs a reconciler pass may have to act on, at most `limit`:
/// queued, launching, launched and running jobs that are overdue
/// against the limits of `now`. See [`crate::reconcile::stalled`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn stalled_jobs(db: &Db, now: u64, limit: u32) -> Result<Vec<JobRow>> {
    db.all(
        &with_columns(STALLED_JOBS, JOB_COLUMNS),
        &params![
            now,
            now.saturating_sub(crate::reconcile::QUEUED_QUIET_MS),
            now.saturating_sub(crate::reconcile::LAUNCH_QUIET_MS),
            now.saturating_sub(crate::reconcile::RUNNER_QUIET_MS),
            limit
        ],
    )
    .await
}

/// Takes the reconciler's lease until `$2` when it ran out before `$1`.
const LEASE: &str = "UPDATE ci_reconciler SET lease_until = $2 WHERE id = 1 AND lease_until < $1";

/// The last schedule minute fired.
const LAST_MINUTE: &str = "SELECT last_minute FROM ci_reconciler WHERE id = 1";

/// Sets the last schedule minute fired to `$1`.
const SET_LAST_MINUTE: &str = "UPDATE ci_reconciler SET last_minute = $1 WHERE id = 1";

/// Jobs a reconciler pass looks at, the least recently changed first, at
/// most `$5`: queued ones due for a launch (`$1` now) or quiet since
/// `$2`, launching or launched ones quiet since `$3`, and running ones
/// without a heartbeat since `$4`, or without a start or any change when
/// the heartbeat is missing. [`crate::reconcile::stalled`] makes the
/// same decisions over the rows.
const STALLED_JOBS: &str = "SELECT {columns} FROM ci_jobs WHERE \
     (status = 'queued' AND (next_launch_at <= $1 OR updated_at < $2)) \
     OR (status IN ('launching', 'launched') AND updated_at < $3) \
     OR (status = 'running' AND COALESCE(heartbeat_at, started_at, updated_at) < $4) \
     ORDER BY updated_at, id LIMIT $5";

/// Deletes the runs that expired before `$1`, at most `$2` of them;
/// their jobs go with them. The oldest go first.
const SWEEP_RUNS: &str = "DELETE FROM ci_runs WHERE id IN (\
       SELECT id FROM ci_runs WHERE expires_at < $1 ORDER BY expires_at, id LIMIT $2) \
     RETURNING id";

/// The most runs one [`sweep`] deletes.
pub const SWEEP_LIMIT: u32 = 100;

/// Deletes delivery ids received before `$1`.
const SWEEP_DELIVERIES: &str = "DELETE FROM ci_deliveries WHERE received_at < $1";

/// `sql` with its `{columns}` filled in.
fn with_columns(sql: &str, columns: &str) -> String {
    sql.replace("{columns}", columns)
}

/// Whether a one-column `seen` query found a row.
async fn exists(db: &Db, sql: &str, params: &[Param]) -> Result<bool> {
    Ok(db.rows(sql, params).await?.into_iter().next().is_some())
}

/// The tenant of `installation_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_for_installation(db: &Db, installation_id: u64) -> Result<Option<TenantRow>> {
    db.first(TENANT_FOR_INSTALLATION, &params![installation_id])
        .await
}

/// Tenant `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant(db: &Db, tenant_id: &str) -> Result<Option<TenantRow>> {
    db.first(TENANT, &params![tenant_id]).await
}

/// Records `repo`, or updates it after a rename or a change.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn upsert_repo(db: &Db, repo: &RepoRow, now: u64) -> Result<()> {
    db.run(
        UPSERT_REPO,
        &params![
            repo.id,
            &repo.tenant_id,
            repo.installation_id,
            &repo.full_name,
            &repo.default_branch,
            repo.private,
            now
        ],
    )
    .await?;
    Ok(())
}

/// Repository `repo_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo(db: &Db, repo_id: u64) -> Result<Option<RepoRow>> {
    db.first(REPO, &params![repo_id]).await
}

/// The repository named `full_name`, ignoring case.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo_by_name(db: &Db, full_name: &str) -> Result<Option<RepoRow>> {
    db.first(REPO_BY_NAME, &params![full_name]).await
}

/// Whether delivery `guid` was recorded.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delivery_seen(db: &Db, guid: &str) -> Result<bool> {
    exists(db, DELIVERY_SEEN, &params![guid]).await
}

/// Records delivery `guid` of `event`, once.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn record_delivery(db: &Db, guid: &str, event: &str, now: u64) -> Result<()> {
    db.run(RECORD_DELIVERY, &params![guid, event, now]).await?;
    Ok(())
}

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
            run.now + RETENTION_MS
        ],
    )
    .await
}

/// Records `jobs` of run `run_id` as waiting and returns each one's
/// plan id and job id, in plan order.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn insert_jobs(
    db: &Db,
    run_id: u64,
    jobs: &[NewJob],
    now: u64,
) -> Result<Vec<(String, u64)>> {
    #[derive(Deserialize)]
    struct Inserted {
        plan_id: String,
        id: u64,
    }
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    let mut bound = params![run_id, now];
    for (i, job) in jobs.iter().enumerate() {
        let base = 3 + i * 3;
        values.push(format!(
            "($1, ${}, ${}, ${}, 'waiting', $2, $2)",
            base,
            base + 1,
            base + 2
        ));
        bound.push(Param::from(&job.plan_id));
        bound.push(Param::from(&job.name));
        bound.push(Param::from(job.timeout_minutes));
    }
    let sql = format!(
        "INSERT INTO ci_jobs (run_id, plan_id, name, timeout_minutes, status, created_at, \
         updated_at) VALUES {} RETURNING plan_id, id",
        values.join(", ")
    );
    let mut inserted: Vec<Inserted> = db.all(&sql, &bound).await?;
    inserted.sort_by_key(|row| row.id);
    Ok(inserted
        .into_iter()
        .map(|row| (row.plan_id, row.id))
        .collect())
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

/// The jobs of `run_id` in plan order.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn jobs_for_run(db: &Db, run_id: u64) -> Result<Vec<JobRow>> {
    db.all(&with_columns(JOBS_FOR_RUN, JOB_COLUMNS), &params![run_id])
        .await
}

/// Jobs that are not completed, the least recently changed first, at
/// most `limit`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn open_jobs(db: &Db, limit: u32) -> Result<Vec<JobRow>> {
    db.all(&with_columns(OPEN_JOBS, JOB_COLUMNS), &params![limit])
        .await
}

/// The jobs with these ids, for checking which still exist.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn jobs_by_ids(db: &Db, ids: &[u64]) -> Result<Vec<JobRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<String> = (1..=ids.len()).map(|n| format!("${n}")).collect();
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM ci_jobs WHERE id IN ({}) ORDER BY id",
        marks.join(", ")
    );
    let bound: Vec<Param> = ids.iter().map(|id| Param::from(*id)).collect();
    db.all(&sql, &bound).await
}

/// Job `job_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn job(db: &Db, job_id: u64) -> Result<Option<JobRow>> {
    db.first(&with_columns(JOB, JOB_COLUMNS), &params![job_id])
        .await
}

/// Starts job `job_id`: running, with `now` as its start and first
/// heartbeat. Only a job that is launching or launched starts; returns
/// whether it did.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn start_job(db: &Db, job_id: u64, now: u64) -> Result<bool> {
    Ok(db.run(START_JOB, &params![job_id, now]).await? == 1)
}

/// Sets job `job_id` up for `attempt`: waiting, with nothing left of
/// the attempt before.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn reset_job(db: &Db, job_id: u64, attempt: u32, now: u64) -> Result<()> {
    db.run(RESET_JOB, &params![job_id, attempt, now]).await?;
    Ok(())
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

/// Queues attempt `attempt` of job `job_id`, unless the job moved on:
/// returns whether the job is queued for that attempt now. Repeating
/// the call is safe; a job already launching or launched stays so and
/// the answer is `false`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn queue_job(db: &Db, job_id: u64, attempt: u32, now: u64) -> Result<bool> {
    Ok(db.run(QUEUE_JOB, &params![job_id, attempt, now]).await? == 1)
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

/// Sets the status and conclusion of job `job_id`. Completing a job
/// takes `now` as its completion time.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_job(
    db: &Db,
    job_id: u64,
    status: &str,
    conclusion: Option<&str>,
    now: u64,
) -> Result<()> {
    db.run(SET_JOB, &params![job_id, status, conclusion, now])
        .await?;
    Ok(())
}

/// Takes the launch of job `job_id` if it is queued: launching, one
/// more launch attempt. `None` when somebody else took it.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn claim_launch(db: &Db, job_id: u64, now: u64) -> Result<Option<JobRow>> {
    db.first(
        &with_columns(CLAIM_LAUNCH, JOB_COLUMNS),
        &params![job_id, now],
    )
    .await
}

/// Records the instance of job `job_id`: launched. Only a launching
/// job takes it; returns whether it did.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn launched(db: &Db, job_id: u64, instance_id: &str, now: u64) -> Result<bool> {
    Ok(db.run(LAUNCHED, &params![job_id, instance_id, now]).await? == 1)
}

/// Records that a launch of job `job_id` failed with `error`. With
/// `retry_at` the job is queued again, not before then; without it the
/// job fails. Only a launching job changes; returns whether it did.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn launch_failed(
    db: &Db,
    job_id: u64,
    error: &str,
    retry_at: Option<u64>,
    now: u64,
) -> Result<bool> {
    let changed = match retry_at {
        Some(at) => {
            db.run(LAUNCH_RETRY, &params![job_id, error, at, now])
                .await?
        }
        None => db.run(LAUNCH_GAVE_UP, &params![job_id, error, now]).await?,
    };
    Ok(changed == 1)
}

/// Records the agent's heartbeat for job `job_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn heartbeat(db: &Db, job_id: u64, now: u64) -> Result<()> {
    db.run(HEARTBEAT, &params![job_id, now]).await?;
    Ok(())
}

/// Completes attempt `attempt` of job `job_id` with its `conclusion`,
/// `steps` and the key of its finished log. A completed job, and a job
/// of another attempt, keep what they have; returns whether this call
/// completed it.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn finish_job(
    db: &Db,
    job_id: u64,
    attempt: u32,
    conclusion: &str,
    steps: &serde_json::Value,
    log_key: Option<&str>,
    now: u64,
) -> Result<bool> {
    let changed = db
        .run(
            FINISH_JOB,
            &params![job_id, conclusion, steps.to_string(), log_key, now, attempt],
        )
        .await?;
    Ok(changed == 1)
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

/// Replaces the schedules of `repo_id` with `schedules`, pairs of
/// workflow and cron, in one transaction.
///
/// # Errors
///
/// Postgres refused a statement; nothing changes.
pub async fn replace_schedules(
    db: &Db,
    repo_id: u64,
    schedules: &[(String, String)],
) -> Result<()> {
    let mut statements = vec![(DELETE_SCHEDULES, params![repo_id])];
    for (workflow, cron) in schedules {
        statements.push((INSERT_SCHEDULE, params![repo_id, workflow, cron]));
    }
    db.batch(&statements).await?;
    Ok(())
}

/// Schedules, at most `limit`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn schedules(db: &Db, limit: u32) -> Result<Vec<ScheduleRow>> {
    db.all(SCHEDULES, &params![limit]).await
}

/// Takes the reconciler's lease until `until` when it ran out before
/// `now`. Returns whether it is taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn lease(db: &Db, now: u64, until: u64) -> Result<bool> {
    Ok(db.run(LEASE, &params![now, until]).await? == 1)
}

/// The last schedule minute fired, in Unix milliseconds.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn last_minute(db: &Db) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Minute {
        last_minute: Option<u64>,
    }
    Ok(db
        .first::<Minute>(LAST_MINUTE, &[])
        .await?
        .and_then(|row| row.last_minute))
}

/// Records the last schedule minute fired.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_last_minute(db: &Db, minute_ms: u64) -> Result<()> {
    db.run(SET_LAST_MINUTE, &params![minute_ms]).await?;
    Ok(())
}

/// Deletes runs that expired before `now`, with their jobs, and
/// delivery ids older than 30 days. At most [`SWEEP_LIMIT`] runs go in
/// one call, oldest first, and the rest the next time. Returns the ids
/// of the runs, so their logs can be purged.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn sweep(db: &Db, now: u64) -> Result<Vec<u64>> {
    #[derive(Deserialize)]
    struct Deleted {
        id: u64,
    }
    let runs: Vec<Deleted> = db.all(SWEEP_RUNS, &params![now, SWEEP_LIMIT]).await?;
    db.run(SWEEP_DELIVERIES, &params![now.saturating_sub(RETENTION_MS)])
        .await?;
    Ok(runs.into_iter().map(|row| row.id).collect())
}

#[cfg(test)]
mod tests {
    use hatchrun_cloud::testdb::Db as TestDb;

    use super::*;

    const DAY: u64 = 86_400_000;

    fn setup() -> TestDb {
        let db = TestDb::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t1', 1, 'acme', 99, 0, 0)",
            &[],
        );
        let repo = RepoRow {
            id: 7,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "acme/app".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();
        db
    }

    fn new_run(delivery: &str, workflow: &str, now: u64) -> NewRun {
        NewRun {
            tenant_id: "t1".into(),
            repo_id: 7,
            workflow: workflow.into(),
            delivery: delivery.into(),
            event: "push".into(),
            git_ref: "refs/heads/main".into(),
            sha: "abc".into(),
            actor: "octocat".into(),
            now,
        }
    }

    fn new_job(plan_id: &str) -> NewJob {
        NewJob {
            plan_id: plan_id.into(),
            name: plan_id.into(),
            timeout_minutes: 360,
        }
    }

    /// A run with one job, which the test moves to `status`.
    fn run_with_job(db: &TestDb, status: &str) -> (RunRow, u64) {
        let run = db
            .wait(insert_run(db.db(), &new_run("d", "ci", 1_000)))
            .unwrap()
            .unwrap();
        let ids = db
            .wait(insert_jobs(db.db(), run.id, &[new_job("build")], 1_000))
            .unwrap();
        let job_id = ids[0].1;
        db.wait(set_job(db.db(), job_id, status, None, 1_001))
            .unwrap();
        (run, job_id)
    }

    #[test]
    fn insert_run_dedupes_on_delivery_and_workflow() {
        let db = setup();
        let first = db
            .wait(insert_run(db.db(), &new_run("d1", "ci", 5)))
            .unwrap();
        let again = db
            .wait(insert_run(db.db(), &new_run("d1", "ci", 6)))
            .unwrap();
        let other = db
            .wait(insert_run(db.db(), &new_run("d1", "lint", 7)))
            .unwrap();
        let first = first.unwrap();
        assert!(again.is_none());
        assert!(other.is_some());
        assert_eq!(first.status, "queued");
        assert_eq!(first.created_at, 5);
        assert_eq!(first.git_ref, "refs/heads/main");
        let expires: u64 = db
            .column(
                "SELECT expires_at AS n FROM ci_runs WHERE id = $1",
                &params![first.id],
                "n",
            )
            .unwrap();
        assert_eq!(expires, 5 + 30 * DAY);
    }

    #[test]
    fn run_numbers_count_per_workflow() {
        let db = setup();
        let mut numbers = Vec::new();
        for (delivery, workflow) in [("a", "ci"), ("b", "ci"), ("c", "lint")] {
            let run = db
                .wait(insert_run(db.db(), &new_run(delivery, workflow, 1)))
                .unwrap()
                .unwrap();
            numbers.push(run.run_number);
        }
        assert_eq!(numbers, [1, 2, 1]);
    }

    #[test]
    fn a_duplicate_does_not_use_up_a_run_number() {
        let db = setup();
        db.wait(insert_run(db.db(), &new_run("a", "ci", 1)))
            .unwrap()
            .unwrap();
        assert!(
            db.wait(insert_run(db.db(), &new_run("a", "ci", 2)))
                .unwrap()
                .is_none()
        );
        let third = db
            .wait(insert_run(db.db(), &new_run("b", "ci", 3)))
            .unwrap()
            .unwrap();
        assert_eq!(third.run_number, 2);
    }

    #[test]
    fn concurrent_duplicates_use_up_one_number() {
        let db = setup();
        let config: tokio_postgres::Config = hatchrun_cloud::testdb::url().parse().unwrap();
        let second = db.connect(&config);
        let first = new_run("same", "ci", 5);
        let other = new_run("same", "ci", 5);
        let (a, b) = db.wait(async {
            futures_util::join!(insert_run(db.db(), &first), insert_run(&second, &other))
        });
        let recorded = [a.unwrap(), b.unwrap()];
        assert_eq!(recorded.iter().flatten().count(), 1);
        let next = db
            .wait(insert_run(db.db(), &new_run("next", "ci", 6)))
            .unwrap()
            .unwrap();
        assert_eq!(next.run_number, 2);
    }

    #[test]
    fn a_transferred_repo_keeps_its_history_with_the_old_tenant() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 100, 0, 0)",
            &[],
        );
        let before = db
            .wait(insert_run(db.db(), &new_run("old", "ci", 5)))
            .unwrap()
            .unwrap();
        let mut moved = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        moved.tenant_id = "t2".into();
        moved.installation_id = 100;
        db.wait(upsert_repo(db.db(), &moved, 6)).unwrap();
        assert_eq!(
            db.wait(runs_for_repo(db.db(), 7, None, 10)).unwrap().len(),
            0
        );
        let mut mine = new_run("new", "ci", 7);
        mine.tenant_id = "t2".into();
        let after = db.wait(insert_run(db.db(), &mine)).unwrap().unwrap();
        let seen = db.wait(runs_for_repo(db.db(), 7, None, 10)).unwrap();
        assert_eq!(seen.iter().map(|r| r.id).collect::<Vec<_>>(), [after.id]);
        // The old run is still there, under its own tenant.
        let old = db.wait(run(db.db(), before.id)).unwrap().unwrap();
        assert_eq!(old.tenant_id, "t1");
    }

    #[test]
    fn jobs_come_back_in_plan_order() {
        let db = setup();
        let run = db
            .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
            .unwrap()
            .unwrap();
        let ids = db
            .wait(insert_jobs(
                db.db(),
                run.id,
                &[new_job("test"), new_job("build"), new_job("deploy")],
                2,
            ))
            .unwrap();
        let plan: Vec<&str> = ids.iter().map(|(plan, _)| plan.as_str()).collect();
        assert_eq!(plan, ["test", "build", "deploy"]);
        let jobs = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
        let names: Vec<&str> = jobs.iter().map(|job| job.plan_id.as_str()).collect();
        assert_eq!(names, ["test", "build", "deploy"]);
        assert!(
            jobs.iter()
                .all(|job| job.status == "waiting" && job.attempt == 1)
        );
        assert_eq!(jobs[0].steps, serde_json::json!([]));
    }

    #[test]
    fn claim_launch_only_once() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        let first = db.wait(claim_launch(db.db(), job_id, 10)).unwrap().unwrap();
        assert_eq!(first.launch_attempts, 1);
        assert!(
            db.wait(claim_launch(db.db(), job_id, 11))
                .unwrap()
                .is_none()
        );
        let job = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(job.status, "launching");
    }

    #[test]
    fn a_failed_launch_retries_or_fails_the_job() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        db.wait(claim_launch(db.db(), job_id, 10)).unwrap().unwrap();
        db.wait(launch_failed(db.db(), job_id, "no capacity", Some(500), 20))
            .unwrap();
        let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(
            (job_row.status.as_str(), job_row.next_launch_at),
            ("queued", Some(500))
        );
        db.wait(claim_launch(db.db(), job_id, 30)).unwrap().unwrap();
        db.wait(launch_failed(db.db(), job_id, "no capacity", None, 40))
            .unwrap();
        let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(job_row.status, "completed");
        assert_eq!(job_row.conclusion.as_deref(), Some("failure"));
        assert_eq!(job_row.completed_at, Some(40));
    }

    #[test]
    fn late_calls_leave_a_completed_job_alone() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        db.wait(claim_launch(db.db(), job_id, 10)).unwrap().unwrap();
        assert!(db.wait(launched(db.db(), job_id, "i-1", 11)).unwrap());
        assert!(db.wait(start_job(db.db(), job_id, 12)).unwrap());
        let steps = serde_json::json!([{"name": "first"}]);
        assert!(
            db.wait(finish_job(
                db.db(),
                job_id,
                1,
                "success",
                &steps,
                Some("k1"),
                20
            ))
            .unwrap()
        );
        let late = serde_json::json!([{"name": "late"}]);
        assert!(
            !db.wait(finish_job(
                db.db(),
                job_id,
                1,
                "failure",
                &late,
                Some("k2"),
                30
            ))
            .unwrap()
        );
        assert!(!db.wait(start_job(db.db(), job_id, 31)).unwrap());
        assert!(!db.wait(launched(db.db(), job_id, "i-2", 32)).unwrap());
        assert!(
            !db.wait(launch_failed(db.db(), job_id, "late", Some(900), 33))
                .unwrap()
        );
        assert!(
            !db.wait(launch_failed(db.db(), job_id, "late", None, 34))
                .unwrap()
        );
        db.wait(heartbeat(db.db(), job_id, 40)).unwrap();
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(row.status, "completed");
        assert_eq!(row.conclusion.as_deref(), Some("success"));
        assert_eq!(row.steps, steps);
        assert_eq!(row.log_key.as_deref(), Some("k1"));
        assert_eq!(row.instance_id.as_deref(), Some("i-1"));
        assert_eq!(
            (row.completed_at, row.heartbeat_at, row.next_launch_at),
            (Some(20), Some(12), None)
        );
        assert_eq!(db.wait(open_jobs(db.db(), 10)).unwrap().len(), 0);
    }

    #[test]
    fn a_launch_result_for_a_job_that_is_not_launching_changes_nothing() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        assert!(!db.wait(launched(db.db(), job_id, "i-1", 11)).unwrap());
        assert!(
            !db.wait(launch_failed(db.db(), job_id, "e", None, 12))
                .unwrap()
        );
        assert!(!db.wait(start_job(db.db(), job_id, 13)).unwrap());
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(row.status, "queued");
    }

    #[test]
    fn launched_records_the_instance() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        db.wait(claim_launch(db.db(), job_id, 10)).unwrap().unwrap();
        db.wait(launched(db.db(), job_id, "i-123", 11)).unwrap();
        let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(job_row.status, "launched");
        assert_eq!(job_row.instance_id.as_deref(), Some("i-123"));
    }

    #[test]
    fn sweep_deletes_expired_runs_and_old_deliveries() {
        let db = setup();
        let old = db
            .wait(insert_run(db.db(), &new_run("old", "ci", 1_000)))
            .unwrap()
            .unwrap();
        db.wait(insert_jobs(db.db(), old.id, &[new_job("build")], 1_000))
            .unwrap();
        let fresh = db
            .wait(insert_run(
                db.db(),
                &new_run("fresh", "ci", 1_000 + 20 * DAY),
            ))
            .unwrap()
            .unwrap();
        let now = 1_000 + 31 * DAY;
        db.wait(record_delivery(db.db(), "old", "push", 1_000))
            .unwrap();
        db.wait(record_delivery(db.db(), "fresh", "push", now - DAY))
            .unwrap();
        let gone = db.wait(sweep(db.db(), now)).unwrap();
        assert_eq!(gone, vec![old.id]);
        assert!(db.wait(run(db.db(), old.id)).unwrap().is_none());
        assert!(db.wait(run(db.db(), fresh.id)).unwrap().is_some());
        let jobs: i64 = db
            .column("SELECT COUNT(*) AS n FROM ci_jobs", &[], "n")
            .unwrap();
        assert_eq!(jobs, 0);
        assert!(!db.wait(delivery_seen(db.db(), "old")).unwrap());
        assert!(db.wait(delivery_seen(db.db(), "fresh")).unwrap());
    }

    #[test]
    fn sweep_deletes_the_oldest_hundred_at_a_time() {
        let db = setup();
        for n in 0..(SWEEP_LIMIT + 5) {
            db.wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", 1_000)))
                .unwrap()
                .unwrap();
        }
        let now = 1_000 + 31 * DAY;
        assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 100);
        assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 5);
        assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 0);
    }

    #[test]
    fn stalled_jobs_match_the_reconcilers_decisions() {
        use crate::reconcile::{Fix, stalled};
        let db = setup();
        let run = db
            .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
            .unwrap()
            .unwrap();
        let names = [
            "fresh-queued",
            "quiet-queued",
            "due-queued",
            "old-launching",
            "old-launched",
            "silent-running",
            "beating-running",
            "old-waiting",
            "old-completed",
        ];
        let jobs: Vec<NewJob> = names.iter().map(|name| new_job(name)).collect();
        let ids = db.wait(insert_jobs(db.db(), run.id, &jobs, 1)).unwrap();
        let now = 10_000_000u64;
        let set =
            |name: &str, status: &str, updated: u64, heartbeat: Option<u64>, due: Option<u64>| {
                let id = ids.iter().find(|(plan, _)| plan == name).unwrap().1;
                db.run(
                    "UPDATE ci_jobs SET status = $2, updated_at = $3, heartbeat_at = $4, \
                 started_at = $3, next_launch_at = $5 WHERE id = $1",
                    &params![id, status, updated, heartbeat, due],
                );
            };
        set("fresh-queued", "queued", now - 1_000, None, None);
        set("quiet-queued", "queued", now - 61_000, None, None);
        set("due-queued", "queued", now - 1_000, None, Some(now - 1));
        set("old-launching", "launching", now - 601_000, None, None);
        set("old-launched", "launched", now - 601_000, None, None);
        set(
            "silent-running",
            "running",
            now - 1_000,
            Some(now - 121_000),
            None,
        );
        set(
            "beating-running",
            "running",
            now - 1_000,
            Some(now - 5_000),
            None,
        );
        set("old-waiting", "waiting", 0, None, None);
        set("old-completed", "completed", 0, None, None);
        let rows = db.wait(stalled_jobs(db.db(), now, 50)).unwrap();
        let mut from_sql: Vec<String> = rows.iter().map(|row| row.plan_id.clone()).collect();
        from_sql.sort();
        assert_eq!(
            from_sql,
            [
                "due-queued",
                "old-launched",
                "old-launching",
                "quiet-queued",
                "silent-running"
            ]
        );
        let all = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
        let mut from_rust: Vec<u64> = stalled(&all, now)
            .into_iter()
            .map(|fix| match fix {
                Fix::Requeue(id) | Fix::Fail(id, _) | Fix::TerminateAndFail(id, ..) => id,
            })
            .collect();
        from_rust.sort_unstable();
        let mut sql_ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        sql_ids.sort_unstable();
        assert_eq!(from_rust, sql_ids);
        assert_eq!(db.wait(stalled_jobs(db.db(), now, 2)).unwrap().len(), 2);
    }

    #[test]
    fn reset_job_clears_the_attempt() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        db.wait(claim_launch(db.db(), job_id, 10)).unwrap().unwrap();
        db.wait(start_job(db.db(), job_id, 20)).unwrap();
        let steps = serde_json::json!([{"name": "build"}]);
        db.wait(finish_job(
            db.db(),
            job_id,
            1,
            "failure",
            &steps,
            Some("logs/x"),
            30,
        ))
        .unwrap();
        let done = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(done.steps, steps);
        assert_eq!(done.log_key.as_deref(), Some("logs/x"));
        db.wait(reset_job(db.db(), job_id, 2, 40)).unwrap();
        let reset = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(reset.status, "waiting");
        assert_eq!(reset.attempt, 2);
        assert_eq!(reset.launch_attempts, 0);
        assert_eq!(reset.conclusion, None);
        assert_eq!(reset.instance_id, None);
        assert_eq!(reset.next_launch_at, None);
        assert_eq!(reset.heartbeat_at, None);
        assert_eq!(reset.started_at, None);
        assert_eq!(reset.completed_at, None);
        assert_eq!(reset.log_key, None);
        assert_eq!(reset.steps, serde_json::json!([]));
        assert_eq!(reset.updated_at, 40);
    }

    #[test]
    fn start_job_sets_the_heartbeat() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launched");
        db.wait(start_job(db.db(), job_id, 77)).unwrap();
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(row.status, "running");
        assert_eq!(row.heartbeat_at, Some(77));
        assert_eq!(row.started_at, Some(77));
        db.wait(heartbeat(db.db(), job_id, 90)).unwrap();
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!((row.heartbeat_at, row.started_at), (Some(90), Some(77)));
    }

    #[test]
    fn late_repeats_of_side_effects_change_nothing() {
        let db = setup();
        let (run_row, job_id) = run_with_job(&db, "queued");
        let steps = serde_json::json!([]);
        db.wait(reset_job(db.db(), job_id, 2, 5)).unwrap();
        db.wait(set_job(db.db(), job_id, "launching", None, 6))
            .unwrap();
        // The same reset again, and a finish of attempt 1, both late.
        db.wait(reset_job(db.db(), job_id, 2, 7)).unwrap();
        assert_eq!(
            db.wait(job(db.db(), job_id)).unwrap().unwrap().status,
            "launching"
        );
        assert!(
            !db.wait(finish_job(db.db(), job_id, 1, "failure", &steps, None, 8))
                .unwrap()
        );
        db.wait(reopen_run_at(db.db(), run_row.id, 2, 9)).unwrap();
        db.wait(finish_run(db.db(), run_row.id, 1, "failure", 10))
            .unwrap();
        let row = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.conclusion), ("in_progress", None));
        assert!(
            db.wait(finish_job(db.db(), job_id, 2, "success", &steps, None, 11))
                .unwrap()
        );
    }

    #[test]
    fn queue_job_only_moves_a_waiting_job_of_that_attempt() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "waiting");
        assert!(!db.wait(queue_job(db.db(), job_id, 2, 5)).unwrap());
        assert!(db.wait(queue_job(db.db(), job_id, 1, 5)).unwrap());
        assert!(db.wait(queue_job(db.db(), job_id, 1, 6)).unwrap());
        db.wait(set_job(db.db(), job_id, "launching", None, 7))
            .unwrap();
        assert!(!db.wait(queue_job(db.db(), job_id, 1, 8)).unwrap());
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(row.status, "launching");
    }

    #[test]
    fn reopen_run_at_sets_the_attempt_and_repeats() {
        let db = setup();
        let (run_row, _) = run_with_job(&db, "queued");
        db.wait(finish_run(db.db(), run_row.id, 1, "failure", 6))
            .unwrap();
        for _ in 0..2 {
            db.wait(reopen_run_at(db.db(), run_row.id, 2, 7)).unwrap();
            let row = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
            assert_eq!((row.run_attempt, row.status.as_str()), (2, "in_progress"));
            assert_eq!(row.conclusion, None);
        }
    }

    #[test]
    fn reopening_a_run_sets_the_attempt() {
        let db = setup();
        let (run_row, _) = run_with_job(&db, "queued");
        db.wait(run_started(db.db(), run_row.id, 5)).unwrap();
        let started = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
        assert_eq!(started.status, "in_progress");
        db.wait(finish_run(db.db(), run_row.id, 1, "failure", 6))
            .unwrap();
        let finished = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
        assert_eq!(finished.status, "completed");
        assert_eq!(finished.conclusion.as_deref(), Some("failure"));
        assert_eq!(finished.completed_at, Some(6));
        db.wait(reopen_run_at(db.db(), run_row.id, 2, 7)).unwrap();
        let reopened = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
        assert_eq!(reopened.status, "in_progress");
        assert_eq!(reopened.conclusion, None);
        assert_eq!(reopened.run_attempt, 2);
    }

    #[test]
    fn lease_is_taken_once() {
        let db = setup();
        assert!(db.wait(lease(db.db(), 1_000, 56_000)).unwrap());
        assert!(!db.wait(lease(db.db(), 1_001, 56_001)).unwrap());
        assert!(db.wait(lease(db.db(), 57_000, 112_000)).unwrap());
    }

    #[test]
    fn the_last_minute_is_kept() {
        let db = setup();
        assert_eq!(db.wait(last_minute(db.db())).unwrap(), None);
        db.wait(set_last_minute(db.db(), 60_000)).unwrap();
        assert_eq!(db.wait(last_minute(db.db())).unwrap(), Some(60_000));
    }

    #[test]
    fn replace_schedules_replaces_the_repo_rows() {
        let db = setup();
        let both = [
            ("ci".to_owned(), "0 * * * *".to_owned()),
            ("lint".to_owned(), "5 * * * *".to_owned()),
        ];
        db.wait(replace_schedules(db.db(), 7, &both)).unwrap();
        assert_eq!(db.wait(schedules(db.db(), 10)).unwrap().len(), 2);
        db.wait(replace_schedules(db.db(), 7, &both[1..])).unwrap();
        let rows = db.wait(schedules(db.db(), 10)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].repo_id, rows[0].workflow.as_str()), (7, "lint"));
        db.wait(replace_schedules(db.db(), 7, &[])).unwrap();
        assert_eq!(db.wait(schedules(db.db(), 10)).unwrap().len(), 0);
    }

    #[test]
    fn tenants_and_repos_are_found() {
        let db = setup();
        let tenant = db
            .wait(tenant_for_installation(db.db(), 99))
            .unwrap()
            .unwrap();
        assert_eq!((tenant.id.as_str(), tenant.ci_enabled), ("t1", false));
        assert!(
            db.wait(tenant_for_installation(db.db(), 1))
                .unwrap()
                .is_none()
        );
        let by_name = db.wait(repo_by_name(db.db(), "ACME/App")).unwrap().unwrap();
        assert_eq!(by_name.id, 7);
        let mut renamed = by_name;
        renamed.full_name = "acme/renamed".into();
        renamed.private = true;
        db.wait(upsert_repo(db.db(), &renamed, 2)).unwrap();
        let again = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        assert_eq!(
            (again.full_name.as_str(), again.private),
            ("acme/renamed", true)
        );
    }

    #[test]
    fn tenants_are_found_by_id_and_runs_by_delivery() {
        let db = setup();
        let found = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
        assert_eq!(found.id, "t1");
        assert!(db.wait(tenant(db.db(), "nope")).unwrap().is_none());
        let run = db
            .wait(insert_run(db.db(), &new_run("d1", "ci", 5)))
            .unwrap()
            .unwrap();
        let found = db
            .wait(run_for_delivery(db.db(), "d1", "ci"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, run.id);
        assert!(
            db.wait(run_for_delivery(db.db(), "d1", "lint"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deliveries_are_recorded_once() {
        let db = setup();
        assert!(!db.wait(delivery_seen(db.db(), "g")).unwrap());
        db.wait(record_delivery(db.db(), "g", "push", 1)).unwrap();
        db.wait(record_delivery(db.db(), "g", "push", 2)).unwrap();
        assert!(db.wait(delivery_seen(db.db(), "g")).unwrap());
    }

    #[test]
    fn runs_page_newest_first() {
        let db = setup();
        let mut ids = Vec::new();
        for n in 0..3 {
            let run = db
                .wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", n)))
                .unwrap()
                .unwrap();
            ids.push(run.id);
        }
        let page = db.wait(runs_for_repo(db.db(), 7, None, 2)).unwrap();
        assert_eq!(
            page.iter().map(|r| r.id).collect::<Vec<_>>(),
            [ids[2], ids[1]]
        );
        let next = db.wait(runs_for_repo(db.db(), 7, Some(ids[1]), 2)).unwrap();
        assert_eq!(next.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[0]]);
    }

    #[test]
    fn open_jobs_skip_completed_ones_and_oldest_come_first() {
        let db = setup();
        let run = db
            .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
            .unwrap()
            .unwrap();
        let ids = db
            .wait(insert_jobs(
                db.db(),
                run.id,
                &[new_job("a"), new_job("b"), new_job("c")],
                100,
            ))
            .unwrap();
        db.wait(set_job(db.db(), ids[0].1, "queued", None, 300))
            .unwrap();
        db.wait(set_job(db.db(), ids[1].1, "queued", None, 200))
            .unwrap();
        db.wait(set_job(
            db.db(),
            ids[2].1,
            "completed",
            Some("success"),
            400,
        ))
        .unwrap();
        let open = db.wait(open_jobs(db.db(), 10)).unwrap();
        assert_eq!(
            open.iter().map(|j| j.id).collect::<Vec<_>>(),
            [ids[1].1, ids[0].1]
        );
        let some = db
            .wait(jobs_by_ids(db.db(), &[ids[2].1, ids[0].1]))
            .unwrap();
        assert_eq!(some.len(), 2);
        assert_eq!(db.wait(jobs_by_ids(db.db(), &[])).unwrap().len(), 0);
        let done = db.wait(job(db.db(), ids[2].1)).unwrap().unwrap();
        assert_eq!(done.completed_at, Some(400));
    }
}
