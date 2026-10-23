// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Wakes the queued CI jobs a tenant's caps hold (`ci_jobs.held`).
//!
//! The CI Worker's launch consumer holds a job and nothing sends it
//! again until its tenant's standing changes. The Workers that learn of
//! the change pick the jobs here and send each one a launch message on
//! the `hatchrun-ci-launches` queue (the `CI_LAUNCHES` binding), where
//! the consumer checks the caps again: the Polar webhook when a CI
//! subscription becomes active, the CI Worker when a job ends or the
//! owner raises the cap, and the CI reconciler's slow backstop.

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

/// The queued jobs of tenant `$1` held for reason `$2`, oldest first,
/// at most `$3`.
const HELD: &str = "SELECT ci_jobs.id AS job_id, ci_jobs.attempt FROM ci_jobs \
     JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
     WHERE ci_runs.tenant_id = $1 AND ci_jobs.status = 'queued' AND ci_jobs.held = $2 \
     ORDER BY ci_jobs.id LIMIT $3";

/// Every tenant's `$2` oldest held jobs that did not change since `$1`,
/// the oldest first overall, at most `$3`.
const STALE_HELD: &str = "SELECT job_id, attempt FROM (\
       SELECT ci_jobs.id AS job_id, ci_jobs.attempt, \
         row_number() OVER (PARTITION BY ci_runs.tenant_id ORDER BY ci_jobs.id) AS place \
       FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id \
       WHERE ci_jobs.status = 'queued' AND ci_jobs.held IS NOT NULL \
         AND ci_jobs.held <> 'approval' AND ci_jobs.updated_at < $1) ranked \
     WHERE place <= $2 ORDER BY job_id LIMIT $3";

/// Turns the card and spend-cap holds of tenant `$1` into concurrency
/// holds, which a finishing job wakes one by one in order. Rows keep
/// their `updated_at`.
const RELEASE_TENANT: &str = "UPDATE ci_jobs SET held = 'concurrency' FROM ci_runs \
     WHERE ci_runs.id = ci_jobs.run_id AND ci_runs.tenant_id = $1 \
       AND ci_jobs.status = 'queued' AND ci_jobs.held IN ('card', 'spend_cap')";

/// The same for the spend-cap holds of every tenant that nobody looked
/// at since `$1`: the month turned, or the cap changed unseen.
const RELEASE_SPEND_CAPS: &str = "UPDATE ci_jobs SET held = 'concurrency' \
     WHERE status = 'queued' AND held = 'spend_cap' AND updated_at < $1";

/// Frees up to `$3` approval holds nobody looked at since `$1` whose
/// environment no longer exists or no longer asks for approval, or whose
/// attempt has a decision already (an approval that raced the hold), due
/// at `$2`, and returns them. The consumer checks such a job again: it runs,
/// or fails when the environment is gone.
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

/// The oldest `limit` queued jobs of `tenant` held for `reason` (`card`,
/// `spend_cap` or `concurrency`).
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn held(db: &Db, tenant: &str, reason: &str, limit: u32) -> worker::Result<Vec<Woken>> {
    db.all(HELD, &params![tenant, reason, limit]).await
}

/// The jobs to wake when the standing of `tenant` improved (the card is
/// back or the cap is higher): its remaining card and spend-cap holds
/// become concurrency holds, so the end of each job wakes the next in
/// order, and the oldest [`WAKE_LIMIT`] are returned to wake now.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn standing_improved(db: &Db, tenant: &str) -> worker::Result<Vec<Woken>> {
    db.run(RELEASE_TENANT, &params![tenant]).await?;
    held(db, tenant, "concurrency", WAKE_LIMIT).await
}

/// The jobs to wake when tenant `tenant`'s card is back.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn card_returned(db: &Db, tenant: &str) -> worker::Result<Vec<Woken>> {
    standing_improved(db, tenant).await
}

/// Turns the spend-cap holds nobody looked at since `quiet_before` into
/// concurrency holds, for the month turn. The backstop wakes the oldest
/// of them and the end of each job wakes the next.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_spend_caps(db: &Db, quiet_before: u64) -> worker::Result<()> {
    db.run(RELEASE_SPEND_CAPS, &params![quiet_before]).await?;
    Ok(())
}

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

/// The jobs the slow backstop wakes: held jobs nobody has looked at
/// since `quiet_before` (approval holds wait for a person, see
/// [`release_orphan_approvals`]), at most `per_tenant` of each tenant and
/// `limit` in all, oldest first.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn stale(
    db: &Db,
    quiet_before: u64,
    per_tenant: u32,
    limit: u32,
) -> worker::Result<Vec<Woken>> {
    db.all(STALE_HELD, &params![quiet_before, per_tenant, limit])
        .await
}
