// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs: their rows, launch claims and holds, and how they finish.

use std::collections::BTreeMap;

use hatchrun_cloud::ci_wake::Woken;
use hatchrun_cloud::db::{Db, Param, params};
use hatchrun_workflow::{Level, SecretRefs, templates};
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
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
    /// The matrix the job belongs to, shared by its jobs within a run.
    pub strategy_group: Option<String>,
    /// The most jobs of the group that run at once.
    pub max_parallel: Option<u32>,
    /// The plan ids of the jobs it waits on.
    pub needs: Vec<String>,
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
    /// How many of those launches EC2 refused for a limit or capacity
    /// that clears on its own; they do not use up the launch attempts.
    pub capacity_attempts: u32,
    /// When the job first waited for EC2 capacity, until it launches.
    pub capacity_since: Option<u64>,
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
    /// `concurrency` (the tenant's running jobs), `approval`, `group` (a
    /// concurrency group of the workflow or job) or `max_parallel`.
    pub held: Option<String>,
    /// The environment the job names.
    pub environment: Option<String>,
    /// Where the job's deployment can be seen, once it ended well.
    pub environment_url: Option<String>,
    /// The attempt the approval below is for.
    pub approval_attempt: Option<u32>,
    /// `approved` or `rejected`.
    pub approval_decision: Option<String>,
    /// The GitHub login of whoever decided.
    pub approval_by: Option<String>,
    /// When they decided.
    pub approval_at: Option<u64>,
    /// The plan ids of the jobs it waits on, a JSON array.
    #[serde(deserialize_with = "json_text")]
    pub needs: serde_json::Value,
}

/// Reads a JSON document that the row carries as text.
fn json_text<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<serde_json::Value, D::Error> {
    let text = String::deserialize(d)?;
    serde_json::from_str(&text).map_err(D::Error::custom)
}

/// The columns of a [`JobRow`].
pub(super) const JOB_COLUMNS: &str = "id, run_id, plan_id, name, status, conclusion, instance_id, attempt, \
     launch_attempts, capacity_attempts, capacity_since, next_launch_at, timeout_minutes, updated_at, heartbeat_at, started_at, \
     completed_at, steps::text AS steps, log_key, template, \
     token_permissions::text AS token_permissions, secret_refs::text AS secret_refs, held, \
     environment, environment_url, approval_attempt, approval_decision, approval_by, \
     approval_at, needs::text AS needs";

/// The jobs of run `$1` in plan order, without those a rerun left out
/// of their matrix.
const JOBS_FOR_RUN: &str =
    "SELECT {columns} FROM ci_jobs WHERE run_id = $1 AND NOT superseded ORDER BY id";

/// Every row of run `$1`, those a rerun left out of their matrix too.
const ALL_JOBS_FOR_RUN: &str = "SELECT {columns} FROM ci_jobs WHERE run_id = $1 ORDER BY id";

/// Gives job `$1` the template `$2`, strategy group `$3`, `max_parallel`
/// `$4` and needs `$5` of the job it is made again as, at `$6`.
const REFRESH_JOB: &str = "UPDATE ci_jobs SET template = $2, strategy_group = $3, \
     max_parallel = $4, needs = $5::jsonb, updated_at = $6 WHERE id = $1";

/// Marks job `$1`, which a rerun left out of its matrix for attempt
/// `$2`, so the run's job list and counts leave it out.
const SUPERSEDE_JOB: &str = "UPDATE ci_jobs SET superseded = true WHERE id = $1 AND attempt = $2";

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
     capacity_attempts = 0, capacity_since = NULL, conclusion = NULL, instance_id = NULL, launch_error = NULL, next_launch_at = NULL, \
     heartbeat_at = NULL, started_at = NULL, completed_at = NULL, log_key = NULL, \
     held = NULL, steps = '[]', environment_url = NULL, superseded = false, updated_at = $3 \
     WHERE id = $1 AND attempt < $2";

/// Makes job `$1`, the stand-in of a job that reads its matrix or
/// `runs-on` when it is ready, the first job it expanded to: its plan
/// id `$2`, template `$3`, strategy group `$4`, `max_parallel` `$5` and
/// needs `$6`, at attempt `$7` or later, at `$8`. Repeating it changes
/// nothing more.
const RETARGET_JOB: &str = "UPDATE ci_jobs SET plan_id = $2, template = $3, strategy_group = $4, \
     max_parallel = $5, needs = $6::jsonb, attempt = GREATEST(attempt, $7), updated_at = $8 \
     WHERE id = $1";

