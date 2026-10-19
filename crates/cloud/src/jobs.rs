// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The SQL on `jobs`, `runs` and `cache_uploads`, and small functions
//! that run it.
//!
//! The webhook records jobs and runs here, the launch claims and fails
//! jobs, the cache keeps uploads and scopes, the app lists runs and
//! jobs, and the reconciler checks, launches and expires them. A few
//! statements on other tables read jobs in a subquery and stay with
//! their own table: the usage minutes in `usage`, and the waiting jobs
//! alert and failed launches in `alerts`.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use worker::Result;

use crate::db::{Db, Param, params};
use crate::reconcile::VERIFIED;
use crate::runs::Details;
use crate::webhook::{Outcome, Placement, QueuedJob};

/// Records a run the first time one of its jobs is queued. It expires
/// with its jobs, `retention_days` after it was queued (see
/// [`QUEUED`]).
pub(crate) const RUN: &str = "INSERT INTO runs (id, installation_id, repository, workflow_name, \
     head_branch, head_sha, attempt, queued_at, created_at, updated_at, expires_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $8, $8 + COALESCE((SELECT retention_days \
       FROM tenants WHERE installation_id = $2), 30) * 86400000) \
     ON CONFLICT (id) DO NOTHING";

/// Moves a run on to attempt `$2` when its first job is queued. A late
/// delivery from an earlier attempt changes nothing. The run expires
/// from the new attempt, as its jobs do. The branch may have moved since
/// the last attempt, so its cache scopes are worked out again.
pub(crate) const RUN_ATTEMPT: &str = "UPDATE runs SET attempt = $2, queued_at = $3, head_sha = $4, \
     head_branch = $5, updated_at = $3, expires_at = $3 + COALESCE((SELECT retention_days \
       FROM tenants WHERE tenants.installation_id = runs.installation_id), 30) * 86400000, \
     cache_write = NULL, cache_reads = NULL \
     WHERE id = $1 AND attempt < $2";

/// Records a queued job once, however often GitHub delivers it. It
/// expires after the `retention_days` of the installation's tenant, or
/// that column's default of 30 days for an installation without one.
/// The lookup rides along with the insert, so the webhook makes no
/// extra round trip for it.
pub(crate) const QUEUED: &str = "INSERT INTO jobs \
     (id, installation_id, repository, run_id, label, status, queued_at, updated_at, \
     name, run_attempt, expires_at) \
     VALUES ($1, $2, $3, $4, $5, 'queued', $6, $6, $7, $8, $6 + COALESCE((SELECT retention_days \
       FROM tenants WHERE installation_id = $2), 30) * 86400000) \
     ON CONFLICT (id) DO NOTHING";

/// Moves a job on unless it completed, with GitHub's conclusion and
/// times when the event has them.
pub(crate) const PROGRESS: &str = "UPDATE jobs SET status = $1, updated_at = $2, \
     ran_on = COALESCE($4, ran_on), conclusion = COALESCE($5, conclusion), \
     started_at = COALESCE($6, started_at), completed_at = COALESCE($7, completed_at) \
     WHERE id = $3 AND status <> 'completed'";

/// Records queued `job` and its run at `now`, in one transaction.
/// Returns whether the run is new.
pub(crate) async fn record_queued(
    db: &Db,
    job: &QueuedJob,
    placement: &Placement,
    now: u64,
) -> Result<bool> {
    let recorded = db
        .batch(&[
            (
                RUN,
                params![
                    job.run_id,
                    job.installation_id,
                    &job.repository,
                    &placement.workflow_name,
                    &placement.head_branch,
                    &placement.head_sha,
                    placement.attempt,
                    now,
                ],
            ),
            (
                RUN_ATTEMPT,
                params![
                    job.run_id,
                    placement.attempt,
                    now,
                    &placement.head_sha,
                    &placement.head_branch,
                ],
            ),
            (
                QUEUED,
                params![
                    job.id,
                    job.installation_id,
                    &job.repository,
                    job.run_id,
                    &job.label,
                    now,
                    &placement.name,
                    placement.attempt,
                ],
            ),
        ])
        .await?;
    Ok(recorded.first() == Some(&1))
}

/// Moves job `id` to `status` at `now` unless it completed (see
/// [`PROGRESS`]).
pub(crate) async fn progress(
    db: &Db,
    id: u64,
    status: &str,
    runner: Option<&str>,
    outcome: &Outcome,
    now: u64,
) -> Result<u64> {
    db.run(
        PROGRESS,
        &params![
            status,
            now,
            id,
            runner,
            &outcome.conclusion,
            outcome.started_at,
            outcome.completed_at,
        ],
    )
    .await
}

/// Records run `$1`'s details at `$7`, once.
pub(crate) const DETAILS: &str = "UPDATE runs SET run_number = $2, event = $3, \
     display_title = $4, actor = $5, path = $6, details_at = $7, updated_at = $7 \
     WHERE id = $1 AND details_at IS NULL";

