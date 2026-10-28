// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Billing state and the usage that goes to Polar.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

/// The billing state of a tenant, usage since `$2`, cap default `$3`;
/// `{owner}` picks the tenant by `$1`.
const STANDING: &str = "SELECT tenants.id AS tenant_id, tenants.org_login, \
       EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id \
         AND subscriptions.status IN ('active', 'trialing')) AS card, \
       COALESCE((SELECT cap_micros FROM ci_spend_caps WHERE tenant_id = tenants.id), $3) \
         AS cap_micros, \
       COALESCE((SELECT json_agg(json_build_object('template', u.template, 'seconds', u.seconds)) \
         FROM (SELECT template, SUM(seconds)::bigint AS seconds FROM ci_usage \
           WHERE tenant_id = tenants.id AND ended_at >= $2 GROUP BY template) u), \
         '[]'::json)::text AS seconds_by_template, \
       (SELECT count(*) FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
         WHERE ci_runs.tenant_id = tenants.id \
         AND ci_jobs.status IN ('launching', 'launched', 'running'))::bigint AS running \
     FROM tenants WHERE {owner}";

/// [`STANDING`]'s `{owner}` for the tenant of job `$1`.
const OWNER_OF_JOB: &str = "tenants.id = (SELECT ci_runs.tenant_id FROM ci_jobs \
     JOIN ci_runs ON ci_runs.id = ci_jobs.run_id WHERE ci_jobs.id = $1)";

/// [`STANDING`]'s `{owner}` for tenant `$1`.
const OWNER_IS_TENANT: &str = "tenants.id = $1::text";

/// Sets the spend cap of tenant `$1` to `$2` micros at `$3`.
const SET_SPEND_CAP: &str = "INSERT INTO ci_spend_caps (tenant_id, cap_micros, updated_at) \
     VALUES ($1, $2, $3) \
     ON CONFLICT (tenant_id) DO UPDATE SET cap_micros = $2, updated_at = $3";

/// Usage Polar has not taken and has not refused, oldest first, at
/// most `$1`.
const UNSENT_USAGE: &str = "SELECT job_id, attempt, tenant_id, template, seconds, units, ended_at \
     FROM ci_usage WHERE sent_at IS NULL AND rejected_at IS NULL \
     ORDER BY ended_at, job_id, attempt LIMIT $1";

/// A tenant's billing state this month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// The tenant.
    pub tenant_id: String,
    /// The organization's login, for messages.
    pub org_login: String,
    /// Whether an active or trialing subscription exists.
    pub card: bool,
    /// The monthly spend cap in micros: the tenant's own or the default.
    pub cap_micros: u64,
    /// Billed seconds metered this month, per template label.
    pub seconds_by_template: Vec<(String, u64)>,
    /// Jobs launching, launched or running now.
    pub running: u32,
}

/// What [`STANDING`] returns; the seconds arrive as JSON text.
#[derive(Deserialize)]
struct StandingRow {
    tenant_id: String,
    org_login: String,
    card: bool,
    cap_micros: u64,
    seconds_by_template: String,
    running: u32,
}

impl StandingRow {
    fn into_standing(self) -> Result<Standing> {
        #[derive(Deserialize)]
        struct Seconds {
            template: String,
            seconds: u64,
        }
        let seconds: Vec<Seconds> = serde_json::from_str(&self.seconds_by_template)?;
        Ok(Standing {
            tenant_id: self.tenant_id,
            org_login: self.org_login,
            card: self.card,
            cap_micros: self.cap_micros,
            seconds_by_template: seconds
                .into_iter()
                .map(|s| (s.template, s.seconds))
                .collect(),
            running: self.running,
        })
    }
}

/// The billing state of the tenant of job `job_id`: usage since
/// `month_start`, and `default_cap` for a tenant with no cap row.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn standing(
    db: &Db,
    job_id: u64,
    month_start: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_OF_JOB);
    db.first::<StandingRow>(&sql, &params![job_id, month_start, default_cap])
        .await?
        .map(StandingRow::into_standing)
        .transpose()
}

/// As [`standing`], for tenant `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_standing(
    db: &Db,
    tenant_id: &str,
    month_start: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_IS_TENANT);
    db.first::<StandingRow>(&sql, &params![tenant_id, month_start, default_cap])
        .await?
        .map(StandingRow::into_standing)
        .transpose()
}

/// Sets tenant `tenant_id`'s monthly spend cap to `cap_micros`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_spend_cap(db: &Db, tenant_id: &str, cap_micros: u64, now: u64) -> Result<()> {
    db.run(SET_SPEND_CAP, &params![tenant_id, cap_micros, now])
        .await
        .map(|_| ())
}

/// One attempt's usage that Polar has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UnsentUsage {
    /// The job.
    pub job_id: u64,
    /// The attempt.
    pub attempt: u32,
    /// The tenant, Polar's external customer id.
    pub tenant_id: String,
    /// The template's label.
    pub template: String,
    /// The billed seconds of the attempt, at least 60.
    pub seconds: u64,
    /// The billing units, priced when the attempt completed.
    pub units: u64,
    /// When the attempt completed, in milliseconds.
    pub ended_at: u64,
}

/// The oldest `limit` usage rows Polar has not taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn unsent_usage(db: &Db, limit: u32) -> Result<Vec<UnsentUsage>> {
    db.all(UNSENT_USAGE, &params![limit]).await
}

/// Marks the usage of the `(job, attempt)` pairs in `keys` as taken by
/// Polar at `now`. A row already marked keeps its time.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn usage_sent(db: &Db, keys: &[(u64, u32)], now: u64) -> Result<()> {
    mark_usage("sent_at = $1", &[], keys, now, db).await
}

/// Quarantines the usage of the `(job, attempt)` pairs in `keys`:
/// Polar refused it for good, so it is never sent again.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn usage_rejected(db: &Db, keys: &[(u64, u32)], reason: &str, now: u64) -> Result<()> {
    mark_usage(
        "rejected_at = $1, reject_reason = $2",
        &[Param::from(reason)],
        keys,
        now,
        db,
    )
    .await
}

/// Sets `assignments` on the open usage rows of `keys`. `$1` is `now`
/// and `extra` binds `$2` on.
async fn mark_usage(
    assignments: &str,
    extra: &[Param],
    keys: &[(u64, u32)],
    now: u64,
    db: &Db,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let first = extra.len() + 2;
    let pairs: Vec<String> = (0..keys.len())
        .map(|n| format!("(${}, ${})", first + 2 * n, first + 2 * n + 1))
        .collect();
    let sql = format!(
        "UPDATE ci_usage SET {assignments} WHERE sent_at IS NULL AND rejected_at IS NULL \
         AND (job_id, attempt) IN ({})",
        pairs.join(", ")
    );
    let mut bound: Vec<Param> = vec![Param::from(now)];
    bound.extend(extra.iter().cloned());
    for (job, attempt) in keys {
        bound.push(Param::from(*job));
        bound.push(Param::from(*attempt));
    }
    db.run(&sql, &bound).await.map(|_| ())
}
