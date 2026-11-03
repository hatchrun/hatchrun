// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use worker::{Response, Result};

/// Builds a `{"error": message}` JSON response with `status`.
///
/// # Errors
///
/// Fails when the response cannot be built.
pub fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}
