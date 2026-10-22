// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The SQL on the `ci_` tables, and small functions that run it.
//!
//! The webhook records runs and jobs here, the launch claims and fails
//! jobs, the agent's calls move them along, the app lists them, and the
//! reconciler checks, restarts and expires them. Times are Unix
//! milliseconds. A job's `steps` column is `jsonb`, which rows do not
//! carry, so every statement reads it as text and [`JobRow`] parses it;
//! so are `token_permissions` and `secret_refs`.

use std::collections::BTreeMap;

use hatchrun_cloud::db::{Db, Param, params};
use hatchrun_workflow::{Level, SecretRefs, templates};
use serde::Deserialize;
use serde::de::{Deserializer, Error as _};
use worker::Result;

/// How long runs, their jobs and delivery ids are kept, in
/// milliseconds.
const RETENTION_MS: u64 = 30 * 86_400_000;

/// Where a tenant stands with its subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiBilling {
    /// No subscription row ever.
    Never,
    /// A subscription exists but none is active or trialing.
    Lapsed,
    /// An active or trialing subscription: a card on file.
    Active,
}

/// A tenant, as far as CI needs it. The billing state is read in the
/// same statement as the tenant, so the gate costs no extra query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TenantRow {
    /// The tenant id.
    pub id: String,
    /// The organization's login.
    pub org_login: String,
    /// Whether the tenant has a card for CI.
    pub ci_billing: CiBilling,
    /// How many days the tenant's runs, logs and state are kept: 30 or 90.
    pub retention_days: u32,
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
    /// How many days the run is kept, as its tenant's retention was when
    /// the run was recorded. It names the tier of the run's R2 keys.
    pub retention_days: u32,
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
     event, ref AS git_ref, sha, actor, status, conclusion, created_at, completed_at, \
     (expires_at - created_at) / 86400000 AS retention_days";

/// The columns of a [`JobRow`].
const JOB_COLUMNS: &str = "id, run_id, plan_id, name, status, conclusion, instance_id, attempt, \
     launch_attempts, next_launch_at, timeout_minutes, updated_at, heartbeat_at, started_at, \
     completed_at, steps::text AS steps, log_key, template, \
     token_permissions::text AS token_permissions, secret_refs::text AS secret_refs, held, \
     environment, approval_attempt, approval_decision, approval_by, approval_at";

/// The columns of a [`TenantRow`]. `ci_billing` is `active` for an
/// active or trialing subscription, `lapsed` for another one, `never`
/// for none. `retention_days` is the tier: 30, or 90
/// for a tenant whose `retention_days` (shared with hatchrun.io, 1 to 90)
/// is above 30. R2 lifecycle rules exist per tier, so CI keeps for 30 or
/// 90 days and nothing between.
const TENANT_COLUMNS: &str = "id, org_login, CASE \
     WHEN EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')) \
     THEN 'active' \
     WHEN EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id) \
     THEN 'lapsed' \
     ELSE 'never' END AS ci_billing, \
     CASE WHEN retention_days > 30 THEN 90 ELSE 30 END AS retention_days";

/// The tenant of installation `$1`, where `{columns}` is
/// [`TENANT_COLUMNS`].
const TENANT_FOR_INSTALLATION: &str = "SELECT {columns} FROM tenants WHERE installation_id = $1";

/// Tenant `$1`, where `{columns}` is [`TENANT_COLUMNS`].
const TENANT: &str = "SELECT {columns} FROM tenants WHERE id = $1";

/// Whether tenant `$1` has an active or trialing subscription.
const HAS_CARD: &str = "SELECT EXISTS (SELECT 1 FROM subscriptions WHERE tenant_id = $1 \
     AND status IN ('active', 'trialing')) AS card";

/// The run of delivery `$1` and workflow `$2`, where `{columns}` is
/// [`RUN_COLUMNS`].
const RUN_FOR_DELIVERY: &str =
    "SELECT {columns} FROM ci_runs WHERE delivery = $1 AND workflow = $2";

/// Frees the name `$2` from a repository other than `$1`. That one was
/// deleted or renamed, and the name is the new owner's now. The row
/// stays, under a name GitHub cannot issue, so its runs stay too.
const FREE_REPO_NAME: &str = "UPDATE ci_repos SET full_name = full_name || '#gone-' || id \
     WHERE lower(full_name) = lower($2) AND id <> $1";

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

/// Locks the row of tenant `$1`, so the launches of one tenant are
/// counted against the concurrency cap one at a time.
const LOCK_TENANT: &str = "SELECT id FROM tenants WHERE id = $1 FOR UPDATE";

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

/// Completes attempt `$4` of run `$1` with conclusion `$2` at `$3`.
const FINISH_RUN: &str = "UPDATE ci_runs SET status = 'completed', conclusion = $2, \
     completed_at = $3, updated_at = $3 WHERE id = $1 AND run_attempt = $4";

/// Records the usage of attempt `$3` of job `$1`, completed at `$2`,
/// from the row as the update before it left it: nothing is written
/// when that update did not apply (another attempt, or a job completed
/// already), or the job never started or has no template. Minutes run
/// from start to completion, rounded up, at least one. `$4` is the JSON map of label to price
/// per minute and `$5` the price of one unit, so the units are those of the
/// price when the job ended; a label the map lacks records nothing.
const RECORD_USAGE: &str = "INSERT INTO ci_usage \
       (job_id, attempt, tenant_id, template, minutes, units, ended_at) \
     SELECT ci_jobs.id, ci_jobs.attempt, ci_runs.tenant_id, ci_jobs.template, \
       m.minutes, m.minutes * ($4::jsonb ->> ci_jobs.template)::bigint / $5, \
       ci_jobs.completed_at \
     FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
       CROSS JOIN LATERAL (SELECT GREATEST(1, \
         CEIL((ci_jobs.completed_at - ci_jobs.started_at) / 60000.0))::bigint AS minutes) m \
     WHERE ci_jobs.id = $1 AND ci_jobs.attempt = $3 AND ci_jobs.status = 'completed' \
       AND ci_jobs.completed_at = $2 AND ci_jobs.started_at IS NOT NULL \
       AND ($4::jsonb ->> ci_jobs.template) IS NOT NULL \
     ON CONFLICT DO NOTHING";

/// The billing state of a tenant, usage since `$2`, cap default `$3`;
/// `{owner}` picks the tenant by `$1`.
const STANDING: &str = "SELECT tenants.id AS tenant_id, tenants.org_login, \
       EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id \
         AND subscriptions.status IN ('active', 'trialing')) AS card, \
       COALESCE((SELECT cap_micros FROM ci_spend_caps WHERE tenant_id = tenants.id), $3) \
         AS cap_micros, \
       COALESCE((SELECT json_agg(json_build_object('template', u.template, 'minutes', u.minutes)) \
         FROM (SELECT template, SUM(minutes)::bigint AS minutes FROM ci_usage \
           WHERE tenant_id = tenants.id AND ended_at >= $2 GROUP BY template) u), \
         '[]'::json)::text AS minutes_by_template, \
       (SELECT count(*) FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
         WHERE ci_runs.tenant_id = tenants.id \
         AND ci_jobs.status IN ('launching', 'launched', 'running'))::bigint AS running \
     FROM tenants WHERE {owner}";

/// [`STANDING`]'s `{owner}` for the tenant of job `$1`.
const OWNER_OF_JOB: &str = "tenants.id = (SELECT ci_runs.tenant_id FROM ci_jobs \
     JOIN ci_runs ON ci_runs.id = ci_jobs.run_id WHERE ci_jobs.id = $1)";

/// [`STANDING`]'s `{owner}` for tenant `$1`.
const OWNER_IS_TENANT: &str = "tenants.id = $1::text";

/// The tenant of GitHub organization id `$1`, where `{columns}` is
/// [`TENANT_COLUMNS`]. The id is unique and survives renames; the login
/// is neither.
const TENANT_BY_ORG: &str = "SELECT {columns} FROM tenants WHERE org_id = $1";

/// Sets the spend cap of tenant `$1` to `$2` micros at `$3`.
const SET_SPEND_CAP: &str = "INSERT INTO ci_spend_caps (tenant_id, cap_micros, updated_at) \
     VALUES ($1, $2, $3) \
     ON CONFLICT (tenant_id) DO UPDATE SET cap_micros = $2, updated_at = $3";

/// Usage Polar has not taken and has not refused, oldest first, at
/// most `$1`.
const UNSENT_USAGE: &str = "SELECT job_id, attempt, tenant_id, template, minutes, units, ended_at \
     FROM ci_usage WHERE sent_at IS NULL AND rejected_at IS NULL \
     ORDER BY ended_at, job_id, attempt LIMIT $1";

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
     (status = 'queued' AND held IS NULL AND (next_launch_at <= $1 OR updated_at < $2)) \
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

/// Deletes usage Polar took before `$1`.
const SWEEP_USAGE: &str = "DELETE FROM ci_usage WHERE sent_at < $1 OR rejected_at < $1";

/// How long sent usage rows are kept, in milliseconds.
const USAGE_RETENTION_MS: u64 = 90 * 86_400_000;

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
    db.first(
        &TENANT_FOR_INSTALLATION.replace("{columns}", TENANT_COLUMNS),
        &params![installation_id],
    )
    .await
}

/// Tenant `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant(db: &Db, tenant_id: &str) -> Result<Option<TenantRow>> {
    db.first(
        &TENANT.replace("{columns}", TENANT_COLUMNS),
        &params![tenant_id],
    )
    .await
}

/// Whether tenant `tenant_id` has an active or trialing subscription.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn has_card(db: &Db, tenant_id: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct Card {
        card: bool,
    }
    Ok(db
        .first::<Card>(HAS_CARD, &params![tenant_id])
        .await?
        .is_some_and(|row| row.card))
}

/// A tenant's billing state this month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// The tenant.
    pub tenant_id: String,
    /// The organization's login, for messages.
    pub org_login: String,
    /// Whether an active or trialing subscription exists.
    pub card: bool,
    /// The monthly spend cap in micros: the tenant's own or the default.
    pub cap_micros: u64,
    /// Minutes metered this month, per template label.
    pub minutes_by_template: Vec<(String, u64)>,
    /// Jobs launching, launched or running now.
    pub running: u32,
}

/// What [`STANDING`] returns; the minutes arrive as JSON text.
#[derive(Deserialize)]
struct StandingRow {
    tenant_id: String,
    org_login: String,
    card: bool,
    cap_micros: u64,
    minutes_by_template: String,
    running: u32,
}

impl StandingRow {
    fn into_standing(self) -> Result<Standing> {
        #[derive(Deserialize)]
        struct Minutes {
            template: String,
            minutes: u64,
        }
        let minutes: Vec<Minutes> = serde_json::from_str(&self.minutes_by_template)?;
        Ok(Standing {
            tenant_id: self.tenant_id,
            org_login: self.org_login,
            card: self.card,
            cap_micros: self.cap_micros,
            minutes_by_template: minutes
                .into_iter()
                .map(|m| (m.template, m.minutes))
                .collect(),
            running: self.running,
        })
    }
}

/// The billing state of the tenant of job `job_id`: usage since
/// `month_start`, and `default_cap` for a tenant with no cap row.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn standing(
    db: &Db,
    job_id: u64,
    month_start: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_OF_JOB);
    db.first::<StandingRow>(&sql, &params![job_id, month_start, default_cap])
        .await?
        .map(StandingRow::into_standing)
        .transpose()
}

/// As [`standing`], for tenant `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_standing(
    db: &Db,
    tenant_id: &str,
    month_start: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_IS_TENANT);
    db.first::<StandingRow>(&sql, &params![tenant_id, month_start, default_cap])
        .await?
        .map(StandingRow::into_standing)
        .transpose()
}

/// The tenant of the GitHub organization with id `org_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_by_org_id(db: &Db, org_id: u64) -> Result<Option<TenantRow>> {
    db.first(
        &TENANT_BY_ORG.replace("{columns}", TENANT_COLUMNS),
        &params![org_id],
    )
    .await
}

/// Sets tenant `tenant_id`'s monthly spend cap to `cap_micros`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_spend_cap(db: &Db, tenant_id: &str, cap_micros: u64, now: u64) -> Result<()> {
    db.run(SET_SPEND_CAP, &params![tenant_id, cap_micros, now])
        .await
        .map(|_| ())
}

/// One attempt's usage that Polar has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UnsentUsage {
    /// The job.
    pub job_id: u64,
    /// The attempt.
    pub attempt: u32,
    /// The tenant, Polar's external customer id.
    pub tenant_id: String,
    /// The template's label.
    pub template: String,
    /// The minutes the attempt ran.
    pub minutes: u64,
    /// The billing units, priced when the attempt completed.
    pub units: u64,
    /// When the attempt completed, in milliseconds.
    pub ended_at: u64,
}

/// The oldest `limit` usage rows Polar has not taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn unsent_usage(db: &Db, limit: u32) -> Result<Vec<UnsentUsage>> {
    db.all(UNSENT_USAGE, &params![limit]).await
}

/// Marks the usage of the `(job, attempt)` pairs in `keys` as taken by
/// Polar at `now`. A row already marked keeps its time.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn usage_sent(db: &Db, keys: &[(u64, u32)], now: u64) -> Result<()> {
    mark_usage("sent_at = $1", &[], keys, now, db).await
}

/// Quarantines the usage of the `(job, attempt)` pairs in `keys`:
/// Polar refused it for good, so it is never sent again.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn usage_rejected(db: &Db, keys: &[(u64, u32)], reason: &str, now: u64) -> Result<()> {
    mark_usage(
        "rejected_at = $1, reject_reason = $2",
        &[Param::from(reason)],
        keys,
        now,
        db,
    )
    .await
}

