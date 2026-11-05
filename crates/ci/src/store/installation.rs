// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The repositories of a tenant's App installation, and which of them
//! run CI.
//!
//! A tenant with `ci_all_repos` runs every repository. Without it, a
//! repository runs when its row here has `enabled`. A repository with
//! no row follows the tenant's flag only.

use hatchrun_cloud::db::{Db, params};
use serde::{Deserialize, Serialize};
use worker::Result;

use crate::github::InstalledRepo;

/// The rows of the JSON list `$2` of repositories.
const GIVEN: &str = "jsonb_to_recordset($2::jsonb) AS g(id bigint, full_name text, \
     default_branch text, private boolean)";

/// A repository of the installation, with its newest run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgRepo {
    /// `owner/repo`.
    pub name: String,
    /// The default branch.
    pub default_branch: String,
    /// Whether the repository runs CI.
    pub enabled: bool,
    /// The newest run of the repository.
    pub last_run: Option<LastRun>,
}

/// The newest run of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    /// The run's id.
    pub id: u64,
    /// The run's number in its workflow.
    pub run_number: u64,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// The conclusion, once completed.
    pub conclusion: Option<String>,
    /// When the run was recorded.
    pub created_at: u64,
}

#[derive(Deserialize)]
struct RepoRow {
    name: String,
    default_branch: String,
    enabled: bool,
    run_id: Option<u64>,
    run_number: Option<u64>,
    run_status: Option<String>,
    run_conclusion: Option<String>,
    run_created_at: Option<u64>,
}

/// Which repositories a list holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct RepoFilter<'a> {
    /// Text in the name, ignoring case.
    pub text: Option<&'a str>,
    /// Only the enabled, or only the disabled, ones.
    pub enabled: Option<bool>,
}

/// The `LIKE` pattern for text in a name.
fn contains(text: &str) -> String {
    let mut out = String::from("%");
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The installation's repositories of `tenant`, the newest run first
/// and then by name: `limit` of them from `offset`, and how many the
/// filter holds in all.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repos_page(
    db: &Db,
    tenant: &str,
    filter: RepoFilter<'_>,
    offset: u64,
    limit: u32,
) -> Result<(Vec<OrgRepo>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let from = "FROM ci_installation_repos i JOIN tenants t ON t.id = i.tenant_id \
         LEFT JOIN LATERAL (SELECT id, run_number, status, conclusion, created_at \
           FROM ci_runs r WHERE r.repo_id = i.repo_id AND r.tenant_id = i.tenant_id \
           ORDER BY r.created_at DESC, r.id DESC LIMIT 1) lr ON true \
         WHERE i.tenant_id = $1 AND ($2::text IS NULL OR i.full_name ILIKE $2::text) \
         AND ($3::boolean IS NULL OR (t.ci_all_repos OR i.enabled) = $3::boolean)";
    let bound = params![tenant, filter.text.map(contains), filter.enabled];
    let total = db
        .first::<Total>(&format!("SELECT count(*) AS total {from}"), &bound)
        .await?
        .map_or(0, |row| row.total);
    let mut bound = bound;
    bound.extend(params![limit, offset]);
    let rows: Vec<RepoRow> = db
        .all(
            &format!(
                "SELECT i.full_name AS name, i.default_branch, (t.ci_all_repos OR i.enabled) AS enabled, \
                 lr.id AS run_id, lr.run_number, lr.status AS run_status, \
                 lr.conclusion AS run_conclusion, lr.created_at AS run_created_at {from} \
                 ORDER BY lr.created_at DESC NULLS LAST, lower(i.full_name), i.repo_id \
                 LIMIT $4 OFFSET $5"
            ),
            &bound,
        )
        .await?;
    let items = rows
        .into_iter()
        .map(|row| OrgRepo {
            last_run: match (
                row.run_id,
                row.run_number,
                row.run_status,
                row.run_created_at,
            ) {
                (Some(id), Some(run_number), Some(status), Some(created_at)) => Some(LastRun {
                    id,
                    run_number,
                    status,
                    conclusion: row.run_conclusion,
                    created_at,
                }),
                _ => None,
            },
            name: row.name,
            default_branch: row.default_branch,
            enabled: row.enabled,
        })
        .collect();
    Ok((items, total))
}

