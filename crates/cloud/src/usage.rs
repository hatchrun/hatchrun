// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Metered billing: job minutes, free minutes and who may launch.
//!
//! A job that ran on a hatchrun runner counts GitHub's `started_at` to
//! `completed_at`, rounded up to whole minutes, once, when it completes
//! (see [`record`]). Every organization gets [`FREE_MINUTES`] each UTC
//! calendar month without a card. An unpaid tenant past them keeps
//! launching for [`GRACE_MS`], then launches nothing until a Polar
//! subscription starts or the next month does. A subscription pays per
//! minute: its jobs' minutes go to Polar's meter (see
//! [`crate::polar::ingest`]), where a meter credit benefit gives the
//! same free minutes each billing period and the price has a cap. Beta
//! tenants count minutes and pay nothing.

use serde::Deserialize;
use worker::{D1Database, Result};

use crate::int;

/// Free job minutes per organization and month. Polar's meter credit
/// benefit on the product grants the same number per billing period.
pub const FREE_MINUTES: u32 = 3000;
/// How long an unpaid tenant keeps launching after it used up its free
/// minutes: seven days.
pub const GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// SQL that is true for a `tenants` row with a Polar subscription that
/// holds a licence.
pub(crate) const PAID: &str = "EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing'))";

/// SQL for the start of the current UTC month, Unix ms.
const MONTH_START: &str = "(unixepoch('now', 'start of month') * 1000)";

/// SQL for a `tenants` row's job minutes this month.
pub(crate) const USED: &str = "(SELECT COALESCE(SUM(usage_events.minutes), 0) \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.ended_at >= (unixepoch('now', 'start of month') * 1000))";

/// SQL that is 1 for a `tenants` row that may launch: in the beta, paid,
/// within its free minutes this month, or within the grace period after
/// it used them up.
pub(crate) const MAY_LAUNCH: &str = "(tenants.beta = 1 \
     OR EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')) \
     OR (SELECT COALESCE(SUM(usage_events.minutes), 0) \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.ended_at >= (unixepoch('now', 'start of month') * 1000)) < 3000 \
     OR (tenants.free_used_up_at >= (unixepoch('now', 'start of month') * 1000) \
     AND tenants.free_used_up_at > unixepoch() * 1000 - 604800000))";

