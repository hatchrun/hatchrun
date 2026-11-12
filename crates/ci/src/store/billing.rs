// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Billing state and the usage that goes to Polar.

use hatchrun_cloud::db::{Db, Param, params};
use serde::Deserialize;
use worker::Result;

/// The billing state of a tenant, usage over its spend cap window and
/// cap default `$5`; `{owner}` picks the tenant by `$1`. `$2` is now,
/// `$3` the UTC calendar month start and `$4` the next UTC calendar
/// month start, all in milliseconds.
///
/// The window starts at the latest active or trialing subscription's
/// period start, or at its period end when now is at or past it (the
/// renewal webhook is late); a tenant with no such subscription keeps
/// the calendar month. The window ends at the period end, or one
/// period length past it when the renewal is late, or at the next
/// calendar month start.
const STANDING: &str = "SELECT tenants.id AS tenant_id, tenants.org_login, \
       (tenants.beta OR EXISTS (SELECT 1 FROM subscriptions \
         WHERE subscriptions.tenant_id = tenants.id \
         AND subscriptions.status IN ('active', 'trialing'))) AS card, \
       EXISTS (SELECT 1 FROM subscriptions \
         WHERE subscriptions.tenant_id = tenants.id \
         AND subscriptions.status IN ('active', 'trialing')) AS subscribed, \
       COALESCE((SELECT cap_micros FROM ci_spend_caps WHERE tenant_id = tenants.id), $5) \
         AS cap_micros, \
       COALESCE(period.start_ms, $3) AS window_start, \
       COALESCE(period.end_ms, $4) AS window_end, \
       COALESCE((SELECT json_agg(json_build_object('template', u.template, 'seconds', u.seconds)) \
         FROM (SELECT template, SUM(seconds)::bigint AS seconds FROM ci_usage \
           WHERE tenant_id = tenants.id AND ended_at >= COALESCE(period.start_ms, $3) \
           GROUP BY template) u), \
         '[]'::json)::text AS seconds_by_template, \
       (SELECT COALESCE(SUM(units), 0)::bigint FROM ci_usage \
         WHERE tenant_id = tenants.id AND ended_at >= COALESCE(period.start_ms, $3)) AS units, \
       (SELECT count(*) FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
         WHERE ci_runs.tenant_id = tenants.id \
         AND ci_jobs.status IN ('launching', 'launched', 'running'))::bigint AS running \
     FROM tenants LEFT JOIN LATERAL (SELECT CASE WHEN one.rolled \
         THEN one.end_ms ELSE one.start_ms END AS start_ms, \
         CASE WHEN one.rolled THEN one.end_ms + (one.end_ms - one.start_ms) \
           ELSE one.end_ms END AS end_ms FROM (SELECT \
         (EXTRACT(epoch FROM s.current_period_start::timestamptz) * 1000)::bigint \
           AS start_ms, \
         CASE WHEN s.current_period_end IS NULL THEN NULL \
           ELSE (EXTRACT(epoch FROM s.current_period_end::timestamptz) * 1000)::bigint \
           END AS end_ms, \
         s.current_period_end IS NOT NULL \
           AND $2 >= (EXTRACT(epoch FROM s.current_period_end::timestamptz) * 1000)::bigint \
           AS rolled \
         FROM subscriptions s WHERE s.tenant_id = tenants.id \
         AND s.status IN ('active', 'trialing') \
         ORDER BY s.modified_at DESC LIMIT 1) AS one) AS period ON true \
     WHERE {owner}";

/// [`STANDING`]'s `{owner}` for the tenant of job `$1`.
const OWNER_OF_JOB: &str = "tenants.id = (SELECT ci_runs.tenant_id FROM ci_jobs \
     JOIN ci_runs ON ci_runs.id = ci_jobs.run_id WHERE ci_jobs.id = $1)";

/// [`STANDING`]'s `{owner}` for tenant `$1`.
const OWNER_IS_TENANT: &str = "tenants.id = $1::text";