/// Records `details` of run `id` at `now`.
pub(crate) async fn record_details(db: &Db, id: u64, details: &Details, now: u64) -> Result<()> {
    db.run(
        DETAILS,
        &params![
            id,
            details.run_number,
            &details.event,
            &details.display_title,
            &details.actor,
            &details.path,
            now,
        ],
    )
    .await?;
    Ok(())
}

/// Fails a job nobody took because its tenant cannot launch.
pub(crate) const REFUSE: &str = "UPDATE jobs SET launch_state = 'failed', launch_error = $2, \
     updated_at = $3, launch_retry = $4, next_launch_at = NULL \
     WHERE id = $1 AND launch_state IS NULL";

/// Records how a launch ended.
pub(crate) const RECORD_LAUNCH: &str = "UPDATE jobs SET launch_state = $2, instance_id = $3, \
     launch_error = $4, updated_at = $5, launch_retry = $6, next_launch_at = $7 WHERE id = $1";

/// Puts a tenant's jobs that wait for its setup back in line.
pub(crate) const RECHECK: &str = "UPDATE jobs SET launch_state = NULL, launch_retry = NULL, \
     next_launch_at = NULL, updated_at = $2 \
     WHERE installation_id = (SELECT installation_id FROM tenants WHERE id = $1) \
     AND status = 'queued' AND launch_state = 'failed' AND launch_retry = 'setup'";

/// Takes a queued job while the tenant has room, counting the attempt.
pub(crate) const CLAIM: &str = "UPDATE jobs SET launch_state = 'launching', runner_name = $2, \
     updated_at = $3, launch_attempts = launch_attempts + 1 \
     WHERE id = $1 AND launch_state IS NULL AND status = 'queued' \
     AND (SELECT COUNT(*) FROM jobs \
          WHERE installation_id = $4 AND status <> 'completed' \
          AND launch_state IN ('launching', 'launched')) < $5 \
     RETURNING launch_attempts";

/// A row of [`CLAIM`].
#[derive(Deserialize)]
struct Claimed {
    launch_attempts: u32,
}

/// Fails job `id` with `error` at `now`, if nobody took it, to wait as
/// `wait` says.
pub(crate) async fn refuse(db: &Db, id: u64, error: &str, now: u64, wait: &str) -> Result<u64> {
    db.run(REFUSE, &params![id, error, now, wait]).await
}

/// How a launch ended, as [`record_launch`] keeps it.
pub(crate) struct Launched<'a> {
    pub state: &'a str,
    pub instance_id: Option<String>,
    pub error: Option<String>,
    pub wait: Option<&'a str>,
    pub next_launch_at: Option<u64>,
}

/// Records how the launch of job `id` ended, at `now`.
pub(crate) async fn record_launch(
    db: &Db,
    id: u64,
    launched: Launched<'_>,
    now: u64,
) -> Result<()> {
    db.run(
        RECORD_LAUNCH,
        &params![
            id,
            launched.state,
            launched.instance_id,
            launched.error,
            now,
            launched.wait,
            launched.next_launch_at,
        ],
    )
    .await?;
    Ok(())
}

/// Puts tenant `tenant_id`'s queued jobs that wait for its setup back
/// in line at `now`. Returns how many.
pub(crate) async fn recheck(db: &Db, tenant_id: &str, now: u64) -> Result<u64> {
    db.run(RECHECK, &params![tenant_id, now]).await
}

/// Locks the tenant of installation `$1` until the transaction ends.
pub(crate) const LOCK_INSTALLATION: &str =
    "SELECT true AS locked FROM tenants WHERE installation_id = $1 FOR UPDATE";

/// Moves job `id` to `launching` as runner `name` at `now` if it is
/// queued, nobody took it, and `installation` has fewer than
/// `max_runners` runners, and counts the attempt. Claims for one
/// installation lock its tenant's row in turn, so the second sees the
/// first's runner in its count and the second delivery of a job finds
/// `launch_state` set. Returns the job's launch attempts so far, this
/// one included, or `None` when it was not taken.
pub(crate) async fn claim(
    db: &Db,
    id: u64,
    name: &str,
    now: u64,
    installation: u64,
    max_runners: u32,
) -> Result<Option<u32>> {
    let lock = params![installation];
    let claimed: Option<Claimed> = db
        .first_locked(
            (LOCK_INSTALLATION, &lock),
            CLAIM,
            &params![id, name, now, installation, max_runners],
        )
        .await?;
    Ok(claimed.map(|claimed| claimed.launch_attempts))
}

