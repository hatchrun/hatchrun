// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Environments, and the approvals that hang on them.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use serde::de::{Deserializer, Error as _};
use worker::Result;

use super::jobs::LOCK_TENANT;

/// Reads a JSON array of strings that the row carries as text.
fn json_strings<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    let text = String::deserialize(d)?;
    serde_json::from_str(&text).map_err(D::Error::custom)
}

/// An environment of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EnvironmentRow {
    /// The environment's identity.
    pub id: u64,
    /// Its repository.
    pub repo_id: u64,
    /// The name, in the case it was first saved with.
    pub name: String,
    /// The branch patterns a run's ref must match; empty allows any.
    #[serde(deserialize_with = "json_strings")]
    pub branches: Vec<String>,
    /// Whether a job that names it waits for an approval.
    pub approval: bool,
    /// When it last changed.
    pub updated_at: u64,
}

/// The columns of an [`EnvironmentRow`].
const ENVIRONMENT_COLUMNS: &str =
    "id, repo_id, name, branches::text AS branches, approval, updated_at";

/// What [`put_environment`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutEnvironment {
    /// Stored: created, or updated.
    Saved(EnvironmentRow),
    /// A new name over the repository's limit.
    TooMany,
}

/// The environments of `repo_id`, by name ignoring case.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn environments(db: &Db, repo_id: u64) -> Result<Vec<EnvironmentRow>> {
    db.all(
        &format!(
            "SELECT {ENVIRONMENT_COLUMNS} FROM ci_environments WHERE repo_id = $1 \
             ORDER BY lower(name), name"
        ),
        &params![repo_id],
    )
    .await
}

/// The environment of `repo_id` called `name`, ignoring case.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn environment(db: &Db, repo_id: u64, name: &str) -> Result<Option<EnvironmentRow>> {
    db.first(
        &format!(
            "SELECT {ENVIRONMENT_COLUMNS} FROM ci_environments \
             WHERE repo_id = $1 AND lower(name) = lower($2)"
        ),
        &params![repo_id, name],
    )
    .await
}

/// Creates the environment, or updates the branches and approval of the
/// row of that name in any case (its stored name stays). A new name is
/// refused with [`PutEnvironment::TooMany`] when the repository already
/// holds `limit`. Runs with the tenant row locked, so two puts cannot
/// both take the last place.
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)] // one column each
pub async fn put_environment(
    db: &Db,
    tenant: &str,
    repo_id: u64,
    name: &str,
    branches: &[String],
    approval: bool,
    limit: u32,
    now: u64,
) -> Result<PutEnvironment> {
    let branches = serde_json::to_string(branches).map_err(worker::Error::from)?;
    let sql = format!(
        "WITH updated AS (UPDATE ci_environments SET branches = $4::jsonb, approval = $5, \
           updated_at = $7 WHERE repo_id = $2 AND lower(name) = lower($3) \
           RETURNING {ENVIRONMENT_COLUMNS}), \
         inserted AS (INSERT INTO ci_environments \
           (tenant_id, repo_id, name, branches, approval, updated_at) \
           SELECT $1, $2::bigint, $3, $4::jsonb, $5, $7 \
           WHERE NOT EXISTS (SELECT 1 FROM updated) \
             AND (SELECT count(*) FROM ci_environments WHERE repo_id = $2) < $6 \
           RETURNING {ENVIRONMENT_COLUMNS}) \
         SELECT * FROM updated UNION ALL SELECT * FROM inserted"
    );
    let lock = params![tenant];
    let saved: Option<EnvironmentRow> = db
        .first_locked(
            (LOCK_TENANT, &lock),
            &sql,
            &params![tenant, repo_id, name, branches, approval, limit, now],
        )
        .await?;
    Ok(saved.map_or(PutEnvironment::TooMany, PutEnvironment::Saved))
}

/// Deletes the environment of `repo_id` called `name`, ignoring case,
/// with its secrets and vars. Returns whether it existed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delete_environment(db: &Db, repo_id: u64, name: &str) -> Result<bool> {
    db.run(
        "DELETE FROM ci_environments WHERE repo_id = $1 AND lower(name) = lower($2)",
        &params![repo_id, name],
    )
    .await
    .map(|n| n > 0)
}

