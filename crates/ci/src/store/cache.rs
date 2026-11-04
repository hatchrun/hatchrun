// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cache entries: the rows behind `actions/cache`. Every statement
//! names the tenant, the repository and the scope (the git ref), so a
//! job reaches the entries of its own ref and the default branch's and
//! no others.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::{Finalized, with_columns};

/// A cache entry, or an upload that is not finalized yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CacheRow {
    /// The entry's identity.
    pub id: u64,
    /// The tenant of the repository.
    pub tenant_id: String,
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The git ref of the run that saved it.
    pub scope: String,
    /// The toolkit's version of the entry.
    pub version: String,
    /// The key the workflow gave it.
    pub key: String,
    /// The S3 key of its bytes.
    pub object_key: String,
    /// The S3 multipart upload, until the entry is finalized.
    pub upload_id: Option<String>,
    /// Bytes, once finalized.
    pub size: Option<u64>,
    /// When the upload started.
    pub created_at: u64,
    /// When the entry was last saved or restored.
    pub last_accessed_at: u64,
    /// When the entry was finalized.
    pub finalized_at: Option<u64>,
}

/// The columns of a [`CacheRow`].
const CACHE_COLUMNS: &str = "id, tenant_id, repo_id, scope, version, key, object_key, upload_id, \
     size, created_at, last_accessed_at, finalized_at";

/// A restore touches an entry at most this often, so a busy entry is
/// not rewritten by every restore. Far below the 7 day eviction.
pub const TOUCH_EVERY_MS: u64 = 3_600_000;

/// The finalized entry of repository `$2` (of tenant `$1`) in scope `$3`
/// at version `$4` with key `$5`.
const EXACT: &str = "SELECT {columns} FROM ci_cache_entries \
     WHERE tenant_id = $1 AND repo_id = $2 AND scope = $3 AND version = $4 AND key = $5 \
     AND finalized_at IS NOT NULL";

/// The newest finalized entry of the same scope and version whose key
/// starts with `$5`.
const PREFIX: &str = "SELECT {columns} FROM ci_cache_entries \
     WHERE tenant_id = $1 AND repo_id = $2 AND scope = $3 AND version = $4 \
     AND starts_with(key, $5) AND finalized_at IS NOT NULL \
     ORDER BY finalized_at DESC, id DESC LIMIT 1";

/// Marks entry `$1` of tenant `$2` as accessed at `$3`, when its last
/// access is before `$4`.
const TOUCH: &str = "UPDATE ci_cache_entries SET last_accessed_at = $3 \
     WHERE id = $1 AND tenant_id = $2 AND finalized_at IS NOT NULL AND last_accessed_at < $4";

/// Records an upload for repository `$2` of tenant `$1` into scope `$3`
/// at version `$4` and key `$5`, to S3 key `$6` as multipart upload `$7`,
/// at `$8`.
const START: &str = "INSERT INTO ci_cache_entries (tenant_id, repo_id, scope, version, key, \
     object_key, upload_id, created_at, last_accessed_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) RETURNING {columns}";

/// The upload `$1` into scope `$4` of repository `$3` of tenant `$2`,
/// which is not finalized.
const PENDING: &str = "SELECT {columns} FROM ci_cache_entries \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4 AND finalized_at IS NULL";

/// Finalizes upload `$1` of the same owner as [`PENDING`] at `$5` with
/// `$6` bytes, unless a finalized entry holds its version and key.
const FINALIZE: &str = "UPDATE ci_cache_entries SET finalized_at = $5, last_accessed_at = $5, \
     size = $6, upload_id = NULL \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4 AND finalized_at IS NULL \
     AND NOT EXISTS (SELECT 1 FROM ci_cache_entries AS held \
       WHERE held.tenant_id = $2 AND held.repo_id = $3 AND held.scope = $4 \
       AND held.version = ci_cache_entries.version AND held.key = ci_cache_entries.key \
       AND held.finalized_at IS NOT NULL)";

/// Forgets upload `$1` of the same owner as [`PENDING`].
const DROP_PENDING: &str = "DELETE FROM ci_cache_entries \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4 AND finalized_at IS NULL";

/// Entries not accessed since `$1`, and uploads started before `$2`
/// that never finished, oldest first, at most `$3`.
const IDLE: &str = "SELECT {columns} FROM ci_cache_entries \
     WHERE (finalized_at IS NOT NULL AND last_accessed_at < $1) \
     OR (finalized_at IS NULL AND created_at < $2) \
     ORDER BY last_accessed_at, id LIMIT $3";

/// Deletes entry `$1`, which [`IDLE`] returned, unless a restore touched
/// it since `$2`.
const DELETE_IDLE: &str = "DELETE FROM ci_cache_entries \
     WHERE id = $1 AND (last_accessed_at < $2 OR finalized_at IS NULL)";

/// The finalized entries of repository `$2` of tenant `$1` that do not
/// fit in `$3` bytes when the most recently used are kept first, least
/// recently used first, at most `$4`. An entry larger than the cap
/// does not fit alone.
const OVER_QUOTA: &str = "SELECT {columns} FROM (\
       SELECT {columns}, sum(size) OVER (ORDER BY last_accessed_at DESC, id DESC) AS kept \
       FROM ci_cache_entries \
       WHERE tenant_id = $1 AND repo_id = $2 AND finalized_at IS NOT NULL) AS ranked \
     WHERE kept > $3 ORDER BY last_accessed_at, id LIMIT $4";