/// The job, its run's scopes while the run is on the job's attempt, and
/// its connected tenant, by job, installation and tenant.
pub(crate) const CACHE_JOB: &str = "SELECT jobs.repository, jobs.run_id, jobs.run_attempt, \
     jobs.cache_write, jobs.cache_reads, \
     CASE WHEN runs.attempt = jobs.run_attempt THEN runs.cache_write END AS run_cache_write, \
     CASE WHEN runs.attempt = jobs.run_attempt THEN runs.cache_reads END AS run_cache_reads, \
     tenants.id AS tenant_id, tenants.role_arn, tenants.region, tenants.cache_bucket \
     FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
     LEFT JOIN runs ON runs.id = jobs.run_id \
     WHERE jobs.id = $1 AND jobs.installation_id = $2 AND tenants.id = $3 \
     AND tenants.role_arn IS NOT NULL AND tenants.region IS NOT NULL";

/// Records a multipart upload a job started.
pub(crate) const START_UPLOAD: &str = "INSERT INTO cache_uploads \
     (id, job_id, object_key, upload_id, created_at) VALUES ($1, $2, $3, $4, $5)";

/// A job's upload, by id.
pub(crate) const UPLOAD: &str =
    "SELECT object_key, upload_id FROM cache_uploads WHERE id = $1 AND job_id = $2";

/// Forgets an upload once completed or abandoned.
pub(crate) const FORGET_UPLOAD: &str = "DELETE FROM cache_uploads WHERE id = $1";

/// Stores the scopes worked out for a run's attempt `$4`, for all its
/// jobs. A job of an earlier attempt leaves a re-run's scopes alone.
pub(crate) const RUN_SCOPES: &str = "UPDATE runs SET cache_write = $2, cache_reads = $3 \
     WHERE id = $1 AND attempt = $4";

/// Stores the scopes worked out for a job whose run has no row, or has
/// moved on to another attempt.
pub(crate) const JOB_SCOPES: &str =
    "UPDATE jobs SET cache_write = $2, cache_reads = $3 WHERE id = $1";

/// Job `id` of `installation` as the cache serves it, when `tenant` is
/// its connected tenant (see [`CACHE_JOB`]).
pub(crate) async fn cache_job<T: DeserializeOwned>(
    db: &Db,
    id: u64,
    installation: u64,
    tenant: &str,
) -> Result<Option<T>> {
    db.first(CACHE_JOB, &params![id, installation, tenant])
        .await
}

/// Records upload `id` of `job_id` to `object_key`, S3's `upload_id`,
/// started at `now`.
pub(crate) async fn start_upload(
    db: &Db,
    id: &str,
    job_id: u64,
    object_key: &str,
    upload_id: &str,
    now: u64,
) -> Result<()> {
    db.run(
        START_UPLOAD,
        &params![id, job_id, object_key, upload_id, now],
    )
    .await?;
    Ok(())
}

/// An upload of a job.
#[derive(Deserialize)]
pub(crate) struct Upload {
    pub object_key: String,
    pub upload_id: String,
}

/// Upload `id` of job `job_id`.
pub(crate) async fn upload(db: &Db, id: &str, job_id: u64) -> Result<Option<Upload>> {
    db.first(UPLOAD, &params![id, job_id]).await
}

/// Forgets upload `id`.
pub(crate) async fn forget_upload(db: &Db, id: &str) -> Result<()> {
    db.run(FORGET_UPLOAD, &params![id]).await?;
    Ok(())
}

/// Stores the scopes of run `run_id`'s attempt `attempt`. Returns the
/// rows changed, 0 when the run has no row or moved on.
pub(crate) async fn run_scopes(
    db: &Db,
    run_id: u64,
    write: &str,
    reads: &str,
    attempt: u64,
) -> Result<u64> {
    db.run(RUN_SCOPES, &params![run_id, write, reads, attempt])
        .await
}

/// Stores the scopes of job `id`.
pub(crate) async fn job_scopes(db: &Db, id: u64, write: &str, reads: &str) -> Result<()> {
    db.run(JOB_SCOPES, &params![id, write, reads]).await?;
    Ok(())
}

/// Runners launching or running.
pub(crate) const ACTIVE: &str =
    "status <> 'completed' AND launch_state IN ('launching', 'launched')";
/// Queued and not taken, or waiting to launch again after a failed
/// launch.
pub(crate) const WAITING: &str = "status = 'queued' AND (launch_state IS NULL \
     OR (launch_state = 'failed' AND launch_retry = 'backoff'))";
/// Launches that failed and are not tried again on a timer. The leading
/// `launch_state = 'failed'` lets Postgres use the `jobs_failed` index.
pub(crate) const FAILED: &str = "launch_state = 'failed' \
     AND NOT (status = 'queued' AND launch_retry IS NOT DISTINCT FROM 'backoff')";
/// A job no later attempt of its run replaced, for a job named `j`.
pub(crate) const CURRENT: &str = "NOT EXISTS (SELECT 1 FROM jobs AS later \
     WHERE later.run_id = j.run_id AND later.name = j.name \
     AND later.run_attempt > j.run_attempt)";

pub(crate) const RUN_COLUMNS: &str = "id, repository, workflow_name, head_branch, head_sha, \
     attempt, queued_at, ended_at, run_number, event, display_title, actor, path";