/// Billed minutes of a job that ran from `started` to `completed`
/// (Unix ms): whole minutes, rounded up, at least one.
#[must_use]
pub fn minutes(started: u64, completed: u64) -> u32 {
    let ms = completed.saturating_sub(started);
    u32::try_from(ms.div_ceil(60_000))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Records `?2` minutes of job `?1`, which completed at `?3`, if a
/// hatchrun runner took it and nothing recorded it before. The row is
/// billed when the tenant pays and is not in the beta.
fn insert_sql() -> String {
    format!(
        "INSERT INTO usage_events (job_id, tenant_id, minutes, ended_at, billed) \
         SELECT jobs.id, tenants.id, ?2, ?3, tenants.beta = 0 AND {PAID} \
         FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
         WHERE jobs.id = ?1 AND jobs.ran_on IS NOT NULL \
         ON CONFLICT (job_id) DO NOTHING"
    )
}

/// Sets `free_used_up_at` to `?2` on the tenant of job `?1` when it is
/// unpaid, not in the beta, has not used up its free minutes yet this
/// month and now has at least `?3` minutes.
fn used_up_sql() -> String {
    format!(
        "UPDATE tenants SET free_used_up_at = ?2 \
         WHERE id = (SELECT tenant_id FROM usage_events WHERE job_id = ?1) \
         AND beta = 0 AND NOT {PAID} \
         AND (free_used_up_at IS NULL OR free_used_up_at < {MONTH_START}) \
         AND {USED} >= ?3"
    )
}

/// Tenant `?1`'s minutes this month, when it used up its free ones this
/// month, and whether it may launch.
fn month_sql() -> String {
    format!(
        "SELECT {USED} AS minutes, \
         CASE WHEN free_used_up_at >= {MONTH_START} THEN free_used_up_at END \
         AS free_used_up_at, {MAY_LAUNCH} AS may_launch FROM tenants WHERE id = ?1"
    )
}

/// Sets `free_used_up_at` to `?2` on tenant `?1` when it is unpaid,
/// not in the beta, has not used up its free minutes yet this month and
/// has at least `?3` minutes: the start of its grace period when a
/// subscription ends after it went past them.
fn grace_sql() -> String {
    format!(
        "UPDATE tenants SET free_used_up_at = ?2 \
         WHERE id = ?1 AND beta = 0 AND NOT {PAID} \
         AND (free_used_up_at IS NULL OR free_used_up_at < {MONTH_START}) \
         AND {USED} >= ?3"
    )
}

/// Clears tenant `?1`'s `free_used_up_at`, so a subscription that ends
/// later starts a grace period of its own.
const CLEAR_GRACE: &str =
    "UPDATE tenants SET free_used_up_at = NULL WHERE id = ?1 AND free_used_up_at IS NOT NULL";

/// Up to `?1` billed minutes Polar has not taken, oldest first.
const UNSENT: &str = "SELECT job_id, tenant_id, minutes, ended_at FROM usage_events \
     WHERE billed = 1 AND sent_at IS NULL ORDER BY ended_at LIMIT ?1";

/// Marks the minutes of the jobs in the JSON array `?1` as taken by
/// Polar at `?2`.
const SENT: &str = "UPDATE usage_events SET sent_at = ?2 \
     WHERE job_id IN (SELECT value FROM json_each(?1))";

/// Records the minutes of job `id`, which ran from `started` to
/// `completed` (Unix ms), if a hatchrun runner took it and it was not
/// recorded before. An unpaid tenant that this job takes past its free
/// minutes gets `free_used_up_at`, which starts its grace period. Both
/// statements run in one D1 batch, which is a transaction. Returns
/// whether the job was recorded now.
pub(crate) async fn record(
    db: &D1Database,
    id: u64,
    started: u64,
    completed: u64,
    now: u64,
) -> Result<bool> {
    let results = db
        .batch(vec![
            db.prepare(insert_sql()).bind(&[
                int(id)?,
                minutes(started, completed).into(),
                int(completed)?,
            ])?,
            db.prepare(used_up_sql())
                .bind(&[int(id)?, int(now)?, FREE_MINUTES.into()])?,
        ])
        .await?;
    let changed = |at: usize| {
        results
            .get(at)
            .and_then(|result| result.meta().ok().flatten())
            .and_then(|meta| meta.changes)
            .unwrap_or(0)
    };
    if changed(1) == 1 {
        worker::console_log!("job {id}: its tenant used up its free minutes this month");
    }
    Ok(changed(0) == 1)
}

/// Starts the grace period of tenant `id` at `now` if it is unpaid and
/// past its free minutes this month without one, as a tenant whose
/// subscription just ended can be. Returns whether it started.
pub(crate) async fn start_grace(db: &D1Database, id: &str, now: u64) -> Result<bool> {
    let result = db
        .prepare(grace_sql())
        .bind(&[id.into(), int(now)?, FREE_MINUTES.into()])?
        .run()
        .await?;
    Ok(result.meta()?.and_then(|meta| meta.changes) == Some(1))
}

/// Forgets when tenant `id` used up its free minutes, as it does once a
/// subscription starts: the grace period it had before paying does not
/// carry over to when the subscription ends.
pub(crate) async fn clear_grace(db: &D1Database, id: &str) -> Result<()> {
    db.prepare(CLEAR_GRACE).bind(&[id.into()])?.run().await?;
    Ok(())
}

/// A tenant's minutes this month, as the app shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Month {
    /// Job minutes since the month started.
    pub minutes: u64,
    /// When it used up its free minutes this month, Unix ms.
    pub free_used_up_at: Option<u64>,
    /// Whether it may launch now (see [`MAY_LAUNCH`]).
    pub may_launch: bool,
}

#[derive(Debug, Deserialize)]
struct MonthRow {
    minutes: u64,
    free_used_up_at: Option<u64>,
    may_launch: u8,
}

/// The minutes of tenant `id` this month.
pub(crate) async fn month(db: &D1Database, id: &str) -> Result<Month> {
    let row: Option<MonthRow> = db
        .prepare(month_sql())
        .bind(&[id.into()])?
        .first(None)
        .await?;
    Ok(row.map_or(
        Month {
            minutes: 0,
            free_used_up_at: None,
            may_launch: false,
        },
        |row| Month {
            minutes: row.minutes,
            free_used_up_at: row.free_used_up_at,
            may_launch: row.may_launch == 1,
        },
    ))
}