/// When `tenant`'s installation repositories were last read from GitHub,
/// and how many there are.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn installation_synced(db: &Db, tenant: &str) -> Result<(Option<u64>, u64)> {
    #[derive(Deserialize)]
    struct Row {
        synced_at: Option<u64>,
        total: u64,
    }
    Ok(db
        .first::<Row>(
            "SELECT ci_repos_synced_at AS synced_at, \
             (SELECT count(*) FROM ci_installation_repos WHERE tenant_id = t.id) AS total \
             FROM tenants t WHERE id = $1",
            &params![tenant],
        )
        .await?
        .map_or((None, 0), |row| (row.synced_at, row.total)))
}

/// Replaces the installation repositories of `tenant` with `repos`, as
/// read from GitHub at `now`. A repository that stays keeps its
/// `enabled`; a new one starts with the tenant's `ci_all_repos`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn sync_installation(
    db: &Db,
    tenant: &str,
    repos: &[InstalledRepo],
    now: u64,
) -> Result<()> {
    #[derive(Serialize)]
    struct Given<'a> {
        id: u64,
        full_name: &'a str,
        default_branch: &'a str,
        private: bool,
    }
    let given: Vec<Given<'_>> = repos
        .iter()
        .map(|repo| Given {
            id: repo.id,
            full_name: &repo.full_name,
            default_branch: if repo.default_branch.is_empty() {
                "main"
            } else {
                &repo.default_branch
            },
            private: repo.private,
        })
        .collect();
    let given = serde_json::to_string(&given).map_err(worker::Error::from)?;
    db.batch(&[
        (
            &format!(
                "INSERT INTO ci_installation_repos \
                 (tenant_id, repo_id, full_name, default_branch, private, enabled, updated_at) \
                 SELECT $1, g.id, g.full_name, g.default_branch, g.private, \
                   (SELECT ci_all_repos FROM tenants WHERE id = $1), $3 FROM {GIVEN} \
                 ON CONFLICT (tenant_id, repo_id) DO UPDATE SET full_name = EXCLUDED.full_name, \
                   default_branch = EXCLUDED.default_branch, private = EXCLUDED.private, \
                   updated_at = EXCLUDED.updated_at"
            ),
            params![tenant, &given, now],
        ),
        (
            &format!(
                "DELETE FROM ci_installation_repos WHERE tenant_id = $1 \
                 AND repo_id NOT IN (SELECT g.id FROM {GIVEN})"
            ),
            params![tenant, &given],
        ),
        (
            "UPDATE tenants SET ci_repos_synced_at = $2 WHERE id = $1",
            params![tenant, now],
        ),
    ])
    .await?;
    Ok(())
}

/// Updates the name, default branch and visibility of a repository that
/// changed, when the tenant's installation list holds it.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn touch_installation_repo(
    db: &Db,
    tenant: &str,
    repo_id: u64,
    full_name: &str,
    default_branch: &str,
    private: bool,
    now: u64,
) -> Result<()> {
    db.run(
        "UPDATE ci_installation_repos SET full_name = $3, default_branch = $4, private = $5, \
         updated_at = $6 WHERE tenant_id = $1 AND repo_id = $2",
        &params![tenant, repo_id, full_name, default_branch, private, now],
    )
    .await?;
    Ok(())
}

/// Whether repository `repo_id` runs CI for `tenant`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo_enabled(db: &Db, tenant: &str, repo_id: u64) -> Result<bool> {
    #[derive(Deserialize)]
    struct Row {
        enabled: bool,
    }
    Ok(db
        .first::<Row>(
            "SELECT (t.ci_all_repos OR COALESCE((SELECT i.enabled FROM ci_installation_repos i \
             WHERE i.tenant_id = t.id AND i.repo_id = $2), false)) AS enabled \
             FROM tenants t WHERE t.id = $1",
            &params![tenant, repo_id],
        )
        .await?
        .is_some_and(|row| row.enabled))
}

