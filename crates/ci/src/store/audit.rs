// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The audit trail of an organization's settings: secrets, vars,
//! environments and the repositories that run CI.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

/// What a row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A secret was set or replaced.
    SecretPut,
    /// A secret was deleted.
    SecretDelete,
    /// A var was set or replaced.
    VarPut,
    /// A var was deleted.
    VarDelete,
    /// An environment was set or replaced.
    EnvironmentPut,
    /// An environment was deleted.
    EnvironmentDelete,
    /// The repositories that run CI changed.
    ReposEnabled,
}

impl Action {
    /// The word the `action` column and the API use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecretPut => "secret.put",
            Self::SecretDelete => "secret.delete",
            Self::VarPut => "var.put",
            Self::VarDelete => "var.delete",
            Self::EnvironmentPut => "environment.put",
            Self::EnvironmentDelete => "environment.delete",
            Self::ReposEnabled => "repos.enabled",
        }
    }
}

/// A change to record.
#[derive(Debug, Clone, Copy)]
pub struct NewAudit<'a> {
    /// The tenant whose settings changed.
    pub tenant: &'a str,
    /// The GitHub login of whoever changed them.
    pub actor: &'a str,
    /// What happened.
    pub action: Action,
    /// `org`, `repo` or `env`.
    pub scope: &'a str,
    /// The repository's `owner/name`, for the `repo` and `env` scopes.
    pub repo: Option<&'a str>,
    /// What changed: a name, `env/name` for an environment's secret or
    /// var, or for the enabled repositories `all` or a count of added
    /// and removed.
    pub target: &'a str,
    /// When.
    pub now: u64,
}

/// A recorded change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuditRow {
    /// The row's identity, which grows with time.
    pub id: u64,
    /// When.
    pub at: u64,
    /// Whoever changed it.
    pub actor: String,
    /// What happened, as [`Action::as_str`] spells it.
    pub action: String,
    /// `org`, `repo` or `env`.
    pub scope: String,
    /// The repository, when the scope has one.
    pub repo: Option<String>,
    /// What changed.
    pub target: String,
}

impl NewAudit<'_> {
    /// The binds of [`NewAudit::insert_sql`], tenant to repository.
    pub(super) fn binds(&self) -> Vec<Param> {
        params![
            self.tenant,
            self.now,
            self.actor,
            self.action.as_str(),
            self.scope,
            self.repo
        ]
    }

    /// The binds of [`NewAudit::insert_sql`] with the target bound
    /// last.
    pub(super) fn binds_with_target(&self) -> Vec<Param> {
        let mut binds = self.binds();
        binds.push(Param::from(self.target));
        binds
    }

    /// An `INSERT` of the row, to run in the transaction of the change
    /// it records. Its binds start at `$first` (see [`NewAudit::binds`]);
    /// `target` is the SQL of the target column, `${first + 6}` after
    /// [`NewAudit::binds_with_target`]. `from` is a table expression
    /// that has a row only when the change happened.
    pub(super) fn insert_sql(first: usize, target: &str, from: &str) -> String {
        format!(
            "INSERT INTO ci_audit (tenant_id, at, actor, action, scope, repo, target) \
             SELECT ${}::text, ${}::bigint, ${}::text, ${}::text, ${}::text, ${}::text, \
             {target} FROM {from}",
            first,
            first + 1,
            first + 2,
            first + 3,
            first + 4,
            first + 5
        )
    }

    /// As [`NewAudit::insert_sql`], with the target bound last.
    pub(super) fn insert_bound_sql(first: usize, from: &str) -> String {
        Self::insert_sql(first, &format!("${}::text", first + 6), from)
    }
}

/// Records `audit` on its own. A change to settings records itself in
/// its own transaction instead (see [`NewAudit::insert_sql`]).
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn record_audit(db: &Db, audit: &NewAudit<'_>) -> Result<()> {
    db.run(
        &NewAudit::insert_bound_sql(1, "(SELECT 1) one"),
        &audit.binds_with_target(),
    )
    .await?;
    Ok(())
}

/// The newest `limit` rows of `tenant`, those older than `before` when
/// given, for one repository (ignoring case) when `repo` is given.
/// Also how many rows match, ignoring `before`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn audit_page(
    db: &Db,
    tenant: &str,
    repo: Option<&str>,
    before: Option<u64>,
    limit: u32,
) -> Result<(Vec<AuditRow>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let filter = "tenant_id = $1 AND ($2::text IS NULL OR lower(repo) = lower($2::text))";
    let total = db
        .first::<Total>(
            &format!("SELECT count(*) AS total FROM ci_audit WHERE {filter}"),
            &params![tenant, repo],
        )
        .await?
        .map_or(0, |row| row.total);
    let rows = db
        .all(
            &format!(
                "SELECT id, at, actor, action, scope, repo, target FROM ci_audit \
                 WHERE {filter} AND ($3::bigint IS NULL OR id < $3::bigint) \
                 ORDER BY id DESC LIMIT $4"
            ),
            &params![tenant, repo, before, limit],
        )
        .await?;
    Ok((rows, total))
}
