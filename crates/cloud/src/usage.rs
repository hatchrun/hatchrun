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
//! same free minutes each billing period. The price has no cap. Beta
//! tenants count minutes and pay nothing.

use serde::Deserialize;
use worker::Result;

use crate::db::{Db, params};

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
const MONTH_START: &str =
    "(extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000)";

/// SQL for a `tenants` row's job minutes this month.
pub(crate) const USED: &str = "(SELECT COALESCE(SUM(usage_events.minutes), 0)::bigint \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.ended_at >= \
     (extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000))";

/// SQL that is true for a `tenants` row that may launch: in the beta,
/// paid, within its free minutes this month, or within the grace period
/// after it used them up.
pub(crate) const MAY_LAUNCH: &str = "(tenants.beta \
     OR EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')) \
     OR (SELECT COALESCE(SUM(usage_events.minutes), 0)::bigint \
     FROM usage_events WHERE usage_events.tenant_id = tenants.id \
     AND usage_events.ended_at >= \
     (extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000)) < 3000 \
     OR (tenants.free_used_up_at >= \
     (extract(epoch from date_trunc('month', now() AT TIME ZONE 'UTC'))::bigint * 1000) \
     AND tenants.free_used_up_at > (floor(extract(epoch from now()))::bigint * 1000) - 604800000))";

/// Billed minutes of a job that ran from `started` to `completed`
/// (Unix ms): whole minutes, rounded up, at least one.
#[must_use]
pub fn minutes(started: u64, completed: u64) -> u32 {
    let ms = completed.saturating_sub(started);
    u32::try_from(ms.div_ceil(60_000))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Records `$2` minutes of job `$1`, which completed at `$3`, if a
/// hatchrun runner took it and nothing recorded it before. The row is
/// billed when the tenant pays and is not in the beta.
pub(crate) fn insert_sql() -> String {
    format!(
        "INSERT INTO usage_events (job_id, tenant_id, minutes, ended_at, billed) \
         SELECT jobs.id, tenants.id, $2::bigint, $3::bigint, NOT tenants.beta AND {PAID} \
         FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
         WHERE jobs.id = $1 AND jobs.ran_on IS NOT NULL \
         ON CONFLICT (job_id) DO NOTHING"
    )
}

/// Sets `free_used_up_at` to `$2` on the tenant of job `$1` when it is
/// unpaid, not in the beta, has not used up its free minutes yet this
/// month and now has at least `$3` minutes.
pub(crate) fn used_up_sql() -> String {
    format!(
        "UPDATE tenants SET free_used_up_at = $2 \
         WHERE id = (SELECT tenant_id FROM usage_events WHERE job_id = $1) \
         AND NOT beta AND NOT {PAID} \
         AND (free_used_up_at IS NULL OR free_used_up_at < {MONTH_START}) \
         AND {USED} >= $3"
    )
}

/// Tenant `$1`'s minutes this month, when it used up its free ones this
/// month, and whether it may launch.
pub(crate) fn month_sql() -> String {
    format!(
        "SELECT {USED} AS minutes, \
         CASE WHEN free_used_up_at >= {MONTH_START} THEN free_used_up_at END \
         AS free_used_up_at, {MAY_LAUNCH} AS may_launch FROM tenants WHERE id = $1"
    )
}

/// Sets `free_used_up_at` to `$2` on tenant `$1` when it is unpaid,
/// not in the beta, has not used up its free minutes yet this month and
/// has at least `$3` minutes: the start of its grace period when a
/// subscription ends after it went past them.
fn grace_sql() -> String {
    format!(
        "UPDATE tenants SET free_used_up_at = $2 \
         WHERE id = $1 AND NOT beta AND NOT {PAID} \
         AND (free_used_up_at IS NULL OR free_used_up_at < {MONTH_START}) \
         AND {USED} >= $3"
    )
}

/// Records up to `$2` jobs that a runner took and that completed at or
/// after `$1` with GitHub's times but have no minutes yet, as when
/// recording failed after the reconciler marked the job completed.
/// Returns each recorded row's tenant.
pub(crate) fn sweep_sql() -> String {
    format!(
        "INSERT INTO usage_events (job_id, tenant_id, minutes, ended_at, billed) \
         SELECT jobs.id, tenants.id, \
         GREATEST(1, (jobs.completed_at - jobs.started_at + 59999) / 60000), jobs.completed_at, \
         NOT tenants.beta AND {PAID} \
         FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
         WHERE jobs.ran_on IS NOT NULL AND jobs.completed_at >= $1 \
         AND jobs.started_at IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_events WHERE usage_events.job_id = jobs.id) \
         ORDER BY jobs.completed_at LIMIT $2 \
         ON CONFLICT (job_id) DO NOTHING RETURNING tenant_id"
    )
}

