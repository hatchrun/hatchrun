// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! One reconciler shard's row in `ci_shards`: its leases, where its
//! passes stopped, and when its last pass finished with how many
//! errors.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::reconciler::StalledCursor;

/// Makes sure the row of the shard from bucket `$1` to `$2` exists. A
/// new row starts its schedule scan where the oldest shard is. A row
/// an older shard count left with another end takes this end once both
/// its leases ran out before `$3`, so a pass of the old layout still
/// holding it keeps its range until it ends.
const ENSURE_SHARD: &str = "INSERT INTO ci_shards (first_bucket, end_bucket, last_minute) \
     SELECT $1, $2, (SELECT min(last_minute) FROM ci_shards) \
     ON CONFLICT (first_bucket) DO UPDATE SET end_bucket = EXCLUDED.end_bucket \
     WHERE ci_shards.end_bucket <> EXCLUDED.end_bucket \
     AND ci_shards.lease_until < $3 AND ci_shards.schedule_lease_until < $3";

/// Takes the pass lease of the shard from `$1` to `$4` until `$3`
/// when it ran out before `$2` and the row has this range. Another
/// range that overlaps this one
/// and still holds its pass lease refuses it, so two passes of
/// different shard counts never fix the same buckets at once. Ranges
/// that do not overlap lease alone.
const SHARD_LEASE: &str = "UPDATE ci_shards SET lease_until = $3 \
     WHERE first_bucket = $1 AND end_bucket = $4 AND lease_until < $2 AND NOT EXISTS (SELECT 1 FROM ci_shards o \
     WHERE o.first_bucket <> $1 AND o.first_bucket < $4 AND o.end_bucket > $1 \
     AND o.lease_until >= $2)";

/// Takes the schedule lease of the shard from `$1` to `$4` until `$3`
/// when it ran out before `$2` and the row has this range. Another
/// overlapping range that still
/// holds its schedule lease refuses it; ranges that do not overlap
/// lease alone.
const SCHEDULE_LEASE: &str = "UPDATE ci_shards SET schedule_lease_until = $3 \
     WHERE first_bucket = $1 AND end_bucket = $4 AND schedule_lease_until < $2 AND NOT EXISTS (SELECT 1 \
     FROM ci_shards o WHERE o.first_bucket <> $1 AND o.first_bucket < $4 \
     AND o.end_bucket > $1 AND o.schedule_lease_until >= $2)";

/// Where the shard starting at `$1` stopped in the stalled jobs.
const SHARD_STALLED_CURSOR: &str = "SELECT stalled_after_at, stalled_after_id \
     FROM ci_shards WHERE first_bucket = $1";

/// Sets where the shard starting at `$1` stopped in the stalled jobs
/// to `$2`, `$3`.
const SET_SHARD_STALLED_CURSOR: &str = "UPDATE ci_shards \
     SET stalled_after_at = $2, stalled_after_id = $3 WHERE first_bucket = $1";

/// The last schedule minute the shard starting at `$1` fired.
const SHARD_LAST_MINUTE: &str = "SELECT last_minute FROM ci_shards WHERE first_bucket = $1";

/// Sets the last schedule minute the shard starting at `$1` fired to
/// `$2`.
const SET_SHARD_LAST_MINUTE: &str = "UPDATE ci_shards SET last_minute = $2 WHERE first_bucket = $1";

/// Stamps the finish of the shard starting at `$1` at `$2`, with `$3`
/// failed steps and items.
const SHARD_FINISHED: &str =
    "UPDATE ci_shards SET finished_at = $2, errors = $3 WHERE first_bucket = $1";

/// Stamps the finish of the schedule pass of the shard starting at
/// `$1` at `$2`, with `$3` failed repositories and fires.
const SCHEDULE_FINISHED: &str = "UPDATE ci_shards \
     SET schedule_finished_at = $2, schedule_errors = $3 WHERE first_bucket = $1";

/// Every shard's finish records, in shard order, rows of an older
/// shard layout included.
const SHARD_HEALTH: &str = "SELECT first_bucket, end_bucket, finished_at, errors, \
     schedule_finished_at, schedule_errors FROM ci_shards ORDER BY first_bucket";