/// Sets `assignments` on the open usage rows of `keys`. `$1` is `now`
/// and `extra` binds `$2` on.
async fn mark_usage(
    assignments: &str,
    extra: &[Param],
    keys: &[(u64, u32)],
    now: u64,
    db: &Db,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let first = extra.len() + 2;
    let pairs: Vec<String> = (0..keys.len())
        .map(|n| format!("(${}, ${})", first + 2 * n, first + 2 * n + 1))
        .collect();
    let sql = format!(
        "UPDATE ci_usage SET {assignments} WHERE sent_at IS NULL AND rejected_at IS NULL \
         AND (job_id, attempt) IN ({})",
        pairs.join(", ")
    );
    let mut bound: Vec<Param> = vec![Param::from(now)];
    bound.extend(extra.iter().cloned());
    for (job, attempt) in keys {
        bound.push(Param::from(*job));
        bound.push(Param::from(*attempt));
    }
    db.run(&sql, &bound).await.map(|_| ())
}

/// Records `repo`, or updates it after a rename or a change.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn upsert_repo(db: &Db, repo: &RepoRow, now: u64) -> Result<()> {
    db.batch(&[
        (FREE_REPO_NAME, params![repo.id, &repo.full_name]),
        // A repository that changes tenant leaves its secrets, vars and
        // environments behind, as GitHub does on transfer.
        (
            "DELETE FROM ci_secrets WHERE repo_id = $1 AND tenant_id <> $2",
            params![repo.id, &repo.tenant_id],
        ),
        (
            "DELETE FROM ci_vars WHERE repo_id = $1 AND tenant_id <> $2",
            params![repo.id, &repo.tenant_id],
        ),
        (
            "DELETE FROM ci_environments WHERE repo_id = $1 AND tenant_id <> $2",
            params![repo.id, &repo.tenant_id],
        ),
        (
            UPSERT_REPO,
            params![
                repo.id,
                &repo.tenant_id,
                repo.installation_id,
                &repo.full_name,
                &repo.default_branch,
                repo.private,
                now
            ],
        ),
    ])
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
            run.now
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
        let base = 3 + i * 7;
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
/// its minutes recorded for this attempt, once.
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
                    templates::UNIT_PRICE_MICROS
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
/// delivery ids older than 30 days, and usage rows Polar took more
/// than 90 days ago. At most [`SWEEP_LIMIT`] runs go in
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
    db.run(
        SWEEP_USAGE,
        &params![now.saturating_sub(USAGE_RETENTION_MS)],
    )
    .await?;
    Ok(runs.into_iter().map(|row| row.id).collect())
}

// The warm pool (see `crate::pool`). Times are Unix milliseconds.

/// The pool rows of template `$1`.
const POOL_ROWS: &str = "SELECT instance_id, template, state, launched_at, since \
     FROM ci_pool WHERE template = $1 ORDER BY launched_at, instance_id";

/// Deletes the oldest ready row of template `$1` and returns its
/// instance. A row another statement holds is skipped, so two jobs never
/// take the same instance.
const TAKE_POOL: &str = "DELETE FROM ci_pool WHERE instance_id = (\
       SELECT instance_id FROM ci_pool WHERE template = $1 AND state = 'ready' \
       ORDER BY launched_at, instance_id LIMIT 1 FOR UPDATE SKIP LOCKED) \
     RETURNING instance_id";

/// Deletes the row of instance `$1`.
const DROP_POOL: &str = "DELETE FROM ci_pool WHERE instance_id = $1";

/// Moves instance `$1` from state `$2` to `$3` at `$4`.
const MOVE_POOL: &str =
    "UPDATE ci_pool SET state = $3, since = $4 WHERE instance_id = $1 AND state = $2";

/// Sets the state of an existing row of instance `$1` to `$2`, since `$3`,
/// in the pool of template `$4`.
const UPDATE_POOL: &str =
    "UPDATE ci_pool SET state = $2, since = $3 WHERE instance_id = $1 AND template = $4";

/// Adds a launched instance.
const INSERT_POOL: &str = "INSERT INTO ci_pool (instance_id, template, state, launched_at, since) \
     VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance_id) DO NOTHING";

/// Jobs claimed since `$1`, by template and bucket of `$2` milliseconds.
const CLAIMS_BY_BUCKET: &str = "SELECT template, started_at / $2 * $2 AS bucket, count(*) AS jobs \
     FROM ci_jobs WHERE started_at >= $1 AND template IS NOT NULL \
     GROUP BY template, started_at / $2";

/// An instance of the warm pool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PoolRow {
    /// The EC2 instance id.
    pub instance_id: String,
    /// The template the instance is kept for, its first label.
    pub template: String,
    /// `warming`, `stopping` or `ready`.
    pub state: String,
    /// When it launched, in Unix milliseconds.
    pub launched_at: u64,
    /// When it entered its state, in Unix milliseconds.
    pub since: u64,
}

/// The pool rows of `template`, oldest first.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pool_rows(db: &Db, template: &str) -> Result<Vec<PoolRow>> {
    db.all(POOL_ROWS, &params![template]).await
}

/// Deletes the oldest ready row of `template` and returns its instance:
/// the job that took it owns it. `None` when none is ready.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn take_pool(db: &Db, template: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Taken {
        instance_id: String,
    }
    Ok(db
        .first::<Taken>(TAKE_POOL, &params![template])
        .await?
        .map(|row| row.instance_id))
}

/// Deletes the row of `instance_id`. Returns whether it was there: when
/// not, a job took the instance first and it is not the pool's to end.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn drop_pool(db: &Db, instance_id: &str) -> Result<bool> {
    Ok(db.run(DROP_POOL, &params![instance_id]).await? == 1)
}

/// Moves the row of `instance_id` from state `from` to `to`. Returns
/// whether it was in `from`: when not, a job took the instance first.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn move_pool(db: &Db, instance_id: &str, from: &str, to: &str, now: u64) -> Result<bool> {
    Ok(db
        .run(MOVE_POOL, &params![instance_id, from, to, now])
        .await?
        == 1)
}

/// Saves the pool of `template` after a pass: `rows` update the rows that
/// still exist, and `launched` are added. A row a job took meanwhile is
/// not brought back.
///
/// # Errors
///
/// Postgres refused a statement; none of them applies.
pub async fn save_pool(
    db: &Db,
    template: &str,
    rows: &[PoolRow],
    launched: &[PoolRow],
) -> Result<()> {
    let mut statements: Vec<(&str, Vec<Param>)> = Vec::new();
    for row in rows {
        statements.push((
            UPDATE_POOL,
            params![
                row.instance_id.as_str(),
                row.state.as_str(),
                row.since,
                template
            ],
        ));
    }
    for row in launched {
        statements.push((
            INSERT_POOL,
            params![
                row.instance_id.as_str(),
                template,
                row.state.as_str(),
                row.launched_at,
                row.since
            ],
        ));
    }
    if !statements.is_empty() {
        db.batch(&statements).await?;
    }
    Ok(())
}

/// Jobs claimed at or after `since`, as (template, start of the bucket,
/// claims), the buckets [`crate::pool::BUCKET_MS`] long. A template is
/// the label the job named, which may be any of its template's labels.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn claims_by_bucket(db: &Db, since: u64) -> Result<Vec<(String, u64, u64)>> {
    #[derive(Deserialize)]
    struct Count {
        template: String,
        bucket: u64,
        jobs: u64,
    }
    let rows: Vec<Count> = db
        .all(CLAIMS_BY_BUCKET, &params![since, crate::pool::BUCKET_MS])
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.template, row.bucket, row.jobs))
        .collect())
}

// Secrets and vars. A secret's value is sealed (see `crate::secrets`);
// a var's is plain text. An [`Owner`] says whose row it is.

/// Where a secret or var row lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The organization.
    Org,
    /// A repository.
    Repo(u64),
    /// An environment of a repository.
    Environment {
        /// The environment's repository.
        repo_id: u64,
        /// The environment.
        environment_id: u64,
    },
}

impl Owner {
    /// The repository, `None` for the organization.
    #[must_use]
    pub fn repo_id(self) -> Option<u64> {
        match self {
            Owner::Org => None,
            Owner::Repo(repo_id) | Owner::Environment { repo_id, .. } => Some(repo_id),
        }
    }

    /// The environment, if the row is an environment's.
    #[must_use]
    pub fn environment_id(self) -> Option<u64> {
        match self {
            Owner::Environment { environment_id, .. } => Some(environment_id),
            _ => None,
        }
    }

    /// From a row's columns: an environment row also has its repository.
    #[must_use]
    pub fn of(repo_id: Option<u64>, environment_id: Option<u64>) -> Owner {
        match (repo_id, environment_id) {
            (Some(repo_id), Some(environment_id)) => Owner::Environment {
                repo_id,
                environment_id,
            },
            (Some(repo_id), None) => Owner::Repo(repo_id),
            (None, _) => Owner::Org,
        }
    }
}

/// Whether a secret or var was stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// Stored, or replaced.
    Saved,
    /// A new name over the scope's limit.
    TooMany,
}

/// Stores secret `$4` of tenant `$1`, repository `$2` and environment
/// `$3` ($7 is the limit, $8 the time, $9 the repositories an
/// organization secret reaches as text like `{1,2}`, or NULL for all). A
/// new name is refused at the limit; replacing one never is. Returns a
/// row when it stored.
const PUT_SECRET: &str = "INSERT INTO ci_secrets \
       (tenant_id, repo_id, environment_id, name, key_id, sealed, updated_at, repositories) \
     SELECT $1, $2::bigint, $3::bigint, $4, $5, $6, $8, $9::bigint[] \
     WHERE EXISTS (SELECT 1 FROM ci_secrets WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint AND name = $4) \
       OR (SELECT count(*) FROM ci_secrets WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint) < $7 \
     ON CONFLICT (tenant_id, repo_id, environment_id, name) DO UPDATE SET \
       key_id = EXCLUDED.key_id, sealed = EXCLUDED.sealed, updated_at = EXCLUDED.updated_at, \
       repositories = EXCLUDED.repositories \
     RETURNING 1 AS saved";

/// As [`PUT_SECRET`], for a var with value `$5`; `$6` is the limit,
/// `$7` the time and `$8` the repositories.
const PUT_VAR: &str = "INSERT INTO ci_vars \
       (tenant_id, repo_id, environment_id, name, value, updated_at, repositories) \
     SELECT $1, $2::bigint, $3::bigint, $4, $5, $7, $8::bigint[] \
     WHERE EXISTS (SELECT 1 FROM ci_vars WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint AND name = $4) \
       OR (SELECT count(*) FROM ci_vars WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint) < $6 \
     ON CONFLICT (tenant_id, repo_id, environment_id, name) DO UPDATE SET \
       value = EXCLUDED.value, updated_at = EXCLUDED.updated_at, \
       repositories = EXCLUDED.repositories \
     RETURNING 1 AS saved";

/// What [`PUT_SECRET`] and [`PUT_VAR`] return.
#[derive(Deserialize)]
struct Saved {}

/// Runs a put statement with the tenant row locked, so two puts cannot
/// both take the last place.
async fn put(db: &Db, tenant: &str, sql: &str, bound: &[Param]) -> Result<Put> {
    let lock = params![tenant];
    let saved: Option<Saved> = db.first_locked((LOCK_TENANT, &lock), sql, bound).await?;
    Ok(if saved.is_some() {
        Put::Saved
    } else {
        Put::TooMany
    })
}

/// `repositories` as the text of a Postgres array, `None` for NULL.
fn array_text(repositories: Option<&[u64]>) -> Option<String> {
    repositories.map(|ids| {
        let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
        format!("{{{}}}", ids.join(","))
    })
}

/// Stores the sealed value of secret `name`, replacing the old one.
/// A new name is refused with [`Put::TooMany`] when the scope already
/// holds `limit` secrets. `repositories` limits an organization secret
/// to those `ci_repos` ids; `None` reaches every repository. Only an
/// organization row may have it (the table refuses the rest).
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)] // one column each
pub async fn put_secret(
    db: &Db,
    tenant: &str,
    owner: Owner,
    repositories: Option<&[u64]>,
    name: &str,
    key_id: &str,
    sealed: &str,
    limit: u32,
    now: u64,
) -> Result<Put> {
    put(
        db,
        tenant,
        PUT_SECRET,
        &params![
            tenant,
            owner.repo_id(),
            owner.environment_id(),
            name,
            key_id,
            sealed,
            limit,
            now,
            array_text(repositories)
        ],
    )
    .await
}

/// Deletes secret `name`. Returns whether it existed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delete_secret(db: &Db, tenant: &str, owner: Owner, name: &str) -> Result<bool> {
    db.run(
        "DELETE FROM ci_secrets WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint AND name = $4",
        &params![tenant, owner.repo_id(), owner.environment_id(), name],
    )
    .await
    .map(|n| n > 0)
}

/// The names of the repositories an organization row reaches: a JSON
/// array of `ci_repos.full_name`, or NULL when it reaches all of them.
/// Repositories renamed to `#gone-` (deleted or transferred) are left
/// out, so a list read back and sent in a `PUT` names only repositories
/// that still exist. For a query that aliases the secrets or vars table
/// as `s`.
const REPOSITORY_NAMES: &str = "CASE WHEN s.repositories IS NULL THEN NULL ELSE \
       COALESCE((SELECT json_agg(r.full_name ORDER BY lower(r.full_name)) FROM ci_repos r \
         WHERE r.id = ANY (s.repositories) AND r.tenant_id = s.tenant_id \
         AND position('#gone-' in r.full_name) = 0), \
         '[]'::json)::text END AS repositories";

/// A listed secret: never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretItem {
    /// The name.
    pub name: String,
    /// When it was last set.
    pub updated_at: u64,
    /// The repositories an organization secret reaches, `None` for all.
    pub repositories: Option<Vec<String>>,
}

/// A listed var.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarItem {
    /// The name.
    pub name: String,
    /// The value.
    pub value: String,
    /// When it was last set.
    pub updated_at: u64,
    /// The repositories an organization var reaches, `None` for all.
    pub repositories: Option<Vec<String>>,
}

/// The names in a row's `repositories` column.
fn repository_list(text: Option<&str>) -> Result<Option<Vec<String>>> {
    Ok(match text {
        Some(text) => Some(serde_json::from_str(text)?),
        None => None,
    })
}

#[derive(Deserialize)]
struct NameRow {
    name: String,
    updated_at: u64,
    repositories: Option<String>,
}

