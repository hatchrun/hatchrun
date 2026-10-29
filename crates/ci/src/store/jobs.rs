// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs: their rows, launch claims and holds, and how they finish.

use std::collections::BTreeMap;

use hatchrun_cloud::db::{Db, Param, params};
use hatchrun_workflow::{Level, SecretRefs, templates};
use serde::Deserialize;
use serde::de::{Deserializer, Error as _};
use worker::Result;

use super::with_columns;

/// A job to record, from a planned job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// The planned job's id in its workflow.
    pub plan_id: String,
    /// Its display name.
    pub name: String,
    /// The most minutes it may run.
    pub timeout_minutes: u32,
    /// The first `runs-on` label of the job's runner template; `None`
    /// for an approval job.
    pub template: Option<String>,
    /// The App permissions of the job's token.
    pub token_permissions: BTreeMap<String, Level>,
    /// The secrets the job reads.
    pub secret_refs: SecretRefs,
    /// The environment the job names.
    pub environment: Option<String>,
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
    /// The first `runs-on` label of the job's runner template; `None`
    /// for an approval job.
    pub template: Option<String>,
    /// The App permissions of the job's token, a JSON object.
    #[serde(deserialize_with = "json_text")]
    pub token_permissions: serde_json::Value,
    /// The secrets the job reads, a JSON object.
    #[serde(deserialize_with = "json_text")]
    pub secret_refs: serde_json::Value,
    /// Why the queued job waits for a launch: `card`, `spend_cap`,
    /// `concurrency` or `approval`.
    pub held: Option<String>,
    /// The environment the job names.
    pub environment: Option<String>,
    /// The attempt the approval below is for.
    pub approval_attempt: Option<u32>,
    /// `approved` or `rejected`.
    pub approval_decision: Option<String>,
    /// The GitHub login of whoever decided.
    pub approval_by: Option<String>,
    /// When they decided.
    pub approval_at: Option<u64>,
}

/// Reads a JSON document that the row carries as text.
fn json_text<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<serde_json::Value, D::Error> {
    let text = String::deserialize(d)?;
    serde_json::from_str(&text).map_err(D::Error::custom)
}

/// The columns of a [`JobRow`].
pub(super) const JOB_COLUMNS: &str = "id, run_id, plan_id, name, status, conclusion, instance_id, attempt, \
     launch_attempts, next_launch_at, timeout_minutes, updated_at, heartbeat_at, started_at, \
     completed_at, steps::text AS steps, log_key, template, \
     token_permissions::text AS token_permissions, secret_refs::text AS secret_refs, held, \
     environment, approval_attempt, approval_decision, approval_by, approval_at";

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
     held = NULL, steps = '[]', updated_at = $3 WHERE id = $1 AND attempt < $2";

/// Queues attempt `$2` of job `$1` at `$3`. Only a job of that attempt
/// that is waiting, or already queued, changes: one that is launching
/// or launched is left alone, so repeating the call launches nothing
/// twice.
const QUEUE_JOB: &str = "UPDATE ci_jobs SET status = 'queued', updated_at = $3 \
     WHERE id = $1 AND attempt = $2 AND status IN ('waiting', 'queued')";

/// Sets job `$1` to status `$2` and conclusion `$3` at `$4`. A
/// completed job takes the time as its completion.
const SET_JOB: &str = "UPDATE ci_jobs SET status = $2, conclusion = $3, updated_at = $4, \
     completed_at = CASE WHEN $2 = 'completed' THEN $4 ELSE completed_at END WHERE id = $1";

/// Locks the row of tenant `$1`, so the launches of one tenant are
/// counted against the concurrency cap one at a time.
pub(super) const LOCK_TENANT: &str = "SELECT id FROM tenants WHERE id = $1 FOR UPDATE";

