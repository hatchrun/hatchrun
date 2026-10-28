// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Repositories CI has seen, and the webhook deliveries it recorded.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::exists;

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
        // Org secrets and vars of the old tenant forget the repository
        // too, so it does not regain access if it transfers back.
        (
            "UPDATE ci_secrets SET repositories = array_remove(repositories, $1::bigint) \
             WHERE repositories IS NOT NULL AND $1::bigint = ANY (repositories) \
             AND tenant_id <> $2 \
             AND tenant_id = (SELECT tenant_id FROM ci_repos WHERE id = $1)",
            params![repo.id, &repo.tenant_id],
        ),
        (
            "UPDATE ci_vars SET repositories = array_remove(repositories, $1::bigint) \
             WHERE repositories IS NOT NULL AND $1::bigint = ANY (repositories) \
             AND tenant_id <> $2 \
             AND tenant_id = (SELECT tenant_id FROM ci_repos WHERE id = $1)",
            params![repo.id, &repo.tenant_id],
        ),
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