/// Makes sure the row of the shard from `first_bucket` to
/// `end_bucket` exists, taking over a row of an older shard count once
/// its leases ran out before `now`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn ensure_shard(db: &Db, first_bucket: u16, end_bucket: u16, now: u64) -> Result<()> {
    db.run(ENSURE_SHARD, &params![first_bucket, end_bucket, now])
        .await?;
    Ok(())
}

/// Takes the pass lease of the shard from `first_bucket` to
/// `end_bucket` until `until` when it ran out before `now`. Returns
/// whether it is taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn shard_lease(
    db: &Db,
    first_bucket: u16,
    end_bucket: u16,
    now: u64,
    until: u64,
) -> Result<bool> {
    Ok(db
        .run(SHARD_LEASE, &params![first_bucket, now, until, end_bucket])
        .await?
        == 1)
}

/// Takes the schedule lease of the shard from `first_bucket` to
/// `end_bucket` until `until` when it ran out before `now`. Returns
/// whether it is taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn schedule_lease(
    db: &Db,
    first_bucket: u16,
    end_bucket: u16,
    now: u64,
    until: u64,
) -> Result<bool> {
    Ok(db
        .run(
            SCHEDULE_LEASE,
            &params![first_bucket, now, until, end_bucket],
        )
        .await?
        == 1)
}

/// Where the shard starting at `first_bucket` stopped in the stalled
/// jobs; the start when none did.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn shard_stalled_cursor(db: &Db, first_bucket: u16) -> Result<StalledCursor> {
    Ok(db
        .first::<StalledCursor>(SHARD_STALLED_CURSOR, &params![first_bucket])
        .await?
        .unwrap_or_default())
}

/// Records where the shard starting at `first_bucket` stopped in the
/// stalled jobs.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_shard_stalled_cursor(
    db: &Db,
    first_bucket: u16,
    cursor: StalledCursor,
) -> Result<()> {
    db.run(
        SET_SHARD_STALLED_CURSOR,
        &params![first_bucket, cursor.updated_at, cursor.id],
    )
    .await?;
    Ok(())
}

/// The last schedule minute the shard starting at `first_bucket`
/// fired, in Unix milliseconds.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn shard_last_minute(db: &Db, first_bucket: u16) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Minute {
        last_minute: Option<u64>,
    }
    Ok(db
        .first::<Minute>(SHARD_LAST_MINUTE, &params![first_bucket])
        .await?
        .and_then(|row| row.last_minute))
}

/// Records the last schedule minute the shard starting at
/// `first_bucket` fired.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_shard_last_minute(db: &Db, first_bucket: u16, minute_ms: u64) -> Result<()> {
    db.run(SET_SHARD_LAST_MINUTE, &params![first_bucket, minute_ms])
        .await?;
    Ok(())
}

/// Stamps the finish of the shard starting at `first_bucket` at `now`,
/// with `errors` failed steps and items.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn shard_finished(db: &Db, first_bucket: u16, now: u64, errors: u32) -> Result<()> {
    db.run(SHARD_FINISHED, &params![first_bucket, now, errors])
        .await?;
    Ok(())
}

/// Stamps the finish of the schedule pass of the shard starting at
/// `first_bucket` at `now`, with `errors` failed repositories and fires.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn schedule_finished(db: &Db, first_bucket: u16, now: u64, errors: u32) -> Result<()> {
    db.run(SCHEDULE_FINISHED, &params![first_bucket, now, errors])
        .await?;
    Ok(())
}

/// One shard's finish records: when its last pass and its last
/// schedule pass finished, and how many of their items failed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShardHealth {
    /// The shard's first bucket.
    pub first_bucket: u16,
    /// The shard's end bucket, exclusive.
    pub end_bucket: u16,
    /// When its last pass finished, in Unix milliseconds.
    pub finished_at: Option<u64>,
    /// How many of that pass's steps and items failed.
    pub errors: i32,
    /// When its last schedule pass finished, in Unix milliseconds.
    pub schedule_finished_at: Option<u64>,
    /// How many of that schedule pass's repositories and fires failed.
    pub schedule_errors: i32,
}

/// Every shard's finish records, in shard order, rows of an older
/// shard count included.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn shard_health(db: &Db) -> Result<Vec<ShardHealth>> {
    db.all(SHARD_HEALTH, &[]).await
}
