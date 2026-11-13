// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! CI usage windows: one row per tenant and 15 minute window, holding
//! the summed units, seconds and job count Polar bills.
//!
//! The reconciler closes ended windows every pass and the Polar webhook
//! closes a tenant's open window at once when its subscription ends
//! (see [`close_windows`]). Both share this module, as the webhook
//! cannot use the CI store without a dependency cycle.

use crate::db::{Db, params};
use worker::Result;

/// A usage window is 15 minutes, aligned to UTC multiples of it.
pub const WINDOW_MS: u64 = 15 * 60_000;

/// A window closes once it ended this long ago, so a row that arrives
/// just after the boundary still joins it.
pub const GRACE_MS: u64 = 2 * 60_000;

/// Closes usage windows at `now` and returns how many usage rows were
/// assigned one. With `tenant` every unsent, unwindowed row of that
/// tenant closes at once, ignoring the window boundary and the grace;
/// without it only rows whose window ended before the grace close, of
/// every tenant.
///
/// Each row joins its natural window: the 15 minute grid window of when
/// it ended, cut at the licensed subscription's period start and end so
/// no window spans two Polar billing periods. A row that arrives late
/// for a window that already exists joins the latest window that ended
/// before the grace instead, never the written one, so a window's
/// totals never change after it is written. When even that window
/// exists already the row waits for the next pass, whose latest ended
/// window moved on.
///
/// One statement closes and assigns, so a crash cannot leave rows half
/// assigned. The reconciler pass and the webhook flush can run at
/// once: both insert the same window, and the loser's insert skips it
/// on the unique key, so its rows stay unwindowed for the next pass.
/// Windows and rows Polar already took or refused are left alone.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn close_windows(db: &Db, now: u64, tenant: Option<&str>) -> Result<u64> {
    db.run(
        CLOSE_WINDOWS,
        &params![now, GRACE_MS, tenant.unwrap_or(""), tenant.is_some()],
    )
    .await
}

/// Closes usage windows: `$1` is now, `$2` the grace, `$3` the tenant to
/// flush or the empty string for every tenant, and `$4` whether to
/// flush. `bounds` holds each tenant's licensed period start and end,
/// `open` every open row with its natural window, `latest` the latest
/// ended window of each tenant, and `assigned` where each row goes. New
/// windows are inserted for groups without one, then every assigned row
/// points at its new window through the insert's `RETURNING` rows, as
/// one statement reads a single snapshot. A late row whose target
/// window exists already stays unwindowed for the next pass.
const CLOSE_WINDOWS: &str = "WITH bounds AS (
       SELECT DISTINCT u.tenant_id,
         (SELECT (EXTRACT(EPOCH FROM s.current_period_start::timestamptz) * 1000)::bigint
          FROM subscriptions s
          WHERE s.tenant_id = u.tenant_id AND s.status IN ('active', 'trialing')
          ORDER BY s.modified_at DESC LIMIT 1) AS ps,
         (SELECT (EXTRACT(EPOCH FROM s.current_period_end::timestamptz) * 1000)::bigint
          FROM subscriptions s
          WHERE s.tenant_id = u.tenant_id AND s.status IN ('active', 'trialing')
          ORDER BY s.modified_at DESC LIMIT 1) AS pe
       FROM ci_usage u
       WHERE u.sent_at IS NULL AND u.rejected_at IS NULL AND u.window_id IS NULL
         AND ($3 = '' OR u.tenant_id = $3)
     ), open AS (
       SELECT job_id, attempt, tenant_id, units, seconds, ws,
         LEAST(gs + 900000,
           CASE WHEN ps > ws THEN ps END,
           CASE WHEN pe > ws THEN pe END) AS we
       FROM (SELECT u.job_id, u.attempt, u.tenant_id, u.units, u.seconds,
             e.ps, e.pe, (u.ended_at / 900000) * 900000 AS gs,
             GREATEST((u.ended_at / 900000) * 900000,
               CASE WHEN e.ps <= u.ended_at THEN e.ps END,
               CASE WHEN e.pe <= u.ended_at THEN e.pe END) AS ws
           FROM ci_usage u JOIN bounds e USING (tenant_id)
           WHERE u.sent_at IS NULL AND u.rejected_at IS NULL AND u.window_id IS NULL
             AND ($3 = '' OR u.tenant_id = $3)) AS base
     ), latest AS (
       SELECT tenant_id, e AS twe,
         GREATEST(floor_e,
           CASE WHEN ps < e THEN ps END,
           CASE WHEN pe < e THEN pe END) AS tws
       FROM (SELECT tenant_id, ps, pe, e, ((e - 1) / 900000) * 900000 AS floor_e
           FROM (SELECT tenant_id, ps, pe,
                   GREATEST((($1 - $2) / 900000) * 900000,
                     CASE WHEN ps <= $1 - $2 THEN ps END,
                     CASE WHEN pe <= $1 - $2 THEN pe END) AS e
                 FROM bounds) AS edge) AS target
     ), assigned AS (
       SELECT o.job_id, o.attempt, o.tenant_id, o.units, o.seconds,
         CASE WHEN EXISTS (SELECT 1 FROM ci_usage_windows w
             WHERE w.tenant_id = o.tenant_id AND w.window_start = o.ws)
           THEN l.tws ELSE o.ws END AS aws,
         CASE WHEN EXISTS (SELECT 1 FROM ci_usage_windows w
             WHERE w.tenant_id = o.tenant_id AND w.window_start = o.ws)
           THEN l.twe ELSE o.we END AS awe
       FROM open o JOIN latest l USING (tenant_id)
       WHERE $4 OR o.we <= $1 - $2
     ), new_windows AS (
       INSERT INTO ci_usage_windows
         (tenant_id, window_start, window_end, units, seconds, jobs, created_at)
       SELECT tenant_id, aws, MIN(awe), SUM(units), SUM(seconds), COUNT(*), $1
       FROM assigned a
       WHERE NOT EXISTS (SELECT 1 FROM ci_usage_windows w
         WHERE w.tenant_id = a.tenant_id AND w.window_start = a.aws)
       GROUP BY tenant_id, aws
       ON CONFLICT (tenant_id, window_start) DO NOTHING
       RETURNING id, tenant_id, window_start
     )
     UPDATE ci_usage u SET window_id = w.id
     FROM new_windows w, assigned a
     WHERE u.job_id = a.job_id AND u.attempt = a.attempt
       AND w.tenant_id = a.tenant_id AND w.window_start = a.aws
       AND u.window_id IS NULL";
