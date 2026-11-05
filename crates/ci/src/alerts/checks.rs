// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The pass history and the checks over what a pass read.

use hatchrun_cloud::time;
use serde::{Deserialize, Serialize};

use super::{
    CONNECTIONS_PERCENT, FAILED_LAUNCHES_FOR, FAILING_PASSES, GB, HISTORY, Key, LISTED, MINUTE,
    RATE_LIMITS_FOR, Seen,
};

/// One pass, as the `ci_reconciler` row remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PassRecord {
    /// When it started.
    pub at: u64,
    /// Steps that failed.
    pub errors: u32,
}

/// `history` with `pass` added, newest last, [`HISTORY`] at most.
pub(crate) fn remember(mut history: Vec<PassRecord>, pass: PassRecord) -> Vec<PassRecord> {
    history.push(pass);
    let extra = history.len().saturating_sub(HISTORY);
    history.drain(..extra);
    history
}

/// The last `count` passes, when there are that many and each says
/// `test`.
fn in_a_row(history: &[PassRecord], count: usize, test: impl Fn(&PassRecord) -> bool) -> bool {
    history.len() >= count && history[history.len() - count..].iter().all(test)
}

/// Whether the last passes all had errors.
pub(super) fn failing(history: &[PassRecord]) -> bool {
    in_a_row(history, FAILING_PASSES, |pass| pass.errors > 0)
}

/// A row of `ci_failed_launches`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct FailedLaunch {
    pub job_id: Option<u64>,
    pub job_name: Option<String>,
    pub error: Option<String>,
    pub attempts: Option<u32>,
    pub failed_at: u64,
}

/// A row of `ci_rate_limits`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LowRateLimit {
    pub installation_id: u64,
    pub remaining: Option<u32>,
    pub limited: bool,
}

/// The database as [`DATABASE`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Database {
    /// Client connections open.
    pub connections: u64,
    /// The server's `max_connections`.
    pub max_connections: u64,
    /// The database's size in bytes.
    pub bytes: u64,
}

/// What the checks after a pass read; `None` where the read failed.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    /// The last passes, newest last, this one among them.
    pub history: Option<Vec<PassRecord>>,
    pub database: Option<Database>,
    pub failed: Option<Vec<FailedLaunch>>,
    pub rate_limits: Option<Vec<LowRateLimit>>,
}

/// What each alert's check finds in `facts`. The reconciler stopping is
/// not among them; `/healthz` tells it to the uptime monitor.
pub(crate) fn check(facts: &Facts) -> Vec<(Key, Seen)> {
    let mut seen = Vec::new();
    let history = facts.history.as_deref();
    seen.push((
        Key::ReconcilerFailing,
        history.map_or(Seen::Unknown, |history| {
            if failing(history) {
                let errors: Vec<String> = history[history.len() - FAILING_PASSES..]
                    .iter()
                    .map(|pass| pass.errors.to_string())
                    .collect();
                Seen::Firing(format!(
                    "The last {FAILING_PASSES} passes had errors: {}. The log has each.",
                    errors.join(", ")
                ))
            } else {
                Seen::Clear
            }
        }),
    ));
    seen.push((
        Key::LaunchFailed,
        facts
            .failed
            .as_deref()
            .map_or(Seen::Unknown, failed_launches),
    ));
    seen.push((
        Key::RateLimit,
        facts
            .rate_limits
            .as_deref()
            .map_or(Seen::Unknown, rate_limited),
    ));
    seen.push((
        Key::DatabaseConnections,
        facts.database.as_ref().map_or(Seen::Unknown, connections),
    ));
    seen
}

/// The database connections alert.
fn connections(database: &Database) -> Seen {
    if database.connections * 100 < database.max_connections * CONNECTIONS_PERCENT {
        return Seen::Clear;
    }
    Seen::Firing(format!(
        "{} of {} connections are in use. New ones fail at the limit, Hyperdrive's \
         among them. The database is {}.{} GB.",
        database.connections,
        database.max_connections,
        database.bytes / GB,
        database.bytes % GB / (GB / 10)
    ))
}

/// The rate limit alert, from the installations found low lately.
fn rate_limited(limits: &[LowRateLimit]) -> Seen {
    if limits.is_empty() {
        return Seen::Clear;
    }
    let lines: Vec<String> = limits
        .iter()
        .map(|limit| match (limit.limited, limit.remaining) {
            (true, _) => format!("installation {}: rate limited", limit.installation_id),
            (_, Some(left)) => format!(
                "installation {}: {left} requests left",
                limit.installation_id
            ),
            (_, None) => format!("installation {}: low", limit.installation_id),
        })
        .collect();
    Seen::Firing(format!(
        "GitHub answers in the last {} min:\n{}",
        RATE_LIMITS_FOR / MINUTE,
        lines.join("\n")
    ))
}

/// The failed launch alert, from the latest failures.
pub(super) fn failed_launches(failed: &[FailedLaunch]) -> Seen {
    if failed.is_empty() {
        return Seen::Clear;
    }
    let known = |value: Option<u64>| value.map_or_else(|| "?".to_owned(), |id| id.to_string());
    let lines: Vec<String> = failed
        .iter()
        .map(|launch| {
            format!(
                "job {} ({}), {} attempts, at {}: {}",
                known(launch.job_id),
                launch.job_name.as_deref().unwrap_or("?"),
                known(launch.attempts.map(u64::from)),
                time::format(launch.failed_at),
                launch.error.as_deref().unwrap_or("no error recorded")
            )
        })
        .collect();
    Seen::Firing(format!(
        "Launch messages reached hatchrun-ci-launches-dlq in the last {} min{}:\n{}\n\n\
         The reconciler sends queued jobs again and fails launches that stalled; the error says why the queue could not.",
        FAILED_LAUNCHES_FOR / MINUTE,
        if failed.len() == LISTED as usize {
            ", the latest of them"
        } else {
            ""
        },
        lines.join("\n")
    ))
}
