// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Wakes the queued CI jobs a tenant's caps hold (`ci_jobs.held`).
//!
//! A held job waits with no queue traffic until its event fires. The
//! Workers that learn of the event pick the jobs here and send each one
//! a launch message on the `hatchrun-ci-launches` queue (the
//! `CI_LAUNCHES` binding), where the consumer checks the caps again:
//! the Polar webhook when a CI subscription becomes active, the CI
//! Worker when a job ends or the owner raises the cap, and the CI
//! reconciler's slow backstop.
//!
//! A wake marks the rows it picks (`next_launch_at` is set, the hold
//! stays) and returns them, so two wakers never pick the same job: the
//! mark is what tells a picked job from one still waiting. A marked job
//! whose message was lost is requeued by the stalled scan (see
//! [`hatchrun_ci::reconcile`]).

use serde::{Deserialize, Serialize};

use crate::db::{Db, params};

/// The launch queue's producer binding.
pub const QUEUE: &str = "CI_LAUNCHES";
/// How many held jobs one wake sends: the concurrency cap of the CI
/// Worker (`hatchrun_workflow::MAX_RUNNING_JOBS`), since more could not
/// start at once.
pub const WAKE_LIMIT: u32 = 5;

/// A held job to send to the launch queue. The fields are the CI
/// Worker's launch message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Woken {
    /// The job.
    pub job_id: u64,
    /// The attempt it is queued for.
    pub attempt: u32,
}

/// Marks the oldest `limit` queued jobs of tenant `$1` held for reason
/// `$2` that no wake picked yet, and returns them. The mark keeps the
/// hold: the consumer checks the caps again.
const HELD: &str = "UPDATE ci_jobs SET next_launch_at = $4 WHERE id IN (\
      SELECT id FROM ci_jobs \
      WHERE tenant_id = $1 AND status = 'queued' AND held = $2 AND next_launch_at IS NULL \
      ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED) \
    AND status = 'queued' AND held = $2 AND next_launch_at IS NULL \
    RETURNING id AS job_id, attempt";

/// Turns the card and spend-cap holds of tenant `$1` in buckets `$2`
/// to `$3` into concurrency holds, which a finishing job wakes one by
/// one in order. Rows keep their `next_launch_at`.
const RELEASE_TENANT: &str = "UPDATE ci_jobs SET held = 'concurrency' \
      WHERE tenant_id = $1 AND status = 'queued' AND held IN ('card', 'spend_cap') \
      AND bucket >= $2 AND bucket < $3";

/// Frees up to `$3` approval holds nobody looked at since `$1` whose
/// environment no longer exists or no longer asks for approval, or whose
/// attempt has a decision already (an approval that raced the hold), in
/// buckets `$4` to `$5`, due at `$2`, and returns them. A row freed
/// by an overlapping pass meanwhile is skipped. The consumer checks
/// such a job again: it runs, or fails when the environment is gone.
const RELEASE_ORPHAN_APPROVALS: &str = "UPDATE ci_jobs SET held = NULL, next_launch_at = $2, \
        updated_at = $2 WHERE id IN (\
        SELECT ci_jobs.id FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
        WHERE ci_jobs.status = 'queued' \
        AND ci_jobs.held = 'approval' AND ci_jobs.updated_at < $1 \
        AND ci_jobs.bucket >= $4 AND ci_jobs.bucket < $5 \
        AND ((ci_jobs.approval_attempt = ci_jobs.attempt AND ci_jobs.approval_decision IS NOT NULL) \
          OR NOT EXISTS (SELECT 1 FROM ci_environments e WHERE e.repo_id = ci_runs.repo_id \
            AND e.tenant_id = ci_runs.tenant_id AND lower(e.name) = lower(ci_jobs.environment) AND e.approval)) \
        ORDER BY ci_jobs.id LIMIT $3) \
      AND held = 'approval' \
      RETURNING ci_jobs.id AS job_id, ci_jobs.attempt";

