// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cache entries: the rows behind `actions/cache`. Every statement
//! names the tenant, the repository and the scope (the git ref), so a
//! job reaches the entries of its own ref, its pull request's base
//! branch and the default branch, and no others.
//!
//! Evicting an entry deletes its row first and returns it, so the caller
//! deletes the object of a row that is already gone. A lookup cannot
//! then find a row whose object went, and a save is never blocked by one.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::{Finalized, with_columns};
use crate::trust::Trust;

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

/// The entry a restore finds in repository `$2` of tenant `$1` at
/// version `$3`: of the scopes in the JSON array `$4`, in order, the
/// first with an entry; within it the key `$5` exactly, then the keys of
/// the JSON array `$6` as prefixes in order, the newest entry of a
/// prefix first.
const LOOKUP: &str = "SELECT {columns} FROM ci_cache_entries \
     JOIN jsonb_array_elements_text($4::jsonb) WITH ORDINALITY AS s(sc, si) \
       ON scope = s.sc \
     JOIN (SELECT $5::text AS pat, true AS exact, 0::bigint AS ki \
           UNION ALL \
           SELECT k.pat, false, k.ki FROM jsonb_array_elements_text($6::jsonb) \
             WITH ORDINALITY AS k(pat, ki)) AS w \
       ON (w.exact AND key = w.pat) OR (NOT w.exact AND starts_with(key, w.pat)) \
     WHERE tenant_id = $1 AND repo_id = $2 AND version = $3 AND finalized_at IS NOT NULL \
     ORDER BY s.si, w.ki, finalized_at DESC, id DESC LIMIT 1";

/// Marks entry `$1` of tenant `$2` as accessed at `$3`, when its last
/// access is before `$4`.
const TOUCH: &str = "UPDATE ci_cache_entries SET last_accessed_at = $3 \
     WHERE id = $1 AND tenant_id = $2 AND finalized_at IS NOT NULL AND last_accessed_at < $4";

/// Records an upload for repository `$2` of tenant `$1` into scope `$3`
/// at version `$4` and key `$5`, to S3 key `$6` as multipart upload `$7`,
/// at `$8`, saved by a run of trust `$9`.
const START: &str = "INSERT INTO ci_cache_entries (tenant_id, repo_id, scope, version, key, \
     object_key, upload_id, created_at, last_accessed_at, trust) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9) RETURNING {columns}";

/// The upload `$1` into scope `$4` of repository `$3` of tenant `$2`,
/// finalized or not.
const UPLOAD: &str = "SELECT {columns} FROM ci_cache_entries \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4";

/// Finalizes upload `$1` of the same owner as [`UPLOAD`] at `$5` with
/// `$6` bytes, unless a finalized entry holds its version and key. Two
/// finalizes at once can both pass the check; the unique index stops the
/// second, which [`finalize_cache_entry`] reads as a refusal.
const FINALIZE: &str = "UPDATE ci_cache_entries SET finalized_at = $5, last_accessed_at = $5, \
     size = $6, upload_id = NULL \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4 AND finalized_at IS NULL \
     AND NOT EXISTS (SELECT 1 FROM ci_cache_entries AS held \
       WHERE held.tenant_id = $2 AND held.repo_id = $3 AND held.scope = $4 \
       AND held.version = ci_cache_entries.version AND held.key = ci_cache_entries.key \
       AND held.finalized_at IS NOT NULL)";

/// Forgets upload `$1` of the same owner as [`UPLOAD`].
const DROP_PENDING: &str = "DELETE FROM ci_cache_entries \
     WHERE id = $1 AND tenant_id = $2 AND repo_id = $3 AND scope = $4 AND finalized_at IS NULL";

/// Deletes and returns the entries not accessed since `$1` and the
/// uploads started before `$2` that never finished, oldest first, at
/// most `$3`. The idleness is checked again by the delete, so an entry a
/// restore touched since the choice stays.
const EVICT_IDLE: &str = "DELETE FROM ci_cache_entries WHERE id IN (\
       SELECT id FROM ci_cache_entries \
       WHERE (finalized_at IS NOT NULL AND last_accessed_at < $1) \
          OR (finalized_at IS NULL AND created_at < $2) \
       ORDER BY last_accessed_at, id LIMIT $3) \
     AND ((finalized_at IS NOT NULL AND last_accessed_at < $1) \
          OR (finalized_at IS NULL AND created_at < $2)) \
     RETURNING {columns}";

/// Deletes and returns the finalized entries saved by trusted runs of
/// repository `$2` of tenant `$1` that do not fit in `$3` bytes when the
/// most recently used are kept first, the least recently used first, at
/// most `$4`. An entry larger than the cap does not fit alone. Entries
/// fork runs saved are not counted and never evicted here: they have
/// their own share (see [`EVICT_FORK_OVER_QUOTA`]), so a flood of them
/// cannot push a base or default branch entry out.
const EVICT_OVER_QUOTA: &str = "DELETE FROM ci_cache_entries WHERE id IN (\
       SELECT id FROM (\
         SELECT id, last_accessed_at, \
           sum(size) OVER (ORDER BY last_accessed_at DESC, id DESC) AS kept \
         FROM ci_cache_entries \
         WHERE tenant_id = $1 AND repo_id = $2 AND finalized_at IS NOT NULL \
           AND trust = 'trusted') AS ranked \
       WHERE kept > $3 ORDER BY last_accessed_at, id LIMIT $4) \
     RETURNING {columns}";

