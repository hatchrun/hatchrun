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
/// period start. When now is at or past its period end (the renewal
/// webhook is late), the window moves forward in steps of one period
/// length until it holds now. A tenant with no such subscription keeps
/// the calendar month, which ends at the next calendar month start.
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
         THEN one.start_ms + (($2 - one.start_ms) / one.len) * one.len \
         ELSE one.start_ms END AS start_ms, \
         CASE WHEN one.rolled \
         THEN one.start_ms + (($2 - one.start_ms) / one.len + 1) * one.len \
         ELSE one.end_ms END AS end_ms \
         FROM (SELECT raw.start_ms, raw.end_ms, \
           GREATEST(raw.end_ms - raw.start_ms, 1) AS len, \
           raw.end_ms IS NOT NULL AND $2 >= raw.end_ms AS rolled FROM (SELECT \
         (EXTRACT(epoch FROM s.current_period_start::timestamptz) * 1000)::bigint \
           AS start_ms, \
         CASE WHEN s.current_period_end IS NULL THEN NULL \
           ELSE (EXTRACT(epoch FROM s.current_period_end::timestamptz) * 1000)::bigint \
           END AS end_ms \
         FROM subscriptions s WHERE s.tenant_id = tenants.id \
         AND s.status IN ('active', 'trialing') \
         ORDER BY s.modified_at DESC LIMIT 1) AS raw) AS one) AS period ON true \
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
const UNSENT_WINDOWS: &str = "SELECT id, tenant_id, window_start, window_end, units, seconds, jobs \
     FROM ci_usage_windows WHERE sent_at IS NULL AND rejected_at IS NULL \
     ORDER BY window_start, id LIMIT $1";

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

/// One window's usage that Polar has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UnsentWindow {
    /// The window's id.
    pub id: i64,
    /// The tenant, Polar's external customer id.
    pub tenant_id: String,
    /// The window's start, in milliseconds.
    pub window_start: u64,
    /// The window's end, in milliseconds.
    pub window_end: u64,
    /// The summed billing units of its jobs.
    pub units: u64,
    /// The summed billed seconds of its jobs.
    pub seconds: u64,
    /// How many jobs the window holds.
    pub jobs: i32,
}

/// The oldest `limit` usage windows Polar has not taken.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn unsent_windows(db: &Db, limit: u32) -> Result<Vec<UnsentWindow>> {
    db.all(UNSENT_WINDOWS, &params![limit]).await
}

/// Closes usage windows at `now`: without `tenant` every tenant's
/// ended windows, with it that tenant's open window at once (see
/// [`hatchrun_cloud::usage::close_windows`]). Returns how many usage
/// rows were assigned a window.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn close_windows(db: &Db, now: u64, tenant: Option<&str>) -> Result<u64> {
    hatchrun_cloud::usage::close_windows(db, now, tenant).await
}

/// Marks the windows `ids` as taken by Polar at `now`, with the usage
/// rows closed into them. A row already marked keeps its time.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn windows_sent(db: &Db, ids: &[i64], now: u64) -> Result<()> {
    mark_windows("sent_at = $1", &[], ids, now, db).await
}

/// Quarantines the windows `ids`: Polar refused them for good, so they
/// are never sent again, nor are the usage rows closed into them.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn windows_rejected(db: &Db, ids: &[i64], reason: &str, now: u64) -> Result<()> {
    mark_windows(
        "rejected_at = $1, reject_reason = $2",
        &[Param::from(reason)],
        ids,
        now,
        db,
    )
    .await
}

/// Sets `assignments` on the open windows `ids` and the usage rows
/// closed into them. `$1` is `now` and `extra` binds `$2` on.
async fn mark_windows(
    assignments: &str,
    extra: &[Param],
    ids: &[i64],
    now: u64,
    db: &Db,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let first = extra.len() + 2;
    let windows: Vec<String> = (0..ids.len()).map(|n| format!("${}", first + n)).collect();
    let mut bound: Vec<Param> = vec![Param::from(now)];
    bound.extend(extra.iter().cloned());
    for id in ids {
        bound.push(Param::from(*id));
    }
    let list = windows.join(", ");
    let statements = [
        (
            format!(
                "UPDATE ci_usage_windows SET {assignments} WHERE sent_at IS NULL \
                 AND rejected_at IS NULL AND id IN ({list})"
            ),
            bound.clone(),
        ),
        (
            format!(
                "UPDATE ci_usage SET {assignments} WHERE sent_at IS NULL \
                 AND rejected_at IS NULL AND window_id IN ({list})"
            ),
            bound,
        ),
    ];
    let statements: Vec<(&str, Vec<Param>)> = statements
        .iter()
        .map(|(sql, params)| (sql.as_str(), params.clone()))
        .collect();
    db.batch(&statements).await.map(|_| ())
}