/// Sets the plan ids job `$1` waits on to `$2`, a JSON array, at `$3`.
const SET_NEEDS: &str = "UPDATE ci_jobs SET needs = $2::jsonb, updated_at = $3 WHERE id = $1";

/// Queues attempt `$2` of job `$1` at `$3`, with the environment `$4` the
/// graph settled on when there is one. Only a job of that attempt
/// that is waiting, or already queued, changes: one that is launching
/// or launched is left alone, so repeating the call launches nothing
/// twice.
const QUEUE_JOB: &str = "UPDATE ci_jobs SET status = 'queued', \
     environment = COALESCE($4::text, environment), updated_at = $3 \
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
/// hold is over. With `$5` a job with a `max_parallel` also needs fewer
/// than that many launching, launched or running jobs in its group, and
/// no older job of the group held for `max_parallel`.
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
     AND (NOT $5::boolean OR ci_jobs.max_parallel IS NULL OR \
          ((SELECT count(*) FROM ci_jobs sibling \
            WHERE sibling.run_id = ci_jobs.run_id \
            AND sibling.strategy_group = ci_jobs.strategy_group \
            AND sibling.status IN ('launching', 'launched', 'running')) < ci_jobs.max_parallel \
           AND NOT EXISTS (SELECT 1 FROM ci_jobs older_sibling \
            WHERE older_sibling.run_id = ci_jobs.run_id \
            AND older_sibling.strategy_group = ci_jobs.strategy_group \
            AND older_sibling.status = 'queued' AND older_sibling.held = 'max_parallel' \
            AND older_sibling.id < ci_jobs.id))) \
     RETURNING {columns}";

/// Whether queued job `$1` is over its group's limit now, and the limit:
/// the same test as [`CLAIM_LAUNCH`], for telling a held group from a
/// full tenant. Run in the claim's transaction, after it and under the
/// same tenant lock, so the answer is about the state the claim saw.
const GROUP_FULL: &str = "SELECT ci_jobs.max_parallel AS max_parallel FROM ci_jobs \
     WHERE ci_jobs.id = $1 AND ci_jobs.status = 'queued' \
     AND ci_jobs.max_parallel IS NOT NULL \
     AND ((SELECT count(*) FROM ci_jobs sibling \
           WHERE sibling.run_id = ci_jobs.run_id \
           AND sibling.strategy_group = ci_jobs.strategy_group \
           AND sibling.status IN ('launching', 'launched', 'running')) >= ci_jobs.max_parallel \
          OR EXISTS (SELECT 1 FROM ci_jobs older_sibling \
           WHERE older_sibling.run_id = ci_jobs.run_id \
           AND older_sibling.strategy_group = ci_jobs.strategy_group \
           AND older_sibling.status = 'queued' AND older_sibling.held = 'max_parallel' \
           AND older_sibling.id < ci_jobs.id))";

/// The oldest queued job held for `max_parallel` in the group of job
/// `$1`, at most `$2`: what a job of the group ending makes room for.
const HELD_SIBLINGS: &str = "SELECT held.id AS job_id, held.attempt FROM ci_jobs ended \
     JOIN ci_jobs held ON held.run_id = ended.run_id \
       AND held.strategy_group = ended.strategy_group \
     WHERE ended.id = $1 AND ended.strategy_group IS NOT NULL \
     AND held.status = 'queued' AND held.held = 'max_parallel' \
     ORDER BY held.id LIMIT $2";

/// Keeps attempt `$2` of queued job `$1` held for reason `$3` until
/// `$4`, at `$5`. Returns whether the reason changed. An approval hold
/// never lands on an attempt that has a decision already: the consumer
/// may have read the job before the admin answered.
const HOLD: &str = "WITH prior AS (SELECT held FROM ci_jobs \
       WHERE id = $1 AND attempt = $2 AND status = 'queued' \
       AND NOT ($3::text = 'approval' AND approval_attempt = attempt \
                AND approval_decision IS NOT NULL) FOR UPDATE) \
     UPDATE ci_jobs SET held = $3::text, next_launch_at = $4, updated_at = $5, \
       capacity_since = CASE WHEN $3::text = 'capacity' THEN capacity_since ELSE NULL END \
     FROM prior WHERE ci_jobs.id = $1 \
     RETURNING (prior.held IS DISTINCT FROM $3::text) AS changed";