/// Frees up to `$3` fork approval holds nobody looked at since `$1`
/// whose run no longer waits (a decision whose wake was lost), in
/// buckets `$4` to `$5`, due at `$2`, and returns them. A row freed
/// by an overlapping pass meanwhile is skipped. The consumer checks
/// such a job again: it runs, or is left to its run's object when the
/// run was cancelled.
const RELEASE_ORPHAN_FORK_APPROVALS: &str = "UPDATE ci_jobs SET held = NULL, \
        next_launch_at = $2, updated_at = $2 WHERE id IN (\
        SELECT ci_jobs.id FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
        WHERE ci_jobs.status = 'queued' AND ci_jobs.held = 'fork_approval' \
        AND ci_jobs.updated_at < $1 AND ci_runs.gate_status NOT IN ('pending', 'deferred') \
        AND ci_jobs.bucket >= $4 AND ci_jobs.bucket < $5 \
        ORDER BY ci_jobs.id LIMIT $3) \
      AND held = 'fork_approval' \
      RETURNING ci_jobs.id AS job_id, ci_jobs.attempt";

/// How many buckets repositories hash into (see `ci_shard_bucket` in
/// migration 0032). A wake that reads no range reads every bucket.
pub const BUCKETS: u16 = 4096;

/// The bucket bounds of `range`: every bucket when it is `None`, as
/// the event paths read.
fn bounds(range: Option<(u16, u16)>) -> (u16, u16) {
    range.unwrap_or((0, BUCKETS))
}

/// The oldest queued jobs held for `reason` of tenant `tenant` that no
/// wake picked yet, marked and returned.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn held(
    db: &Db,
    tenant: &str,
    reason: &str,
    limit: u32,
    now: u64,
) -> worker::Result<Vec<Woken>> {
    db.all(HELD, &params![tenant, reason, limit, now]).await
}

/// The jobs to wake when the standing of `tenant` improved (the card is
/// back or the cap is higher): its remaining card and spend-cap holds
/// in `range` become concurrency holds, so the end of each job wakes
/// the next in order, and the oldest of them the tenant has free places
/// for, at most [`WAKE_LIMIT`], are marked and returned to wake now.
/// `None` reads every bucket, as the event paths do.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn standing_improved(
    db: &Db,
    tenant: &str,
    now: u64,
    range: Option<(u16, u16)>,
) -> worker::Result<Vec<Woken>> {
    let (lo, hi) = bounds(range);
    db.run(RELEASE_TENANT, &params![tenant, lo, hi]).await?;
    // Only as many as the tenant has places for: a tenant that still
    // runs its most jobs wakes nothing, and each end wakes the next.
    lost_wakes(db, Some(tenant), WAKE_LIMIT, now, WAKE_LIMIT, range).await
}

/// The jobs to wake when tenant `tenant`'s card is back.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn card_returned(db: &Db, tenant: &str, now: u64) -> worker::Result<Vec<Woken>> {
    standing_improved(db, tenant, now, None).await
}

/// A tenant with queued card or spend-cap holds, and the reasons.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BillingHolds {
    /// The tenant.
    pub tenant_id: String,
    /// The billing reasons its queued jobs are held for, comma
    /// separated: `card`, `spend_cap` or both.
    pub reasons: String,
}

