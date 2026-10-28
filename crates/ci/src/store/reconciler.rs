// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What the reconciler reads and writes: schedules, its lease, the
//! last minute fired, stalled jobs and the sweep of expired rows.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::jobs::{JOB_COLUMNS, JobRow};
use super::with_columns;

/// How long runs, their jobs and delivery ids are kept, in
/// milliseconds.
const RETENTION_MS: u64 = 30 * 86_400_000;

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