/// Records `$2` as the launch error of job `$1` while it is queued, at `$3`.
const NOTE_QUEUED_ERROR: &str = "UPDATE ci_jobs SET launch_error = $2, updated_at = $3 \
     WHERE id = $1 AND status = 'queued'";

/// Records the instance `$2` of job `$1` at `$3`.
const LAUNCHED: &str = "UPDATE ci_jobs SET status = 'launched', instance_id = $2, \
     launch_error = NULL, next_launch_at = NULL, capacity_since = NULL, updated_at = $3 \
     WHERE id = $1 AND status = 'launching'";

/// Queues job `$1` again, held for `capacity`, after EC2 refused its
/// launch for a limit or capacity that clears on its own: error `$2`,
/// not before `$3`, at `$4`. The launch does not count against the
/// launch attempts, and `$4` is when the wait began if it had not.
const CAPACITY_WAIT: &str = "UPDATE ci_jobs SET status = 'queued', held = 'capacity', \
     launch_error = $2, next_launch_at = $3, capacity_attempts = capacity_attempts + 1, \
     capacity_since = COALESCE(capacity_since, $4), updated_at = $4 \
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
     steps = $3::jsonb, log_key = $4, completed_at = $5, updated_at = $5, \
     environment_url = $7::text \
     WHERE id = $1 AND status <> 'completed' AND attempt = $6";

/// Records the usage of attempt `$3` of job `$1`, completed at `$2`,
/// from the row as the update before it left it: nothing is written
/// when that update did not apply (another attempt, or a job completed
/// already), or the job never started or has no template. A beta
/// tenant's row is written already rejected, so Polar never gets it. Seconds run
/// from start to completion, rounded up, at least 60. `$4` is the JSON
/// map of label to price per minute and `$5` the price of one 2 vCPU
/// minute, so the units (one 2 vCPU second each) are those of the price
/// when the job ended; a label the map lacks records nothing.
const RECORD_USAGE: &str = "INSERT INTO ci_usage \
       (job_id, attempt, tenant_id, template, seconds, units, ended_at, \
        rejected_at, reject_reason) \
     SELECT ci_jobs.id, ci_jobs.attempt, ci_runs.tenant_id, ci_jobs.template, \
       s.seconds, s.seconds * ($4::jsonb ->> ci_jobs.template)::bigint / $5, \
       ci_jobs.completed_at, \
       CASE WHEN tenants.beta THEN ci_jobs.completed_at END, \
       CASE WHEN tenants.beta THEN 'beta tenant, not billed' END \
     FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
       JOIN tenants ON tenants.id = ci_runs.tenant_id \
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
        let base = 3 + i * 10;
        highest = base + 9;
        values.push(format!(
            "($1, ${}, ${}, ${}, ${}, ${}::jsonb, ${}::jsonb, ${}, ${}, ${}, ${}::jsonb, 'waiting', $2, $2)",
            base,
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9
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
        bound.push(Param::from(job.strategy_group.as_deref()));
        bound.push(Param::from(job.max_parallel));
        bound.push(Param::from(
            serde_json::to_string(&job.needs).map_err(worker::Error::from)?,
        ));
    }
    debug_assert_eq!(bound.len(), highest);
    let sql = format!(
        "INSERT INTO ci_jobs (run_id, plan_id, name, timeout_minutes, template, \
         token_permissions, secret_refs, environment, strategy_group, max_parallel, needs, status, \
         created_at, updated_at) \
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

/// Makes job `job_id` the first job it expanded to, `new`, at
/// `attempt`. See [`RETARGET_JOB`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn retarget_job(
    db: &Db,
    job_id: u64,
    new: &NewJob,
    attempt: u32,
    now: u64,
) -> Result<()> {
    let needs = serde_json::to_string(&new.needs).map_err(worker::Error::from)?;
    db.run(
        RETARGET_JOB,
        &params![
            job_id,
            &new.plan_id,
            new.template.as_deref(),
            new.strategy_group.as_deref(),
            new.max_parallel,
            needs,
            attempt,
            now
        ],
    )
    .await?;
    Ok(())
}

/// Sets the plan ids job `job_id` waits on.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_needs(db: &Db, job_id: u64, needs: &[String], now: u64) -> Result<()> {
    let needs = serde_json::to_string(needs).map_err(worker::Error::from)?;
    db.run(SET_NEEDS, &params![job_id, needs, now]).await?;
    Ok(())
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

/// Every row of `run_id` in plan order, the jobs a rerun left out of
/// their matrix included: what makes a job's row again, or ends it as
/// skipped, has to find them.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn all_jobs_for_run(db: &Db, run_id: u64) -> Result<Vec<JobRow>> {
    db.all(
        &with_columns(ALL_JOBS_FOR_RUN, JOB_COLUMNS),
        &params![run_id],
    )
    .await
}