/// What the launch consumer and the claim need about a job's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnvironment {
    /// The job's run.
    pub run_id: u64,
    /// The run's repository.
    pub repo_id: u64,
    /// `owner/repo`.
    pub full_name: String,
    /// The event that started the run.
    pub event: String,
    /// The run's ref.
    pub git_ref: String,
    /// The environment the job names, `None` when the repository has
    /// none of that name.
    pub environment: Option<EnvironmentRow>,
}

/// The columns [`job_environment`] reads; the environment's are null
/// when the repository has none of the job's name.
#[derive(Deserialize)]
struct JobEnvironmentRow {
    run_id: u64,
    repo_id: u64,
    full_name: String,
    event: String,
    git_ref: String,
    environment_id: Option<u64>,
    environment_name: Option<String>,
    branches: Option<String>,
    approval: Option<bool>,
    environment_updated_at: Option<u64>,
}

/// The run of job `job_id` and the environment it names.
///
/// # Errors
///
/// Postgres refused the statement, or a stored `branches` is not a JSON
/// array of strings.
pub async fn job_environment(db: &Db, job_id: u64) -> Result<Option<JobEnvironment>> {
    let row: Option<JobEnvironmentRow> = db
        .first(
            "SELECT ci_runs.id AS run_id, ci_runs.repo_id, ci_repos.full_name, ci_runs.event, \
               ci_runs.ref AS git_ref, e.id AS environment_id, e.name AS environment_name, \
               e.branches::text AS branches, e.approval, e.updated_at AS environment_updated_at \
             FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
               JOIN ci_repos ON ci_repos.id = ci_runs.repo_id \
               LEFT JOIN ci_environments e ON e.repo_id = ci_runs.repo_id \
                 AND e.tenant_id = ci_runs.tenant_id \
                 AND lower(e.name) = lower(ci_jobs.environment) \
             WHERE ci_jobs.id = $1",
            &params![job_id],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let environment = match (
        row.environment_id,
        row.environment_name,
        row.branches,
        row.approval,
        row.environment_updated_at,
    ) {
        (Some(id), Some(name), Some(branches), Some(approval), Some(updated_at)) => {
            Some(EnvironmentRow {
                id,
                repo_id: row.repo_id,
                name,
                branches: serde_json::from_str(&branches).map_err(worker::Error::from)?,
                approval,
                updated_at,
            })
        }
        _ => None,
    };
    Ok(Some(JobEnvironment {
        run_id: row.run_id,
        repo_id: row.repo_id,
        full_name: row.full_name,
        event: row.event,
        git_ref: row.git_ref,
        environment,
    }))
}

/// Clears the approval holds of queued jobs in repository `repo_id` that
/// name environment `name` (ignoring case), due at `now`, and returns
/// them for the launch queue.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_approval_holds(
    db: &Db,
    repo_id: u64,
    name: &str,
    now: u64,
) -> Result<Vec<hatchrun_cloud::ci_wake::Woken>> {
    db.all(
        "UPDATE ci_jobs SET held = NULL, next_launch_at = $3, updated_at = $3 FROM ci_runs \
         WHERE ci_runs.id = ci_jobs.run_id AND ci_runs.repo_id = $1 \
           AND ci_runs.tenant_id = (SELECT tenant_id FROM ci_repos WHERE id = $1) \
           AND lower(ci_jobs.environment) = lower($2) \
           AND ci_jobs.status = 'queued' AND ci_jobs.held = 'approval' \
         RETURNING ci_jobs.id AS job_id, ci_jobs.attempt",
        &params![repo_id, name, now],
    )
    .await
}

/// Records the decision on attempt `attempt` of job `job_id` while it
/// is queued and held for approval: the hold is cleared and the job is
/// due at `now`. Returns whether it applied; a job that was decided
/// already, is held for another reason or is on another attempt is left
/// as it is.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn decide_approval(
    db: &Db,
    job_id: u64,
    attempt: u32,
    approved: bool,
    login: &str,
    now: u64,
) -> Result<bool> {
    let decision = if approved { "approved" } else { "rejected" };
    Ok(db
        .run(
            "UPDATE ci_jobs SET approval_attempt = $2, approval_decision = $3, \
             approval_by = $4, approval_at = $5, held = NULL, next_launch_at = $5, \
             updated_at = $5 \
             WHERE id = $1 AND attempt = $2 AND status = 'queued' AND held = 'approval'",
            &params![job_id, attempt, decision, login, now],
        )
        .await?
        == 1)
}