/// A recorded job's minutes that Polar has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Unsent {
    /// The job.
    pub job_id: u64,
    /// The tenant, which is Polar's external customer id.
    pub tenant_id: String,
    /// Its minutes.
    pub minutes: u32,
    /// When it completed, Unix ms.
    pub ended_at: u64,
}

/// Up to `limit` billed events Polar has not taken, oldest first.
pub(crate) async fn unsent(db: &D1Database, limit: u32) -> Result<Vec<Unsent>> {
    db.prepare(UNSENT)
        .bind(&[limit.into()])?
        .all()
        .await?
        .results()
}

/// Marks the events of `jobs` as taken by Polar at `now`.
pub(crate) async fn sent(db: &D1Database, jobs: &[u64], now: u64) -> Result<()> {
    let ids =
        serde_json::to_string(jobs).map_err(|err| worker::Error::RustError(err.to_string()))?;
    db.prepare(SENT)
        .bind(&[ids.into(), int(now)?])?
        .run()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_jobs_up_to_whole_minutes() {
        assert_eq!(minutes(0, 0), 1);
        assert_eq!(minutes(0, 1), 1);
        assert_eq!(minutes(0, 60_000), 1);
        assert_eq!(minutes(0, 60_001), 2);
        assert_eq!(minutes(1_000, 601_000), 10);
        // A clock that runs backwards still bills the job once.
        assert_eq!(minutes(5_000, 1_000), 1);
    }

    #[test]
    fn the_launch_rule_matches_the_constants() {
        // MAY_LAUNCH spells out PAID, USED, FREE_MINUTES and GRACE_MS,
        // since a const &str cannot be formatted.
        assert!(MAY_LAUNCH.contains(PAID));
        assert!(MAY_LAUNCH.contains(&format!("{USED} < {FREE_MINUTES}")));
        assert!(MAY_LAUNCH.contains(&format!("unixepoch() * 1000 - {GRACE_MS}")));
        assert!(MAY_LAUNCH.contains(MONTH_START));
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

    /// Tenants `free`, `paid` (with an active subscription) and `beta`,
    /// on installations 1, 2 and 3.
    fn seed() -> Db {
        let db = Db::new();
        for (id, installation, beta) in [("free", 1u64, 0u32), ("paid", 2, 0), ("beta", 3, 1)] {
            db.run(
                "INSERT INTO tenants (id, org_id, org_login, installation_id, beta, \
                 created_at, updated_at) VALUES (?1, ?2, ?1, ?2, ?3, 1, 1)",
                &params![id, installation, beta],
            );
        }
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ('sub', 'paid', 'active', 'c', 'p', 0, 't', 1)",
            &[],
        );
        db
    }

    /// Records job `id` of `installation` that ran `mins` minutes and
    /// completed `ago` ms before now. Returns the rows each statement
    /// changed.
    fn record(db: &Db, id: u64, installation: u64, mins: u64, ago: u64, ran: bool) -> Vec<usize> {
        let runner = ran.then(|| format!("hatchrun-{id}-ab"));
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, ran_on) VALUES (?1, ?2, 'acme/app', 7, 'hatchrun', \
             'completed', 1, 1, ?3)",
            &params![id, installation, runner],
        );
        let completed = now() - ago;
        let used = minutes(completed - mins * 60_000, completed);
        db.batch(&[
            (insert_sql().as_str(), params![id, used, completed]),
            (used_up_sql().as_str(), params![id, now(), FREE_MINUTES]),
        ])
        .unwrap()
    }

    fn month(db: &Db, id: &str) -> MonthRow {
        db.first(&month_sql(), &params![id]).unwrap()
    }

    #[test]
    fn free_minutes_then_grace_then_stop() {
        let db = seed();
        let row = month(&db, "free");
        assert_eq!((row.minutes, row.may_launch), (0, 1));

        assert_eq!(record(&db, 1, 1, 2999, 0, true), [1, 0]);
        let row = month(&db, "free");
        assert_eq!(
            (row.minutes, row.free_used_up_at, row.may_launch),
            (2999, None, 1)
        );

        // The job past the free minutes starts the grace period, once.
        assert_eq!(record(&db, 2, 1, 5, 0, true), [1, 1]);
        assert_eq!(record(&db, 3, 1, 5, 0, true), [1, 0]);
        let row = month(&db, "free");
        assert_eq!(row.minutes, 3009);
        assert!(row.free_used_up_at.is_some());
        assert_eq!(row.may_launch, 1);

        let used_up = |ago: u64| {
            db.run(
                "UPDATE tenants SET free_used_up_at = ?1 WHERE id = 'free'",
                &params![now() - ago],
            );
            month(&db, "free").may_launch
        };
        assert_eq!(used_up(6 * DAY), 1);
        assert_eq!(used_up(8 * DAY), 0);
    }

    #[test]
    fn a_job_counts_once_and_only_on_a_runner() {
        let db = seed();
        assert_eq!(record(&db, 1, 1, 10, 0, true)[0], 1);
        let again = db.run(&insert_sql(), &params![1u64, 10u32, now()]);
        assert_eq!(again, 0);
        // Cancelled before a runner took it.
        assert_eq!(record(&db, 2, 1, 10, 0, false)[0], 0);
        assert_eq!(month(&db, "free").minutes, 10);
    }

    #[test]
    fn last_month_does_not_count() {
        let db = seed();
        // Past the free minutes and the grace, but before this month.
        record(&db, 1, 1, 4000, 40 * DAY, true);
        db.run(
            "UPDATE tenants SET free_used_up_at = ?1 WHERE id = 'free'",
            &params![now() - 40 * DAY],
        );
        let row = month(&db, "free");
        assert_eq!(
            (row.minutes, row.free_used_up_at, row.may_launch),
            (0, None, 1)
        );
    }

    #[test]
    fn paid_minutes_go_to_polar_and_beta_ones_do_not() {
        let db = seed();
        assert_eq!(record(&db, 1, 2, 5000, 0, true), [1, 0]);
        assert_eq!(record(&db, 2, 3, 5000, 0, true), [1, 0]);
        assert_eq!(month(&db, "paid").may_launch, 1);
        assert_eq!(month(&db, "beta").may_launch, 1);

        let unsent: Vec<Unsent> = db.all(UNSENT, &params![500u32]);
        assert_eq!(unsent.len(), 1);
        assert_eq!(
            (unsent[0].job_id, unsent[0].tenant_id.as_str()),
            (1, "paid")
        );
        assert_eq!(unsent[0].minutes, 5000);

        db.run(SENT, &params!["[1]", now()]);
        assert_eq!(db.all::<Unsent>(UNSENT, &params![500u32]), []);
    }

    #[test]
    fn grace_starts_when_a_subscription_ends() {
        let db = seed();
        // Past the free minutes while paid: no grace period yet.
        assert_eq!(record(&db, 1, 2, 5000, 0, true), [1, 0]);
        let grace = || db.run(&grace_sql(), &params!["paid", now(), FREE_MINUTES]);
        assert_eq!(grace(), 0);

        db.run("UPDATE subscriptions SET status = 'canceled'", &[]);
        assert_eq!(grace(), 1);
        assert_eq!(grace(), 0);
        let row = month(&db, "paid");
        assert!(row.free_used_up_at.is_some());
        assert_eq!(row.may_launch, 1);

        // Within its free minutes, nothing starts.
        assert_eq!(
            db.run(&grace_sql(), &params!["free", now(), FREE_MINUTES]),
            0
        );
    }

    #[test]
    fn a_subscription_that_ends_gets_a_fresh_grace_period() {
        let db = seed();
        // Used up the free minutes while unpaid, ten days ago.
        record(&db, 1, 1, 3500, 0, true);
        db.run(
            "UPDATE tenants SET free_used_up_at = ?1 WHERE id = 'free'",
            &params![now() - 10 * DAY],
        );
        assert_eq!(month(&db, "free").may_launch, 0);

        // Adds a card, which clears it, then the subscription ends.
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ('sub2', 'free', 'active', 'c', 'p', 0, 't', 1)",
            &[],
        );
        assert_eq!(db.run(CLEAR_GRACE, &params!["free"]), 1);
        db.run(
            "UPDATE subscriptions SET status = 'canceled' WHERE id = 'sub2'",
            &[],
        );
        assert_eq!(
            db.run(&grace_sql(), &params!["free", now(), FREE_MINUTES]),
            1
        );
        assert_eq!(month(&db, "free").may_launch, 1);
    }

    #[test]
    fn queries_use_the_indexes() {
        let db = seed();
        let plan = db.plan(&month_sql(), &params!["free"]);
        assert!(plan.contains("usage_events_month"), "{plan}");
        let plan = db.plan(UNSENT, &params![500u32]);
        assert!(plan.contains("usage_events_unsent"), "{plan}");
    }
}