/// The rows the jobs of a matrix run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixRows {
    /// The row of each job, in the order the jobs were given.
    pub ids: Vec<u64>,
    /// The rows among them that an earlier attempt used and that need
    /// resetting for this one.
    pub again: Vec<u64>,
}

/// Finds or makes the row of each job of a matrix that `stand_in` (a
/// row) read when it was ready, for `attempt`.
///
/// A job takes the row that has its plan id, whichever it is, and the
/// template and limits of the job it is made as. A job with no row takes
/// the stand-in's own row when nothing else uses it, else a new row. So a
/// matrix that changes from `[A, B]` to `[B]` keeps the row of `B` and
/// does not rename the row of `A` to `B` beside it, which the plan id
/// being unique per run would refuse. A row left over is the caller's to
/// end as skipped.
///
/// # Errors
///
/// Postgres refused a statement, or a row was not made.
pub async fn assign_matrix_rows(
    db: &Db,
    run_id: u64,
    stand_in: u64,
    jobs: &[NewJob],
    attempt: u32,
    now: u64,
) -> Result<MatrixRows> {
    let existing = all_jobs_for_run(db, run_id).await?;
    let by_plan: BTreeMap<&str, &JobRow> =
        existing.iter().map(|r| (r.plan_id.as_str(), r)).collect();
    let mut rows: Vec<Option<u64>> = Vec::with_capacity(jobs.len());
    let mut again = Vec::new();
    let mut stand_in_free = true;
    for new in jobs {
        let Some(row) = by_plan.get(new.plan_id.as_str()) else {
            rows.push(None);
            continue;
        };
        if row.id == stand_in {
            stand_in_free = false;
        }
        refresh_job(db, row.id, new, now).await?;
        if row.attempt < attempt && row.id != stand_in {
            again.push(row.id);
        }
        rows.push(Some(row.id));
    }
    let mut fresh = Vec::new();
    for (new, row) in jobs.iter().zip(&mut rows) {
        if row.is_some() {
            continue;
        }
        if stand_in_free {
            retarget_job(db, stand_in, new, attempt, now).await?;
            *row = Some(stand_in);
            stand_in_free = false;
        } else {
            fresh.push(new.clone());
        }
    }
    let inserted = insert_jobs(db, run_id, &fresh, now).await?;
    let mut ids = Vec::with_capacity(jobs.len());
    for (new, row) in jobs.iter().zip(rows) {
        if let Some(id) = row {
            ids.push(id);
            continue;
        }
        let (_, id) = inserted
            .iter()
            .find(|(plan, _)| *plan == new.plan_id)
            .ok_or_else(|| worker::Error::RustError("a job of the matrix has no row".into()))?;
        if attempt > 1 {
            reset_job(db, *id, attempt, now).await?;
        }
        ids.push(*id);
    }
    Ok(MatrixRows { ids, again })
}

/// Gives job `job_id`, made again by a matrix that was read again, what
/// the job it is made as plans: its template, strategy group, limit and
/// needs. The attempt and the result are left to the reset.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn refresh_job(db: &Db, job_id: u64, new: &NewJob, now: u64) -> Result<()> {
    db.run(
        REFRESH_JOB,
        &params![
            job_id,
            new.template.as_deref(),
            new.strategy_group.as_deref(),
            new.max_parallel,
            serde_json::to_string(&new.needs).map_err(worker::Error::from)?,
            now
        ],
    )
    .await?;
    Ok(())
}

