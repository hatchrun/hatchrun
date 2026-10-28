// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Secrets and vars. A secret's value is sealed (see `crate::secrets`);
//! a var's is plain text. An [`Owner`] says whose row it is.

use std::collections::BTreeMap;

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

use super::jobs::LOCK_TENANT;

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
pub(super) struct Saved {}

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