/// The tenants whose card or spend-cap holds something may have freed
/// since the oldest of them was written, the longest waiting first, at
/// most `$3`, each with its reasons. `$1` is now and `$2` the start of
/// the UTC calendar month. Something may have freed them when:
///
/// - the tenant has a card (beta, or an active or trialing
///   subscription) while a job waits for one;
/// - Polar sent a subscription, or the owner changed the cap, after the
///   hold or in the same millisecond;
/// - a spend-cap hold is older than the start of its window: the
///   calendar month for a tenant with no subscription, or the current
///   period rolled forward the way the standing rolls it when the
///   renewal webhook is late.
///
/// A tenant whose holds stand after the check has them marked checked
/// (see [`BILLING_CHECKED`]), and one whose wake was lost has them
/// turned into concurrency holds, so either way it leaves the list
/// until the next such event.
const BILLING_CANDIDATES: &str = "SELECT h.tenant_id, \
       string_agg(DISTINCT h.held, ',' ORDER BY h.held) AS reasons \
     FROM ci_jobs h JOIN tenants t ON t.id = h.tenant_id \
     WHERE h.status = 'queued' AND h.held IN ('card', 'spend_cap') \
     AND h.bucket >= $4 AND h.bucket < $5 \
     GROUP BY h.tenant_id, t.beta \
     HAVING (bool_or(h.held = 'card') AND (t.beta OR EXISTS (SELECT 1 FROM subscriptions s \
         WHERE s.tenant_id = h.tenant_id AND s.status IN ('active', 'trialing')))) \
       OR EXISTS (SELECT 1 FROM subscriptions s \
         WHERE s.tenant_id = h.tenant_id AND s.updated_at >= min(h.updated_at)) \
       OR EXISTS (SELECT 1 FROM ci_spend_caps c \
         WHERE c.tenant_id = h.tenant_id AND c.updated_at >= min(h.updated_at)) \
       OR (bool_or(h.held = 'spend_cap') AND ( \
         (NOT EXISTS (SELECT 1 FROM subscriptions s WHERE s.tenant_id = h.tenant_id \
             AND s.status IN ('active', 'trialing')) \
           AND $2::bigint > min(h.updated_at)) \
         OR EXISTS (SELECT 1 FROM (SELECT \
             (EXTRACT(epoch FROM s.current_period_start::timestamptz) * 1000)::bigint AS start_ms, \
             CASE WHEN s.current_period_end IS NULL THEN NULL ELSE \
               (EXTRACT(epoch FROM s.current_period_end::timestamptz) * 1000)::bigint END AS end_ms \
             FROM subscriptions s WHERE s.tenant_id = h.tenant_id \
             AND s.status IN ('active', 'trialing')) p \
           WHERE CASE WHEN p.end_ms IS NOT NULL AND $1::bigint >= p.end_ms \
             THEN p.start_ms + (($1::bigint - p.start_ms) / GREATEST(p.end_ms - p.start_ms, 1)) \
               * GREATEST(p.end_ms - p.start_ms, 1) \
             ELSE p.start_ms END > min(h.updated_at)))) \
     ORDER BY min(h.updated_at), h.tenant_id LIMIT $3";

/// Marks the card and spend-cap holds of tenant `$1` in buckets `$3`
/// to `$4` checked at `$2`: the backstop read the standing and the
/// holds stand.
const BILLING_CHECKED: &str = "UPDATE ci_jobs SET updated_at = $2 \
     WHERE tenant_id = $1 AND status = 'queued' AND held IN ('card', 'spend_cap') \
     AND bucket >= $3 AND bucket < $4";

/// The tenants whose billing holds an event may have freed without
/// waking them, holding jobs in `range`, at most `limit`: see
/// [`BILLING_CANDIDATES`]. A tenant with no such event is not read,
/// however long its jobs wait. `None` reads every bucket, as the event
/// paths do.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn billing_candidates(
    db: &Db,
    now: u64,
    month_start: u64,
    limit: u32,
    range: Option<(u16, u16)>,
) -> worker::Result<Vec<BillingHolds>> {
    let (lo, hi) = bounds(range);
    db.all(
        BILLING_CANDIDATES,
        &params![now, month_start, limit, lo, hi],
    )
    .await
}

/// Marks the billing holds of `tenant` in `range` checked at `now`, so
/// it leaves [`billing_candidates`] until the next event. `None` marks
/// every bucket, as the event paths do.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn billing_checked(
    db: &Db,
    tenant: &str,
    now: u64,
    range: Option<(u16, u16)>,
) -> worker::Result<()> {
    let (lo, hi) = bounds(range);
    db.run(BILLING_CHECKED, &params![tenant, now, lo, hi])
        .await?;
    Ok(())
}

