// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The SQL the checks and the dead-letter consumer run.

use crate::reconcile::VERIFIED;

/// Verified tenants with jobs queued before `$1` that no runner is
/// launching for, while they have room for another runner, at most
/// `$2`. A job waiting for a backoff counts; one waiting for its owner
/// to finish the setup does not. Reads `jobs_open` per tenant.
pub(crate) fn waiting_query() -> String {
    format!(
        "SELECT tenants.org_login AS org, COUNT(*) AS jobs, MIN(jobs.queued_at) AS oldest \
         FROM tenants JOIN jobs ON jobs.installation_id = tenants.installation_id \
         WHERE {VERIFIED} AND jobs.status <> 'completed' AND jobs.status = 'queued' \
         AND (jobs.launch_state IS NULL OR jobs.launch_retry = 'backoff') \
         AND jobs.queued_at < $1 \
         AND (SELECT COUNT(*) FROM jobs AS busy \
           WHERE busy.installation_id = tenants.installation_id \
           AND busy.status <> 'completed' \
           AND busy.launch_state IN ('launching', 'launched')) < tenants.max_runners \
         GROUP BY tenants.id ORDER BY oldest LIMIT $2"
    )
}

/// Launches that failed since `$1`, newest first, at most `$2`.
pub(crate) const FAILED_SINCE: &str = "SELECT job_id, installation_id, error, attempts, \
     failed_at FROM failed_launches WHERE failed_at >= $1 ORDER BY failed_at DESC LIMIT $2";

/// Installations whose rate limit was low since `$1`, at most `$2`.
pub(crate) const LOW_SINCE: &str = "SELECT installation_id, remaining, limited FROM rate_limits \
     WHERE seen_at >= $1 ORDER BY seen_at DESC LIMIT $2";

/// Notes an installation's low rate limit at `$4`.
pub(crate) const NOTE_RATE_LIMIT: &str = "INSERT INTO rate_limits \
     (installation_id, remaining, limited, seen_at) VALUES ($1, $2, $3, $4) \
     ON CONFLICT (installation_id) DO UPDATE SET remaining = excluded.remaining, \
     limited = excluded.limited, seen_at = excluded.seen_at";

/// Records a failed launch message `$1`, with its job's error and
/// attempts, once.
pub(crate) const RECORD_FAILED: &str = "INSERT INTO failed_launches \
     (message_id, job_id, installation_id, error, attempts, failed_at) \
     VALUES ($1, $2, $3, (SELECT launch_error FROM jobs WHERE id = $2), \
       (SELECT launch_attempts FROM jobs WHERE id = $2), $4) \
     ON CONFLICT (message_id) DO NOTHING";

/// Deletes up to `$2` failed launches recorded before `$1`.
pub(crate) const DELETE_FAILED: &str = "DELETE FROM failed_launches WHERE id IN \
     (SELECT id FROM failed_launches WHERE failed_at < $1 LIMIT $2)";

/// Deletes rate limits seen before `$1`.
pub(crate) const DELETE_RATE_LIMITS: &str = "DELETE FROM rate_limits WHERE seen_at < $1";

/// The database's client connections, its limit and its size. The
/// Worker's role sees every row of `pg_stat_activity`, if not what each
/// one runs.
pub(crate) const DATABASE: &str = "SELECT \
     (SELECT COUNT(*) FROM pg_stat_activity WHERE backend_type = 'client backend') \
       AS connections, \
     current_setting('max_connections')::bigint AS max_connections, \
     pg_database_size(current_database()) AS bytes";
