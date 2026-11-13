// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What the reconciler reads and writes: schedules, its lease, the
//! last minute fired, stalled jobs and partition upkeep.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

use super::jobs::{JOB_COLUMNS, JobRow};
use super::with_columns;

/// How long delivery ids are kept, in milliseconds. Runs, jobs, usage
/// and audit rows go with the month partition they sit in.
const RETENTION_MS: u64 = 30 * 86_400_000;

/// The retention tiers, in days: the `tier` of a partitioned row is the
/// retention its tenant had when the row was recorded.
const TIERS: [i64; 2] = [30, 90];

/// How long usage rows are kept, in milliseconds. Usage keeps 90 days
/// for every tenant.
const USAGE_RETENTION_MS: u64 = 90 * 86_400_000;

/// How long audit rows are kept, in milliseconds.
const AUDIT_RETENTION_MS: u64 = 400 * 86_400_000;

const DAY_MS: u64 = 86_400_000;

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

/// Schedules after repository `$1`, workflow `$2` and cron `$3`, at
/// most `$4`.
const SCHEDULES_AFTER: &str = "SELECT repo_id, workflow, cron FROM ci_schedules \
     WHERE (repo_id, workflow, cron) > ($1, $2, $3) \
     ORDER BY repo_id, workflow, cron LIMIT $4";

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
/// `$2` (a job held for capacity only when due), launching or launched ones quiet since `$3`, and running ones
/// without a heartbeat since `$4`, or without a start or any change when
/// the heartbeat is missing. [`crate::reconcile::stalled`] makes the
/// same decisions over the rows.
const STALLED_JOBS: &str = "SELECT {columns} FROM ci_jobs WHERE \
     (status = 'queued' AND held IS NULL AND (next_launch_at <= $1 OR updated_at < $2)) \
     OR (status = 'queued' AND held = 'capacity' AND next_launch_at <= $1) \
     OR (status IN ('launching', 'launched') AND updated_at < $3) \
     OR (status = 'running' AND COALESCE(heartbeat_at, started_at, updated_at) < $4) \
     ORDER BY updated_at, id LIMIT $5";

/// Deletes delivery ids received before `$1`.
const SWEEP_DELIVERIES: &str = "DELETE FROM ci_deliveries WHERE received_at < $1";

/// The month partitions of parent `$1` in this schema, oldest first.
const PARTITIONS: &str = "SELECT c.relname AS name FROM pg_class c      JOIN pg_inherits i ON i.inhrelid = c.oid      JOIN pg_class p ON p.oid = i.inhparent      JOIN pg_namespace n ON n.oid = c.relnamespace      WHERE p.relname = $1 AND n.nspname = current_schema() ORDER BY 1";

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
    schedules_after(db, None, limit).await
}

/// Schedules after `after` in the order of repository, workflow and
/// cron, at most `limit`; from the first one when `after` is `None`.
/// Passing the last row of a page reads the next page.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn schedules_after(
    db: &Db,
    after: Option<&ScheduleRow>,
    limit: u32,
) -> Result<Vec<ScheduleRow>> {
    match after {
        None => db.all(SCHEDULES, &params![limit]).await,
        Some(row) => {
            db.all(
                SCHEDULES_AFTER,
                &params![row.repo_id, &row.workflow, &row.cron, limit],
            )
            .await
        }
    }
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

/// Makes sure the partitions of this month and the next two months
/// exist: the runs and jobs of each tier, and the usage and audit
/// months. Creating one that exists already changes nothing.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn ensure_partitions(db: &Db, now: u64) -> Result<()> {
    use crate::billing::{month_label, month_start, next_month_start};
    let mut months = Vec::with_capacity(3);
    let mut start = month_start(now);
    for _ in 0..3 {
        months.push((month_label(start), start));
        start = next_month_start(start);
    }
    let mut sql = Vec::new();
    for (label, start) in &months {
        let end = next_month_start(*start);
        for tier in TIERS {
            for parent in ["ci_runs", "ci_jobs"] {
                sql.push(format!(
                    "CREATE TABLE IF NOT EXISTS {parent}_t{tier}_{label}                      PARTITION OF {parent}_t{tier} FOR VALUES FROM ({start}) TO ({end})"
                ));
            }
        }
        for parent in ["ci_usage", "ci_audit"] {
            sql.push(format!(
                "CREATE TABLE IF NOT EXISTS {parent}_{label}              PARTITION OF {parent} FOR VALUES FROM ({start}) TO ({end})"
            ));
        }
    }
    let statements: Vec<(&str, Vec<Param>)> =
        sql.iter().map(|sql| (sql.as_str(), Vec::new())).collect();
    db.batch(&statements).await?;
    Ok(())
}

