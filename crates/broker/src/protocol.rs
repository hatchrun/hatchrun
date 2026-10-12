// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Requests, limits and checks, kept apart from the Workers runtime so
//! they test natively.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Ticket length: 32 random bytes in unpadded base64url.
pub const TICKET_LEN: usize = 43;
/// Largest config the broker keeps. JIT configs are a few kilobytes.
pub const MAX_CONFIG: usize = 64 * 1024;
/// How long a config waits for its instance when the scaler does not say.
pub const DEFAULT_TTL: u64 = 15 * 60;
/// Shortest and longest wait the scaler may ask for, in seconds.
pub const TTL_RANGE: std::ops::RangeInclusive<u64> = 60..=60 * 60;

/// What a request asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Route<'a> {
    /// `PUT /v1/tickets/{ticket}` from the scaler.
    Store(&'a str),
    /// `POST /v1/tickets/{ticket}/claim` from the agent.
    Claim(&'a str),
    /// Anything else.
    NotFound,
}

/// Matches a request to a route. Malformed tickets do not match, so they
/// never reach a Durable Object.
#[must_use]
pub fn route<'a>(method: &str, path: &'a str) -> Route<'a> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("PUT", ["v1", "tickets", ticket]) if valid_ticket(ticket) => Route::Store(ticket),
        ("POST", ["v1", "tickets", ticket, "claim"]) if valid_ticket(ticket) => {
            Route::Claim(ticket)
        }
        _ => Route::NotFound,
    }
}

fn valid_ticket(ticket: &str) -> bool {
    ticket.len() == TICKET_LEN
        && ticket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Names the Durable Object of a ticket by its hash, so neither object
/// names nor logs hold a ticket that still works.
#[must_use]
pub fn object_name(ticket: &str) -> String {
    hex(&Sha256::digest(ticket.as_bytes()))
}

/// Checks `Authorization: Bearer {token}` against the broker's token.
/// Hashing both sides first keeps the comparison constant time, length
/// included.
#[must_use]
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    !token.is_empty()
        && Sha256::digest(given.as_bytes())
            .as_slice()
            .ct_eq(Sha256::digest(token.as_bytes()).as_slice())
            .into()
}

/// The body of a store request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreRequest {
    /// The encoded JIT config.
    pub config: String,
    /// Seconds the config waits for its instance.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// What the Durable Object keeps.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The encoded JIT config.
    pub config: String,
    /// When the record expires, in milliseconds since the Unix epoch.
    pub expires_at: u64,
}

impl StoreRequest {
    /// Parses and checks a store request body and returns the record to
    /// keep.
    ///
    /// # Errors
    ///
    /// A message for the scaler when the body is not acceptable.
    pub fn parse(body: &[u8], now_ms: u64) -> Result<Record, String> {
        if body.len() > MAX_CONFIG + 1024 {
            return Err("body too large".into());
        }
        let request: Self =
            serde_json::from_slice(body).map_err(|err| format!("invalid body: {err}"))?;
        if request.config.is_empty() || request.config.len() > MAX_CONFIG {
            return Err(format!("config must be 1 to {MAX_CONFIG} bytes"));
        }
        let ttl = request.expires_in.unwrap_or(DEFAULT_TTL);
        if !TTL_RANGE.contains(&ttl) {
            return Err(format!(
                "expiresIn must be {} to {} seconds",
                TTL_RANGE.start(),
                TTL_RANGE.end()
            ));
        }
        Ok(Record {
            config: request.config,
            expires_at: now_ms.saturating_add(ttl * 1000),
        })
    }
}

/// The body of a successful claim.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimResponse {
    /// The encoded JIT config.
    pub config: String,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-_0";

    #[test]
    fn routes() {
        assert_eq!(
            route("PUT", &format!("/v1/tickets/{TICKET}")),
            Route::Store(TICKET)
        );
        assert_eq!(
            route("POST", &format!("/v1/tickets/{TICKET}/claim")),
            Route::Claim(TICKET)
        );
        for (method, path) in [
            ("GET", format!("/v1/tickets/{TICKET}")),
            ("POST", format!("/v1/tickets/{TICKET}")),
            ("PUT", "/v1/tickets/short".to_owned()),
            ("PUT", format!("/v1/tickets/{}", &TICKET[1..])),
            ("PUT", format!("/v1/tickets/{}!", &TICKET[1..])),
            ("POST", format!("/v1/tickets/{TICKET}/claim/x")),
            ("GET", "/".to_owned()),
        ] {
            assert_eq!(route(method, &path), Route::NotFound, "{method} {path}");
        }
    }

    #[test]
    fn object_name_is_the_ticket_hash() {
        let name = object_name(TICKET);
        assert_eq!(name.len(), 64);
        assert!(!name.contains(TICKET));
        assert_eq!(name, object_name(TICKET));
        assert_ne!(name, object_name(&TICKET.replace('A', "B")));
    }

    #[test]
    fn authorization() {
        assert!(authorized(Some("Bearer s3cret"), "s3cret"));
        assert!(!authorized(Some("Bearer s3cre"), "s3cret"));
        assert!(!authorized(Some("s3cret"), "s3cret"));
        assert!(!authorized(None, "s3cret"));
        // An unset token must not let an empty one through.
        assert!(!authorized(Some("Bearer "), ""));
    }

    #[test]
    fn store_request() {
        let record = StoreRequest::parse(br#"{"config":"jit"}"#, 1_000).unwrap();
        assert_eq!(
            record,
            Record {
                config: "jit".into(),
                expires_at: 1_000 + DEFAULT_TTL * 1000
            }
        );
        let record = StoreRequest::parse(br#"{"config":"jit","expiresIn":120}"#, 1_000).unwrap();
        assert_eq!(record.expires_at, 121_000);
    }

    #[test]
    fn store_request_limits() {
        let big = format!(r#"{{"config":"{}"}}"#, "x".repeat(MAX_CONFIG + 1));
        for body in [
            br#"{"config":""}"#.to_vec(),
            br#"{"config":"jit","expiresIn":10}"#.to_vec(),
            br#"{"config":"jit","expiresIn":7200}"#.to_vec(),
            br#"{"config":"jit","extra":1}"#.to_vec(),
            b"not json".to_vec(),
            big.into_bytes(),
        ] {
            assert!(
                StoreRequest::parse(&body, 0).is_err(),
                "{}",
                String::from_utf8_lossy(&body[..body.len().min(60)])
            );
        }
    }
}
