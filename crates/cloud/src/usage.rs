// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Metered billing: runner seconds and who may launch.
//!
//! Runner time is billed per second, at least a minute a job. A job
//! that ran on a hatchrun runner counts GitHub's `started_at` to
//! `completed_at`, rounded up to whole seconds, once, when it completes
//! (see [`record`]). There is no free tier: a tenant in the beta or
//! with a Polar subscription launches, any other tenant launches
//! nothing. A subscription's jobs' seconds go to Polar's meter (see
//! [`crate::polar::ingest`]). Beta tenants have their seconds counted
//! and not billed.

use serde::Deserialize;
use worker::Result;

use crate::db::{Db, params};

/// The least a job is billed: one minute.
pub const MIN_BILLED_SECONDS: u64 = 60;
/// The runner price, $0.001 a minute, billed per second, in millionths
/// of a dollar. Polar's price on the "Runner seconds" meter is this
/// divided by 60.
pub const PRICE_MICROS_PER_MINUTE: u64 = 1_000;

/// SQL that is true for a `tenants` row with a Polar subscription that
/// holds a licence.
pub(crate) const PAID: &str = "EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing'))";

/// SQL for a `tenants` row's runner seconds this month.
pub(crate) const USED: &str = "(SELECT COALESCE(SUM(usage_events.seconds), 0)::bigint \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.ended_at >= \
     (extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000))";

/// [`USED`] for the seconds Polar bills only.
pub(crate) const BILLED: &str = "(SELECT COALESCE(SUM(usage_events.seconds), 0)::bigint \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.billed AND usage_events.ended_at >= \
     (extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000))";

/// In the beta or paid. Spelled out, a const &str cannot be formatted.
pub(crate) const MAY_LAUNCH: &str = "(tenants.beta OR EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')))";

/// Billed seconds of a job that ran from `started` to `completed` (Unix
/// ms): whole seconds, rounded up, at least [`MIN_BILLED_SECONDS`].
#[must_use]
pub fn job_seconds(started: u64, completed: u64) -> u64 {
    completed
        .saturating_sub(started)
        .div_ceil(1000)
        .max(MIN_BILLED_SECONDS)
}

/// What `seconds` of runner time cost, in millionths of a dollar,
/// rounded down once.
#[must_use]
pub fn spend_micros(seconds: u64) -> u64 {
    seconds.saturating_mul(PRICE_MICROS_PER_MINUTE) / 60
}

/// Records `$2` seconds of job `$1`, which completed at `$3`, if a
/// hatchrun runner took it and nothing recorded it before. The row is
/// billed when the tenant pays and is not in the beta.
pub(crate) fn insert_sql() -> String {
    format!(
        "INSERT INTO usage_events (job_id, tenant_id, seconds, ended_at, billed) \
         SELECT jobs.id, tenants.id, $2::bigint, $3::bigint, NOT tenants.beta AND {PAID} \
         FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
         WHERE jobs.id = $1 AND jobs.ran_on IS NOT NULL \
         ON CONFLICT (job_id) DO NOTHING"
    )
}

/// Tenant `$1`'s seconds this month, the billed part of them, and
/// whether it may launch.
pub(crate) fn month_sql() -> String {
    format!(
        "SELECT {USED} AS seconds, {BILLED} AS billed_seconds, \
         {MAY_LAUNCH} AS may_launch FROM tenants WHERE id = $1"
    )
}

/// Records up to `$2` jobs that a runner took and that completed at or
/// after `$1` with GitHub's times but have no seconds yet, as when
/// recording failed after the reconciler marked the job completed.
pub(crate) fn sweep_sql() -> String {
    format!(
        "INSERT INTO usage_events (job_id, tenant_id, seconds, ended_at, billed) \
         SELECT jobs.id, tenants.id, \
         GREATEST(60, (jobs.completed_at - jobs.started_at + 999) / 1000), jobs.completed_at, \
         NOT tenants.beta AND {PAID} \
         FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
         WHERE jobs.ran_on IS NOT NULL AND jobs.completed_at >= $1 \
         AND jobs.started_at IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_events WHERE usage_events.job_id = jobs.id) \
         ORDER BY jobs.completed_at LIMIT $2 \
         ON CONFLICT (job_id) DO NOTHING"
    )
}

/// Up to `$1` billed seconds Polar has not taken, oldest first.
pub(crate) const UNSENT: &str = "SELECT job_id, tenant_id, seconds, ended_at FROM usage_events \
     WHERE billed AND sent_at IS NULL ORDER BY ended_at LIMIT $1";

/// Marks the seconds of the jobs in the JSON array `$1` as taken by
/// Polar at `$2`.
pub(crate) const SENT: &str = "UPDATE usage_events SET sent_at = $2 \
     WHERE job_id IN (SELECT json_array_elements_text($1::json)::bigint)";

