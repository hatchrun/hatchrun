// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A signed-in app session made without GitHub's sign-in, so a script
//! can call the routes that need one.

use anyhow::{Result, anyhow};
use hatchrun_cloud::session::{ID_BYTES, NONCE_BYTES, csrf_token, encode_id, key, seal};
use serde_json::{Value, json};

/// A new session holding `github_token`: the cookie value and CSRF
/// token a request sends, and the `key` and `sealed` token of its
/// `sessions` row.
///
/// # Errors
///
/// The system has no randomness to give.
pub fn new(github_token: &str) -> Result<Value> {
    let mut id = [0u8; ID_BYTES];
    let mut nonce = [0u8; NONCE_BYTES];
    getrandom::fill(&mut id).map_err(|err| anyhow!("random session id: {err}"))?;
    getrandom::fill(&mut nonce).map_err(|err| anyhow!("random nonce: {err}"))?;
    Ok(json!({
        "cookie": encode_id(&id),
        "csrf": csrf_token(&id),
        "key": key(&id),
        "sealed": seal(&id, &nonce, github_token),
    }))
}

#[cfg(test)]
mod tests {
    use hatchrun_cloud::session::{check_csrf, decode_id, open};

    use super::*;

    #[test]
    fn the_row_opens_with_the_cookie() {
        let session = new("ghu_x").unwrap();
        let id = decode_id(session["cookie"].as_str().unwrap()).unwrap();
        assert_eq!(session["key"], key(&id));
        assert_eq!(
            open(&id, session["sealed"].as_str().unwrap()).as_deref(),
            Some("ghu_x")
        );
        assert!(check_csrf(&id, session["csrf"].as_str()));
    }
}
