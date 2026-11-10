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

mod artifacts;
mod audit;
mod billing;
mod browse;
mod cache;
mod concurrency;
mod environments;
mod forks;
mod installation;
mod jobs;
mod pool;
mod reconciler;
mod repos;
mod runs;
mod secrets;
mod tenants;

pub use self::artifacts::{
    ArtifactRow, Finalized, artifact_by_name, artifact_count, artifacts, delete_artifact,
    drop_pending_artifact, finalize_artifact, pending_artifact, replaced_artifact, start_artifact,
};
pub use self::audit::{Action, AuditRow, NewAudit, audit_page, record_audit};
pub use self::billing::{
    DayUsage, Standing, UnsentUsage, daily_usage, set_spend_cap, standing, tenant_standing,
    unsent_usage, usage_rejected, usage_sent,
};
pub use self::browse::{
    BranchAfter, BranchRow, NewPull, PullAfter, PullRow, PullWrite, RepoCounts, WorkflowRow,
    branches_page, pull_head_sha, pulls_page, repo_counts, upsert_pull, upsert_pull_forced,
    workflows_page,
};
pub use self::cache::{
    CacheOwner, CacheRow, TOUCH_EVERY_MS, cache_entry, drop_pending_cache_entry,
    evict_idle_cache_entries, evict_over_fork_quota_cache_entries, evict_over_quota_cache_entries,
    finalize_cache_entry, fork_cache_bytes, lookup_cache_entry, pending_cache_entry,
    start_cache_entry, touch_cache_entry,
};
pub use self::concurrency::{
    Blocker, NewSlot, Slot, Victim, blocker, enter, leave, register, slots, unfinished_superseded,
    victims, wake_targets,
};
pub use self::environments::{
    EnvironmentRow, JobEnvironment, OrgEnvironment, PutEnvironment, decide_approval,
    delete_environment, environment, environments, job_environment, org_environments,
    put_environment, release_approval_holds,
};
pub use self::forks::{
    admit_run, approve_by_check, approve_run, clear_decided_mark, deferred_group, deferred_runs,
    end_gate, end_pull_gates, expire_gates, fork_waiting, mark_scanned, pending_gate_runs,
    put_repo_policy, reject_run, repo_policy, runs_needing_card, set_card_dirty, set_gate_check,
    unfinished_decisions,
};
pub use self::installation::{
    LastRun, OrgRepo, RepoFilter, RepoVisibility, SetupRow, installation_synced, patch_enabled,
    repo_enabled, repos_page, sync_installation, tenant_installation, tenant_repo_visibility,
    tenant_setup, touch_installation_repo, unknown_repos,
};
pub use self::jobs::{
    ApprovalJob, Claim, JobCounts, JobRow, MatrixRows, NewJob, all_jobs_for_run, approval_jobs,
    assign_matrix_rows, capacity_wait, claim_launch, finish_job, fork_approval_held, fork_running,
    heartbeat, held_fork_jobs, held_siblings, hold, insert_jobs, job, job_counts, job_scopes,
    jobs_by_ids, jobs_for_run, launch_failed, launched, note_queued_error, open_jobs,
    parse_cache_scopes, queue_job, refresh_job, reset_job, retarget_job, set_cache_scopes, set_job,
    set_needs, start_job, supersede_job,
};
pub use self::pool::{
    PoolRow, claims_by_bucket, drop_pool, move_pool, pool_rows, save_pool, take_pool,
};
pub use self::reconciler::{
    SWEEP_LIMIT, ScheduleRow, defer_runs, delete_runs, last_minute, lease, replace_schedules,
    schedules, schedules_after, set_last_minute, stalled_jobs, sweep,
};
pub use self::repos::{RepoRow, delivery_seen, record_delivery, repo, repo_by_name, upsert_repo};
pub use self::runs::{
    After, Bucket, GateStatus, ListedRun, NewRun, RunCounts, RunFilter, RunRow, RunStatus,
    activity, finish_run, insert_run, job_load, latest_on_default, listed_runs, reopen_run_at, run,
    run_counts, run_for_delivery, run_head_checkout, run_started, runs_page, set_run_message,
};
pub use self::secrets::{
    Owner, Put, SealedRow, SecretItem, StaleRow, VarItem, delete_secret, delete_var, job_var_rows,
    job_vars, mark_used, put_secret, put_var, repo_ids, reseal, sealed_secrets, secret_names,
    stale_secrets, tenant_repos, vars, winners,
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