/// Records the billed seconds of job `id`, which ran from `started` to
/// `completed` (Unix ms), if a hatchrun runner took it and it was not
/// recorded before. Returns whether the job was recorded now.
pub(crate) async fn record(db: &Db, id: u64, started: u64, completed: u64) -> Result<bool> {
    let changed = db
        .run(
            &insert_sql(),
            &params![id, job_seconds(started, completed), completed],
        )
        .await?;
    Ok(changed == 1)
}

/// Records the seconds of up to `limit` jobs that completed at or after
/// `since` (Unix ms) and that no webhook or sync recorded. Returns how
/// many it recorded.
pub(crate) async fn sweep(db: &Db, since: u64, limit: u32) -> Result<usize> {
    let changed = db.run(&sweep_sql(), &params![since, limit]).await?;
    Ok(usize::try_from(changed).unwrap_or(usize::MAX))
}

/// A tenant's runner seconds this month, as the app shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Month {
    /// Runner seconds since the month started.
    pub seconds: u64,
    /// The part of them that Polar bills.
    pub billed_seconds: u64,
    /// Whether it may launch now (see [`MAY_LAUNCH`]).
    pub may_launch: bool,
}

/// The seconds of tenant `id` this month.
pub(crate) async fn month(db: &Db, id: &str) -> Result<Month> {
    let row: Option<Month> = db.first(&month_sql(), &params![id]).await?;
    Ok(row.unwrap_or(Month {
        seconds: 0,
        billed_seconds: 0,
        may_launch: false,
    }))
}

/// A recorded job's seconds that Polar has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Unsent {
    /// The job.
    pub job_id: u64,
    /// The tenant, which is Polar's external customer id.
    pub tenant_id: String,
    /// Its billed seconds.
    pub seconds: u64,
    /// When it completed, Unix ms.
    pub ended_at: u64,
}

/// Up to `limit` billed events Polar has not taken, oldest first.
pub(crate) async fn unsent(db: &Db, limit: u32) -> Result<Vec<Unsent>> {
    db.all(UNSENT, &params![limit]).await
}