/// Billed seconds and units per UTC day, from `$2`, of tenant `$1`.
const DAILY_USAGE: &str = "SELECT to_char(to_timestamp(ended_at / 1000) AT TIME ZONE 'UTC', \
       'YYYY-MM-DD') AS day, SUM(seconds)::bigint AS seconds, SUM(units)::bigint AS units \
     FROM ci_usage WHERE tenant_id = $1 AND ended_at >= $2 GROUP BY 1 ORDER BY 1";

/// One day of [`daily_usage`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DayUsage {
    /// The UTC day, `YYYY-MM-DD`.
    pub day: String,
    /// Billed seconds.
    pub seconds: u64,
    /// Billing units.
    pub units: u64,
}

/// What `tenant` used each UTC day since `since`, oldest first; days
/// with no usage are left out.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn daily_usage(db: &Db, tenant: &str, since: u64) -> Result<Vec<DayUsage>> {
    db.all(DAILY_USAGE, &params![tenant, since]).await
}

/// Sets the spend cap of tenant `$1` to `$2` micros at `$3`.
const SET_SPEND_CAP: &str = "INSERT INTO ci_spend_caps (tenant_id, cap_micros, updated_at) \
     VALUES ($1, $2, $3) \
     ON CONFLICT (tenant_id) DO UPDATE SET cap_micros = $2, updated_at = $3";

/// Usage Polar has not taken and has not refused, oldest first, at
/// most `$1`.
const UNSENT_USAGE: &str = "SELECT job_id, attempt, tenant_id, template, seconds, units, ended_at \
     FROM ci_usage WHERE sent_at IS NULL AND rejected_at IS NULL \
     ORDER BY ended_at, job_id, attempt LIMIT $1";

/// A tenant's billing state over its spend cap window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// The tenant.
    pub tenant_id: String,
    /// The organization's login, for messages.
    pub org_login: String,
    /// Whether the tenant is in the beta or has an active or trialing
    /// subscription.
    pub card: bool,
    /// Whether the tenant has an active or trialing subscription, which
    /// a beta tenant may lack.
    pub subscribed: bool,
    /// The spend cap in micros: the tenant's own or the default.
    pub cap_micros: u64,
    /// The spend cap window's start, in milliseconds: the licensed
    /// subscription's period start, or the UTC calendar month start.
    pub window_start: u64,
    /// The window's end, in milliseconds: the subscription's period
    /// end, or the next calendar month start.
    pub window_end: u64,
    /// Billed seconds metered over the window, per template label.
    pub seconds_by_template: Vec<(String, u64)>,
    /// Billing units metered over the window, as priced when each job
    /// completed.
    pub units: u64,
    /// Jobs launching, launched or running now.
    pub running: u32,
}

/// What [`STANDING`] returns; the seconds arrive as JSON text.
#[derive(Deserialize)]
struct StandingRow {
    tenant_id: String,
    org_login: String,
    card: bool,
    subscribed: bool,
    cap_micros: u64,
    window_start: u64,
    window_end: u64,
    seconds_by_template: String,
    units: u64,
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
            subscribed: self.subscribed,
            cap_micros: self.cap_micros,
            window_start: self.window_start,
            window_end: self.window_end,
            seconds_by_template: seconds
                .into_iter()
                .map(|s| (s.template, s.seconds))
                .collect(),
            units: self.units,
            running: self.running,
        })
    }
}

/// The billing state of the tenant of job `job_id` at `now`, with
/// `default_cap` for a tenant with no cap row.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn standing(
    db: &Db,
    job_id: u64,
    now: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_OF_JOB);
    db.first::<StandingRow>(
        &sql,
        &params![
            job_id,
            now,
            crate::billing::month_start(now),
            crate::billing::next_month_start(now),
            default_cap
        ],
    )
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
    now: u64,
    default_cap: u64,
) -> Result<Option<Standing>> {
    let sql = STANDING.replace("{owner}", OWNER_IS_TENANT);
    db.first::<StandingRow>(
        &sql,
        &params![
            tenant_id,
            now,
            crate::billing::month_start(now),
            crate::billing::next_month_start(now),
            default_cap
        ],
    )
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