/// Clears tenant `$1`'s `free_used_up_at`, so a subscription that ends
/// later starts a grace period of its own.
const CLEAR_GRACE: &str =
    "UPDATE tenants SET free_used_up_at = NULL WHERE id = $1 AND free_used_up_at IS NOT NULL";

/// Up to `$1` billed minutes Polar has not taken, oldest first.
pub(crate) const UNSENT: &str = "SELECT job_id, tenant_id, minutes, ended_at FROM usage_events \
     WHERE billed AND sent_at IS NULL ORDER BY ended_at LIMIT $1";

/// Marks the minutes of the jobs in the JSON array `$1` as taken by
/// Polar at `$2`.
pub(crate) const SENT: &str = "UPDATE usage_events SET sent_at = $2 \
     WHERE job_id IN (SELECT json_array_elements_text($1::json)::bigint)";

/// Locks the tenant of job `$1` until the transaction ends.
pub(crate) const LOCK_TENANT: &str = "SELECT 1 FROM tenants WHERE installation_id = \
     (SELECT installation_id FROM jobs WHERE id = $1) FOR UPDATE";

/// Records the minutes of job `id`, which ran from `started` to
/// `completed` (Unix ms), if a hatchrun runner took it and it was not
/// recorded before. An unpaid tenant that this job takes past its free
/// minutes gets `free_used_up_at`, which starts its grace period. The
/// statements run in one transaction that first locks the job's tenant,
/// so two jobs of a tenant completing at once take turns and the second
/// counts the first's minutes. Returns whether the job was recorded now.
pub(crate) async fn record(
    db: &crate::db::Db,
    id: u64,
    started: u64,
    completed: u64,
    now: u64,
) -> Result<bool> {
    let insert = insert_sql();
    let used_up = used_up_sql();
    let results = db
        .batch(&[
            (LOCK_TENANT, params![id]),
            (
                insert.as_str(),
                params![id, minutes(started, completed), completed],
            ),
            (used_up.as_str(), params![id, now, FREE_MINUTES]),
        ])
        .await?;
    let changed = |at: usize| results.get(at).copied().unwrap_or(0);
    if changed(2) == 1 {
        worker::console_log!("job {id}: its tenant used up its free minutes this month");
    }
    Ok(changed(1) == 1)
}

/// Starts the grace period of tenant `id` at `now` if it is unpaid and
/// past its free minutes this month without one, as a tenant whose
/// subscription just ended can be. Returns whether it started.
pub(crate) async fn start_grace(db: &Db, id: &str, now: u64) -> Result<bool> {
    let changed = db
        .run(&grace_sql(), &params![id, now, FREE_MINUTES])
        .await?;
    Ok(changed == 1)
}