pub(crate) const JOB_COLUMNS: &str = "id, run_id, name, run_attempt, label, status, conclusion, \
     launch_state, runner_name, instance_id, launch_error, launch_attempts, launch_retry, \
     next_launch_at, queued_at, started_at, completed_at, updated_at";

/// A status tab of the runs table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Group {
    Active,
    Waiting,
    Failed,
}

impl Group {
    pub(crate) fn predicate(self) -> &'static str {
        match self {
            Group::Active => ACTIVE,
            Group::Waiting => WAITING,
            Group::Failed => FAILED,
        }
    }
}

/// The column the runs table sorts by. A run's state is not here: it
/// rolls up from its jobs and changes while it runs, so a page boundary
/// in it moves under the reader. A run's time is kept on the run as
/// `ended_at` (the `jobs_end` trigger in `migrations/0001_schema.sql`), and a live run sorts by
/// when it was queued, which does not move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sort {
    /// When the newest attempt's first job was queued.
    #[default]
    Started,
    /// By repository, then newest first within it.
    Repository,
    /// The runs still going, oldest first, in either direction; then by
    /// how long a finished run took, ties by id.
    Took,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Direction {
    Asc,
    #[default]
    Desc,
}

/// Matches `$6`, the dashboard's `contains` pattern, against a run and
/// its jobs of every attempt. Postgres reads the jobs through an index,
/// one run at a time through `jobs_run` or the installation's jobs once
/// into a hash, whichever it costs lower.
pub(crate) const SEARCH: &str = "(repository ILIKE $6 ESCAPE '\\' \
     OR workflow_name ILIKE $6 ESCAPE '\\' \
     OR head_branch ILIKE $6 ESCAPE '\\' \
     OR EXISTS (SELECT 1 FROM jobs AS s \
       WHERE s.run_id = runs.id AND s.installation_id = $1 \
       AND (s.name ILIKE $6 ESCAPE '\\' OR s.label ILIKE $6 ESCAPE '\\' \
         OR s.instance_id ILIKE $6 ESCAPE '\\')))";

/// Where the page after the cursor `$2`, `$3` and `$5` starts, and the
/// order it is in. `$5` is the repository by repository and the end by
/// how long runs took. Within a repository runs stay newest first
/// either way. By how long runs took, the key is how long a finished
/// run took and when a live run was queued, ahead of the finished ones;
/// each side of the comparison is the expression its index holds. The
/// casts give Postgres the parameters' types where nothing else does.
fn order(sort: Sort, dir: Direction) -> (&'static str, &'static str) {
    match (sort, dir) {
        // `$5` is null here; naming it keeps the parameter count.
        (Sort::Started, Direction::Desc) => (
            "(queued_at, id) < ($2, $3) AND $5::text IS NULL",
            "queued_at DESC, id DESC",
        ),
        (Sort::Started, Direction::Asc) => (
            "(queued_at, id) > ($2, $3) AND $5::text IS NULL",
            "queued_at, id",
        ),
        (Sort::Repository, Direction::Asc) => (
            "(lower(repository) > lower($5) \
             OR (lower(repository) = lower($5) AND (queued_at, id) < ($2, $3)))",
            "lower(repository), queued_at DESC, id DESC",
        ),
        (Sort::Repository, Direction::Desc) => (
            "(lower(repository) < lower($5) \
             OR (lower(repository) = lower($5) AND (queued_at, id) < ($2, $3)))",
            "lower(repository) DESC, queued_at DESC, id DESC",
        ),
        (Sort::Took, Direction::Asc) => (
            "(ended_at IS NOT NULL, COALESCE(ended_at - queued_at, queued_at), id) \
             > ($5::bigint IS NOT NULL, COALESCE($5::bigint - $2::bigint, $2::bigint), $3)",
            "ended_at IS NOT NULL, COALESCE(ended_at - queued_at, queued_at), id",
        ),
        (Sort::Took, Direction::Desc) => (
            "(ended_at IS NOT NULL, \
             CASE WHEN ended_at IS NULL THEN queued_at ELSE queued_at - ended_at END, id) \
             > ($5::bigint IS NOT NULL, \
             CASE WHEN $5::bigint IS NULL THEN $2::bigint ELSE $2::bigint - $5::bigint END, $3)",
            "ended_at IS NOT NULL, \
             CASE WHEN ended_at IS NULL THEN queued_at ELSE queued_at - ended_at END, id",
        ),
    }
}