/// Marks the events of `jobs` as taken by Polar at `now`.
pub(crate) async fn sent(db: &Db, jobs: &[u64], now: u64) -> Result<()> {
    let ids =
        serde_json::to_string(jobs).map_err(|err| worker::Error::RustError(err.to_string()))?;
    db.run(SENT, &params![ids, now]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_seconds_bill_at_least_a_minute() {
        assert_eq!(job_seconds(0, 0), 60);
        assert_eq!(job_seconds(0, 10_000), 60);
        assert_eq!(job_seconds(0, 60_000), 60);
        assert_eq!(job_seconds(0, 60_001), 61);
        assert_eq!(job_seconds(1_000, 61_500), 61);
        assert_eq!(job_seconds(0, 600_000), 600);
        // A clock that runs backwards still bills the job once.
        assert_eq!(job_seconds(5_000, 1_000), 60);
    }

    #[test]
    fn spend_is_per_second_and_rounds_down_once() {
        assert_eq!(spend_micros(0), 0);
        assert_eq!(spend_micros(60), 1_000);
        assert_eq!(spend_micros(61), 1_016);
        assert_eq!(spend_micros(3_600), 60_000);
    }

    #[test]
    fn the_launch_rule_matches_the_constants() {
        // MAY_LAUNCH spells out PAID, since a const &str cannot be
        // formatted.
        assert_eq!(MAY_LAUNCH, format!("(tenants.beta OR {PAID})"));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod sql {
    use super::*;
    use crate::testdb::{Db, params};

    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn now() -> u64 {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    }

    /// Tenants `unpaid`, `paid` (with an active subscription) and
    /// `beta`, on installations 1, 2 and 3.
    fn seed() -> Db {
        let db = Db::new();
        for (id, installation, beta) in [
            ("unpaid", 1u64, false),
            ("paid", 2, false),
            ("beta", 3, true),
        ] {
            db.run(
                "INSERT INTO tenants (id, org_id, org_login, installation_id, beta, \
                 created_at, updated_at) VALUES ($1, $2, $1, $2, $3, 1, 1)",
                &params![id, installation, beta],
            );
        }
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ('sub', 'paid', 'active', 'c', 'p', false, 't', 1)",
            &[],
        );
        db
    }

    /// A completed job `id` of `installation`.
    fn job(db: &Db, id: u64, installation: u64, ran: bool) {
        let runner = ran.then(|| format!("hatchrun-{id}-ab"));
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, ran_on) VALUES ($1, $2, 'acme/app', 7, 'hatchrun', \
             'completed', 1, 1, $3)",
            &params![id, installation, runner],
        );
    }

    /// Records job `id` of `installation` that billed `secs` seconds and
    /// completed `ago` ms before now. Returns the rows the insert
    /// changed.
    fn record(db: &Db, id: u64, installation: u64, secs: u64, ago: u64, ran: bool) -> usize {
        job(db, id, installation, ran);
        db.run(&insert_sql(), &params![id, secs, now() - ago])
    }

    fn month(db: &Db, id: &str) -> Month {
        db.first(&month_sql(), &params![id]).unwrap()
    }

    #[test]
    fn only_beta_and_paid_tenants_launch() {
        let db = seed();
        assert!(!month(&db, "unpaid").may_launch);
        assert!(month(&db, "paid").may_launch);
        assert!(month(&db, "beta").may_launch);
        // Past any amount of usage, the rule stays the same.
        record(&db, 1, 1, 1_000_000, 0, true);
        assert!(!month(&db, "unpaid").may_launch);

        db.run(
            "UPDATE subscriptions SET status = 'canceled' WHERE id = 'sub'",
            &[],
        );
        assert!(!month(&db, "paid").may_launch);
        db.run(
            "UPDATE subscriptions SET status = 'trialing' WHERE id = 'sub'",
            &[],
        );
        assert!(month(&db, "paid").may_launch);
    }

    #[test]
    fn a_job_counts_once_and_only_on_a_runner() {
        let db = seed();
        assert_eq!(record(&db, 1, 1, 600, 0, true), 1);
        let again = db.run(&insert_sql(), &params![1u64, 600u64, now()]);
        assert_eq!(again, 0);
        // Cancelled before a runner took it.
        assert_eq!(record(&db, 2, 1, 600, 0, false), 0);
        assert_eq!(month(&db, "unpaid").seconds, 600);
    }

    #[test]
    fn last_month_does_not_count() {
        let db = seed();
        record(&db, 1, 1, 240_000, 40 * DAY, true);
        assert_eq!(month(&db, "unpaid").seconds, 0);
    }

    #[test]
    fn paid_seconds_go_to_polar_and_beta_ones_do_not() {
        let db = seed();
        assert_eq!(record(&db, 1, 2, 90, 0, true), 1);
        assert_eq!(record(&db, 2, 3, 300, 0, true), 1);

        let unsent: Vec<Unsent> = db.all(UNSENT, &params![500u32]);
        assert_eq!(unsent.len(), 1);
        assert_eq!(
            (
                unsent[0].job_id,
                unsent[0].tenant_id.as_str(),
                unsent[0].seconds
            ),
            (1, "paid", 90)
        );

        db.run(SENT, &params!["[1]", now()]);
        assert_eq!(db.all::<Unsent>(UNSENT, &params![500u32]), []);
    }

    #[test]
    fn the_month_spends_billed_seconds_only() {
        let db = seed();
        record(&db, 1, 2, 36_000, 0, true);
        record(&db, 2, 3, 600, 0, true);
        // The subscription ends: this job is counted and not billed.
        db.run(
            "UPDATE subscriptions SET status = 'canceled' WHERE id = 'sub'",
            &[],
        );
        record(&db, 3, 2, 61, 0, true);

        let paid = month(&db, "paid");
        assert_eq!((paid.seconds, paid.billed_seconds), (36_061, 36_000));
        let beta = month(&db, "beta");
        assert_eq!((beta.seconds, beta.billed_seconds), (600, 0));
    }

    #[test]
    fn the_sweep_records_what_no_webhook_did() {
        let db = seed();
        let job = |id: u64, ran: bool, started: Option<u64>, completed: u64| {
            db.run(
                "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
                 queued_at, updated_at, ran_on, started_at, completed_at) \
                 VALUES ($1, 1, 'acme/app', 7, 'hatchrun', 'completed', 1, 1, $2, $3, $4)",
                &params![id, ran.then_some("hatchrun-x"), started, completed],
            );
        };
        let t = now();
        job(1, true, Some(t - 600_001), t);
        job(2, false, Some(t - 60_000), t);
        job(3, true, None, t);
        job(4, true, Some(t - 2 * DAY - 60_000), t - 2 * DAY);
        job(5, true, Some(t - 30_000), t);
        // Already recorded by its webhook.
        db.run(&insert_sql(), &params![5u64, 60u64, t]);

        assert_eq!(db.run(&sweep_sql(), &params![t - DAY, 100u32]), 1);
        let rows: Vec<serde_json::Value> = db.all(
            "SELECT job_id, seconds, billed FROM usage_events ORDER BY job_id",
            &[],
        );
        assert_eq!(
            rows,
            [
                serde_json::json!({ "job_id": 1, "seconds": 601, "billed": false }),
                serde_json::json!({ "job_id": 5, "seconds": 60, "billed": false }),
            ]
        );
        assert_eq!(db.run(&sweep_sql(), &params![t - DAY, 100u32]), 0);
        let plan = db.plan(&sweep_sql(), &params![t - DAY, 100u32]);
        assert!(plan.contains("jobs_completed"), "{plan}");
    }

    #[test]
    fn queries_use_the_indexes() {
        let db = seed();
        let plan = db.plan(&month_sql(), &params!["unpaid"]);
        assert!(plan.contains("usage_events_month"), "{plan}");
        let plan = db.plan(UNSENT, &params![500u32]);
        assert!(plan.contains("usage_events_unsent"), "{plan}");
    }
}