/// Marks up to `$4` unwoken held jobs of tenant `$1` (every tenant when
/// `$1` is NULL) in buckets `$5` to `$6` whose condition is clear now,
/// and returns them: the post-hold recheck and the backstop use the same
/// query. Each reason wakes at most the places free for it, oldest
/// first. Only the picked rows are in the range: the room reads count
/// every bucket of the tenant. Two shards that wake one tenant at the
/// same moment can wake more jobs than it has places for; the consumer
/// checks the caps again and holds the extra ones.
///
/// - `concurrency`: at most `$3` minus the tenant's live jobs
///   (launching, launched, running) minus its marked concurrency holds;
/// - `max_parallel`: per run and group, at most the group's limit minus
///   its live siblings minus its marked holds;
/// - `fork_limit`: per repository, at most the policy's cap (2 by
///   default) minus its live fork-run jobs minus its marked holds;
/// - `group`: one of the job's concurrency rows runs and none of them
///   waits or is still new, as the launch gate reads them (a job's own
///   row of another attempt is left over and does not count). A job
///   with a row that still waits would only be held again, and its
///   recheck would wake it again.
///
/// A row two wakers pick at once is marked once: the update only marks
/// jobs still unmarked.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn lost_wakes(
    db: &Db,
    tenant: Option<&str>,
    max_running: u32,
    now: u64,
    limit: u32,
    range: Option<(u16, u16)>,
) -> worker::Result<Vec<Woken>> {
    let (lo, hi) = bounds(range);
    db.all(
        LOST_WAKES,
        &params![tenant, now, max_running, limit, lo, hi],
    )
    .await
}