/// Takes the launch of job `$1` at `$2`, if it is queued and fewer than
/// `$4` jobs of tenant `$3` are launching, launched or running, and no
/// older job of the tenant waits for a place (held for `concurrency`),
/// so new jobs do not jump the line; run after [`LOCK_TENANT`]. The
/// hold is over.
const CLAIM_LAUNCH: &str = "UPDATE ci_jobs SET status = 'launching', held = NULL, \
     launch_attempts = launch_attempts + 1, updated_at = $2 \
     WHERE id = $1 AND status = 'queued' \
     AND (SELECT count(*) FROM ci_jobs other JOIN ci_runs ON ci_runs.id = other.run_id \
          WHERE ci_runs.tenant_id = $3 \
          AND other.status IN ('launching', 'launched', 'running')) < $4 \
     AND NOT EXISTS (SELECT 1 FROM ci_jobs older JOIN ci_runs older_run \
          ON older_run.id = older.run_id \
          WHERE older_run.tenant_id = $3 AND older.status = 'queued' \
          AND older.held = 'concurrency' AND older.id < ci_jobs.id) \
     RETURNING {columns}";

/// Keeps attempt `$2` of queued job `$1` held for reason `$3` until
/// `$4`, at `$5`. Returns whether the reason changed. An approval hold
/// never lands on an attempt that has a decision already: the consumer
/// may have read the job before the admin answered.
const HOLD: &str = "WITH prior AS (SELECT held FROM ci_jobs \
       WHERE id = $1 AND attempt = $2 AND status = 'queued' \
       AND NOT ($3::text = 'approval' AND approval_attempt = attempt \
                AND approval_decision IS NOT NULL) FOR UPDATE) \
     UPDATE ci_jobs SET held = $3::text, next_launch_at = $4, updated_at = $5 \
     FROM prior WHERE ci_jobs.id = $1 \
     RETURNING (prior.held IS DISTINCT FROM $3::text) AS changed";

/// Records `$2` as the launch error of job `$1` while it is queued, at `$3`.
const NOTE_QUEUED_ERROR: &str = "UPDATE ci_jobs SET launch_error = $2, updated_at = $3 \
     WHERE id = $1 AND status = 'queued'";

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

/// Records the usage of attempt `$3` of job `$1`, completed at `$2`,
/// from the row as the update before it left it: nothing is written
/// when that update did not apply (another attempt, or a job completed
/// already), or the job never started or has no template. Seconds run
/// from start to completion, rounded up, at least 60. `$4` is the JSON
/// map of label to price per minute and `$5` the price of one 2 vCPU
/// minute, so the units (one 2 vCPU second each) are those of the price
/// when the job ended; a label the map lacks records nothing.
const RECORD_USAGE: &str = "INSERT INTO ci_usage \
       (job_id, attempt, tenant_id, template, seconds, units, ended_at) \
     SELECT ci_jobs.id, ci_jobs.attempt, ci_runs.tenant_id, ci_jobs.template, \
       s.seconds, s.seconds * ($4::jsonb ->> ci_jobs.template)::bigint / $5, \
       ci_jobs.completed_at \
     FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
       CROSS JOIN LATERAL (SELECT GREATEST(60, \
         CEIL((ci_jobs.completed_at - ci_jobs.started_at) / 1000.0))::bigint AS seconds) s \
     WHERE ci_jobs.id = $1 AND ci_jobs.attempt = $3 AND ci_jobs.status = 'completed' \
       AND ci_jobs.completed_at = $2 AND ci_jobs.started_at IS NOT NULL \
       AND ($4::jsonb ->> ci_jobs.template) IS NOT NULL \
     ON CONFLICT DO NOTHING";

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
    let mut highest = 0;
    for (i, job) in jobs.iter().enumerate() {
        let base = 3 + i * 7;
        highest = base + 6;
        values.push(format!(
            "($1, ${}, ${}, ${}, ${}, ${}::jsonb, ${}::jsonb, ${}, 'waiting', $2, $2)",
            base,
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6
        ));
        bound.push(Param::from(&job.plan_id));
        bound.push(Param::from(&job.name));
        bound.push(Param::from(job.timeout_minutes));
        bound.push(Param::from(job.template.as_deref()));
        bound.push(Param::from(
            serde_json::to_string(&job.token_permissions).map_err(worker::Error::from)?,
        ));
        bound.push(Param::from(
            serde_json::to_string(&job.secret_refs).map_err(worker::Error::from)?,
        ));
        bound.push(Param::from(job.environment.as_deref()));
    }
    debug_assert_eq!(bound.len(), highest);
    let sql = format!(
        "INSERT INTO ci_jobs (run_id, plan_id, name, timeout_minutes, template, \
         token_permissions, secret_refs, environment, status, created_at, updated_at) \
         VALUES {} RETURNING plan_id, id",
        values.join(", ")
    );
    let mut inserted: Vec<Inserted> = db.all(&sql, &bound).await?;
    inserted.sort_by_key(|row| row.id);
    Ok(inserted
        .into_iter()
        .map(|row| (row.plan_id, row.id))
        .collect())
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