/// The query for one page of runs, of one group or of all, in `sort`
/// order and, unless `$6` is null, only the runs matching it. A run is
/// in a group when one of its current jobs is; the inner query starts
/// from the jobs, through their indexes, so a tenant with few failures
/// does not read every run. Every piece is a constant; the search text
/// is only ever bound. Each order names `$5` and the search `$6`, so
/// every query takes the same six values, as Postgres wants as many
/// values as the query has parameters.
pub(crate) fn list_query(group: Option<Group>, sort: Sort, dir: Direction) -> String {
    let filter = group.map_or(String::new(), |g| {
        format!(
            " AND id IN (SELECT j.run_id FROM jobs AS j \
             WHERE j.installation_id = $1 AND {} AND {CURRENT})",
            g.predicate()
        )
    });
    let (after, order) = order(sort, dir);
    format!(
        "SELECT {RUN_COLUMNS} FROM runs \
         WHERE installation_id = $1 AND ($2::bigint IS NULL OR {after}){filter} \
         AND ($6::text IS NULL OR {SEARCH}) \
         ORDER BY {order} LIMIT $4"
    )
}

/// The jobs, of every attempt, of the runs in the JSON array `$2`.
pub(crate) fn jobs_query() -> String {
    format!(
        "SELECT {JOB_COLUMNS} FROM jobs \
         WHERE installation_id = $1 \
         AND run_id IN (SELECT json_array_elements_text($2::json)::bigint) \
         ORDER BY run_id, run_attempt, queued_at, id"
    )
}

/// One run of an installation.
pub(crate) fn run_query() -> String {
    format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = $1 AND installation_id = $2")
}

/// The query for the counts. `$2` is the start of the last day.
pub(crate) fn counts_query() -> String {
    format!(
        "SELECT \
         COUNT(*) FILTER (WHERE {WAITING}) AS waiting, \
         COUNT(*) FILTER (WHERE {ACTIVE}) AS active, \
         COUNT(*) FILTER (WHERE status = 'in_progress' AND ({ACTIVE})) AS running, \
         COUNT(*) FILTER (WHERE status = 'completed' AND updated_at >= $2) \
           AS completed_today, \
         COUNT(*) FILTER (WHERE updated_at >= $2 AND ({FAILED})) AS failed_today, \
         COUNT(DISTINCT run_id) FILTER (WHERE ({WAITING}) AND {CURRENT}) AS waiting_runs, \
         COUNT(DISTINCT run_id) FILTER (WHERE ({ACTIVE}) AND {CURRENT}) AS active_runs, \
         COUNT(DISTINCT run_id) FILTER (WHERE updated_at >= $2 AND ({FAILED}) AND {CURRENT}) \
           AS failed_runs_today \
         FROM jobs AS j WHERE installation_id = $1 \
         AND (status <> 'completed' OR updated_at >= $2)"
    )
}

/// The query for the activity buckets of `bucket_ms`. `$2` is the start
/// of the first bucket. A completed job's `updated_at` is when it
/// finished. The division is integer division, which is the floor since
/// `updated_at >= $2`.
pub(crate) fn activity_query(bucket_ms: u64) -> String {
    format!(
        "SELECT (updated_at - $2) / {bucket_ms} AS bucket, \
         COUNT(*) FILTER (WHERE status = 'completed') AS finished, \
         COUNT(*) FILTER (WHERE {FAILED}) AS failed \
         FROM jobs WHERE installation_id = $1 AND updated_at >= $2 \
         GROUP BY bucket"
    )
}

/// A page of runs, as [`list_runs`] reads it.
pub(crate) struct RunsPage {
    pub group: Option<Group>,
    pub sort: Sort,
    pub dir: Direction,
    /// The cursor's run, none for the first page.
    pub queued_at: Option<u64>,
    pub id: Option<u64>,
    pub limit: u32,
    /// `$5`: the cursor's repository by repository, its end by how long
    /// runs took, null otherwise.
    pub fifth: Param,
    /// A `contains` pattern, or none for every run.
    pub search: Option<String>,
}

/// One page of `installation`'s runs (see [`list_query`]).
pub(crate) async fn list_runs<T: DeserializeOwned>(
    db: &Db,
    installation: u64,
    page: RunsPage,
) -> Result<Vec<T>> {
    db.all(
        &list_query(page.group, page.sort, page.dir),
        &params![
            installation,
            page.queued_at,
            page.id,
            page.limit,
            page.fifth,
            page.search,
        ],
    )
    .await
}

/// The jobs of `installation`'s runs in the JSON array `runs` (see
/// [`jobs_query`]).
pub(crate) async fn jobs_of_runs<T: DeserializeOwned>(
    db: &Db,
    installation: u64,
    runs: String,
) -> Result<Vec<T>> {
    db.all(&jobs_query(), &params![installation, runs]).await
}

/// Run `id` of `installation`.
pub(crate) async fn run<T: DeserializeOwned>(
    db: &Db,
    id: u64,
    installation: u64,
) -> Result<Option<T>> {
    db.first(&run_query(), &params![id, installation]).await
}

/// `installation`'s counts since `since` (see [`counts_query`]).
pub(crate) async fn counts<T: DeserializeOwned>(
    db: &Db,
    installation: u64,
    since: u64,
) -> Result<Option<T>> {
    db.first(&counts_query(), &params![installation, since])
        .await
}