/// Marks job `job_id`, which a rerun left out of its matrix for
/// `attempt`, so the run's job list and counts leave it out. Nothing
/// changes when the job is on another attempt.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn supersede_job(db: &Db, job_id: u64, attempt: u32) -> Result<()> {
    db.run(SUPERSEDE_JOB, &params![job_id, attempt]).await?;
    Ok(())
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
/// returns whether the job is queued for that attempt now. `environment`
/// is the name its environment settled on, when it has one. Repeating
/// the call is safe; a job already launching or launched stays so and
/// the answer is `false`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn queue_job(
    db: &Db,
    job_id: u64,
    attempt: u32,
    environment: Option<&str>,
    now: u64,
) -> Result<bool> {
    Ok(db
        .run(QUEUE_JOB, &params![job_id, attempt, now, environment])
        .await?
        == 1)
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
    /// The job is queued, but its matrix runs `max_parallel` jobs
    /// already, or an older job of it waits for a place.
    Capped {
        /// The group's limit.
        max_parallel: u32,
    },
    /// Somebody else took the launch, or the job moved on.
    Gone,
}

/// Takes the launch of job `job_id` of tenant `tenant_id` if it is
/// queued: launching, one more launch attempt, no hold. The tenant's
/// row is locked first, so two consumers cannot both take the last of
/// `max_running` places: the job is claimed only while fewer than
/// `max_running` of the tenant's jobs are launching, launched or
/// running. With `groups` a job of a matrix with `max-parallel` also
/// waits for a place in its group: see [`Claim::Capped`]. A job that is
/// only being ended takes no place and passes `false`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn claim_launch(
    db: &Db,
    job_id: u64,
    tenant_id: &str,
    max_running: u32,
    groups: bool,
    now: u64,
) -> Result<Claim> {
    #[derive(Deserialize)]
    struct Full {
        max_parallel: u32,
    }
    let lock = params![tenant_id];
    // The reason a claim fails is decided in the claim's transaction,
    // under the tenant lock: a group count read after the lock was
    // released could show a full group when the tenant cap was the cause.
    let (claimed, full): (Option<JobRow>, Option<Full>) = db
        .first_locked_then(
            (LOCK_TENANT, &lock),
            (
                &with_columns(CLAIM_LAUNCH, JOB_COLUMNS),
                &params![job_id, now, tenant_id, max_running, groups],
            ),
            (GROUP_FULL, &params![job_id]),
        )
        .await?;
    if let Some(job) = claimed {
        return Ok(Claim::Claimed(job));
    }
    let Some(row) = job(db, job_id).await? else {
        return Ok(Claim::Gone);
    };
    if row.status != "queued" {
        return Ok(Claim::Gone);
    }
    if groups && let Some(full) = full {
        return Ok(Claim::Capped {
            max_parallel: full.max_parallel,
        });
    }
    Ok(Claim::Busy)
}

/// The oldest queued job held for `max_parallel` in the group of job
/// `job_id`, to wake when `job_id` ends and leaves a place.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn held_siblings(db: &Db, job_id: u64) -> Result<Vec<Woken>> {
    db.all(HELD_SIBLINGS, &params![job_id, 1_u32]).await
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