/// A scope's secrets: names and times, never values.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn secret_names(db: &Db, tenant: &str, owner: Owner) -> Result<Vec<SecretItem>> {
    let sql = format!(
        "SELECT s.name, s.updated_at, {REPOSITORY_NAMES} FROM ci_secrets s \
         WHERE s.tenant_id = $1 AND s.repo_id IS NOT DISTINCT FROM $2::bigint \
         AND s.environment_id IS NOT DISTINCT FROM $3::bigint ORDER BY s.name"
    );
    let rows: Vec<NameRow> = db
        .all(
            &sql,
            &params![tenant, owner.repo_id(), owner.environment_id()],
        )
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SecretItem {
                name: r.name,
                updated_at: r.updated_at,
                repositories: repository_list(r.repositories.as_deref())?,
            })
        })
        .collect()
}

/// A sealed secret as stored.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct SealedRow {
    /// The repository, or `None` for the organization.
    pub repo_id: Option<u64>,
    /// The environment, or `None` for the organization and a repository.
    pub environment_id: Option<u64>,
    /// The name.
    pub name: String,
    /// The key that sealed it.
    pub key_id: String,
    /// Base64 of nonce and ciphertext.
    pub sealed: String,
}

impl SealedRow {
    /// Whose row it is.
    #[must_use]
    pub fn owner(&self) -> Owner {
        Owner::of(self.repo_id, self.environment_id)
    }
}

impl std::fmt::Debug for SealedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedRow")
            .field("repo_id", &self.repo_id)
            .field("environment_id", &self.environment_id)
            .field("name", &self.name)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The organization's (those reaching the repository), repository
/// `repo_id`'s and (when set) environment `environment_id`'s sealed
/// secrets, named in `names` or all of them when `None`, ordered by
/// name, then organization, repository, environment. Every row of a
/// name is returned, so a caller that inserts them in order lets the
/// most specific win.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn sealed_secrets(
    db: &Db,
    tenant: &str,
    repo_id: u64,
    environment_id: Option<u64>,
    names: Option<&[String]>,
) -> Result<Vec<SealedRow>> {
    let mut sql = "SELECT repo_id, environment_id, name, key_id, sealed FROM ci_secrets \
                   WHERE tenant_id = $1 AND ((repo_id IS NULL \
                   AND (repositories IS NULL OR $2::bigint = ANY (repositories))) \
                   OR (repo_id = $2 \
                   AND (environment_id IS NULL OR environment_id = $3::bigint)))"
        .to_owned();
    let mut bound = params![tenant, repo_id, environment_id];
    if let Some(names) = names {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let marks: Vec<String> = (4..4 + names.len()).map(|n| format!("${n}")).collect();
        sql = format!("{sql} AND name IN ({})", marks.join(", "));
        bound.extend(names.iter().map(|name| Param::from(name.as_str())));
    }
    sql.push_str(" ORDER BY name, repo_id NULLS FIRST, environment_id NULLS FIRST");
    db.all(&sql, &bound).await
}

/// Stores var `name`, replacing the old value. A new name is refused
/// with [`Put::TooMany`] when the scope already holds `limit` vars.
/// `repositories` is as for [`put_secret`].
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)] // one column each
pub async fn put_var(
    db: &Db,
    tenant: &str,
    owner: Owner,
    repositories: Option<&[u64]>,
    name: &str,
    value: &str,
    limit: u32,
    now: u64,
) -> Result<Put> {
    put(
        db,
        tenant,
        PUT_VAR,
        &params![
            tenant,
            owner.repo_id(),
            owner.environment_id(),
            name,
            value,
            limit,
            now,
            array_text(repositories)
        ],
    )
    .await
}

/// Deletes var `name`. Returns whether it existed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delete_var(db: &Db, tenant: &str, owner: Owner, name: &str) -> Result<bool> {
    db.run(
        "DELETE FROM ci_vars WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint AND name = $4",
        &params![tenant, owner.repo_id(), owner.environment_id(), name],
    )
    .await
    .map(|n| n > 0)
}

#[derive(Deserialize)]
struct VarRow {
    name: String,
    value: String,
    updated_at: u64,
    repositories: Option<String>,
}

/// The vars of a scope, by name.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn vars(db: &Db, tenant: &str, owner: Owner) -> Result<Vec<VarItem>> {
    let sql = format!(
        "SELECT s.name, s.value, s.updated_at, {REPOSITORY_NAMES} FROM ci_vars s \
         WHERE s.tenant_id = $1 AND s.repo_id IS NOT DISTINCT FROM $2::bigint \
         AND s.environment_id IS NOT DISTINCT FROM $3::bigint ORDER BY s.name"
    );
    let rows: Vec<VarRow> = db
        .all(
            &sql,
            &params![tenant, owner.repo_id(), owner.environment_id()],
        )
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(VarItem {
                name: r.name,
                value: r.value,
                updated_at: r.updated_at,
                repositories: repository_list(r.repositories.as_deref())?,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct JobVar {
    name: String,
    value: String,
}

/// The organization's vars that reach repository `repo_id`, overlaid
/// with the repository's, then with environment `environment_id`'s.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn job_vars(
    db: &Db,
    tenant: &str,
    repo_id: u64,
    environment_id: Option<u64>,
) -> Result<BTreeMap<String, String>> {
    let rows: Vec<JobVar> = db
        .all(
            "SELECT name, value FROM ci_vars WHERE tenant_id = $1 \
             AND ((repo_id IS NULL \
               AND (repositories IS NULL OR $2::bigint = ANY (repositories))) \
               OR (repo_id = $2 \
               AND (environment_id IS NULL OR environment_id = $3::bigint))) \
             ORDER BY repo_id NULLS FIRST, environment_id NULLS FIRST",
            &params![tenant, repo_id, environment_id],
        )
        .await?;
    Ok(rows.into_iter().map(|r| (r.name, r.value)).collect())
}

#[derive(Deserialize)]
struct RepoName {
    full_name: String,
}

#[derive(Deserialize)]
struct RepoId {
    full_name: String,
    id: u64,
}

/// The tenant's repositories CI has seen, by `lower(full_name)`, without
/// names that hold `#gone-`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_repos(db: &Db, tenant: &str) -> Result<Vec<String>> {
    let rows: Vec<RepoName> = db
        .all(
            "SELECT full_name FROM ci_repos WHERE tenant_id = $1 \
             AND position('#gone-' in full_name) = 0 ORDER BY lower(full_name)",
            &params![tenant],
        )
        .await?;
    Ok(rows.into_iter().map(|r| r.full_name).collect())
}

/// The ids of the tenant's repositories named in `names` (ignoring case),
/// as (name as stored, id).
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo_ids(db: &Db, tenant: &str, names: &[String]) -> Result<Vec<(String, u64)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<String> = (2..2 + names.len())
        .map(|n| format!("lower(${n})"))
        .collect();
    let sql = format!(
        "SELECT full_name, id FROM ci_repos WHERE tenant_id = $1 \
         AND lower(full_name) IN ({}) AND position('#gone-' in full_name) = 0 \
         ORDER BY lower(full_name)",
        marks.join(", ")
    );
    let mut bound = params![tenant];
    bound.extend(names.iter().map(|name| Param::from(name.as_str())));
    let rows: Vec<RepoId> = db.all(&sql, &bound).await?;
    Ok(rows.into_iter().map(|r| (r.full_name, r.id)).collect())
}

/// A secret sealed with a key other than the current one.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct StaleRow {
    /// The tenant.
    pub tenant: String,
    /// The repository, or `None` for the organization.
    pub repo_id: Option<u64>,
    /// The environment, or `None` for the organization and a repository.
    pub environment_id: Option<u64>,
    /// The name.
    pub name: String,
    /// The key that sealed it.
    pub key_id: String,
    /// Base64 of nonce and ciphertext.
    pub sealed: String,
}

impl StaleRow {
    /// Whose row it is.
    #[must_use]
    pub fn owner(&self) -> Owner {
        Owner::of(self.repo_id, self.environment_id)
    }
}

impl std::fmt::Debug for StaleRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaleRow")
            .field("tenant", &self.tenant)
            .field("repo_id", &self.repo_id)
            .field("environment_id", &self.environment_id)
            .field("name", &self.name)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Up to `limit` secrets not sealed with `current_key_id`. The pick is
/// random, so rows that cannot be opened do not starve the others.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn stale_secrets(db: &Db, current_key_id: &str, limit: u32) -> Result<Vec<StaleRow>> {
    db.all(
        "SELECT tenant_id AS tenant, repo_id, environment_id, name, key_id, sealed \
         FROM ci_secrets WHERE key_id <> $1 ORDER BY random() LIMIT $2",
        &params![current_key_id, limit],
    )
    .await
}

/// Replaces the value of `row` with `sealed` under `key_id`, only while
/// the row still holds the key id and sealed text of `row`. Returns
/// whether it did: `false` means the secret changed meanwhile.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn reseal(db: &Db, row: &StaleRow, key_id: &str, sealed: &str) -> Result<bool> {
    db.run(
        "UPDATE ci_secrets SET key_id = $6, sealed = $7 WHERE tenant_id = $1 \
         AND repo_id IS NOT DISTINCT FROM $2::bigint \
         AND environment_id IS NOT DISTINCT FROM $3::bigint AND name = $4 AND key_id = $5 \
         AND sealed = $8",
        &params![
            row.tenant.as_str(),
            row.repo_id,
            row.environment_id,
            row.name.as_str(),
            row.key_id.as_str(),
            key_id,
            sealed,
            row.sealed.as_str()
        ],
    )
    .await
    .map(|n| n > 0)
}

// Environments.

/// Reads a JSON array of strings that the row carries as text.
fn json_strings<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    let text = String::deserialize(d)?;
    serde_json::from_str(&text).map_err(D::Error::custom)
}

/// An environment of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EnvironmentRow {
    /// The environment's identity.
    pub id: u64,
    /// Its repository.
    pub repo_id: u64,
    /// The name, in the case it was first saved with.
    pub name: String,
    /// The branch patterns a run's ref must match; empty allows any.
    #[serde(deserialize_with = "json_strings")]
    pub branches: Vec<String>,
    /// Whether a job that names it waits for an approval.
    pub approval: bool,
    /// When it last changed.
    pub updated_at: u64,
}

/// The columns of an [`EnvironmentRow`].
const ENVIRONMENT_COLUMNS: &str =
    "id, repo_id, name, branches::text AS branches, approval, updated_at";

/// What [`put_environment`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutEnvironment {
    /// Stored: created, or updated.
    Saved(EnvironmentRow),
    /// A new name over the repository's limit.
    TooMany,
}

/// The environments of `repo_id`, by name ignoring case.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn environments(db: &Db, repo_id: u64) -> Result<Vec<EnvironmentRow>> {
    db.all(
        &format!(
            "SELECT {ENVIRONMENT_COLUMNS} FROM ci_environments WHERE repo_id = $1 \
             ORDER BY lower(name), name"
        ),
        &params![repo_id],
    )
    .await
}

/// The environment of `repo_id` called `name`, ignoring case.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn environment(db: &Db, repo_id: u64, name: &str) -> Result<Option<EnvironmentRow>> {
    db.first(
        &format!(
            "SELECT {ENVIRONMENT_COLUMNS} FROM ci_environments \
             WHERE repo_id = $1 AND lower(name) = lower($2)"
        ),
        &params![repo_id, name],
    )
    .await
}

/// Creates the environment, or updates the branches and approval of the
/// row of that name in any case (its stored name stays). A new name is
/// refused with [`PutEnvironment::TooMany`] when the repository already
/// holds `limit`. Runs with the tenant row locked, so two puts cannot
/// both take the last place.
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)] // one column each
pub async fn put_environment(
    db: &Db,
    tenant: &str,
    repo_id: u64,
    name: &str,
    branches: &[String],
    approval: bool,
    limit: u32,
    now: u64,
) -> Result<PutEnvironment> {
    let branches = serde_json::to_string(branches).map_err(worker::Error::from)?;
    let sql = format!(
        "WITH updated AS (UPDATE ci_environments SET branches = $4::jsonb, approval = $5, \
           updated_at = $7 WHERE repo_id = $2 AND lower(name) = lower($3) \
           RETURNING {ENVIRONMENT_COLUMNS}), \
         inserted AS (INSERT INTO ci_environments \
           (tenant_id, repo_id, name, branches, approval, updated_at) \
           SELECT $1, $2::bigint, $3, $4::jsonb, $5, $7 \
           WHERE NOT EXISTS (SELECT 1 FROM updated) \
             AND (SELECT count(*) FROM ci_environments WHERE repo_id = $2) < $6 \
           RETURNING {ENVIRONMENT_COLUMNS}) \
         SELECT * FROM updated UNION ALL SELECT * FROM inserted"
    );
    let lock = params![tenant];
    let saved: Option<EnvironmentRow> = db
        .first_locked(
            (LOCK_TENANT, &lock),
            &sql,
            &params![tenant, repo_id, name, branches, approval, limit, now],
        )
        .await?;
    Ok(saved.map_or(PutEnvironment::TooMany, PutEnvironment::Saved))
}

/// Deletes the environment of `repo_id` called `name`, ignoring case,
/// with its secrets and vars. Returns whether it existed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delete_environment(db: &Db, repo_id: u64, name: &str) -> Result<bool> {
    db.run(
        "DELETE FROM ci_environments WHERE repo_id = $1 AND lower(name) = lower($2)",
        &params![repo_id, name],
    )
    .await
    .map(|n| n > 0)
}

/// What the launch consumer and the claim need about a job's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnvironment {
    /// The job's run.
    pub run_id: u64,
    /// The run's repository.
    pub repo_id: u64,
    /// `owner/repo`.
    pub full_name: String,
    /// The event that started the run.
    pub event: String,
    /// The run's ref.
    pub git_ref: String,
    /// The environment the job names, `None` when the repository has
    /// none of that name.
    pub environment: Option<EnvironmentRow>,
}