/// Deletes entry `$1` of repository `$3` of tenant `$2`.
const DELETE_ENTRY: &str =
    "DELETE FROM ci_cache_entries WHERE id = $1 AND tenant_id = $2 AND repo_id = $3";

/// Who owns an entry: where a call looks for it.
#[derive(Debug, Clone, Copy)]
pub struct CacheOwner<'a> {
    /// The tenant.
    pub tenant_id: &'a str,
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The git ref.
    pub scope: &'a str,
}

/// The finalized entry in `owner`'s scope at `version`: the one named
/// `key` when `exact`, else the newest whose key starts with `key`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn cache_entry(
    db: &Db,
    owner: CacheOwner<'_>,
    version: &str,
    key: &str,
    exact: bool,
) -> Result<Option<CacheRow>> {
    let sql = if exact { EXACT } else { PREFIX };
    db.first(
        &with_columns(sql, CACHE_COLUMNS),
        &params![owner.tenant_id, owner.repo_id, owner.scope, version, key],
    )
    .await
}

/// Records that entry `id` of `tenant_id` was restored at `now`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn touch_cache_entry(db: &Db, id: u64, tenant_id: &str, now: u64) -> Result<()> {
    db.run(
        TOUCH,
        &params![id, tenant_id, now, now.saturating_sub(TOUCH_EVERY_MS)],
    )
    .await?;
    Ok(())
}

/// Records the start of an upload into `owner`'s scope.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn start_cache_entry(
    db: &Db,
    owner: CacheOwner<'_>,
    version: &str,
    key: &str,
    object_key: &str,
    upload_id: &str,
    now: u64,
) -> Result<CacheRow> {
    let row = db
        .first(
            &with_columns(START, CACHE_COLUMNS),
            &params![
                owner.tenant_id,
                owner.repo_id,
                owner.scope,
                version,
                key,
                object_key,
                upload_id,
                now
            ],
        )
        .await?;
    row.ok_or_else(|| worker::Error::RustError("cache insert returned no row".to_owned()))
}

/// Upload `id` into `owner`'s scope, while it is not finalized.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pending_cache_entry(
    db: &Db,
    id: u64,
    owner: CacheOwner<'_>,
) -> Result<Option<CacheRow>> {
    db.first(
        &with_columns(PENDING, CACHE_COLUMNS),
        &params![id, owner.tenant_id, owner.repo_id, owner.scope],
    )
    .await
}

/// Finalizes upload `id` into `owner`'s scope with `size` bytes.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn finalize_cache_entry(
    db: &Db,
    id: u64,
    owner: CacheOwner<'_>,
    size: u64,
    now: u64,
) -> Result<Finalized> {
    let changes = db
        .run(
            FINALIZE,
            &params![id, owner.tenant_id, owner.repo_id, owner.scope, now, size],
        )
        .await?;
    Ok(if changes == 1 {
        Finalized::Done
    } else {
        Finalized::Refused
    })
}

/// Forgets upload `id` into `owner`'s scope.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn drop_pending_cache_entry(db: &Db, id: u64, owner: CacheOwner<'_>) -> Result<()> {
    db.run(
        DROP_PENDING,
        &params![id, owner.tenant_id, owner.repo_id, owner.scope],
    )
    .await?;
    Ok(())
}

/// The entries to evict at `now`: finalized ones not accessed for
/// `idle_ms`, and uploads older than `stale_ms`, at most `limit`. Their
/// rows stay until [`delete_idle_cache_entries`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn idle_cache_entries(
    db: &Db,
    now: u64,
    idle_ms: u64,
    stale_ms: u64,
    limit: u32,
) -> Result<Vec<CacheRow>> {
    db.all(
        &with_columns(IDLE, CACHE_COLUMNS),
        &params![
            now.saturating_sub(idle_ms),
            now.saturating_sub(stale_ms),
            limit
        ],
    )
    .await
}

/// Deletes the rows of `ids`, whose objects are gone. An entry restored
/// since `now - idle_ms` stays.
///
/// # Errors
///
/// Postgres refused a statement; nothing changes.
pub async fn delete_idle_cache_entries(db: &Db, ids: &[u64], now: u64, idle_ms: u64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let cutoff = now.saturating_sub(idle_ms);
    let statements: Vec<_> = ids
        .iter()
        .map(|&id| (DELETE_IDLE, params![id, cutoff]))
        .collect();
    db.batch(&statements).await?;
    Ok(())
}

/// The entries of repository `repo_id` of `tenant_id` that push it past
/// `cap` bytes: the least recently used, at most `limit`. Their rows stay
/// until [`delete_cache_entries`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn over_quota_cache_entries(
    db: &Db,
    tenant_id: &str,
    repo_id: u64,
    cap: u64,
    limit: u32,
) -> Result<Vec<CacheRow>> {
    db.all(
        &with_columns(OVER_QUOTA, CACHE_COLUMNS),
        &params![tenant_id, repo_id, cap, limit],
    )
    .await
}

/// Deletes the rows of `ids` of repository `repo_id` of `tenant_id`,
/// whose objects are gone.
///
/// # Errors
///
/// Postgres refused a statement; nothing changes.
pub async fn delete_cache_entries(
    db: &Db,
    tenant_id: &str,
    repo_id: u64,
    ids: &[u64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let statements: Vec<_> = ids
        .iter()
        .map(|&id| (DELETE_ENTRY, params![id, tenant_id, repo_id]))
        .collect();
    db.batch(&statements).await?;
    Ok(())
}
