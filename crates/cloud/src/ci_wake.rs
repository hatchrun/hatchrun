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

/// Turns the card and spend-cap holds of tenant `$1` into concurrency
/// holds, which a finishing job wakes one by one in order. Rows keep
/// their `next_launch_at`.
const RELEASE_TENANT: &str = "UPDATE ci_jobs SET held = 'concurrency' \
      WHERE tenant_id = $1 AND status = 'queued' AND held IN ('card', 'spend_cap')";

/// Frees up to `$3` approval holds nobody looked at since `$1` whose
/// environment no longer exists or no longer asks for approval, or whose
/// attempt has a decision already (an approval that raced the hold), due
/// at `$2`, and returns them. The consumer checks such a job again: it
/// runs, or fails when the environment is gone.
const RELEASE_ORPHAN_APPROVALS: &str = "UPDATE ci_jobs SET held = NULL, next_launch_at = $2, \
        updated_at = $2 WHERE id IN (\
        SELECT ci_jobs.id FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
        WHERE ci_jobs.status = 'queued' \
        AND ci_jobs.held = 'approval' AND ci_jobs.updated_at < $1 \
        AND ((ci_jobs.approval_attempt = ci_jobs.attempt AND ci_jobs.approval_decision IS NOT NULL) \
          OR NOT EXISTS (SELECT 1 FROM ci_environments e WHERE e.repo_id = ci_runs.repo_id \
            AND e.tenant_id = ci_runs.tenant_id AND lower(e.name) = lower(ci_jobs.environment) AND e.approval)) \
        ORDER BY ci_jobs.id LIMIT $3) \
      RETURNING ci_jobs.id AS job_id, ci_jobs.attempt";

/// Frees up to `$3` fork approval holds nobody looked at since `$1`
/// whose run no longer waits (a decision whose wake was lost), due at
/// `$2`, and returns them. The consumer checks such a job again: it
/// runs, or is left to its run's object when the run was cancelled.
const RELEASE_ORPHAN_FORK_APPROVALS: &str = "UPDATE ci_jobs SET held = NULL, \
        next_launch_at = $2, updated_at = $2 WHERE id IN (\
        SELECT ci_jobs.id FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
        WHERE ci_jobs.status = 'queued' AND ci_jobs.held = 'fork_approval' \
        AND ci_jobs.updated_at < $1 AND ci_runs.gate_status NOT IN ('pending', 'deferred') \
        ORDER BY ci_jobs.id LIMIT $3) \
      RETURNING ci_jobs.id AS job_id, ci_jobs.attempt";

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
/// become concurrency holds, so the end of each job wakes the next in
/// order, and the oldest of them the tenant has free places for, at
/// most [`WAKE_LIMIT`], are marked and returned to wake now.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn standing_improved(db: &Db, tenant: &str, now: u64) -> worker::Result<Vec<Woken>> {
    db.run(RELEASE_TENANT, &params![tenant]).await?;
    // Only as many as the tenant has places for: a tenant that still
    // runs its most jobs wakes nothing, and each end wakes the next.
    lost_wakes(db, Some(tenant), WAKE_LIMIT, now, WAKE_LIMIT).await
}

/// The jobs to wake when tenant `tenant`'s card is back.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn card_returned(db: &Db, tenant: &str, now: u64) -> worker::Result<Vec<Woken>> {
    standing_improved(db, tenant, now).await
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

/// The tenants after `$1` with queued card or spend-cap holds, in
/// order, at most `$2`, each with its reasons.
const BILLING_HOLDS: &str = "SELECT tenant_id, string_agg(DISTINCT held, ',' ORDER BY held) \
       AS reasons FROM ci_jobs \
     WHERE status = 'queued' AND held IN ('card', 'spend_cap') AND tenant_id > $1 \
     GROUP BY tenant_id ORDER BY tenant_id LIMIT $2";

/// The tenants after `after` with card or spend-cap holds, at most
/// `limit`: what the backstop checks for a lost billing wake. The
/// backstop starts the next pass after the last one, so every tenant
/// takes its turn however many wait.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn billing_holds(db: &Db, after: &str, limit: u32) -> worker::Result<Vec<BillingHolds>> {
    db.all(BILLING_HOLDS, &params![after, limit]).await
}

/// Marks up to `$4` unwoken held jobs of tenant `$1` (every tenant when
/// `$1` is NULL) whose condition is clear now, and returns them: the
/// post-hold recheck and the backstop use the same query. Each reason
/// wakes at most the places free for it, oldest first:
///
/// - `concurrency`: at most `$3` minus the tenant's live jobs
///   (launching, launched, running) minus its marked concurrency holds;
/// - `max_parallel`: per run and group, at most the group's limit minus
///   its live siblings minus its marked holds;
/// - `fork_limit`: per repository, at most the policy's cap (2 by
///   default) minus its live fork-run jobs minus its marked holds;
/// - `group`: whose concurrency row runs and none waits.
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
) -> worker::Result<Vec<Woken>> {
    db.all(LOST_WAKES, &params![tenant, now, max_running, limit])
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
          AND ($1::text IS NULL OR j.tenant_id = $1)) ranked \
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
          AND ($1::text IS NULL OR j.tenant_id = $1)) ranked \
      WHERE place <= room \
    UNION ALL \
      SELECT j.id FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id \
      WHERE j.status = 'queued' AND j.held = 'group' AND j.next_launch_at IS NULL \
        AND ($1::text IS NULL OR j.tenant_id = $1) \
        AND EXISTS (SELECT 1 FROM ci_concurrency c WHERE c.repo_id = r.repo_id \
          AND (c.job_id = j.id OR (c.job_id IS NULL AND c.run_id = j.run_id)) \
          AND c.state = 'running')) \
    UPDATE ci_jobs SET next_launch_at = $2 WHERE id IN (\
      SELECT id FROM candidates ORDER BY id LIMIT $4) \
    AND status = 'queued' AND held IN ('concurrency', 'max_parallel', 'fork_limit', 'group') \
      AND next_launch_at IS NULL \
    RETURNING id AS job_id, attempt";

/// Frees the approval holds whose environment is gone or has approval
/// off and that nobody looked at since `quiet_before`, so a release
/// that failed when the environment changed never strands a job. The
/// jobs come back to be sent to the launch queue.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_orphan_approvals(
    db: &Db,
    quiet_before: u64,
    now: u64,
    limit: u32,
) -> worker::Result<Vec<Woken>> {
    db.all(RELEASE_ORPHAN_APPROVALS, &params![quiet_before, now, limit])
        .await
}

/// Frees the fork approval holds whose run was decided and that nobody
/// looked at since `quiet_before`, so a wake that failed after an
/// approval never strands a job. The jobs come back to be sent to the
/// launch queue.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_orphan_fork_approvals(
    db: &Db,
    quiet_before: u64,
    now: u64,
    limit: u32,
) -> worker::Result<Vec<Woken>> {
    db.all(
        RELEASE_ORPHAN_FORK_APPROVALS,
        &params![quiet_before, now, limit],
    )
    .await
}