const LOST_WAKES: &str = "WITH candidates AS (\
      SELECT id FROM (\
        SELECT j.id, \
          row_number() OVER (PARTITION BY j.tenant_id ORDER BY j.id) AS place, \
          $3::bigint \
          - (SELECT count(*) FROM ci_jobs l WHERE l.tenant_id = j.tenant_id \
              AND l.status IN ('launching', 'launched', 'running')) \
          - (SELECT count(*) FROM ci_jobs m WHERE m.tenant_id = j.tenant_id \
              AND m.status = 'queued' AND m.held = 'concurrency' \
              AND m.next_launch_at IS NOT NULL) AS room \
        FROM ci_jobs j \
        WHERE j.status = 'queued' AND j.held = 'concurrency' AND j.next_launch_at IS NULL \
          AND ($1::text IS NULL OR j.tenant_id = $1) \
          AND j.bucket >= $5 AND j.bucket < $6) ranked \
      WHERE place <= room \
    UNION ALL \
      SELECT id FROM (\
        SELECT j.id, \
          row_number() OVER (PARTITION BY j.run_id, j.strategy_group ORDER BY j.id) AS place, \
          j.max_parallel \
          - (SELECT count(*) FROM ci_jobs l WHERE l.run_id = j.run_id \
              AND l.strategy_group = j.strategy_group \
              AND l.status IN ('launching', 'launched', 'running')) \
          - (SELECT count(*) FROM ci_jobs m WHERE m.run_id = j.run_id \
              AND m.strategy_group = j.strategy_group AND m.status = 'queued' \
              AND m.held = 'max_parallel' AND m.next_launch_at IS NOT NULL) AS room \
        FROM ci_jobs j \
        WHERE j.status = 'queued' AND j.held = 'max_parallel' AND j.next_launch_at IS NULL \
          AND j.strategy_group IS NOT NULL AND j.max_parallel IS NOT NULL \
          AND j.bucket >= $5 AND j.bucket < $6 \
          AND ($1::text IS NULL OR j.tenant_id = $1)) ranked \
      WHERE place <= room \
    UNION ALL \
      SELECT id FROM (\
        SELECT j.id, \
          row_number() OVER (PARTITION BY r.repo_id ORDER BY j.id) AS place, \
          COALESCE((SELECT p.fork_max_running FROM ci_repo_policy p \
              WHERE p.repo_id = r.repo_id), 2) \
          - (SELECT count(*) FROM ci_jobs l WHERE l.tenant_id = j.tenant_id \
              AND l.status IN ('launching', 'launched', 'running') \
              AND EXISTS (SELECT 1 FROM ci_runs lr WHERE lr.id = l.run_id \
                AND lr.repo_id = r.repo_id AND lr.trust = 'fork')) \
          - (SELECT count(*) FROM ci_jobs m WHERE m.tenant_id = j.tenant_id \
              AND m.status = 'queued' AND m.held = 'fork_limit' \
              AND m.next_launch_at IS NOT NULL \
              AND EXISTS (SELECT 1 FROM ci_runs mr WHERE mr.id = m.run_id \
                AND mr.repo_id = r.repo_id)) AS room \
        FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id \
        WHERE j.status = 'queued' AND j.held = 'fork_limit' AND j.next_launch_at IS NULL \
          AND r.trust = 'fork' \
          AND j.bucket >= $5 AND j.bucket < $6 \
          AND ($1::text IS NULL OR j.tenant_id = $1)) ranked \
      WHERE place <= room \
    UNION ALL \
      SELECT j.id FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id \
      WHERE j.status = 'queued' AND j.held = 'group' AND j.next_launch_at IS NULL \
        AND j.bucket >= $5 AND j.bucket < $6 \
        AND ($1::text IS NULL OR j.tenant_id = $1) \
        AND EXISTS (SELECT 1 FROM ci_concurrency c WHERE c.repo_id = r.repo_id \
          AND ((c.job_id = j.id AND c.attempt = j.attempt) \
            OR (c.job_id IS NULL AND c.run_id = j.run_id)) \
          AND c.state = 'running') \
        AND NOT EXISTS (SELECT 1 FROM ci_concurrency c WHERE c.repo_id = r.repo_id \
          AND ((c.job_id = j.id AND c.attempt = j.attempt) \
            OR (c.job_id IS NULL AND c.run_id = j.run_id)) \
          AND c.state IN ('new', 'pending'))) \
    UPDATE ci_jobs SET next_launch_at = $2 WHERE id IN (\
      SELECT id FROM candidates ORDER BY id LIMIT $4) \
    AND status = 'queued' AND held IN ('concurrency', 'max_parallel', 'fork_limit', 'group') \
      AND next_launch_at IS NULL \
    RETURNING id AS job_id, attempt";

/// Frees the approval holds whose environment is gone or has approval
/// off and that nobody looked at since `quiet_before`, in buckets
/// `range`, so a release that failed when the environment changed never
/// strands a job. The jobs come back to be sent to the launch queue.
/// `None` reads every bucket, as the event paths do.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_orphan_approvals(
    db: &Db,
    quiet_before: u64,
    now: u64,
    limit: u32,
    range: Option<(u16, u16)>,
) -> worker::Result<Vec<Woken>> {
    let (lo, hi) = bounds(range);
    db.all(
        RELEASE_ORPHAN_APPROVALS,
        &params![quiet_before, now, limit, lo, hi],
    )
    .await
}

/// Frees the fork approval holds whose run was decided and that nobody
/// looked at since `quiet_before`, in buckets `range`, so a wake that
/// failed after an approval never strands a job. The jobs come back to
/// be sent to the launch queue. `None` reads every bucket, as the
/// event paths do.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_orphan_fork_approvals(
    db: &Db,
    quiet_before: u64,
    now: u64,
    limit: u32,
    range: Option<(u16, u16)>,
) -> worker::Result<Vec<Woken>> {
    let (lo, hi) = bounds(range);
    db.all(
        RELEASE_ORPHAN_FORK_APPROVALS,
        &params![quiet_before, now, limit, lo, hi],
    )
    .await
}