/// `installation`'s finished and failed jobs in buckets of `bucket_ms`
/// from `since` (see [`activity_query`]).
pub(crate) async fn activity<T: DeserializeOwned>(
    db: &Db,
    installation: u64,
    since: u64,
    bucket_ms: u64,
) -> Result<Vec<T>> {
    db.all(&activity_query(bucket_ms), &params![installation, since])
        .await
}

/// One job of an installation and its run, from the database. Binds the job id
/// and the installation. A job from before runs were recorded has no
/// run until the backfill gives it one.
pub(crate) const JOB_WITH_RUN: &str = "SELECT jobs.id, jobs.repository, jobs.run_id, jobs.name, \
    jobs.run_attempt, jobs.label, jobs.status, jobs.conclusion, jobs.runner_name, \
    jobs.instance_id, jobs.queued_at, jobs.started_at, jobs.completed_at, jobs.updated_at, \
    runs.workflow_name, runs.run_number \
    FROM jobs LEFT JOIN runs ON runs.id = jobs.run_id \
    WHERE jobs.id = $1 AND jobs.installation_id = $2";

/// Job `id` of `installation` and its run (see [`JOB_WITH_RUN`]).
pub(crate) async fn job_with_run<T: DeserializeOwned>(
    db: &Db,
    id: u64,
    installation: u64,
) -> Result<Option<T>> {
    db.first(JOB_WITH_RUN, &params![id, installation]).await
}

/// Open jobs quiet since `$1`, quietest first, at most `$2`.
pub(crate) const OPEN_JOBS: &str = "SELECT id, installation_id, repository, status FROM jobs \
     WHERE status <> 'completed' AND COALESCE(checked_at, updated_at) < $1 \
     ORDER BY COALESCE(checked_at, updated_at) LIMIT $2";

/// Records what GitHub said about job `$1`, while its status is still
/// `$5`: the status, the runner, and the conclusion and times `$6` to
/// `$8` once GitHub has them.
pub(crate) const RECORD_CHECK: &str = "UPDATE jobs SET status = $2, \
     ran_on = COALESCE($3, ran_on), \
     checked_at = $4, updated_at = CASE WHEN status <> $2 THEN $4 ELSE updated_at END, \
     conclusion = COALESCE($6, conclusion), started_at = COALESCE($7, started_at), \
     completed_at = COALESCE($8, completed_at) \
     WHERE id = $1 AND status = $5";

/// Runs without details, not created or tried since `$1`, least
/// recently tried first, at most `$2`.
pub(crate) const RUNS_WITHOUT_DETAILS: &str = "SELECT id, installation_id, repository \
     FROM runs \
     WHERE details_at IS NULL AND COALESCE(details_tried_at, created_at) < $1 \
     ORDER BY COALESCE(details_tried_at, created_at) LIMIT $2";

/// Stamps the runs of a JSON array `$2` as tried at `$1`, so a run that
/// keeps failing, such as one of an uninstalled App, does not hold the
/// others back.
pub(crate) const DETAILS_TRIED: &str = "UPDATE runs SET details_tried_at = $1 \
     WHERE id IN (SELECT value::bigint FROM json_array_elements_text($2::json)) \
     AND details_at IS NULL";

/// An installation's open jobs, and the finished ones whose id is in the
/// JSON array `$2` or whose runner is in `$3`. Three lookups, through
/// `jobs_open`, the primary key and `jobs_ran_on`, so a tenant's
/// finished jobs are never read; the `+ 0` and `|| ''` keep Postgres
/// off the installation and status indexes in the last two. They share no row, so
/// `UNION ALL` needs no sort to drop duplicates.
pub(crate) const TENANT_JOBS: &str = "SELECT id, label, status, launch_state, runner_name, \
     ran_on, queued_at, updated_at, launch_retry, next_launch_at FROM jobs \
     WHERE installation_id = $1 AND status <> 'completed' \
     UNION ALL SELECT id, label, status, launch_state, runner_name, ran_on, \
     queued_at, updated_at, launch_retry, next_launch_at FROM jobs \
     WHERE id IN (SELECT value::bigint FROM json_array_elements_text($2::json)) \
     AND installation_id + 0 = $1 AND status || '' = 'completed' \
     UNION ALL SELECT id, label, status, launch_state, runner_name, ran_on, \
     queued_at, updated_at, launch_retry, next_launch_at FROM jobs \
     WHERE ran_on IN (SELECT value FROM json_array_elements_text($3::json)) \
     AND installation_id + 0 = $1 AND status || '' = 'completed' \
     AND id NOT IN (SELECT value::bigint FROM json_array_elements_text($2::json))";

/// Adopts instance `$2` for a job still launching.
pub(crate) const ADOPT: &str = "UPDATE jobs SET launch_state = 'launched', instance_id = $2, \
     updated_at = $3 WHERE id = $1 AND launch_state = 'launching'";