/// The expired month partitions of `parent` whose whole month is
/// older than `retention` milliseconds, oldest first. `parent` names a
/// tier table like `ci_runs_t30` or a monthly table like `ci_usage`.
async fn expired_months(db: &Db, parent: &str, retention: u64, now: u64) -> Result<Vec<String>> {
    use crate::billing::{month_start_of, next_month_start};
    #[derive(Deserialize)]
    struct Partition {
        name: String,
    }
    let mut expired = Vec::new();
    for partition in db.all::<Partition>(PARTITIONS, &params![parent]).await? {
        let Some(label) = partition
            .name
            .strip_prefix(&format!("{parent}_"))
            .and_then(|label| {
                label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    .then_some(label)
            })
        else {
            continue;
        };
        let Some(start) = month_start_of(label) else {
            continue;
        };
        if next_month_start(start).saturating_add(retention) <= now {
            expired.push(partition.name);
        }
    }
    expired.sort();
    Ok(expired)
}

/// The expired run month partitions, oldest first. Their partitions
/// drop once every run in them that never concluded, and so never
/// armed its own purge, is purged through its object (see
/// [`crate::reconcile`]).
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn expired_run_partitions(db: &Db, now: u64) -> Result<Vec<String>> {
    let mut expired = Vec::new();
    for tier in TIERS {
        let retention = u64::try_from(tier).unwrap_or(30) * DAY_MS;
        expired.extend(expired_months(db, &format!("ci_runs_t{tier}"), retention, now).await?);
    }
    expired.sort();
    Ok(expired)
}

/// The ids of the runs in month partition `partition` whose object may
/// still hold state, oldest first, at most `limit`: the runs that never
/// concluded, so they never snapshotted and never armed their own
/// purge. Concluded runs purge themselves once their snapshot lands.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn runs_needing_purge(db: &Db, partition: &str, limit: u32) -> Result<Vec<u64>> {
    #[derive(Deserialize)]
    struct Run {
        id: u64,
    }
    let sql =
        format!("SELECT id FROM {partition} WHERE status <> 'completed' ORDER BY id LIMIT $1");
    Ok(db
        .all::<Run>(&sql, &params![limit])
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect())
}

/// Marks the runs `ids` of month partition `partition` concluded as
/// cancelled at `now`, once their objects were purged, so
/// [`runs_needing_purge`] no longer lists them. The month is past its
/// retention and drops once none is left.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn mark_purged(db: &Db, partition: &str, ids: &[u64], now: u64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = (0..ids.len()).map(|n| format!("${}", n + 2)).collect();
    let sql = format!(
        "UPDATE {partition} SET status = 'completed', \
         conclusion = COALESCE(conclusion, 'cancelled'), \
         completed_at = COALESCE(completed_at, $1) \
         WHERE status <> 'completed' AND id IN ({})",
        list.join(", ")
    );
    let mut bound = vec![Param::from(now)];
    bound.extend(ids.iter().map(|&id| Param::from(id)));
    db.run(&sql, &bound).await.map(|_| ())
}

/// Drops the month partitions whose whole month is older than what
/// they keep: a usage month older than 90 days, an audit month older
/// than 400 days, and the run months in `purged` with the job months of
/// the same tier and month. The artifacts and concurrency rows of a
/// dropped run month go in the same pass, and delivery ids older than
/// 30 days with them. A run month drops only once every run in it that
/// never concluded is purged, so the caller purges through
/// [`expired_run_partitions`] and [`runs_needing_purge`] first and
/// lists here what is fully purged. Returns the partitions dropped,
/// oldest first.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn drop_expired_partitions(db: &Db, now: u64, purged: &[String]) -> Result<Vec<String>> {
    let mut expired = expired_months(db, "ci_usage", USAGE_RETENTION_MS, now).await?;
    expired.extend(expired_months(db, "ci_audit", AUDIT_RETENTION_MS, now).await?);
    let mut runs = Vec::new();
    for tier in TIERS {
        let retention = u64::try_from(tier).unwrap_or(30) * DAY_MS;
        let parent = format!("ci_runs_t{tier}");
        for partition in expired_months(db, &parent, retention, now).await? {
            if purged.contains(&partition) {
                runs.push(partition);
            }
        }
        let jobs = expired_months(db, &format!("ci_jobs_t{tier}"), retention, now).await?;
        for partition in jobs {
            let Some(month) = partition.strip_prefix(&format!("ci_jobs_t{tier}_")) else {
                continue;
            };
            if purged.contains(&format!("ci_runs_t{tier}_{month}")) {
                expired.push(partition);
            }
        }
    }
    runs.sort();
    expired.extend(runs.iter().cloned());
    expired.sort();
    let mut sql = vec![(
        SWEEP_DELIVERIES.to_owned(),
        params![now.saturating_sub(RETENTION_MS)],
    )];
    for partition in &runs {
        sql.push((
            format!("DELETE FROM ci_artifacts WHERE run_id IN (SELECT id FROM {partition})"),
            Vec::new(),
        ));
        sql.push((
            format!("DELETE FROM ci_concurrency WHERE run_id IN (SELECT id FROM {partition})"),
            Vec::new(),
        ));
    }
    for partition in &expired {
        sql.push((format!("DROP TABLE {partition}"), Vec::new()));
    }
    let statements: Vec<(&str, Vec<Param>)> = sql
        .iter()
        .map(|(sql, params)| (sql.as_str(), params.clone()))
        .collect();
    db.batch(&statements).await?;
    Ok(expired)
}
