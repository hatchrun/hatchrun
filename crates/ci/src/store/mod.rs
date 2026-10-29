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

mod billing;
mod environments;
mod jobs;
mod pool;
mod reconciler;
mod repos;
mod runs;
mod secrets;
mod tenants;

pub use self::billing::{
    Standing, UnsentUsage, set_spend_cap, standing, tenant_standing, unsent_usage, usage_rejected,
    usage_sent,
};
pub use self::environments::{
    EnvironmentRow, JobEnvironment, PutEnvironment, decide_approval, delete_environment,
    environment, environments, job_environment, put_environment, release_approval_holds,
};
pub use self::jobs::{
    Claim, JobRow, NewJob, claim_launch, finish_job, heartbeat, hold, insert_jobs, job,
    jobs_by_ids, jobs_for_run, launch_failed, launched, note_queued_error, open_jobs, queue_job,
    reset_job, set_job, start_job,
};
pub use self::pool::{
    PoolRow, claims_by_bucket, drop_pool, move_pool, pool_rows, save_pool, take_pool,
};
pub use self::reconciler::{
    SWEEP_LIMIT, ScheduleRow, delete_runs, last_minute, lease, replace_schedules, schedules,
    schedules_after, set_last_minute, stalled_jobs, sweep,
};
pub use self::repos::{RepoRow, delivery_seen, record_delivery, repo, repo_by_name, upsert_repo};
pub use self::runs::{
    NewRun, RunRow, finish_run, insert_run, reopen_run_at, run, run_for_delivery, run_started,
    runs_for_repo,
};
pub use self::secrets::{
    Owner, Put, SealedRow, SecretItem, StaleRow, VarItem, delete_secret, delete_var, job_vars,
    put_secret, put_var, repo_ids, reseal, sealed_secrets, secret_names, stale_secrets,
    tenant_repos, vars,
};
pub use self::tenants::{
    CiBilling, TenantRow, has_card, tenant, tenant_by_org_id, tenant_for_installation,
};

use hatchrun_cloud::db::{Db, Param};
use worker::Result;

/// `sql` with its `{columns}` filled in.
fn with_columns(sql: &str, columns: &str) -> String {
    sql.replace("{columns}", columns)
}

/// Whether a one-column `seen` query found a row.
async fn exists(db: &Db, sql: &str, params: &[Param]) -> Result<bool> {
    Ok(db.rows(sql, params).await?.into_iter().next().is_some())
}

#[cfg(test)]
mod tests;