/// Undoes a launch so the job can launch again.
pub(crate) const RESET: &str = "UPDATE jobs SET launch_state = NULL, runner_name = NULL, \
     instance_id = NULL, launch_retry = NULL, next_launch_at = NULL, updated_at = $3 \
     WHERE id = $1 AND status = 'queued' AND launch_state = $2 \
     AND runner_name IS NOT DISTINCT FROM $4";

/// Fails a launch whose runner `$4` waited too long.
pub(crate) const GIVE_UP: &str = "UPDATE jobs SET launch_state = 'failed', launch_error = $3, \
     launch_retry = NULL, next_launch_at = NULL, updated_at = $2 \
     WHERE id = $1 AND status = 'queued' AND launch_state = 'launched' AND runner_name = $4";

/// The jobs of a JSON array of ids, as `launch` takes them, oldest
/// first.
pub(crate) const QUEUED_JOBS: &str = "SELECT id, installation_id, repository, run_id, label \
     FROM jobs \
     WHERE id IN (SELECT value::bigint FROM json_array_elements_text($1::json)) \
     ORDER BY queued_at, id";

/// Lost launches, queued before `$1`, of installations with no verified
/// tenant, at most `$2`.
pub(crate) fn unconnected_query() -> String {
    format!(
        "SELECT id, installation_id, repository, run_id, label FROM jobs \
         WHERE status = 'queued' AND launch_state IS NULL AND queued_at < $1 \
         AND installation_id NOT IN (SELECT installation_id FROM tenants \
           WHERE installation_id IS NOT NULL AND {VERIFIED}) \
         ORDER BY queued_at LIMIT $2"
    )
}

/// Gives up to `$2` jobs without `expires_at` one `$1` after they were
/// queued. `jobs_expiry` holds the NULLs, so once none are left this
/// reads nothing.
pub(crate) const BACKFILL_JOBS: &str = "UPDATE jobs SET expires_at = queued_at + $1 WHERE id IN \
     (SELECT id FROM jobs WHERE expires_at IS NULL LIMIT $2)";

/// As [`BACKFILL_JOBS`], for runs.
pub(crate) const BACKFILL_RUNS: &str = "UPDATE runs SET expires_at = queued_at + $1 WHERE id IN \
     (SELECT id FROM runs WHERE expires_at IS NULL LIMIT $2)";

/// Deletes up to `$2` jobs that expired before `$1`, oldest first.
pub(crate) const DELETE_JOBS: &str = "DELETE FROM jobs WHERE id IN \
     (SELECT id FROM jobs WHERE expires_at < $1 ORDER BY expires_at LIMIT $2)";

/// Deletes up to `$2` runs that expired before `$1` and have no jobs
/// left.
pub(crate) const DELETE_RUNS: &str = "DELETE FROM runs WHERE id IN \
     (SELECT id FROM runs WHERE expires_at < $1 \
       AND NOT EXISTS (SELECT 1 FROM jobs WHERE jobs.run_id = runs.id) \
     ORDER BY expires_at LIMIT $2)";

/// Deletes up to `$2` cache uploads started before `$1`.
pub(crate) const DELETE_UPLOADS: &str = "DELETE FROM cache_uploads WHERE id IN \
     (SELECT id FROM cache_uploads WHERE created_at < $1 LIMIT $2)";

/// Open jobs quiet since `quiet`, quietest first, at most `limit`.
pub(crate) async fn open_jobs<T: DeserializeOwned>(
    db: &Db,
    quiet: u64,
    limit: u32,
) -> Result<Vec<T>> {
    db.all(OPEN_JOBS, &params![quiet, limit]).await
}

/// Records at `now` what GitHub said about job `id`, while its status
/// is still `stored` (see [`RECORD_CHECK`]). Returns the rows changed.
pub(crate) async fn record_check(
    db: &Db,
    id: u64,
    status: &str,
    runner: Option<&str>,
    stored: &str,
    outcome: Option<&Outcome>,
    now: u64,
) -> Result<u64> {
    db.run(
        RECORD_CHECK,
        &params![
            id,
            status,
            runner,
            now,
            stored,
            outcome.and_then(|outcome| outcome.conclusion.as_deref()),
            outcome.and_then(|outcome| outcome.started_at),
            outcome.and_then(|outcome| outcome.completed_at),
        ],
    )
    .await
}

/// Runs without details, not created or tried since `before`, at most
/// `limit`.
pub(crate) async fn runs_without_details<T: DeserializeOwned>(
    db: &Db,
    before: u64,
    limit: u32,
) -> Result<Vec<T>> {
    db.all(RUNS_WITHOUT_DETAILS, &params![before, limit]).await
}

/// Stamps the runs of the JSON array `runs` as tried at `now`.
pub(crate) async fn details_tried(db: &Db, runs: &str, now: u64) -> Result<()> {
    db.run(DETAILS_TRIED, &params![now, runs]).await?;
    Ok(())
}