/// Like [`EVICT_OVER_QUOTA`], but only entries saved by fork runs are
/// counted and deleted: a fork run's saves never push out an entry of
/// the base or default branch.
const EVICT_FORK_OVER_QUOTA: &str = "DELETE FROM ci_cache_entries WHERE id IN (\
       SELECT id FROM (\
         SELECT id, last_accessed_at, \
           sum(size) OVER (ORDER BY last_accessed_at DESC, id DESC) AS kept \
         FROM ci_cache_entries \
         WHERE tenant_id = $1 AND repo_id = $2 AND finalized_at IS NOT NULL \
           AND trust = 'fork') AS ranked \
       WHERE kept > $3 ORDER BY last_accessed_at, id LIMIT $4) \
     RETURNING {columns}";

/// Bytes of finalized entries of repository `$2` of tenant `$1` that
/// fork runs saved.
const FORK_BYTES: &str = "SELECT COALESCE(sum(size), 0)::bigint AS n FROM ci_cache_entries \
     WHERE tenant_id = $1 AND repo_id = $2 AND finalized_at IS NOT NULL AND trust = 'fork'";

/// Who owns an entry: where a call looks for it.
#[derive(Debug, Clone, Copy)]
pub struct CacheOwner<'a> {
    /// The trust of the run that saves; recorded on a new entry.
    pub trust: Trust,
    /// The tenant.
    pub tenant_id: &'a str,
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The git ref.
    pub scope: &'a str,
}

/// The finalized entry in `owner`'s scope at `version` named `key`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn cache_entry(
    db: &Db,
    owner: CacheOwner<'_>,
    version: &str,
    key: &str,
) -> Result<Option<CacheRow>> {
    db.first(
        &with_columns(EXACT, CACHE_COLUMNS),
        &params![owner.tenant_id, owner.repo_id, owner.scope, version, key],
    )
    .await
}

/// The entry a restore finds in repository `repo_id` of `tenant_id` at
/// `version`, in one query: the first of `scopes` that has one, and in
/// it `key` exactly, else the newest entry of the first of
/// `restore_keys` that is a prefix of one.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn lookup_cache_entry(
    db: &Db,
    tenant_id: &str,
    repo_id: u64,
    version: &str,
    scopes: &[String],
    key: &str,
    restore_keys: &[String],
) -> Result<Option<CacheRow>> {
    let list = |items: &[String]| serde_json::to_string(items).unwrap_or_else(|_| "[]".into());
    db.first(
        &with_columns(LOOKUP, CACHE_COLUMNS),
        &params![
            tenant_id,
            repo_id,
            version,
            list(scopes),
            key,
            list(restore_keys)
        ],
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
                now,
                owner.trust.as_str()
            ],
        )
        .await?;
    row.ok_or_else(|| worker::Error::RustError("cache insert returned no row".to_owned()))
}

/// Upload `id` into `owner`'s scope, finalized or not, so a retried
/// complete finds the entry its first try made.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn cache_upload(db: &Db, id: u64, owner: CacheOwner<'_>) -> Result<Option<CacheRow>> {
    db.first(
        &with_columns(UPLOAD, CACHE_COLUMNS),
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
    let changes = match db
        .run(
            FINALIZE,
            &params![id, owner.tenant_id, owner.repo_id, owner.scope, now, size],
        )
        .await
    {
        Ok(changes) => changes,
        // The index that settles two finalizes of one entry at once.
        Err(err) if err.to_string().contains("ci_cache_entries_key") => 0,
        Err(err) => return Err(err),
    };
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

/// Deletes and returns the entries to evict at `now`: finalized ones not
/// accessed for `idle_ms`, and uploads older than `stale_ms`, at most
/// `limit`. The caller deletes their objects.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn evict_idle_cache_entries(
    db: &Db,
    now: u64,
    idle_ms: u64,
    stale_ms: u64,
    limit: u32,
) -> Result<Vec<CacheRow>> {
    db.all(
        &with_columns(EVICT_IDLE, CACHE_COLUMNS),
        &params![
            now.saturating_sub(idle_ms),
            now.saturating_sub(stale_ms),
            limit
        ],
    )
    .await
}

/// Deletes and returns the entries of repository `repo_id` of
/// `tenant_id` that push it past `cap` bytes: the least recently used,
/// at most `limit`. The caller deletes their objects.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn evict_over_quota_cache_entries(
    db: &Db,
    tenant_id: &str,
    repo_id: u64,
    cap: u64,
    limit: u32,
) -> Result<Vec<CacheRow>> {
    db.all(
        &with_columns(EVICT_OVER_QUOTA, CACHE_COLUMNS),
        &params![tenant_id, repo_id, cap, limit],
    )
    .await
}

/// Deletes and returns the entries saved by fork runs of repository
/// `repo_id` of `tenant_id` that push the fork entries past `cap`
/// bytes: the least recently used, at most `limit`. Entries of base and
/// default branches are never touched. The caller deletes their
/// objects.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn evict_over_fork_quota_cache_entries(
    db: &Db,
    tenant_id: &str,
    repo_id: u64,
    cap: u64,
    limit: u32,
) -> Result<Vec<CacheRow>> {
    db.all(
        &with_columns(EVICT_FORK_OVER_QUOTA, CACHE_COLUMNS),
        &params![tenant_id, repo_id, cap, limit],
    )
    .await
}

/// The bytes of finalized entries fork runs saved in repository
/// `repo_id` of `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn fork_cache_bytes(db: &Db, tenant_id: &str, repo_id: u64) -> Result<u64> {
    #[derive(Deserialize)]
    struct Bytes {
        n: u64,
    }
    Ok(db
        .first::<Bytes>(FORK_BYTES, &params![tenant_id, repo_id])
        .await?
        .map_or(0, |row| row.n))
}