/// The columns [`job_environment`] reads; the environment's are null
/// when the repository has none of the job's name.
#[derive(Deserialize)]
struct JobEnvironmentRow {
    run_id: u64,
    repo_id: u64,
    full_name: String,
    event: String,
    git_ref: String,
    environment_id: Option<u64>,
    environment_name: Option<String>,
    branches: Option<String>,
    approval: Option<bool>,
    environment_updated_at: Option<u64>,
}

/// The run of job `job_id` and the environment it names.
///
/// # Errors
///
/// Postgres refused the statement, or a stored `branches` is not a JSON
/// array of strings.
pub async fn job_environment(db: &Db, job_id: u64) -> Result<Option<JobEnvironment>> {
    let row: Option<JobEnvironmentRow> = db
        .first(
            "SELECT ci_runs.id AS run_id, ci_runs.repo_id, ci_repos.full_name, ci_runs.event, \
               ci_runs.ref AS git_ref, e.id AS environment_id, e.name AS environment_name, \
               e.branches::text AS branches, e.approval, e.updated_at AS environment_updated_at \
             FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
               JOIN ci_repos ON ci_repos.id = ci_runs.repo_id \
               LEFT JOIN ci_environments e ON e.repo_id = ci_runs.repo_id \
                 AND lower(e.name) = lower(ci_jobs.environment) \
             WHERE ci_jobs.id = $1",
            &params![job_id],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let environment = match (
        row.environment_id,
        row.environment_name,
        row.branches,
        row.approval,
        row.environment_updated_at,
    ) {
        (Some(id), Some(name), Some(branches), Some(approval), Some(updated_at)) => {
            Some(EnvironmentRow {
                id,
                repo_id: row.repo_id,
                name,
                branches: serde_json::from_str(&branches).map_err(worker::Error::from)?,
                approval,
                updated_at,
            })
        }
        _ => None,
    };
    Ok(Some(JobEnvironment {
        run_id: row.run_id,
        repo_id: row.repo_id,
        full_name: row.full_name,
        event: row.event,
        git_ref: row.git_ref,
        environment,
    }))
}

/// Clears the approval holds of queued jobs in repository `repo_id` that
/// name environment `name` (ignoring case), due at `now`, and returns
/// them for the launch queue.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_approval_holds(
    db: &Db,
    repo_id: u64,
    name: &str,
    now: u64,
) -> Result<Vec<hatchrun_cloud::ci_wake::Woken>> {
    db.all(
        "UPDATE ci_jobs SET held = NULL, next_launch_at = $3, updated_at = $3 FROM ci_runs \
         WHERE ci_runs.id = ci_jobs.run_id AND ci_runs.repo_id = $1 \
           AND lower(ci_jobs.environment) = lower($2) \
           AND ci_jobs.status = 'queued' AND ci_jobs.held = 'approval' \
         RETURNING ci_jobs.id AS job_id, ci_jobs.attempt",
        &params![repo_id, name, now],
    )
    .await
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

    /// Claims job `job_id` of tenant t1, which must succeed.
    fn claim(db: &TestDb, job_id: u64, now: u64) -> JobRow {
        match db
            .wait(claim_launch(db.db(), job_id, "t1", 5, now))
            .unwrap()
        {
            Claim::Claimed(job) => job,
            other => panic!("not claimed: {other:?}"),
        }
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
            template: Some("ubuntu-latest".into()),
            token_permissions: BTreeMap::from([("metadata".to_owned(), Level::Read)]),
            secret_refs: SecretRefs::default(),
            environment: None,
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

    /// As [`run_with_job`], for a run of delivery `delivery`.
    fn run_with_job_named(db: &TestDb, delivery: &str, status: &str) -> (RunRow, u64) {
        let run = db
            .wait(insert_run(db.db(), &new_run(delivery, "ci", 1_000)))
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

    fn pool_row(id: &str, template: &str, state: &str, launched_at: u64) -> PoolRow {
        PoolRow {
            instance_id: id.into(),
            template: template.into(),
            state: state.into(),
            launched_at,
            since: launched_at,
        }
    }

    fn save(db: &TestDb, template: &str, rows: &[PoolRow], launched: &[PoolRow]) {
        db.wait(save_pool(db.db(), template, rows, launched))
            .unwrap();
    }

    #[test]
    fn take_pool_takes_ready_only() {
        let db = setup();
        save(
            &db,
            "ubuntu-latest",
            &[],
            &[pool_row("i-warm", "ubuntu-latest", "warming", 1)],
        );
        assert_eq!(
            db.wait(take_pool(db.db(), "ubuntu-latest")).unwrap(),
            None,
            "a warming row is not ready"
        );
        save(
            &db,
            "ubuntu-latest",
            &[],
            &[
                pool_row("i-b", "ubuntu-latest", "ready", 20),
                pool_row("i-a", "ubuntu-latest", "ready", 10),
            ],
        );
        save(
            &db,
            "ubuntu-26.04-8core",
            &[],
            &[pool_row("i-other", "ubuntu-26.04-8core", "ready", 1)],
        );
        let taken = |db: &TestDb| db.wait(take_pool(db.db(), "ubuntu-latest")).unwrap();
        assert_eq!(taken(&db).as_deref(), Some("i-a"));
        assert_eq!(taken(&db).as_deref(), Some("i-b"));
        assert_eq!(taken(&db), None);
        let left = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].instance_id, "i-warm");
    }

    #[test]
    fn trim_skips_a_taken_instance() {
        let db = setup();
        save(
            &db,
            "ubuntu-latest",
            &[],
            &[pool_row("i-1", "ubuntu-latest", "ready", 1)],
        );
        assert_eq!(
            db.wait(take_pool(db.db(), "ubuntu-latest"))
                .unwrap()
                .as_deref(),
            Some("i-1")
        );
        assert!(!db.wait(drop_pool(db.db(), "i-1")).unwrap());
    }

    #[test]
    fn a_warming_row_moves_to_stopping_once() {
        let db = setup();
        save(
            &db,
            "ubuntu-latest",
            &[],
            &[pool_row("i-1", "ubuntu-latest", "warming", 1)],
        );
        let to_stopping = |db: &TestDb| {
            db.wait(move_pool(db.db(), "i-1", "warming", "stopping", 9))
                .unwrap()
        };
        assert!(to_stopping(&db));
        assert!(!to_stopping(&db));
        let rows = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
        assert_eq!((rows[0].state.as_str(), rows[0].since), ("stopping", 9));
    }

    #[test]
    fn save_pool_never_brings_back_a_taken_row() {
        let db = setup();
        save(
            &db,
            "ubuntu-latest",
            &[],
            &[
                pool_row("i-1", "ubuntu-latest", "ready", 1),
                pool_row("i-2", "ubuntu-latest", "warming", 2),
            ],
        );
        let before = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
        assert_eq!(before.len(), 2);
        // A job takes i-1 while the pass works on its copy.
        assert_eq!(
            db.wait(take_pool(db.db(), "ubuntu-latest"))
                .unwrap()
                .as_deref(),
            Some("i-1")
        );
        let mut stopping = pool_row("i-2", "ubuntu-latest", "stopping", 2);
        stopping.since = 50;
        save(
            &db,
            "ubuntu-latest",
            &[pool_row("i-1", "ubuntu-latest", "ready", 1), stopping],
            &[pool_row("i-3", "ubuntu-latest", "warming", 60)],
        );
        let rows = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
        let ids: Vec<_> = rows.iter().map(|row| row.instance_id.as_str()).collect();
        assert_eq!(ids, ["i-2", "i-3"], "i-1 stays gone");
        assert_eq!((rows[0].state.as_str(), rows[0].since), ("stopping", 50));
    }

    #[test]
    fn claims_by_bucket_groups_by_template() {
        let db = setup();
        let claimed = |delivery: &str, template: &str, started_at: u64| {
            let (_, job) = run_with_job_named(&db, delivery, "queued");
            db.run(
                "UPDATE ci_jobs SET template = $2, started_at = $3 WHERE id = $1",
                &params![job, template, started_at],
            );
        };
        claimed("a", "ubuntu-latest", 600_001);
        claimed("b", "ubuntu-latest", 899_999);
        claimed("c", "ubuntu-latest", 900_000);
        claimed("d", "ubuntu-26.04-8core", 650_000);
        claimed("e", "ubuntu-latest", 100_000);
        let (_, never) = run_with_job_named(&db, "f", "queued");
        db.run(
            "UPDATE ci_jobs SET template = 'ubuntu-latest' WHERE id = $1",
            &params![never],
        );
        let mut found = db.wait(claims_by_bucket(db.db(), 600_000)).unwrap();
        found.sort();
        assert_eq!(
            found,
            [
                ("ubuntu-26.04-8core".to_owned(), 600_000, 1),
                ("ubuntu-latest".to_owned(), 600_000, 2),
                ("ubuntu-latest".to_owned(), 900_000, 1),
            ]
        );
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
    fn expiry_follows_the_tenant() {
        let db = setup();
        let expires = |run: &RunRow| -> u64 {
            db.column(
                "SELECT expires_at AS n FROM ci_runs WHERE id = $1",
                &params![run.id],
                "n",
            )
            .unwrap()
        };
        let short = db
            .wait(insert_run(db.db(), &new_run("a", "ci", 5)))
            .unwrap()
            .unwrap();
        assert_eq!((expires(&short), short.retention_days), (5 + 30 * DAY, 30));
        db.run(
            "UPDATE tenants SET retention_days = 90 WHERE id = 't1'",
            &[],
        );
        let long = db
            .wait(insert_run(db.db(), &new_run("b", "ci", 5)))
            .unwrap()
            .unwrap();
        assert_eq!((expires(&long), long.retention_days), (5 + 90 * DAY, 90));
        let found = db.wait(run(db.db(), long.id)).unwrap().unwrap();
        assert_eq!(found.retention_days, 90);
        let tenant = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
        assert_eq!(tenant.retention_days, 90);
    }

    #[test]
    fn retention_days_is_30_or_90() {
        let db = setup();
        // hatchrun.io lets a tenant keep 1 to 90 days; CI has two tiers.
        for (set, tier) in [(1, 30), (30, 30), (31, 90), (60, 90), (90, 90)] {
            db.run(
                "UPDATE tenants SET retention_days = $1 WHERE id = 't1'",
                &params![set],
            );
            let tenant = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
            assert_eq!(tenant.retention_days, tier, "{set} days");
            let made = db
                .wait(insert_run(db.db(), &new_run(&format!("r{set}"), "ci", 5)))
                .unwrap()
                .unwrap();
            assert_eq!(made.retention_days, tier, "{set} days");
        }
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
    fn a_transferred_repo_leaves_its_secrets_and_vars_behind() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 100, 0, 0)",
            &[],
        );
        put_secret_ok(&db, Owner::Repo(7), "REPO_S", "k", "s1");
        put_secret_ok(&db, Owner::Org, "ORG_S", "k", "s2");
        put_var_ok(&db, Owner::Repo(7), "REPO_V", "1");
        put_var_ok(&db, Owner::Org, "ORG_V", "2");
        // Re-registering the repository under its own tenant keeps them.
        let mut same = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        db.wait(upsert_repo(db.db(), &same, 6)).unwrap();
        assert_eq!(
            db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
                .unwrap()
                .len(),
            1
        );
        same.tenant_id = "t2".into();
        same.installation_id = 100;
        db.wait(upsert_repo(db.db(), &same, 7)).unwrap();
        assert_eq!(
            db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            db.wait(vars(db.db(), "t1", Owner::Repo(7))).unwrap().len(),
            0
        );
        // The old organization's own rows stay.
        assert_eq!(
            db.wait(secret_names(db.db(), "t1", Owner::Org))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(db.wait(vars(db.db(), "t1", Owner::Org)).unwrap().len(), 1);
    }

    #[test]
    fn a_recreated_repository_takes_the_name_and_the_old_runs_stay() {
        let db = setup();
        let old = db
            .wait(insert_run(db.db(), &new_run("d1", "ci", 2)))
            .unwrap()
            .unwrap();
        let fresh = RepoRow {
            id: 8,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "Acme/App".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
        let by_name = db.wait(repo_by_name(db.db(), "acme/app")).unwrap().unwrap();
        assert_eq!(by_name.id, 8);
        let stale = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        assert_eq!(stale.full_name, "acme/app#gone-7");
        assert!(db.wait(run(db.db(), old.id)).unwrap().is_some());
    }

    #[test]
    fn a_rename_swap_keeps_both_repositories() {
        let db = setup();
        let other = RepoRow {
            id: 8,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "acme/lib".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &other, 2)).unwrap();
        let name = |id| db.wait(repo(db.db(), id)).unwrap().unwrap().full_name;
        // app and lib swap names. The event for app arrives first, while
        // lib still holds its new name.
        let mut app = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        app.full_name = "acme/lib".into();
        db.wait(upsert_repo(db.db(), &app, 3)).unwrap();
        assert_eq!(name(7), "acme/lib");
        assert_eq!(name(8), "acme/lib#gone-8");
        // Then the event for lib.
        let mut lib = db.wait(repo(db.db(), 8)).unwrap().unwrap();
        lib.full_name = "acme/app".into();
        db.wait(upsert_repo(db.db(), &lib, 4)).unwrap();
        assert_eq!(name(7), "acme/lib");
        assert_eq!(name(8), "acme/app");
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
    fn insert_jobs_keeps_template_permissions_and_refs() {
        let db = setup();
        let run = db
            .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
            .unwrap()
            .unwrap();
        let build = NewJob {
            template: Some("ubuntu-26.04-8core-arm".into()),
            token_permissions: BTreeMap::from([
                ("contents".to_owned(), Level::Write),
                ("metadata".to_owned(), Level::Read),
            ]),
            secret_refs: SecretRefs {
                all: false,
                names: ["NPM_TOKEN".to_owned()].into(),
            },
            ..new_job("build")
        };
        let approve = NewJob {
            template: None,
            ..new_job("approve")
        };
        db.wait(insert_jobs(db.db(), run.id, &[build, approve], 2))
            .unwrap();
        let jobs = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
        assert_eq!(jobs[0].template.as_deref(), Some("ubuntu-26.04-8core-arm"));
        assert_eq!(
            jobs[0].token_permissions,
            serde_json::json!({ "contents": "write", "metadata": "read" })
        );
        assert_eq!(
            jobs[0].secret_refs,
            serde_json::json!({ "all": false, "names": ["NPM_TOKEN"] })
        );
        let one = db.wait(job(db.db(), jobs[0].id)).unwrap().unwrap();
        assert_eq!(one, jobs[0]);
        assert_eq!(jobs[1].template, None);
        assert_eq!(
            jobs[1].secret_refs,
            serde_json::json!({ "all": false, "names": [] })
        );
    }

    #[test]
    fn claim_launch_only_once() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        let first = claim(&db, job_id, 10);
        assert_eq!(first.launch_attempts, 1);
        assert_eq!(
            db.wait(claim_launch(db.db(), job_id, "t1", 5, 11)).unwrap(),
            Claim::Gone
        );
        let job = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(job.status, "launching");
    }

    fn held_of(db: &TestDb, job_id: u64) -> Option<String> {
        db.column(
            "SELECT held AS h FROM ci_jobs WHERE id = $1",
            &params![job_id],
            "h",
        )
        .unwrap()
    }

    #[test]
    fn claim_launch_holds_the_sixth_job() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 98, 0, 0)",
            &[],
        );
        db.run(
            "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
             private, updated_at) VALUES (8, 't2', 98, 'other/app', 'main', false, 1)",
            &[],
        );
        let other = db
            .wait(insert_run(
                db.db(),
                &NewRun {
                    tenant_id: "t2".into(),
                    repo_id: 8,
                    ..new_run("o", "ci", 1_000)
                },
            ))
            .unwrap()
            .unwrap();
        let other_jobs: Vec<NewJob> = (0..5).map(|n| new_job(&format!("o{n}"))).collect();
        for (_, id) in db
            .wait(insert_jobs(db.db(), other.id, &other_jobs, 1_000))
            .unwrap()
        {
            db.wait(set_job(db.db(), id, "running", None, 1_001))
                .unwrap();
        }
        let statuses = ["launching", "launched", "running", "running", "launching"];
        for (n, status) in statuses.into_iter().enumerate() {
            run_with_job_named(&db, &format!("d{n}"), status);
        }
        let (_, sixth) = run_with_job_named(&db, "d9", "queued");
        assert_eq!(
            db.wait(claim_launch(db.db(), sixth, "t1", 5, 10)).unwrap(),
            Claim::Busy
        );
        let row = db.wait(job(db.db(), sixth)).unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.launch_attempts), ("queued", 0));
        // A completed job frees a place.
        let first = db
            .wait(insert_run(db.db(), &new_run("free", "ci", 1_000)))
            .unwrap()
            .unwrap();
        let done = db
            .wait(insert_jobs(db.db(), first.id, &[new_job("done")], 1_000))
            .unwrap()[0]
            .1;
        db.wait(set_job(db.db(), done, "completed", Some("success"), 1_002))
            .unwrap();
        assert_eq!(
            db.wait(claim_launch(db.db(), sixth, "t1", 5, 11)).unwrap(),
            Claim::Busy,
            "completed jobs do not count, but five still run"
        );
        let running: u64 = db
            .column(
                "SELECT id AS n FROM ci_jobs WHERE status = 'launching' ORDER BY id LIMIT 1",
                &[],
                "n",
            )
            .unwrap();
        db.wait(set_job(
            db.db(),
            running,
            "completed",
            Some("success"),
            1_003,
        ))
        .unwrap();
        assert!(matches!(
            db.wait(claim_launch(db.db(), sixth, "t1", 5, 12)).unwrap(),
            Claim::Claimed(_)
        ));
        let gone = db
            .wait(claim_launch(db.db(), 999_999, "t1", 5, 13))
            .unwrap();
        assert_eq!(gone, Claim::Gone);
    }

    /// Sets the hold of a job directly, the way `hold` leaves it.
    fn set_held(db: &TestDb, job_id: u64, reason: &str, updated: u64) {
        db.run(
            "UPDATE ci_jobs SET held = $2, updated_at = $3 WHERE id = $1",
            &params![job_id, reason, updated],
        );
    }

    fn woken_ids(woken: Vec<hatchrun_cloud::ci_wake::Woken>) -> Vec<u64> {
        woken.into_iter().map(|w| w.job_id).collect()
    }

    #[test]
    fn a_new_job_does_not_jump_a_held_one() {
        let db = setup();
        for (n, status) in ["running", "running", "running", "running"]
            .into_iter()
            .enumerate()
        {
            run_with_job_named(&db, &format!("r{n}"), status);
        }
        let (_, older) = run_with_job_named(&db, "older", "queued");
        let (_, newer) = run_with_job_named(&db, "newer", "queued");
        set_held(&db, older, "concurrency", 5);
        // One place is free, and the older job waits for it.
        assert_eq!(
            db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
            Claim::Busy
        );
        assert!(matches!(
            db.wait(claim_launch(db.db(), older, "t1", 5, 11)).unwrap(),
            Claim::Claimed(_)
        ));
        assert_eq!(
            db.wait(claim_launch(db.db(), newer, "t1", 5, 12)).unwrap(),
            Claim::Busy,
            "the tenant is full again"
        );
    }

    #[test]
    fn a_held_job_of_another_reason_or_tenant_blocks_nobody() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 98, 0, 0)",
            &[],
        );
        db.run(
            "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
             private, updated_at) VALUES (8, 't2', 98, 'other/app', 'main', false, 1)",
            &[],
        );
        let other = db
            .wait(insert_run(
                db.db(),
                &NewRun {
                    tenant_id: "t2".into(),
                    repo_id: 8,
                    ..new_run("o", "ci", 1_000)
                },
            ))
            .unwrap()
            .unwrap();
        let held_elsewhere = db
            .wait(insert_jobs(db.db(), other.id, &[new_job("o")], 1_000))
            .unwrap()[0]
            .1;
        db.wait(set_job(db.db(), held_elsewhere, "queued", None, 1_001))
            .unwrap();
        set_held(&db, held_elsewhere, "concurrency", 5);
        let (_, capped) = run_with_job_named(&db, "capped", "queued");
        set_held(&db, capped, "spend_cap", 5);
        let (_, newer) = run_with_job_named(&db, "newer", "queued");
        assert!(matches!(
            db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
            Claim::Claimed(_)
        ));
    }

    #[test]
    fn the_oldest_held_job_wakes_first() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        let mut ids = Vec::new();
        for n in 0..3 {
            let (_, id) = run_with_job_named(&db, &format!("d{n}"), "queued");
            // The newest was held longest ago: age is the job id.
            set_held(&db, id, "concurrency", 100 - n);
            ids.push(id);
        }
        let (_, running) = run_with_job_named(&db, "run", "running");
        set_held(&db, running, "concurrency", 1);
        let (_, card) = run_with_job_named(&db, "card", "queued");
        set_held(&db, card, "card", 1);
        let one = db
            .wait(ci_wake::held(db.db(), "t1", "concurrency", 1))
            .unwrap();
        assert_eq!(woken_ids(one), [ids[0]]);
        let all = db
            .wait(ci_wake::held(db.db(), "t1", "concurrency", 5))
            .unwrap();
        assert_eq!(woken_ids(all), ids);
        let other = db
            .wait(ci_wake::held(db.db(), "t2", "concurrency", 5))
            .unwrap();
        assert_eq!(woken_ids(other), Vec::<u64>::new());
    }

    #[test]
    fn a_returned_card_turns_the_holds_into_concurrency_holds_and_wakes_five() {
        use hatchrun_cloud::ci_wake;
        assert_eq!(ci_wake::WAKE_LIMIT, hatchrun_workflow::MAX_RUNNING_JOBS);
        let db = setup();
        let mut card = Vec::new();
        for n in 0..7 {
            let (_, id) = run_with_job_named(&db, &format!("c{n}"), "queued");
            set_held(&db, id, "card", 5);
            card.push(id);
        }
        let (_, capped) = run_with_job_named(&db, "cap", "queued");
        set_held(&db, capped, "spend_cap", 5);
        let (_, other) = run_with_job_named(&db, "plain", "queued");
        subscribe(&db, "s1", "active");
        let woken = db.wait(ci_wake::card_returned(db.db(), "t1")).unwrap();
        assert_eq!(woken_ids(woken), card[..5]);
        for id in card.iter().chain([&capped]) {
            assert_eq!(held_of(&db, *id).as_deref(), Some("concurrency"));
        }
        assert_eq!(held_of(&db, other), None, "an unheld job stays unheld");
        // A finishing job wakes the next one in order.
        let next = db
            .wait(ci_wake::held(db.db(), "t1", "concurrency", 1))
            .unwrap();
        assert_eq!(woken_ids(next), card[..1]);
    }

    #[test]
    fn a_new_month_turns_old_spend_cap_holds_into_concurrency_holds() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        let (_, old) = run_with_job_named(&db, "old", "queued");
        set_held(&db, old, "spend_cap", 100);
        let (_, recent) = run_with_job_named(&db, "recent", "queued");
        set_held(&db, recent, "spend_cap", 10_000);
        let (_, card) = run_with_job_named(&db, "card", "queued");
        set_held(&db, card, "card", 100);
        db.wait(ci_wake::release_spend_caps(db.db(), 5_000))
            .unwrap();
        assert_eq!(held_of(&db, old).as_deref(), Some("concurrency"));
        assert_eq!(held_of(&db, recent).as_deref(), Some("spend_cap"));
        assert_eq!(held_of(&db, card).as_deref(), Some("card"));
    }

    #[test]
    fn the_backstop_picks_quiet_held_jobs_per_tenant() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        let mut ids = Vec::new();
        for n in 0..8 {
            let (_, id) = run_with_job_named(&db, &format!("h{n}"), "queued");
            set_held(&db, id, if n % 2 == 0 { "card" } else { "spend_cap" }, 100);
            ids.push(id);
        }
        let (_, recent) = run_with_job_named(&db, "recent", "queued");
        set_held(&db, recent, "card", 10_000);
        let (_, plain) = run_with_job_named(&db, "plain", "queued");
        let _ = plain;
        let picked = db.wait(ci_wake::stale(db.db(), 5_000, 5, 40)).unwrap();
        assert_eq!(woken_ids(picked), ids[..5], "5 per tenant, oldest first");
        let capped = db.wait(ci_wake::stale(db.db(), 5_000, 5, 2)).unwrap();
        assert_eq!(woken_ids(capped), ids[..2], "at most the limit");
        let none = db.wait(ci_wake::stale(db.db(), 50, 5, 40)).unwrap();
        assert!(none.is_empty(), "nothing is quiet that long");
    }

    #[test]
    fn the_backstop_skips_approval_holds() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        let (_, approval) = run_with_job_named(&db, "a", "queued");
        set_held(&db, approval, "approval", 100);
        let (_, card) = run_with_job_named(&db, "c", "queued");
        set_held(&db, card, "card", 100);
        let picked = db.wait(ci_wake::stale(db.db(), 5_000, 5, 40)).unwrap();
        assert_eq!(woken_ids(picked), [card]);
    }

    #[test]
    fn an_approval_hold_blocks_nobody() {
        let db = setup();
        let (_, older) = run_with_job_named(&db, "older", "queued");
        set_held(&db, older, "approval", 5);
        let (_, newer) = run_with_job_named(&db, "newer", "queued");
        assert!(matches!(
            db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
            Claim::Claimed(_)
        ));
        let month = 1_790_812_800_000_u64;
        let standing = db
            .wait(standing(db.db(), older, month, 1_000_000))
            .unwrap()
            .unwrap();
        assert_eq!(standing.running, 1, "the launching job, not the held one");
    }

    fn decide(db: &TestDb, job_id: u64, attempt_of_decision: u32) {
        db.run(
            "UPDATE ci_jobs SET approval_attempt = $2, approval_decision = 'approved', \
             approval_by = 'octo', approval_at = 1 WHERE id = $1",
            &params![job_id, attempt_of_decision],
        );
    }

    #[test]
    fn an_approval_hold_does_not_land_on_a_decided_attempt() {
        let db = setup();
        let (_, job) = run_with_job_named(&db, "raced", "queued");
        let attempt: u32 = 1;
        decide(&db, job, attempt);
        assert!(
            !db.wait(hold(db.db(), job, attempt, "approval", 9_000, 2_000))
                .unwrap()
        );
        assert_eq!(held_of(&db, job), None);
        // A billing hold still lands on an approved attempt.
        assert!(
            db.wait(hold(db.db(), job, attempt, "card", 9_000, 2_000))
                .unwrap()
        );
        assert_eq!(held_of(&db, job).as_deref(), Some("card"));
        // A decision of an earlier attempt does not count.
        let (_, other) = run_with_job_named(&db, "rerun", "queued");
        decide(&db, other, 0);
        assert!(
            db.wait(hold(db.db(), other, attempt, "approval", 9_000, 2_000))
                .unwrap()
        );
    }

    #[test]
    fn the_backstop_frees_decided_approval_holds_up_to_the_limit() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        save_environment(&db, "Guarded", &[], true);
        let mut decided = Vec::new();
        for n in 0..3 {
            let id = job_naming(&db, &format!("d{n}"), Some("guarded"));
            db.run(
                "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 100 \
                 WHERE id = $1",
                &params![id],
            );
            decided.push(id);
        }
        decide(&db, decided[0], 1);
        decide(&db, decided[1], 1);
        let freed = woken_ids(
            db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 1))
                .unwrap(),
        );
        assert_eq!(freed, [decided[0]], "limit of one, oldest first");
        let freed = woken_ids(
            db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 10))
                .unwrap(),
        );
        assert_eq!(freed, [decided[1]]);
        assert_eq!(held_of(&db, decided[2]).as_deref(), Some("approval"));
    }

    #[test]
    fn a_queued_job_keeps_the_reason_it_was_ended_for() {
        let db = setup();
        let (_, job) = run_with_job_named(&db, "q", "queued");
        assert!(
            db.wait(note_queued_error(db.db(), job, "no such environment", 5))
                .unwrap()
        );
        let error: Option<String> = db
            .column(
                "SELECT launch_error AS e FROM ci_jobs WHERE id = $1",
                &params![job],
                "e",
            )
            .unwrap();
        assert_eq!(error.as_deref(), Some("no such environment"));
        let (_, running) = run_with_job_named(&db, "r", "running");
        assert!(
            !db.wait(note_queued_error(db.db(), running, "x", 5))
                .unwrap()
        );
    }

    #[test]
    fn the_backstop_frees_approval_holds_without_a_rule() {
        use hatchrun_cloud::ci_wake;
        let db = setup();
        let held = |name: &str, environment: &str, updated: u64| {
            let id = job_naming(&db, name, Some(environment));
            db.run(
                "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = $2 \
                 WHERE id = $1",
                &params![id, updated],
            );
            id
        };
        save_environment(&db, "Guarded", &[], true);
        save_environment(&db, "Open", &[], false);
        let gone = held("gone", "deleted", 100);
        let off = held("off", "open", 100);
        let guarded = held("guarded", "guarded", 100);
        let recent = held("recent", "deleted", 9_000);
        let mut freed = woken_ids(
            db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 10))
                .unwrap(),
        );
        freed.sort_unstable();
        assert_eq!(freed, [gone, off]);
        for job in [gone, off] {
            assert_eq!(held_of(&db, job), None);
        }
        assert_eq!(held_of(&db, guarded).as_deref(), Some("approval"));
        assert_eq!(held_of(&db, recent).as_deref(), Some("approval"));
    }

    #[test]
    fn held_jobs_are_not_stalled() {
        let db = setup();
        let (_, held) = run_with_job_named(&db, "held", "queued");
        db.run(
            "UPDATE ci_jobs SET held = 'card', next_launch_at = 1, updated_at = 1 WHERE id = $1",
            &params![held],
        );
        let (_, plain) = run_with_job_named(&db, "plain", "queued");
        db.run(
            "UPDATE ci_jobs SET next_launch_at = 1, updated_at = 1 WHERE id = $1",
            &params![plain],
        );
        let rows = db.wait(stalled_jobs(db.db(), 10_000_000, 50)).unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [plain]);
    }

    #[test]
    fn claim_clears_the_hold() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        assert!(db.wait(hold(db.db(), job_id, 1, "card", 50, 5)).unwrap());
        assert_eq!(held_of(&db, job_id).as_deref(), Some("card"));
        claim(&db, job_id, 10);
        assert_eq!(held_of(&db, job_id), None);
    }

    #[test]
    fn hold_reports_a_change_once() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        assert!(db.wait(hold(db.db(), job_id, 1, "card", 500, 5)).unwrap());
        assert!(!db.wait(hold(db.db(), job_id, 1, "card", 530, 6)).unwrap());
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!((row.next_launch_at, row.updated_at), (Some(530), 6));
        assert!(
            db.wait(hold(db.db(), job_id, 1, "concurrency", 560, 7))
                .unwrap()
        );
        assert_eq!(held_of(&db, job_id).as_deref(), Some("concurrency"));
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(row.next_launch_at, Some(560));
    }

    #[test]
    fn hold_leaves_another_attempt_alone() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        assert!(!db.wait(hold(db.db(), job_id, 2, "card", 500, 5)).unwrap());
        assert_eq!(held_of(&db, job_id), None);
        let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!((row.next_launch_at, row.updated_at), (None, 1_001));
        let (_, launching) = run_with_job_named(&db, "d2", "launching");
        assert!(
            !db.wait(hold(db.db(), launching, 1, "card", 500, 5))
                .unwrap()
        );
        assert_eq!(held_of(&db, launching), None);
    }

    #[test]
    fn a_rerun_clears_the_hold() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        db.wait(hold(db.db(), job_id, 1, "spend_cap", 50, 5))
            .unwrap();
        db.wait(reset_job(db.db(), job_id, 2, 40)).unwrap();
        assert_eq!(held_of(&db, job_id), None);
    }

    #[test]
    fn a_failed_launch_retries_or_fails_the_job() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "queued");
        claim(&db, job_id, 10);
        db.wait(launch_failed(db.db(), job_id, "no capacity", Some(500), 20))
            .unwrap();
        let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
        assert_eq!(
            (job_row.status.as_str(), job_row.next_launch_at),
            ("queued", Some(500))
        );
        claim(&db, job_id, 30);
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
        claim(&db, job_id, 10);
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
        claim(&db, job_id, 10);
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
        claim(&db, job_id, 10);
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
        assert_eq!(
            (
                tenant.id.as_str(),
                tenant.org_login.as_str(),
                tenant.ci_billing
            ),
            ("t1", "acme", CiBilling::Never)
        );
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

    fn subscribe(db: &TestDb, id: &str, status: &str) {
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ($1, 't1', $2, 'c', 'p', false, 't', 0)",
            &params![id, status],
        );
    }

    #[test]
    fn has_card_needs_an_active_subscription() {
        let db = setup();
        let card = |db: &TestDb| db.wait(has_card(db.db(), "t1")).unwrap();
        assert!(!card(&db), "no subscription");
        subscribe(&db, "s1", "canceled");
        assert!(!card(&db), "canceled");
        subscribe(&db, "s2", "active");
        assert!(card(&db));
        db.run(
            "UPDATE subscriptions SET status = 'trialing' WHERE id = 's2'",
            &[],
        );
        assert!(card(&db), "trialing");
        assert!(!db.wait(has_card(db.db(), "nope")).unwrap());
    }

    #[test]
    fn tenant_row_reads_the_billing_state() {
        let db = setup();
        let state = |db: &TestDb| {
            let by_id = db.wait(tenant(db.db(), "t1")).unwrap().unwrap().ci_billing;
            let by_installation = db
                .wait(tenant_for_installation(db.db(), 99))
                .unwrap()
                .unwrap()
                .ci_billing;
            assert_eq!(by_id, by_installation);
            by_id
        };
        assert_eq!(state(&db), CiBilling::Never);
        subscribe(&db, "s1", "canceled");
        assert_eq!(state(&db), CiBilling::Lapsed);
        subscribe(&db, "s2", "active");
        assert_eq!(state(&db), CiBilling::Active);
        db.run(
            "UPDATE subscriptions SET status = 'canceled' WHERE id = 's2'",
            &[],
        );
        assert_eq!(state(&db), CiBilling::Lapsed);
        db.run(
            "UPDATE subscriptions SET status = 'trialing' WHERE id = 's2'",
            &[],
        );
        assert_eq!(state(&db), CiBilling::Active);
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

    fn usage_rows(db: &TestDb) -> Vec<(u64, u64, String, u64, u64)> {
        db.rows(
            "SELECT job_id, attempt, template, minutes, ended_at FROM ci_usage \
             ORDER BY job_id, attempt",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row["job_id"].as_u64().unwrap(),
                row["attempt"].as_u64().unwrap(),
                row["template"].as_str().unwrap().to_owned(),
                row["minutes"].as_u64().unwrap(),
                row["ended_at"].as_u64().unwrap(),
            )
        })
        .collect()
    }

    fn finish(db: &TestDb, job_id: u64, attempt: u32, now: u64) -> bool {
        db.wait(finish_job(
            db.db(),
            job_id,
            attempt,
            "success",
            &serde_json::json!([]),
            None,
            now,
        ))
        .unwrap()
    }

    #[test]
    fn finish_job_records_minutes_from_claim() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launching");
        assert!(db.wait(start_job(db.db(), job_id, 10_000)).unwrap());
        assert!(finish(&db, job_id, 1, 71_000));
        assert_eq!(
            usage_rows(&db),
            [(job_id, 1, "ubuntu-latest".to_owned(), 2, 71_000)]
        );
    }

    #[test]
    fn a_job_never_claimed_is_not_metered() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launched");
        assert!(finish(&db, job_id, 1, 71_000));
        assert_eq!(usage_rows(&db).len(), 0);
    }

    #[test]
    fn finishing_twice_records_once() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launching");
        db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
        assert!(finish(&db, job_id, 1, 71_000));
        assert!(!finish(&db, job_id, 1, 71_000), "same time");
        assert!(!finish(&db, job_id, 1, 500_000), "later");
        assert_eq!(usage_rows(&db).len(), 1);
        assert_eq!(usage_rows(&db)[0].4, 71_000);
    }

    #[test]
    fn each_attempt_is_metered() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launching");
        db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
        assert!(finish(&db, job_id, 1, 20_000));
        db.wait(reset_job(db.db(), job_id, 2, 30_000)).unwrap();
        db.wait(set_job(db.db(), job_id, "launching", None, 30_001))
            .unwrap();
        db.wait(start_job(db.db(), job_id, 31_000)).unwrap();
        assert!(finish(&db, job_id, 2, 100_000));
        let rows = usage_rows(&db);
        assert_eq!(
            rows.iter().map(|r| (r.1, r.3)).collect::<Vec<_>>(),
            [(1, 1), (2, 2)]
        );
    }

    #[test]
    fn stale_result_is_not_metered() {
        let db = setup();
        let (_, job_id) = run_with_job(&db, "launching");
        db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
        db.wait(reset_job(db.db(), job_id, 2, 20_000)).unwrap();
        db.wait(set_job(db.db(), job_id, "launching", None, 20_001))
            .unwrap();
        db.wait(start_job(db.db(), job_id, 21_000)).unwrap();
        assert!(!finish(&db, job_id, 1, 90_000));
        assert_eq!(usage_rows(&db).len(), 0);
    }

    #[test]
    fn minutes_round_up_like_usage_minutes() {
        let db = setup();
        let mut got = Vec::new();
        for (n, ms) in [0_u64, 1, 60_000, 60_001].into_iter().enumerate() {
            let run = db
                .wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", 1)))
                .unwrap()
                .unwrap();
            let ids = db
                .wait(insert_jobs(db.db(), run.id, &[new_job("build")], 1))
                .unwrap();
            let job_id = ids[0].1;
            db.wait(set_job(db.db(), job_id, "launching", None, 2))
                .unwrap();
            db.wait(start_job(db.db(), job_id, 1_000)).unwrap();
            assert!(finish(&db, job_id, 1, 1_000 + ms));
            got.push(usage_rows(&db).last().unwrap().3);
            assert_eq!(
                got.last().copied().unwrap(),
                u64::from(hatchrun_cloud::usage::minutes(1_000, 1_000 + ms))
            );
        }
        assert_eq!(got, [1, 1, 1, 2]);
        // 2 vCPU: one unit a minute, priced when the job ended.
        assert!(usage_rows(&db).iter().all(|r| {
            r.3 == db
                .rows(
                    "SELECT units FROM ci_usage WHERE job_id = $1",
                    &params![r.0],
                )
                .unwrap()[0]["units"]
                .as_u64()
                .unwrap()
        }));
    }

    #[test]
    fn standing_counts_this_month_and_running_jobs() {
        let db = setup();
        let month = 1_790_812_800_000_u64;
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 98, 0, 0)",
            &[],
        );
        for (job, tenant, template, minutes, ended) in [
            (1, "t1", "ubuntu-latest", 7, month - 1),
            (2, "t1", "ubuntu-latest", 5, month + DAY),
            (3, "t1", "ubuntu-26.04-4core", 3, month + 2 * DAY),
            (4, "t2", "ubuntu-latest", 100, month + DAY),
        ] {
            db.run(
                "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, minutes, units, \
                 ended_at) VALUES ($1, 1, $2, $3, $4, $4, $5)",
                &params![job, tenant, template, minutes, ended],
            );
        }
        let (_, first) = run_with_job(&db, "running");
        let (_, second) = run_with_job_named(&db, "d2", "launching");
        let (_, _done) = run_with_job_named(&db, "d3", "completed");
        let (_, _queued) = run_with_job_named(&db, "d4", "queued");
        let default_cap = 50_000_000;
        let mut standing_t1 = db
            .wait(standing(db.db(), first, month, default_cap))
            .unwrap()
            .unwrap();
        standing_t1.minutes_by_template.sort();
        assert_eq!(
            standing_t1,
            Standing {
                tenant_id: "t1".into(),
                org_login: "acme".into(),
                card: false,
                cap_micros: default_cap,
                minutes_by_template: vec![
                    ("ubuntu-26.04-4core".into(), 3),
                    ("ubuntu-latest".into(), 5)
                ],
                running: 2,
            }
        );
        let by_tenant = db
            .wait(tenant_standing(db.db(), "t1", month, default_cap))
            .unwrap()
            .unwrap();
        let by_second_job = db
            .wait(standing(db.db(), second, month, default_cap))
            .unwrap()
            .unwrap();
        assert_eq!(by_tenant.running, 2);
        assert_eq!(by_second_job.tenant_id, "t1");
        // Another tenant: its own rows, no jobs, and a cap row wins.
        db.run(
            "INSERT INTO ci_spend_caps (tenant_id, cap_micros, updated_at) \
             VALUES ('t2', 70000000, 0)",
            &[],
        );
        subscribe(&db, "s1", "active");
        let other = db
            .wait(tenant_standing(db.db(), "t2", month, default_cap))
            .unwrap()
            .unwrap();
        assert_eq!(
            other.minutes_by_template,
            [("ubuntu-latest".to_owned(), 100)]
        );
        assert_eq!(
            (other.running, other.cap_micros, other.card),
            (0, 70_000_000, false)
        );
        let first_again = db
            .wait(tenant_standing(db.db(), "t1", month, default_cap))
            .unwrap()
            .unwrap();
        assert!(first_again.card);
        assert!(
            db.wait(tenant_standing(db.db(), "nope", month, default_cap))
                .unwrap()
                .is_none()
        );
        assert!(
            db.wait(standing(db.db(), 999_999, month, default_cap))
                .unwrap()
                .is_none()
        );
        let empty = db
            .wait(tenant_standing(
                db.db(),
                "t1",
                month + 40 * DAY,
                default_cap,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(empty.minutes_by_template.len(), 0);
    }

    #[test]
    fn sweep_deletes_usage_sent_long_ago() {
        let db = setup();
        let now = 200 * DAY;
        for (job, sent) in [
            (1, Some(now - 91 * DAY)),
            (2, Some(now - 89 * DAY)),
            (3, None),
        ] {
            db.run(
                "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, minutes, units, \
                 ended_at, sent_at) VALUES ($1, 1, 't1', 'ubuntu-latest', 1, 1, 0, $2)",
                &params![job, sent],
            );
        }
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, minutes, units, \
             ended_at, rejected_at) VALUES (4, 1, 't1', 'ubuntu-latest', 1, 1, 0, $1)",
            &params![now - 91 * DAY],
        );
        db.wait(sweep(db.db(), now)).unwrap();
        assert_eq!(
            usage_rows(&db).iter().map(|r| r.0).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn caps_are_set_and_replaced() {
        let db = setup();
        let month = 1_790_812_800_000_u64;
        let default_cap = 50_000_000;
        db.wait(set_spend_cap(db.db(), "t1", 75_000_000, 5))
            .unwrap();
        let s = db
            .wait(tenant_standing(db.db(), "t1", month, default_cap))
            .unwrap()
            .unwrap();
        assert_eq!(s.cap_micros, 75_000_000);
        db.wait(set_spend_cap(db.db(), "t1", 60_000_000, 6))
            .unwrap();
        let s = db
            .wait(tenant_standing(db.db(), "t1", month, default_cap))
            .unwrap()
            .unwrap();
        assert_eq!(s.cap_micros, 60_000_000);
        let t = db.wait(tenant_by_org_id(db.db(), 1)).unwrap().unwrap();
        assert_eq!(t.id, "t1");
        assert!(db.wait(tenant_by_org_id(db.db(), 2)).unwrap().is_none());
    }

    #[test]
    fn usage_goes_to_polar_oldest_first_and_once() {
        let db = setup();
        for (job, attempt, ended) in [(3, 1, 30), (1, 1, 10), (1, 2, 20), (2, 1, 20)] {
            db.run(
                "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, minutes, units, \
                 ended_at) VALUES ($1, $2, 't1', 'ubuntu-latest', 4, 4, $3)",
                &params![job, attempt, ended],
            );
        }
        let keys = |rows: &[UnsentUsage]| -> Vec<(u64, u32)> {
            rows.iter().map(|r| (r.job_id, r.attempt)).collect()
        };
        let all = db.wait(unsent_usage(db.db(), 10)).unwrap();
        assert_eq!(keys(&all), [(1, 1), (1, 2), (2, 1), (3, 1)]);
        assert_eq!(all[0].tenant_id, "t1");
        assert_eq!(all[0].minutes, 4);
        assert_eq!(all[0].units, 4);
        let two = db.wait(unsent_usage(db.db(), 2)).unwrap();
        assert_eq!(keys(&two), [(1, 1), (1, 2)]);
        db.wait(usage_sent(db.db(), &keys(&two), 99)).unwrap();
        db.wait(usage_sent(db.db(), &[], 99)).unwrap();
        let rest = db.wait(unsent_usage(db.db(), 10)).unwrap();
        assert_eq!(keys(&rest), [(2, 1), (3, 1)]);
        // A quarantined row is never listed again, nor marked sent.
        db.wait(usage_rejected(db.db(), &[(2, 1)], "no such customer", 150))
            .unwrap();
        db.wait(usage_sent(db.db(), &[(2, 1)], 160)).unwrap();
        let rest = db.wait(unsent_usage(db.db(), 10)).unwrap();
        assert_eq!(keys(&rest), [(3, 1)]);
        let row = db
            .rows(
                "SELECT sent_at, rejected_at, reject_reason FROM ci_usage \
                 WHERE job_id = 2",
                &[],
            )
            .unwrap();
        assert!(row[0]["sent_at"].is_null());
        assert_eq!(row[0]["rejected_at"].as_u64(), Some(150));
        assert_eq!(row[0]["reject_reason"].as_str(), Some("no such customer"));
        // A second mark does not move the time.
        db.wait(usage_sent(db.db(), &[(1, 1)], 200)).unwrap();
        let sent = db
            .rows(
                "SELECT sent_at FROM ci_usage WHERE job_id = 1 AND attempt = 1",
                &[],
            )
            .unwrap();
        assert_eq!(sent[0]["sent_at"].as_u64(), Some(99));
    }

    fn put_var_ok(db: &TestDb, owner: Owner, name: &str, value: &str) {
        let put = db
            .wait(put_var(db.db(), "t1", owner, None, name, value, 100, 5))
            .unwrap();
        assert_eq!(put, Put::Saved);
    }

    fn put_secret_ok(db: &TestDb, owner: Owner, name: &str, key: &str, sealed: &str) {
        let put = db
            .wait(put_secret(
                db.db(),
                "t1",
                owner,
                None,
                name,
                key,
                sealed,
                100,
                5,
            ))
            .unwrap();
        assert_eq!(put, Put::Saved);
    }

    #[test]
    fn repo_value_overrides_org() {
        let db = setup();
        put_var_ok(&db, Owner::Org, "A", "1");
        put_var_ok(&db, Owner::Org, "B", "2");
        put_var_ok(&db, Owner::Repo(7), "A", "3");
        let vars = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
        assert_eq!(
            vars,
            BTreeMap::from([("A".into(), "3".into()), ("B".into(), "2".into())])
        );
        let org = db.wait(vars_of(db.db(), Owner::Org)).unwrap();
        assert_eq!(org.len(), 2);
        assert!(
            db.wait(delete_var(db.db(), "t1", Owner::Repo(7), "A"))
                .unwrap()
        );
        assert!(
            !db.wait(delete_var(db.db(), "t1", Owner::Repo(7), "A"))
                .unwrap()
        );
        let vars = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
        assert_eq!(vars["A"], "1");
    }

    async fn vars_of(db: &Db, owner: Owner) -> Result<Vec<VarItem>> {
        vars(db, "t1", owner).await
    }

    #[test]
    fn put_counts_against_the_limit() {
        let db = setup();
        let put = |name: &str, limit| {
            db.wait(put_secret(
                db.db(),
                "t1",
                Owner::Org,
                None,
                name,
                "k",
                "s",
                limit,
                5,
            ))
            .unwrap()
        };
        assert_eq!(put("A", 2), Put::Saved);
        assert_eq!(put("B", 2), Put::Saved);
        assert_eq!(put("C", 2), Put::TooMany);
        // Overwriting a name at the limit is allowed.
        assert_eq!(put("A", 2), Put::Saved);
        let names = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(names.len(), 2);
        // The limit is per scope.
        let repo = db
            .wait(put_secret(
                db.db(),
                "t1",
                Owner::Repo(7),
                None,
                "C",
                "k",
                "s",
                2,
                5,
            ))
            .unwrap();
        assert_eq!(repo, Put::Saved);
        // And the same for vars.
        let var = |name: &str| {
            db.wait(put_var(db.db(), "t1", Owner::Org, None, name, "v", 1, 5))
                .unwrap()
        };
        assert_eq!(var("A"), Put::Saved);
        assert_eq!(var("B"), Put::TooMany);
        assert_eq!(var("A"), Put::Saved);
    }

    #[test]
    fn sealed_secrets_filters_by_name() {
        let db = setup();
        put_secret_ok(&db, Owner::Org, "A", "k", "org-a");
        put_secret_ok(&db, Owner::Repo(7), "A", "k", "repo-a");
        put_secret_ok(&db, Owner::Repo(7), "B", "k", "repo-b");
        put_secret_ok(&db, Owner::Org, "C", "k", "org-c");
        let all = db
            .wait(sealed_secrets(db.db(), "t1", 7, None, None))
            .unwrap();
        assert_eq!(all.len(), 4);
        let names = ["A".to_owned(), "C".to_owned()];
        let some = db
            .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&names)))
            .unwrap();
        let got: Vec<_> = some
            .iter()
            .map(|r| (r.name.as_str(), r.repo_id, r.sealed.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("A", None, "org-a"),
                ("A", Some(7), "repo-a"),
                ("C", None, "org-c")
            ]
        );
        let none = db
            .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&[])))
            .unwrap();
        assert_eq!(none.len(), 0);
        // Another repository sees only the organization's rows.
        let other = db
            .wait(sealed_secrets(db.db(), "t1", 8, None, None))
            .unwrap();
        assert_eq!(other.len(), 2);
        assert!(!format!("{:?}", all[0]).contains("org-a"));
    }

    #[test]
    fn reseal_skips_a_row_that_changed() {
        let db = setup();
        put_secret_ok(&db, Owner::Repo(7), "A", "old", "s1");
        put_secret_ok(&db, Owner::Org, "B", "new", "s2");
        let stale = db.wait(stale_secrets(db.db(), "new", 10)).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "A");
        assert!(!format!("{:?}", stale[0]).contains("s1"));
        // The secret is set again between the read and the re-seal.
        put_secret_ok(&db, Owner::Repo(7), "A", "old", "s1-changed");
        assert!(
            !db.wait(reseal(db.db(), &stale[0], "new", "s1-resealed"))
                .unwrap()
        );
        let now = db.wait(stale_secrets(db.db(), "new", 10)).unwrap();
        assert_eq!(now[0].sealed, "s1-changed");
        assert!(
            db.wait(reseal(db.db(), &now[0], "new", "s1-resealed"))
                .unwrap()
        );
        assert_eq!(db.wait(stale_secrets(db.db(), "new", 10)).unwrap().len(), 0);
    }

    #[test]
    fn stale_secrets_is_bounded() {
        let db = setup();
        for name in ["A", "B", "C"] {
            put_secret_ok(&db, Owner::Org, name, "old", "s");
        }
        assert_eq!(db.wait(stale_secrets(db.db(), "new", 2)).unwrap().len(), 2);
    }

    #[test]
    fn deleting_a_repo_or_tenant_removes_its_secrets() {
        let db = setup();
        put_secret_ok(&db, Owner::Repo(7), "A", "k", "s");
        assert!(
            db.wait(delete_secret(db.db(), "t1", Owner::Repo(7), "A"))
                .unwrap()
        );
        assert!(
            !db.wait(delete_secret(db.db(), "t1", Owner::Repo(7), "A"))
                .unwrap()
        );
        put_secret_ok(&db, Owner::Repo(7), "A", "k", "s");
        db.run("DELETE FROM tenants WHERE id = 't1'", &[]);
        let left = db.rows("SELECT 1 FROM ci_secrets", &[]).unwrap();
        assert_eq!(left.len(), 0);
    }

    fn save_environment(
        db: &TestDb,
        name: &str,
        branches: &[&str],
        approval: bool,
    ) -> EnvironmentRow {
        let branches: Vec<String> = branches.iter().map(|b| (*b).to_owned()).collect();
        match db
            .wait(put_environment(
                db.db(),
                "t1",
                7,
                name,
                &branches,
                approval,
                10,
                5,
            ))
            .unwrap()
        {
            PutEnvironment::Saved(row) => row,
            PutEnvironment::TooMany => panic!("too many"),
        }
    }

    fn job_naming(db: &TestDb, delivery: &str, environment: Option<&str>) -> u64 {
        let run = db
            .wait(insert_run(db.db(), &new_run(delivery, "ci", 1_000)))
            .unwrap()
            .unwrap();
        let job = NewJob {
            environment: environment.map(str::to_owned),
            ..new_job("deploy")
        };
        db.wait(insert_jobs(db.db(), run.id, &[job], 1_000))
            .unwrap()[0]
            .1
    }

    #[test]
    fn environments_are_unique_per_repo_ignoring_case() {
        let db = setup();
        save_environment(&db, "Production", &["release"], false);
        let again = save_environment(&db, "production", &["main"], true);
        assert_eq!(again.name, "Production");
        assert_eq!(again.branches, ["main"]);
        assert!(again.approval);
        let all = db.wait(environments(db.db(), 7)).unwrap();
        assert_eq!(all, [again]);
        // The index refuses a second row that differs only in case.
        let tried = db.try_run(
            "INSERT INTO ci_environments (tenant_id, repo_id, name, branches, approval, updated_at) \
             VALUES ('t1', 7, 'PRODUCTION', '[]', false, 0)",
            &[],
        );
        assert!(tried.is_err());
    }

    #[test]
    fn environment_lookup_ignores_case() {
        let db = setup();
        let made = save_environment(&db, "Production", &["main"], true);
        let found = db.wait(environment(db.db(), 7, "PRODUCTION")).unwrap();
        assert_eq!(found, Some(made.clone()));
        assert_eq!(db.wait(environment(db.db(), 7, "staging")).unwrap(), None);
        assert_eq!(
            db.wait(environment(db.db(), 8, "production")).unwrap(),
            None
        );

        let named = job_naming(&db, "d1", Some("production"));
        let got = db.wait(job_environment(db.db(), named)).unwrap().unwrap();
        assert_eq!(got.environment, Some(made));
        assert_eq!(got.repo_id, 7);
        assert_eq!(got.full_name, "acme/app");
        assert_eq!(got.event, "push");
        assert_eq!(got.git_ref, "refs/heads/main");
        let missing = job_naming(&db, "d2", Some("nope"));
        let got = db.wait(job_environment(db.db(), missing)).unwrap().unwrap();
        assert_eq!(got.environment, None);
        let none = job_naming(&db, "d3", None);
        let got = db.wait(job_environment(db.db(), none)).unwrap().unwrap();
        assert_eq!(got.environment, None);
        assert_eq!(db.wait(job_environment(db.db(), 999_999)).unwrap(), None);
    }

    #[test]
    fn put_environment_counts_against_the_limit() {
        let db = setup();
        let put = |name: &str| {
            db.wait(put_environment(db.db(), "t1", 7, name, &[], false, 2, 5))
                .unwrap()
        };
        assert!(matches!(put("a"), PutEnvironment::Saved(_)));
        assert!(matches!(put("b"), PutEnvironment::Saved(_)));
        assert_eq!(put("c"), PutEnvironment::TooMany);
        assert!(matches!(put("A"), PutEnvironment::Saved(_)));
        assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 2);
    }

    #[test]
    fn deleting_an_environment_deletes_its_secrets_and_vars() {
        let db = setup();
        let env = save_environment(&db, "Production", &[], false);
        let owner = Owner::Environment {
            repo_id: 7,
            environment_id: env.id,
        };
        put_secret_ok(&db, owner, "S", "k", "s");
        put_var_ok(&db, owner, "V", "1");
        put_secret_ok(&db, Owner::Repo(7), "S", "k", "repo");
        assert!(
            db.wait(delete_environment(db.db(), 7, "production"))
                .unwrap()
        );
        assert!(
            !db.wait(delete_environment(db.db(), 7, "production"))
                .unwrap()
        );
        let left = db
            .rows(
                "SELECT 1 FROM ci_secrets WHERE environment_id IS NOT NULL",
                &[],
            )
            .unwrap();
        assert_eq!(left.len(), 0);
        let left = db
            .rows(
                "SELECT 1 FROM ci_vars WHERE environment_id IS NOT NULL",
                &[],
            )
            .unwrap();
        assert_eq!(left.len(), 0);
        assert_eq!(
            db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn environment_values_win_over_repo_and_org() {
        let db = setup();
        let env = save_environment(&db, "e", &[], false);
        let owner = Owner::Environment {
            repo_id: 7,
            environment_id: env.id,
        };
        put_var_ok(&db, Owner::Org, "X", "o");
        put_var_ok(&db, Owner::Repo(7), "X", "r");
        put_var_ok(&db, owner, "X", "e");
        let with = db.wait(job_vars(db.db(), "t1", 7, Some(env.id))).unwrap();
        assert_eq!(with, BTreeMap::from([("X".into(), "e".into())]));
        let without = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
        assert_eq!(without, BTreeMap::from([("X".into(), "r".into())]));

        put_secret_ok(&db, owner, "S", "k", "env");
        put_secret_ok(&db, Owner::Org, "S", "k", "org");
        put_secret_ok(&db, Owner::Repo(7), "S", "k", "repo");
        let rows = db
            .wait(sealed_secrets(db.db(), "t1", 7, Some(env.id), None))
            .unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.owner(), r.sealed.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Owner::Org, "org"),
                (Owner::Repo(7), "repo"),
                (owner, "env")
            ]
        );
        let rows = db
            .wait(sealed_secrets(db.db(), "t1", 7, None, None))
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.environment_id.is_none()));
    }

    #[test]
    fn another_environment_stays_out() {
        let db = setup();
        let e = save_environment(&db, "e", &[], false);
        let f = save_environment(&db, "f", &[], false);
        let of_f = Owner::Environment {
            repo_id: 7,
            environment_id: f.id,
        };
        put_secret_ok(&db, of_f, "S", "k", "f");
        put_var_ok(&db, of_f, "V", "f");
        let rows = db
            .wait(sealed_secrets(db.db(), "t1", 7, Some(e.id), None))
            .unwrap();
        assert_eq!(rows.len(), 0);
        assert!(
            db.wait(job_vars(db.db(), "t1", 7, Some(e.id)))
                .unwrap()
                .is_empty()
        );
        // Scopes count their own rows: the repository's limit ignores them.
        let put = db
            .wait(put_secret(
                db.db(),
                "t1",
                Owner::Repo(7),
                None,
                "R",
                "k",
                "s",
                1,
                5,
            ))
            .unwrap();
        assert_eq!(put, Put::Saved);
        assert_eq!(db.wait(secret_names(db.db(), "t1", of_f)).unwrap().len(), 1);
    }

    #[test]
    fn release_approval_holds_frees_the_jobs() {
        let db = setup();
        let hold = |job: u64, reason: &str| {
            db.run(
                "UPDATE ci_jobs SET status = 'queued', held = $2, updated_at = 1 WHERE id = $1",
                &params![job, reason],
            );
        };
        let a = job_naming(&db, "a", Some("Production"));
        let b = job_naming(&db, "b", Some("production"));
        let card = job_naming(&db, "c", Some("production"));
        let other = job_naming(&db, "d", Some("staging"));
        hold(a, "approval");
        hold(b, "approval");
        hold(card, "card");
        hold(other, "approval");
        let woken = db
            .wait(release_approval_holds(db.db(), 7, "PRODUCTION", 50))
            .unwrap();
        assert_eq!(woken_ids(woken.clone()), [a, b]);
        assert!(woken.iter().all(|w| w.attempt == 1));
        for job in [a, b] {
            assert_eq!(held_of(&db, job), None);
            let at: Option<u64> = db
                .column(
                    "SELECT next_launch_at AS n FROM ci_jobs WHERE id = $1",
                    &params![job],
                    "n",
                )
                .unwrap();
            assert_eq!(at, Some(50));
        }
        assert_eq!(held_of(&db, card).as_deref(), Some("card"));
        assert_eq!(held_of(&db, other).as_deref(), Some("approval"));
    }

    #[test]
    fn environment_changes_release_holds_through_the_handler_path() {
        use crate::settings_api::release_holds;

        let db = setup();
        let hold = |job: u64| {
            db.run(
                "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 1 \
                 WHERE id = $1",
                &params![job],
            );
        };
        let release = |approval: bool, deleted: bool, now: u64| {
            db.wait(release_holds(
                db.db(),
                7,
                "production",
                approval,
                deleted,
                now,
            ))
            .unwrap()
        };
        let job = job_naming(&db, "a", Some("Production"));
        hold(job);

        // A save that keeps approval on frees nothing.
        save_environment(&db, "Production", &["main"], true);
        assert_eq!(woken_ids(release(true, false, 10)), Vec::<u64>::new());
        assert_eq!(held_of(&db, job).as_deref(), Some("approval"));

        // Turning approval off frees the job and schedules its launch.
        let row = save_environment(&db, "Production", &["main"], false);
        let woken = release(row.approval, false, 20);
        assert_eq!(woken_ids(woken), [job]);
        assert_eq!(held_of(&db, job), None);

        // Deleting the environment frees a job held on it.
        let later = job_naming(&db, "b", Some("Production"));
        hold(later);
        save_environment(&db, "Production", &[], true);
        assert!(
            db.wait(delete_environment(db.db(), 7, "production"))
                .unwrap()
        );
        assert_eq!(woken_ids(release(false, true, 30)), [later]);
        assert_eq!(held_of(&db, later), None);
    }

    #[test]
    fn a_transferred_repo_leaves_its_environments() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 100, 0, 0)",
            &[],
        );
        save_environment(&db, "Production", &[], false);
        let mut same = db.wait(repo(db.db(), 7)).unwrap().unwrap();
        db.wait(upsert_repo(db.db(), &same, 6)).unwrap();
        assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 1);
        same.tenant_id = "t2".into();
        same.installation_id = 100;
        db.wait(upsert_repo(db.db(), &same, 7)).unwrap();
        assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 0);
    }

    #[test]
    fn job_rows_carry_environment() {
        let db = setup();
        let id = job_naming(&db, "d", Some("production"));
        let row = db.wait(job(db.db(), id)).unwrap().unwrap();
        assert_eq!(row.environment.as_deref(), Some("production"));
        assert_eq!(row.approval_attempt, None);
        assert_eq!(row.approval_decision, None);
        assert_eq!(row.approval_by, None);
        assert_eq!(row.approval_at, None);
        let id = job_naming(&db, "e", None);
        assert_eq!(
            db.wait(job(db.db(), id)).unwrap().unwrap().environment,
            None
        );
    }

    /// Repositories 8 (acme/web) and 9 (acme/lib) join repo 7 (acme/app).
    fn with_repos() -> TestDb {
        let db = setup();
        for (id, name) in [(8, "acme/web"), (9, "acme/lib")] {
            let repo = RepoRow {
                id,
                tenant_id: "t1".into(),
                installation_id: 99,
                full_name: name.into(),
                default_branch: "main".into(),
                private: false,
            };
            db.wait(upsert_repo(db.db(), &repo, 2)).unwrap();
        }
        db
    }

    fn put_selected(db: &TestDb, kind: &str, name: &str, repos: Option<&[u64]>) {
        let put = if kind == "secret" {
            db.wait(put_secret(
                db.db(),
                "t1",
                Owner::Org,
                repos,
                name,
                "k",
                name,
                100,
                5,
            ))
        } else {
            db.wait(put_var(
                db.db(),
                "t1",
                Owner::Org,
                repos,
                name,
                name,
                100,
                5,
            ))
        };
        assert_eq!(put.unwrap(), Put::Saved);
    }

    fn secret_values(db: &TestDb, repo_id: u64) -> Vec<String> {
        db.wait(sealed_secrets(db.db(), "t1", repo_id, None, None))
            .unwrap()
            .into_iter()
            .map(|row| row.sealed)
            .collect()
    }

    fn var_names(db: &TestDb, repo_id: u64) -> Vec<String> {
        db.wait(job_vars(db.db(), "t1", repo_id, None))
            .unwrap()
            .into_keys()
            .collect()
    }

    #[test]
    fn selected_org_rows_reach_only_their_repos() {
        let db = with_repos();
        for kind in ["secret", "var"] {
            put_selected(&db, kind, "A", Some(&[8]));
            put_selected(&db, kind, "B", None);
        }
        assert_eq!(secret_values(&db, 7), ["B"]);
        assert_eq!(secret_values(&db, 8), ["A", "B"]);
        assert_eq!(var_names(&db, 7), ["B"]);
        assert_eq!(var_names(&db, 8), ["A", "B"]);
        // The names filter goes through the same rule.
        let names = vec!["A".to_owned()];
        let rows = db
            .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&names)))
            .unwrap();
        assert_eq!(rows, []);
    }

    #[test]
    fn an_empty_selection_reaches_no_repo() {
        let db = with_repos();
        put_selected(&db, "secret", "A", Some(&[]));
        put_selected(&db, "var", "A", Some(&[]));
        for repo_id in [7, 8] {
            assert_eq!(secret_values(&db, repo_id), Vec::<String>::new());
            assert_eq!(var_names(&db, repo_id), Vec::<String>::new());
        }
        let listed = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(listed[0].repositories, Some(Vec::new()));
    }

    #[test]
    fn selection_never_crosses_tenants() {
        let db = setup();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ('t2', 2, 'other', 100, 0, 0)",
            &[],
        );
        let foreign = RepoRow {
            id: 9,
            tenant_id: "t2".into(),
            installation_id: 100,
            full_name: "other/x".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &foreign, 2)).unwrap();
        put_selected(&db, "secret", "A", Some(&[9]));
        put_selected(&db, "var", "A", Some(&[9]));
        let rows = db
            .wait(sealed_secrets(db.db(), "t2", 9, None, None))
            .unwrap();
        assert_eq!(rows, []);
        assert_eq!(
            db.wait(job_vars(db.db(), "t2", 9, None)).unwrap(),
            BTreeMap::new()
        );
        let listed = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(listed[0].repositories, Some(Vec::new()));
        let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(listed[0].repositories, Some(Vec::new()));
    }

    #[test]
    fn listing_names_the_selected_repos() {
        let db = with_repos();
        put_selected(&db, "secret", "A", Some(&[8, 7]));
        put_selected(&db, "secret", "B", None);
        put_selected(&db, "var", "A", Some(&[8, 7]));
        put_selected(&db, "var", "B", None);
        let both = Some(vec!["acme/app".to_owned(), "acme/web".to_owned()]);
        let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(secrets[0].repositories, both);
        assert_eq!(secrets[1].repositories, None);
        let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(listed[0].repositories, both);
        assert_eq!(listed[1].repositories, None);
        // A replacing put can widen the selection again.
        put_selected(&db, "secret", "A", None);
        let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(secrets[0].repositories, None);
    }

    #[test]
    fn listing_leaves_out_gone_repos() {
        let db = with_repos();
        put_selected(&db, "secret", "A", Some(&[7, 8]));
        put_selected(&db, "var", "A", Some(&[7, 8]));
        // Repo 7 is renamed away when a new repository takes its name.
        let fresh = RepoRow {
            id: 10,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "acme/app".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
        let only_web = Some(vec!["acme/web".to_owned()]);
        let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(secrets[0].repositories, only_web);
        let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
        assert_eq!(listed[0].repositories, only_web);
        let names = db.wait(tenant_repos(db.db(), "t1")).unwrap();
        assert!(names.iter().all(|name| !name.contains("#gone-")));
    }

    #[test]
    fn a_repo_row_cannot_select_repos() {
        let db = with_repos();
        for table in ["ci_secrets", "ci_vars"] {
            let sql = if table == "ci_secrets" {
                "INSERT INTO ci_secrets (tenant_id, repo_id, name, key_id, sealed, updated_at, \
                 repositories) VALUES ('t1', 7, 'A', 'k', 's', 1, '{7}')"
            } else {
                "INSERT INTO ci_vars (tenant_id, repo_id, name, value, updated_at, \
                 repositories) VALUES ('t1', 7, 'A', 'v', 1, '{7}')"
            };
            assert!(db.try_run(sql, &[]).is_err(), "{table}");
        }
        // Through the store too: an environment row is a repo_id row.
        let put = db.wait(put_var(
            db.db(),
            "t1",
            Owner::Repo(7),
            Some(&[8]),
            "A",
            "v",
            100,
            5,
        ));
        assert!(put.is_err());
    }

    #[test]
    fn tenant_repos_leaves_out_gone_names() {
        let db = with_repos();
        // Repo 7 takes the name of a new repository 10, and 7 is renamed away.
        let fresh = RepoRow {
            id: 10,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: "acme/app".into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
        let names = db.wait(tenant_repos(db.db(), "t1")).unwrap();
        assert_eq!(names, ["acme/app", "acme/lib", "acme/web"]);
        assert_eq!(
            db.wait(tenant_repos(db.db(), "t2")).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn repo_ids_ignore_case() {
        let db = with_repos();
        let names = vec!["ACME/APP".to_owned(), "acme/nope".to_owned()];
        let found = db.wait(repo_ids(db.db(), "t1", &names)).unwrap();
        assert_eq!(found, [("acme/app".to_owned(), 7)]);
        assert_eq!(db.wait(repo_ids(db.db(), "t1", &[])).unwrap(), []);
        assert_eq!(db.wait(repo_ids(db.db(), "t2", &names)).unwrap(), []);
    }
}