/// `installation`'s open jobs, and the finished ones of the JSON arrays
/// `ids` and `runners` (see [`TENANT_JOBS`]).
pub(crate) async fn tenant_jobs<T: DeserializeOwned>(
    db: &Db,
    installation: u64,
    ids: &str,
    runners: &str,
) -> Result<Vec<T>> {
    db.all(TENANT_JOBS, &params![installation, ids, runners])
        .await
}

/// A change to a job's launch that the reconciler planned.
pub(crate) enum LaunchChange<'a> {
    /// Adopts `instance_id` for job `id` (see [`ADOPT`]).
    Adopt { id: u64, instance_id: &'a str },
    /// Undoes job `id`'s launch (see [`RESET`]).
    Reset {
        id: u64,
        launch_state: &'a str,
        runner_name: Option<&'a str>,
    },
    /// Fails job `id`'s launch on `runner` with `reason` (see
    /// [`GIVE_UP`]).
    GiveUp {
        id: u64,
        reason: String,
        runner: &'a str,
    },
}

/// Applies `changes` at `now` in one transaction. Returns the rows each
/// changed.
pub(crate) async fn update_launches(
    db: &Db,
    changes: &[LaunchChange<'_>],
    now: u64,
) -> Result<Vec<u64>> {
    let statements: Vec<(&str, Vec<Param>)> = changes
        .iter()
        .map(|change| match change {
            LaunchChange::Adopt { id, instance_id } => (ADOPT, params![*id, *instance_id, now]),
            LaunchChange::Reset {
                id,
                launch_state,
                runner_name,
            } => (RESET, params![*id, *launch_state, now, *runner_name]),
            LaunchChange::GiveUp { id, reason, runner } => {
                (GIVE_UP, params![*id, now, reason, *runner])
            }
        })
        .collect();
    db.batch(&statements).await
}

/// The jobs of the JSON array `ids`, oldest first.
pub(crate) async fn queued_jobs<T: DeserializeOwned>(db: &Db, ids: &str) -> Result<Vec<T>> {
    db.all(QUEUED_JOBS, &params![ids]).await
}

/// Lost launches queued before `before` of installations with no
/// verified tenant, at most `limit`.
pub(crate) async fn unconnected<T: DeserializeOwned>(
    db: &Db,
    before: u64,
    limit: u32,
) -> Result<Vec<T>> {
    db.all(&unconnected_query(), &params![before, limit]).await
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testdb;

    const NOW: u64 = 1_792_140_000_000;

    /// A second client on `db`'s schema.
    fn other(db: &testdb::Db) -> Db {
        db.connect(&testdb::url().parse().unwrap())
    }

    fn job(db: &testdb::Db, id: u64, name: &str, status: &str, launch: Option<&str>) {
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, name, launch_state) \
             VALUES ($1, 42, 'acme/app', 7, 'hatchrun', $2, $3, $3, $4, $5)",
            &testdb::params![id, status, NOW, name, launch],
        );
    }

    #[test]
    fn claims_at_once_stop_at_max_runners() {
        let db = testdb::Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, \
             updated_at) VALUES ('t', 9, 'acme', 42, 1, 1)",
            &[],
        );
        job(&db, 1, "a", "in_progress", Some("launched"));
        job(&db, 2, "b", "queued", None);
        job(&db, 3, "c", "queued", None);
        let other = other(&db);
        // One place left under max_runners 2, two claims sent at once.
        let (first, second) = db.wait(async {
            futures_util::join!(
                claim(db.db(), 2, "hatchrun-2", NOW, 42, 2),
                claim(&other, 3, "hatchrun-3", NOW, 42, 2)
            )
        });
        let taken = [first.unwrap(), second.unwrap()];
        assert_eq!(taken.iter().flatten().count(), 1, "{taken:?}");
    }

    #[test]
    fn jobs_of_a_run_completing_together_end_it() {
        let db = testdb::Db::new();
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, queued_at, \
             created_at, updated_at) VALUES (7, 42, 'acme/app', 'ab', 1, $1, $1, $1)",
            &testdb::params![NOW],
        );
        job(&db, 1, "a", "in_progress", Some("launched"));
        job(&db, 2, "b", "in_progress", Some("launched"));
        let other = other(&db);
        let complete = |id: u64, at: u64| {
            (
                PROGRESS,
                params![
                    "completed",
                    at,
                    id,
                    None::<&str>,
                    "success",
                    None::<u64>,
                    at
                ],
            )
        };
        // The first completion holds its transaction open while the
        // second one waits for the run's row.
        let slow = [
            complete(1, NOW + 1000),
            ("SELECT pg_sleep(0.2)", Vec::new()),
        ];
        let quick = [complete(2, NOW + 2000)];
        let (first, second) = db.wait(async {
            futures_util::join!(db.db().batch(&slow), async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                other.batch(&quick).await
            })
        });
        first.unwrap();
        second.unwrap();
        let ended: Option<u64> = db.column("SELECT ended_at FROM runs", &[], "ended_at");
        assert_eq!(ended, Some(NOW + 2000));
    }
}