/// Queues job `job_id` again, held for `capacity`, after a launch EC2
/// refused for a limit or capacity that clears on its own: `error`, not
/// before `retry_at`. Only a launching job changes; returns whether it
/// did.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn capacity_wait(
    db: &Db,
    job_id: u64,
    error: &str,
    retry_at: u64,
    now: u64,
) -> Result<bool> {
    Ok(db
        .run(CAPACITY_WAIT, &params![job_id, error, retry_at, now])
        .await?
        == 1)
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
/// `steps`, the key of its finished log and the URL of its deployment. A completed job, and a job
/// of another attempt, keep what they have; returns whether this call
/// completed it. In the same transaction a job that was claimed gets
/// its seconds recorded for this attempt, once.
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)] // one column each
pub async fn finish_job(
    db: &Db,
    job_id: u64,
    attempt: u32,
    conclusion: &str,
    steps: &serde_json::Value,
    log_key: Option<&str>,
    environment_url: Option<&str>,
    now: u64,
) -> Result<bool> {
    let changes = db
        .batch(&[
            (
                FINISH_JOB,
                params![
                    job_id,
                    conclusion,
                    steps.to_string(),
                    log_key,
                    now,
                    attempt,
                    environment_url
                ],
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

/// How many jobs of a run are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct JobCounts {
    /// All jobs.
    pub total: u64,
    /// Completed with `success`.
    pub passed: u64,
    /// Completed with `failure`.
    pub failed: u64,
    /// Launching or running.
    pub running: u64,
    /// Not started: waiting on another job or queued.
    pub waiting: u64,
    /// Completed with `cancelled`.
    pub cancelled: u64,
    /// Completed with `skipped`.
    pub skipped: u64,
}

impl JobCounts {
    /// Counts `n` jobs in `status` with `conclusion`.
    pub fn add(&mut self, status: &str, conclusion: Option<&str>, n: u64) {
        self.total += n;
        match (status, conclusion) {
            ("completed", Some("success")) => self.passed += n,
            ("completed", Some("failure")) => self.failed += n,
            ("completed", Some("cancelled")) => self.cancelled += n,
            ("completed", Some("skipped")) => self.skipped += n,
            ("launching" | "launched" | "running", _) => self.running += n,
            ("waiting" | "queued", _) => self.waiting += n,
            _ => {}
        }
    }

    /// The counts of `jobs`.
    #[must_use]
    pub fn of(jobs: &[JobRow]) -> Self {
        let mut counts = Self::default();
        for job in jobs {
            counts.add(&job.status, job.conclusion.as_deref(), 1);
        }
        counts
    }
}

/// The job counts of each run in `run_ids`; a run with no jobs is not
/// in the answer.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn job_counts(db: &Db, run_ids: &[u64]) -> Result<BTreeMap<u64, JobCounts>> {
    #[derive(Deserialize)]
    struct Row {
        run_id: u64,
        status: String,
        conclusion: Option<String>,
        n: u64,
    }
    if run_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ids: Vec<String> = run_ids.iter().map(u64::to_string).collect();
    let rows: Vec<Row> = db
        .all(
            "SELECT run_id, status, conclusion, count(*) AS n FROM ci_jobs \
             WHERE run_id = ANY (string_to_array($1, ',')::bigint[]) AND NOT superseded \
             GROUP BY run_id, status, conclusion",
            &params![ids.join(",")],
        )
        .await?;
    let mut counts: BTreeMap<u64, JobCounts> = BTreeMap::new();
    for row in rows {
        counts
            .entry(row.run_id)
            .or_default()
            .add(&row.status, row.conclusion.as_deref(), row.n);
    }
    Ok(counts)
}

/// A job that waits for an approval, with the run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApprovalJob {
    /// The job.
    pub job_id: u64,
    /// Its name.
    pub job_name: String,
    /// The environment it waits on.
    pub environment: Option<String>,
    /// When the job began to wait.
    pub since: u64,
    /// The run.
    #[serde(flatten)]
    pub run: super::runs::ListedRun,
}

/// The jobs of `tenant` held for approval, the longest waiting first,
/// at most `limit`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn approval_jobs(db: &Db, tenant: &str, limit: u32) -> Result<Vec<ApprovalJob>> {
    db.all(
        &format!(
            "SELECT j.id AS job_id, j.name AS job_name, j.environment, j.updated_at AS since, \
             {columns} \
             FROM ci_jobs j, ci_runs r, ci_repos p \
             WHERE j.run_id = r.id AND p.id = r.repo_id AND p.tenant_id = r.tenant_id \
             AND p.tenant_id = $1 AND j.status = 'queued' AND j.held = 'approval' \
             ORDER BY j.updated_at, j.id LIMIT $2",
            columns = super::runs::LISTED_COLUMNS
        ),
        &params![tenant, limit],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::JobCounts;

    #[test]
    fn counts_split_the_completed_conclusions() {
        let mut counts = JobCounts::default();
        counts.add("completed", Some("success"), 2);
        counts.add("completed", Some("failure"), 1);
        counts.add("completed", Some("cancelled"), 3);
        counts.add("completed", Some("skipped"), 4);
        counts.add("running", None, 1);
        counts.add("queued", None, 5);
        assert_eq!(
            counts,
            JobCounts {
                total: 16,
                passed: 2,
                failed: 1,
                running: 1,
                waiting: 5,
                cancelled: 3,
                skipped: 4,
            }
        );
    }
}
