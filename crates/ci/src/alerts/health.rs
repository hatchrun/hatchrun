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

/// A pass history as the `ci_reconciler` row keeps it; an unreadable one
/// is empty.
pub(crate) fn history(text: Option<&str>) -> Vec<PassRecord> {
    text.and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}

/// `/healthz`'s status and body at `now`: 200 while the last pass is
/// recent, 503 once it is not. Failing passes show in the body only: the
/// reconciler emails those, and 503 is for what email cannot cover, a
/// cron that stopped.
pub(crate) fn health(row: &HealthRow, now: u64) -> (u16, serde_json::Value) {
    let age = row.finished_at.map(|at| now.saturating_sub(at));
    let status = if age.is_none_or(|age| age > STALE) {
        "stale"
    } else if failing(&history(row.history.as_deref())) {
        "failing"
    } else {
        "ok"
    };
    let body = serde_json::json!({
        "status": status,
        "lastPass": row.finished_at.map(time::format),
        "ageSeconds": age.map(|age| age / 1000),
    });
    (if status == "stale" { 503 } else { 200 }, body)
}