/// Records the minutes of up to `limit` jobs that completed at or after
/// `since` (Unix ms) and that no webhook or sync recorded, and starts
/// the grace period of any tenant they take past its free minutes.
/// Returns how many it recorded.
pub(crate) async fn sweep(db: &Db, since: u64, limit: u32, now: u64) -> Result<usize> {
    #[derive(Deserialize)]
    struct Recorded {
        tenant_id: String,
    }
    let recorded: Vec<Recorded> = db.all(&sweep_sql(), &params![since, limit]).await?;
    let mut tenants: Vec<&str> = recorded.iter().map(|row| row.tenant_id.as_str()).collect();
    tenants.sort_unstable();
    tenants.dedup();
    for tenant in tenants {
        start_grace(db, tenant, now).await?;
    }
    Ok(recorded.len())
}

/// Forgets when tenant `id` used up its free minutes, as it does once a
/// subscription starts: the grace period it had before paying does not
/// carry over to when the subscription ends.
pub(crate) async fn clear_grace(db: &Db, id: &str) -> Result<()> {
    db.run(CLEAR_GRACE, &params![id]).await?;
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
    may_launch: bool,
}

/// The minutes of tenant `id` this month.
pub(crate) async fn month(db: &Db, id: &str) -> Result<Month> {
    let row: Option<MonthRow> = db.first(&month_sql(), &params![id]).await?;
    Ok(row.map_or(
        Month {
            minutes: 0,
            free_used_up_at: None,
            may_launch: false,
        },
        |row| Month {
            minutes: row.minutes,
            free_used_up_at: row.free_used_up_at,
            may_launch: row.may_launch,
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
        assert!(MAY_LAUNCH.contains(&format!(
            "(floor(extract(epoch from now()))::bigint * 1000) - {GRACE_MS}"
        )));
        assert!(MAY_LAUNCH.contains(MONTH_START));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod sql {
    use super::*;
    use crate::db::Param;
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
        for (id, installation, beta) in
            [("free", 1u64, false), ("paid", 2, false), ("beta", 3, true)]
        {
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

    /// [`super::record`]'s statements for job `id` that ran `mins`
    /// minutes and completed `ago` ms before now.
    fn statements(id: u64, mins: u64, ago: u64) -> Vec<(String, Vec<Param>)> {
        let completed = now() - ago;
        let used = minutes(completed - mins * 60_000, completed);
        vec![
            (LOCK_TENANT.to_owned(), params![id]),
            (insert_sql(), params![id, used, completed]),
            (used_up_sql(), params![id, now(), FREE_MINUTES]),
        ]
    }

    /// Runs `statements` as [`super::record`] does on `db`. Returns the
    /// rows the insert and the update changed.
    async fn send(db: &crate::db::Db, statements: Vec<(String, Vec<Param>)>) -> Vec<usize> {
        let statements: Vec<(&str, Vec<Param>)> = statements
            .iter()
            .map(|(sql, params)| (sql.as_str(), params.clone()))
            .collect();
        let changes = db.batch(&statements).await.unwrap();
        changes[1..]
            .iter()
            .map(|n| usize::try_from(*n).unwrap())
            .collect()
    }

    /// Records job `id` of `installation` that ran `mins` minutes and
    /// completed `ago` ms before now. Returns the rows the insert and
    /// the update changed.
    fn record(db: &Db, id: u64, installation: u64, mins: u64, ago: u64, ran: bool) -> Vec<usize> {
        job(db, id, installation, ran);
        db.wait(send(db.db(), statements(id, mins, ago)))
    }

    #[test]
    fn jobs_completing_together_start_the_grace_period() {
        let db = seed();
        assert_eq!(record(&db, 1, 1, 2995, 0, true), [1, 0]);
        job(&db, 2, 1, true);
        job(&db, 3, 1, true);
        let other = db.connect(&crate::testdb::url().parse().unwrap());
        // Each alone stays under the free minutes; both together pass
        // them. Sent at once on two connections, the second waits for
        // the first's lock on the tenant and counts its minutes.
        let (first, second) = db.wait(async {
            futures_util::join!(
                send(db.db(), statements(2, 3, 0)),
                send(&other, statements(3, 3, 0))
            )
        });
        assert_eq!(first[0] + second[0], 2);
        assert_eq!(first[1] + second[1], 1, "{first:?} {second:?}");
        let row = month(&db, "free");
        assert_eq!(row.minutes, 3001);
        assert!(row.free_used_up_at.is_some());
        assert!(row.may_launch);
    }

    fn month(db: &Db, id: &str) -> MonthRow {
        db.first(&month_sql(), &params![id]).unwrap()
    }

    #[test]
    fn free_minutes_then_grace_then_stop() {
        let db = seed();
        let row = month(&db, "free");
        assert_eq!((row.minutes, row.may_launch), (0, true));

        assert_eq!(record(&db, 1, 1, 2999, 0, true), [1, 0]);
        let row = month(&db, "free");
        assert_eq!(
            (row.minutes, row.free_used_up_at, row.may_launch),
            (2999, None, true)
        );

        // The job past the free minutes starts the grace period, once.
        assert_eq!(record(&db, 2, 1, 5, 0, true), [1, 1]);
        assert_eq!(record(&db, 3, 1, 5, 0, true), [1, 0]);
        let row = month(&db, "free");
        assert_eq!(row.minutes, 3009);
        assert!(row.free_used_up_at.is_some());
        assert!(row.may_launch);

        let used_up = |ago: u64| {
            db.run(
                "UPDATE tenants SET free_used_up_at = $1 WHERE id = 'free'",
                &params![now() - ago],
            );
            month(&db, "free").may_launch
        };
        assert!(used_up(6 * DAY));
        assert!(!used_up(8 * DAY));
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
            "UPDATE tenants SET free_used_up_at = $1 WHERE id = 'free'",
            &params![now() - 40 * DAY],
        );
        let row = month(&db, "free");
        assert_eq!(
            (row.minutes, row.free_used_up_at, row.may_launch),
            (0, None, true)
        );
    }

    #[test]
    fn paid_minutes_go_to_polar_and_beta_ones_do_not() {
        let db = seed();
        assert_eq!(record(&db, 1, 2, 5000, 0, true), [1, 0]);
        assert_eq!(record(&db, 2, 3, 5000, 0, true), [1, 0]);
        assert!(month(&db, "paid").may_launch);
        assert!(month(&db, "beta").may_launch);

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

        db.run(
            "UPDATE subscriptions SET status = 'canceled' WHERE id = 'sub'",
            &[],
        );
        assert_eq!(grace(), 1);
        assert_eq!(grace(), 0);
        let row = month(&db, "paid");
        assert!(row.free_used_up_at.is_some());
        assert!(row.may_launch);

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
            "UPDATE tenants SET free_used_up_at = $1 WHERE id = 'free'",
            &params![now() - 10 * DAY],
        );
        assert!(!month(&db, "free").may_launch);

        // Adds a card, which clears it, then the subscription ends.
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ('sub2', 'free', 'active', 'c', 'p', false, 't', 1)",
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
        assert!(month(&db, "free").may_launch);
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
        db.run(&insert_sql(), &params![5u64, 1u32, t]);

        let swept: Vec<serde_json::Value> = db.all(&sweep_sql(), &params![t - DAY, 100u32]);
        assert_eq!(swept, [serde_json::json!({ "tenant_id": "free" })]);
        let rows: Vec<serde_json::Value> = db.all(
            "SELECT job_id, minutes, billed FROM usage_events ORDER BY job_id",
            &[],
        );
        assert_eq!(
            rows,
            [
                serde_json::json!({ "job_id": 1, "minutes": 11, "billed": false }),
                serde_json::json!({ "job_id": 5, "minutes": 1, "billed": false }),
            ]
        );
        let again: Vec<serde_json::Value> = db.all(&sweep_sql(), &params![t - DAY, 100u32]);
        assert_eq!(again, Vec::<serde_json::Value>::new());
        let plan = db.plan(&sweep_sql(), &params![t - DAY, 100u32]);
        assert!(plan.contains("jobs_completed"), "{plan}");
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
