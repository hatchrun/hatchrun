// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/healthz`: whether the last pass is recent.

use hatchrun_cloud::time;
use serde::Deserialize;

use super::STALE;
use super::checks::{PassRecord, failing};

/// The `ci_reconciler` row as `/healthz` reads it.
pub(crate) const HEALTH: &str = "SELECT finished_at, history FROM ci_reconciler WHERE id = 1";

/// A row of [`HEALTH`].
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HealthRow {
    pub finished_at: Option<u64>,
    pub history: Option<String>,
}

/// A pass history as the `ci_reconciler` row keeps it. No history, or a
/// blank one, is empty; text that does not parse is `None`, unknown
/// rather than clear.
pub(crate) fn history(text: Option<&str>) -> Option<Vec<PassRecord>> {
    match text {
        None => Some(Vec::new()),
        Some(text) if text.trim().is_empty() => Some(Vec::new()),
        Some(text) => serde_json::from_str(text).ok(),
    }
}

/// `/healthz`'s status and body at `now`: 200 while the last pass is
/// recent, 503 once it is not. A history that does not read shows as
/// `unknown`, not `ok`. Failing passes show in the body only: the
/// reconciler emails those, and 503 is for what email cannot cover, a
/// cron that stopped.
pub(crate) fn health(row: &HealthRow, now: u64) -> (u16, serde_json::Value) {
    let age = row.finished_at.map(|at| now.saturating_sub(at));
    let status = if age.is_none_or(|age| age > STALE) {
        "stale"
    } else {
        match history(row.history.as_deref()) {
            None => "unknown",
            Some(history) if failing(&history) => "failing",
            Some(_) => "ok",
        }
    };
    let body = serde_json::json!({
        "status": status,
        "lastPass": row.finished_at.map(time::format),
        "ageSeconds": age.map(|age| age / 1000),
    });
    (if status == "stale" { 503 } else { 200 }, body)
}
