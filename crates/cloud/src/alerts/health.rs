// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/healthz`: whether the last pass is recent and not failing.

use serde::Deserialize;

use super::STALE;
use super::checks::{PassRecord, failing};
use crate::reconcile::time;

/// The `reconciler` row as `/healthz` reads it.
pub(crate) const HEALTH: &str = "SELECT finished_at, history FROM reconciler WHERE id = 1";

/// A row of [`HEALTH`].
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HealthRow {
    pub finished_at: Option<u64>,
    pub history: Option<String>,
}

/// A pass history as the `reconciler` row keeps it; an unreadable one
/// is empty.
pub(crate) fn history(text: Option<&str>) -> Vec<PassRecord> {
    text.and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}

/// `/healthz`'s status and body at `now`: 200 while the last pass is
/// recent and the passes are not all failing, 503 otherwise.
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
    (if status == "ok" { 200 } else { 503 }, body)
}
