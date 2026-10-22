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
         AND ci_jobs.updated_at < $1) ranked \
     WHERE place <= $2 ORDER BY job_id LIMIT $3";

/// The oldest `limit` queued jobs of `tenant` held for `reason` (`card`,
/// `spend_cap` or `concurrency`).
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn held(db: &Db, tenant: &str, reason: &str, limit: u32) -> worker::Result<Vec<Woken>> {
    db.all(HELD, &params![tenant, reason, limit]).await
}

/// The jobs to wake when tenant `tenant`'s card is back: its oldest
/// jobs held for the card, up to [`WAKE_LIMIT`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn card_returned(db: &Db, tenant: &str) -> worker::Result<Vec<Woken>> {
    held(db, tenant, "card", WAKE_LIMIT).await
}

/// The jobs the slow backstop wakes: held jobs nobody has looked at
/// since `quiet_before`, at most `per_tenant` of each tenant and
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