/// What [`claim_launch`] found.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // one value per launch, never stored
pub enum Claim {
    /// The job is launching now, one more launch attempt.
    Claimed(JobRow),
    /// The job is queued, but the tenant runs its most jobs already.
    Busy,
    /// Somebody else took the launch, or the job moved on.
    Gone,
}

/// Takes the launch of job `job_id` of tenant `tenant_id` if it is
/// queued: launching, one more launch attempt, no hold. The tenant's
/// row is locked first, so two consumers cannot both take the last of
/// `max_running` places: the job is claimed only while fewer than
/// `max_running` of the tenant's jobs are launching, launched or
/// running.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn claim_launch(
    db: &Db,
    job_id: u64,
    tenant_id: &str,
    max_running: u32,
    now: u64,
) -> Result<Claim> {
    let lock = params![tenant_id];
    let claimed: Option<JobRow> = db
        .first_locked(
            (LOCK_TENANT, &lock),
            &with_columns(CLAIM_LAUNCH, JOB_COLUMNS),
            &params![job_id, now, tenant_id, max_running],
        )
        .await?;
    if let Some(job) = claimed {
        return Ok(Claim::Claimed(job));
    }
    Ok(match job(db, job_id).await? {
        Some(row) if row.status == "queued" => Claim::Busy,
        _ => Claim::Gone,
    })
}

/// Keeps attempt `attempt` of job `job_id` queued with reason `hold`
/// until `retry_at`, and bumps its `updated_at`. Returns whether the
/// reason changed; another attempt, or a job that is not queued, is
/// left alone and the answer is `false`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn hold(
    db: &Db,
    job_id: u64,
    attempt: u32,
    hold: &str,
    retry_at: u64,
    now: u64,
) -> Result<bool> {
    #[derive(Deserialize)]
    struct Changed {
        changed: bool,
    }
    Ok(db
        .first::<Changed>(HOLD, &params![job_id, attempt, hold, retry_at, now])
        .await?
        .is_some_and(|row| row.changed))
}

/// Records `error` as the launch error of job `job_id` while it is
/// still queued; returns whether it was.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn note_queued_error(db: &Db, job_id: u64, error: &str, now: u64) -> Result<bool> {
    Ok(db
        .run(NOTE_QUEUED_ERROR, &params![job_id, error, now])
        .await?
        == 1)
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
/// completed it. In the same transaction a job that was claimed gets
/// its seconds recorded for this attempt, once.
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
    let changes = db
        .batch(&[
            (
                FINISH_JOB,
                params![job_id, conclusion, steps.to_string(), log_key, now, attempt],
            ),
            (
                RECORD_USAGE,
                params![
                    job_id,
                    now,
                    attempt,
                    template_prices(),
                    templates::UNIT_PRICE_MICROS_PER_MINUTE
                ],
            ),
        ])
        .await?;
    Ok(changes.first() == Some(&1))
}

/// The price per minute of every template label, as JSON text.
fn template_prices() -> String {
    let prices: BTreeMap<&str, u64> = templates::TEMPLATES
        .iter()
        .flat_map(|t| {
            t.labels
                .iter()
                .map(|label| (*label, t.price_micros_per_minute))
        })
        .collect();
    serde_json::to_string(&prices).unwrap_or_default()
}
