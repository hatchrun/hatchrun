// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The warm pool (see `crate::pool`). Times are Unix milliseconds.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

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