/// The names in `names` that are not installation repositories of
/// `tenant` (ignoring case), as given.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn unknown_repos(db: &Db, tenant: &str, names: &[String]) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Known {
        name: String,
    }
    let known: Vec<Known> = db
        .all(
            "SELECT lower(full_name) AS name FROM ci_installation_repos WHERE tenant_id = $1 \
             AND lower(full_name) = ANY (string_to_array(lower($2), ','))",
            &params![tenant, names.join(",")],
        )
        .await?;
    Ok(names
        .iter()
        .filter(|name| !known.iter().any(|k| k.name == name.to_lowercase()))
        .cloned()
        .collect())
}

/// Sets what runs for `tenant`: every repository with `all`, else the
/// repositories in `names`. Each repository's `enabled` is set to
/// match, so the choice survives switching `all` off.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_enabled(db: &Db, tenant: &str, all: bool, names: &[String]) -> Result<()> {
    db.batch(&[
        (
            "UPDATE tenants SET ci_all_repos = $2 WHERE id = $1",
            params![tenant, all],
        ),
        (
            "UPDATE ci_installation_repos SET enabled = $2 \
             OR lower(full_name) = ANY (string_to_array(lower($3), ',')) WHERE tenant_id = $1",
            params![tenant, all, names.join(",")],
        ),
    ])
    .await?;
    Ok(())
}

/// What the setup page counts for `tenant`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SetupRow {
    /// The App installation, when there is one.
    pub installation_id: Option<u64>,
    /// Whether every repository runs.
    pub all_repos: bool,
    /// The installation's repositories.
    pub repos: u64,
    /// How many of them run.
    pub enabled: u64,
    /// The tenant's first run.
    pub first_run_id: Option<u64>,
    /// The repository of that run.
    pub first_run_repository: Option<String>,
}

/// The counts of the setup page of `tenant`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_setup(db: &Db, tenant: &str) -> Result<Option<SetupRow>> {
    db.first(
        "SELECT t.installation_id, t.ci_all_repos AS all_repos, \
         (SELECT count(*) FROM ci_installation_repos i WHERE i.tenant_id = t.id) AS repos, \
         (SELECT count(*) FROM ci_installation_repos i WHERE i.tenant_id = t.id \
            AND (t.ci_all_repos OR i.enabled)) AS enabled, \
         fr.id AS first_run_id, fr.full_name AS first_run_repository \
         FROM tenants t LEFT JOIN LATERAL (SELECT r.id, p.full_name FROM ci_runs r \
           JOIN ci_repos p ON p.id = r.repo_id WHERE r.tenant_id = t.id \
           ORDER BY r.id LIMIT 1) fr ON true WHERE t.id = $1",
        &params![tenant],
    )
    .await
}

/// The App installation of `tenant`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_installation(db: &Db, tenant: &str) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Row {
        installation_id: Option<u64>,
    }
    Ok(db
        .first::<Row>(
            "SELECT installation_id FROM tenants WHERE id = $1",
            &params![tenant],
        )
        .await?
        .and_then(|row| row.installation_id))
}

/// The ids and visibility of the repositories CI has seen for `tenant`,
/// by the installation that reaches them.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_repo_visibility(db: &Db, tenant: &str) -> Result<Vec<RepoVisibility>> {
    db.all(
        "SELECT id, private, installation_id FROM ci_repos WHERE tenant_id = $1 \
         AND position('#gone-' in full_name) = 0",
        &params![tenant],
    )
    .await
}

/// A repository CI has seen, as far as access needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RepoVisibility {
    /// GitHub's repository id.
    pub id: u64,
    /// Whether it is private.
    pub private: bool,
    /// The installation that reaches it.
    pub installation_id: u64,
}
