// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Job tokens: what the control plane signs and the agent presents.
//!
//! A token is `v1.<claims>.<signature>`. Both parts are base64url without
//! padding, and the signature is HMAC-SHA256 over `v1.<claims>`. The
//! caller supplies the clock.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const VERSION: &str = "v1";

/// Seconds an instance may take to boot before the job starts.
pub const BOOT_SECS: u64 = 15 * 60;
/// Seconds past the job's timeout that a token stays valid.
pub const GRACE_SECS: u64 = 10 * 60;

/// What a job token says.
///
/// A signature proves only that the control plane issued the token.
/// Callers must check `job`, `run` and `attempt` against the job row
/// before they act on a token. A rerun gives the job a new attempt, so an
/// instance from an earlier attempt holds a token that no longer matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The job's id.
    pub job: u64,
    /// The run's id.
    pub run: u64,
    /// Which attempt of the job the token was minted for, from 1.
    pub attempt: i32,
    /// When the token stops working, in Unix seconds.
    pub exp: u64,
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// The token does not have the `v1.<claims>.<signature>` shape.
    Malformed,
    /// The signature does not match the claims under this key.
    BadSignature,
    /// The token's `exp` has passed.
    Expired,
}

/// How long a token lives for a job with this timeout, in seconds.
#[must_use]
pub fn lifetime_secs(timeout_minutes: u32) -> u64 {
    u64::from(timeout_minutes) * 60 + BOOT_SECS + GRACE_SECS
}

/// Signs `claims` with `key`. The claims carry the attempt, so a token
/// minted for one attempt fails the check against the next.
///
/// # Panics
///
/// Panics if the claims fail to serialize, which plain integer fields
/// never do.
#[must_use]
pub fn sign(key: &[u8], claims: &Claims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(claims).expect("claims are plain integers and always serialize"),
    );
    let signing = format!("{VERSION}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(mac(key, &signing).finalize().into_bytes());
    format!("{signing}.{signature}")
}

/// Checks a token's signature and expiry, and returns its claims.
///
/// The caller must still compare `job`, `run` and `attempt` with the job
/// row. This function does not know the row.
///
/// # Errors
///
/// Returns [`TokenError::Malformed`] for a token that does not parse,
/// [`TokenError::BadSignature`] when the signature fails under `key`, and
/// [`TokenError::Expired`] once `now_secs` reaches `exp`.
pub fn verify(key: &[u8], token: &str, now_secs: u64) -> Result<Claims, TokenError> {
    let (signing, signature) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
    let (version, payload) = signing.split_once('.').ok_or(TokenError::Malformed)?;
    if version != VERSION || payload.contains('.') {
        return Err(TokenError::Malformed);
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| TokenError::Malformed)?;
    mac(key, signing)
        .verify_slice(&signature)
        .map_err(|_| TokenError::BadSignature)?;
    let json = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| TokenError::Malformed)?;
    let claims: Claims = serde_json::from_slice(&json).map_err(|_| TokenError::Malformed)?;
    if now_secs >= claims.exp {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

fn mac(key: &[u8], signing: &str) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(signing.as_bytes());
    mac
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::hash::{BuildHasher, RandomState};
    use std::sync::LazyLock;

    /// The key for this test run. Every run draws a fresh one, so no key
    /// in the tests is fixed.
    static KEY: LazyLock<[u8; 32]> = LazyLock::new(random_key);

    /// Fresh random key bytes.
    pub(crate) fn random_key() -> [u8; 32] {
        let state = RandomState::new();
        std::array::from_fn(|at| state.hash_one(at).to_le_bytes()[0])
    }

    /// Fresh random nonce bytes.
    pub(crate) fn random_nonce() -> [u8; 12] {
        let state = RandomState::new();
        std::array::from_fn(|at| state.hash_one(at).to_le_bytes()[0])
    }

    fn claims() -> Claims {
        Claims {
            job: 7,
            run: 3,
            attempt: 1,
            exp: 1_000,
        }
    }

    /// Splits a token into its signing input and its signature part.
    fn split_signature(token: &str) -> (&str, &str) {
        token.rsplit_once('.').expect("a signed token has dots")
    }

    /// Returns the token with its signature replaced.
    fn with_signature(token: &str, signature: &str) -> String {
        let (signing, _) = split_signature(token);
        format!("{signing}.{signature}")
    }

    #[test]
    fn round_trips() {
        let token = sign(&*KEY, &claims());
        assert_eq!(verify(&*KEY, &token, 999), Ok(claims()));
    }

    #[test]
    fn rejects_expired() {
        let token = sign(&*KEY, &claims());
        assert_eq!(verify(&*KEY, &token, 1_000), Err(TokenError::Expired));
    }

    #[test]
    fn rejects_a_changed_claim() {
        let token = sign(&*KEY, &claims());
        let other = sign(&*KEY, &Claims { job: 8, ..claims() });
        let parts: Vec<&str> = token.split('.').collect();
        let swapped: Vec<&str> = other.split('.').collect();
        let forged = format!("{}.{}.{}", parts[0], swapped[1], parts[2]);
        assert_eq!(verify(&*KEY, &forged, 999), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_another_key() {
        let token = sign(&*KEY, &claims());
        assert_eq!(
            verify(&random_key(), &token, 999),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert_eq!(verify(&*KEY, "v1.abc", 0), Err(TokenError::Malformed));
        assert_eq!(verify(&*KEY, "v2.abc.def", 0), Err(TokenError::Malformed));
    }

    #[test]
    fn carries_the_attempt() {
        let token = sign(
            &*KEY,
            &Claims {
                attempt: 1,
                ..claims()
            },
        );
        let decoded = verify(&*KEY, &token, 999).unwrap();
        assert_eq!(decoded.attempt, 1);
    }

    #[test]
    fn rejects_a_changed_attempt() {
        // An attempt-1 signature cannot be moved onto an attempt-2 payload.
        let first = sign(
            &*KEY,
            &Claims {
                attempt: 1,
                ..claims()
            },
        );
        let rerun = sign(
            &*KEY,
            &Claims {
                attempt: 2,
                ..claims()
            },
        );
        let (rerun_signing, _) = split_signature(&rerun);
        let (_, first_signature) = split_signature(&first);
        let forged = format!("{rerun_signing}.{first_signature}");
        assert_eq!(verify(&*KEY, &forged, 999), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_an_empty_signature() {
        let token = sign(&*KEY, &claims());
        let forged = with_signature(&token, "");
        assert_eq!(verify(&*KEY, &forged, 999), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_a_truncated_signature() {
        let token = sign(&*KEY, &claims());
        let (_, signature) = split_signature(&token);
        // 40 characters decode to 30 bytes, a clean but short signature.
        let forged = with_signature(&token, &signature[..40]);
        assert_eq!(verify(&*KEY, &forged, 999), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_a_flipped_signature_bit() {
        let token = sign(&*KEY, &claims());
        let (_, signature) = split_signature(&token);
        let mut bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
        bytes[0] ^= 0x01;
        let forged = with_signature(&token, &URL_SAFE_NO_PAD.encode(bytes));
        assert_eq!(verify(&*KEY, &forged, 999), Err(TokenError::BadSignature));
    }

    #[test]
    fn lifetime_covers_the_timeout() {
        assert_eq!(lifetime_secs(360), 360 * 60 + 1500);
    }
}
