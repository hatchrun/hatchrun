// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The SQL the checks and the dead-letter consumer run.

/// Launches that failed since `$1`, newest first, at most `$2`.
pub(crate) const FAILED_SINCE: &str = "SELECT job_id, job_name, error, attempts, failed_at \
     FROM ci_failed_launches WHERE failed_at >= $1 ORDER BY failed_at DESC LIMIT $2";

/// Webhook deliveries that failed since `$1`, newest first, at most
/// `$2`.
pub(crate) const FAILED_DELIVERIES_SINCE: &str = "SELECT guid, event, error, attempts, \
     failed_at FROM ci_deliveries WHERE state = 'failed' AND failed_at >= $1 \
     ORDER BY failed_at DESC LIMIT $2";

/// Installations whose rate limit was low since `$1`, at most `$2`.
pub(crate) const LOW_SINCE: &str = "SELECT installation_id, remaining, limited \
     FROM ci_rate_limits WHERE seen_at >= $1 ORDER BY seen_at DESC LIMIT $2";

/// Notes an installation's low rate limit at `$4`. A row seen later than
/// `$4` stays: a pass that ran late does not overwrite what a newer one
/// found.
pub(crate) const NOTE_RATE_LIMIT: &str = "INSERT INTO ci_rate_limits \
     (installation_id, remaining, limited, seen_at) VALUES ($1, $2, $3, $4) \
     ON CONFLICT (installation_id) DO UPDATE SET remaining = excluded.remaining, \
     limited = excluded.limited, seen_at = excluded.seen_at \
     WHERE ci_rate_limits.seen_at <= excluded.seen_at";

/// Records a failed launch message `$1`, with its job's name, error and
/// attempts, once.
pub(crate) const RECORD_FAILED: &str = "INSERT INTO ci_failed_launches \
     (message_id, job_id, job_name, error, attempts, failed_at) \
     VALUES ($1, $2, (SELECT name FROM ci_jobs WHERE id = $2), \
       (SELECT launch_error FROM ci_jobs WHERE id = $2), \
       (SELECT launch_attempts FROM ci_jobs WHERE id = $2), $3) \
     ON CONFLICT (message_id) DO NOTHING";

/// Records a message `$1` that did not read, with what is known of it
/// as the error `$2`, once.
pub(crate) const RECORD_UNREADABLE: &str = "INSERT INTO ci_failed_launches \
     (message_id, error, failed_at) VALUES ($1, $2, $3) \
     ON CONFLICT (message_id) DO NOTHING";

/// Deletes up to `$2` failed launches recorded before `$1`.
pub(crate) const DELETE_FAILED: &str = "DELETE FROM ci_failed_launches WHERE id IN \
     (SELECT id FROM ci_failed_launches WHERE failed_at < $1 LIMIT $2)";

/// Deletes rate limits seen before `$1`.
pub(crate) const DELETE_RATE_LIMITS: &str = "DELETE FROM ci_rate_limits WHERE seen_at < $1";

/// The database's client connections, its limit and its size. The
/// Worker's role sees every row of `pg_stat_activity`, if not what each
/// one runs.
pub(crate) const DATABASE: &str = "SELECT \
     (SELECT COUNT(*) FROM pg_stat_activity WHERE backend_type = 'client backend') \
       AS connections, \
     current_setting('max_connections')::bigint AS max_connections, \
     pg_database_size(current_database()) AS bytes";
