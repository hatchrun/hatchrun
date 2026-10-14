// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Sessions of app.hatchrun.io, kept apart from the Workers runtime so
//! they test natively.
//!
//! A session id is 32 random bytes. The browser holds it, base64url
//! encoded, in an `HttpOnly`, `Secure`, `SameSite=Lax` cookie; D1 holds
//! only its SHA-256 ([`key`]), so a copy of the database cannot be
//! replayed as a cookie. The user's GitHub token is stored next to it,
//! sealed with AES-256-GCM under a key derived from the session id
//! ([`seal`]), which only the browser has.
//!
//! Requests that change something carry [`csrf_token`] in the
//! [`CSRF_HEADER`] header. Another site can make the browser send the
//! cookie, but cannot read the token, which the app fetches from
//! `/api/session` and which is derived from the session id.

use aes_gcm::aead::{Aead, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::jwt::b64url;

/// Cookie with the session id. `__Host-` makes browsers refuse it
/// unless it is `Secure`, has `Path=/` and no `Domain`, so no other
/// hatchrun.io host can set or read it.
pub const COOKIE: &str = "__Host-hatchrun_session";
/// Cookie that carries the OAuth state and PKCE verifier from sign-in
/// to the callback.
pub const OAUTH_COOKIE: &str = "__Host-hatchrun_oauth";
/// Header that carries the CSRF token.
pub const CSRF_HEADER: &str = "X-CSRF-Token";
/// Bytes in a session id.
pub const ID_BYTES: usize = 32;
/// Bytes in an AES-GCM nonce.
pub const NONCE_BYTES: usize = 12;

/// The value of cookie `name` in a `Cookie` header.
#[must_use]
pub fn cookie<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// A `Set-Cookie` value for a cookie only this host's pages send, for
/// `max_age` seconds.
#[must_use]
pub fn set_cookie(name: &str, value: &str, max_age: u64) -> String {
    format!("{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")
}

/// A `Set-Cookie` value that removes the cookie.
#[must_use]
pub fn clear_cookie(name: &str) -> String {
    set_cookie(name, "", 0)
}

/// The session id in a cookie value, if it is one.
#[must_use]
pub fn decode_id(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .filter(|id| id.len() == ID_BYTES)
}

/// The cookie value for a session id.
#[must_use]
pub fn encode_id(id: &[u8]) -> String {
    b64url(id)
}

/// The key D1 stores the session under: the id's SHA-256 in hex.
#[must_use]
pub fn key(id: &[u8]) -> String {
    hex(&Sha256::digest(id))
}

fn derive(id: &[u8], purpose: &str) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(id).expect("HMAC takes any key");
    mac.update(purpose.as_bytes());
    mac.finalize().into_bytes().into()
}

/// The CSRF token of a session.
#[must_use]
pub fn csrf_token(id: &[u8]) -> String {
    b64url(derive(id, "hatchrun csrf"))
}

/// Whether `given` is the session's CSRF token, compared in constant
/// time.
///
/// # Panics
///
/// Never: HMAC takes keys of any length.
#[must_use]
pub fn check_csrf(id: &[u8], given: Option<&str>) -> bool {
    let Some(given) = given.and_then(|value| URL_SAFE_NO_PAD.decode(value).ok()) else {
        return false;
    };
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(id).expect("HMAC takes any key");
    mac.update(b"hatchrun csrf");
    mac.verify_slice(&given).is_ok()
}

/// Seals `plain` for storage next to the session: AES-256-GCM under a
/// key derived from the session id, with a fresh 12-byte `nonce`.
/// Returns base64url of the nonce and the ciphertext.
///
/// # Panics
///
/// Never: HMAC takes keys of any length and AES-GCM seals anything
/// shorter than 64 GiB.
#[must_use]
pub fn seal(id: &[u8], nonce: &[u8; NONCE_BYTES], plain: &str) -> String {
    let cipher = Aes256Gcm::new(&derive(id, "hatchrun github token").into());
    let sealed = cipher
        .encrypt(Nonce::from_slice(nonce), plain.as_bytes())
        .expect("AES-GCM seals any length a token has");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    b64url(out)
}

/// Opens what [`seal`] sealed under the same session id.
#[must_use]
pub fn open(id: &[u8], sealed: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(sealed).ok()?;
    if bytes.len() < NONCE_BYTES {
        return None;
    }
    let (nonce, data) = bytes.split_at(NONCE_BYTES);
    let cipher = Aes256Gcm::new(&derive(id, "hatchrun github token").into());
    let plain = cipher.decrypt(Nonce::from_slice(nonce), data).ok()?;
    String::from_utf8(plain).ok()
}

/// Lower-case hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher, RandomState};

    use super::*;

    /// Fresh random bytes for every run, so no key or nonce in the tests
    /// is fixed.
    fn random<const N: usize>() -> [u8; N] {
        let state = RandomState::new();
        std::array::from_fn(|at| state.hash_one(at).to_le_bytes()[0])
    }

    #[test]
    fn finds_cookies_by_exact_name() {
        let header = Some("a=1; __Host-hatchrun_session=abc; __Host-hatchrun_session_x=no");
        assert_eq!(cookie(header, COOKIE), Some("abc"));
        assert_eq!(cookie(header, "a"), Some("1"));
        assert_eq!(cookie(header, "b"), None);
        assert_eq!(cookie(None, COOKIE), None);
    }

    #[test]
    fn cookies_are_host_only_and_hidden_from_scripts() {
        assert_eq!(
            set_cookie(COOKIE, "v", 60),
            "__Host-hatchrun_session=v; Path=/; Max-Age=60; HttpOnly; Secure; SameSite=Lax"
        );
        assert!(clear_cookie(COOKIE).contains("Max-Age=0"));
    }

    #[test]
    fn ids_round_trip_and_must_be_full_length() {
        let id: [u8; ID_BYTES] = random();
        assert_eq!(decode_id(&encode_id(&id)).unwrap(), id);
        assert_eq!(decode_id(&encode_id(&id[..31])), None);
        assert_eq!(decode_id("not base64!"), None);
    }

    #[test]
    fn stores_only_a_hash_of_the_id() {
        let id: [u8; ID_BYTES] = random();
        let stored = key(&id);
        assert_eq!(stored.len(), 64);
        assert!(!stored.contains(&encode_id(&id)));
        assert_ne!(key(&random::<ID_BYTES>()), stored);
    }

    #[test]
    fn csrf_tokens_belong_to_one_session() {
        let id: [u8; ID_BYTES] = random();
        let token = csrf_token(&id);
        assert!(check_csrf(&id, Some(&token)));
        assert!(!check_csrf(&random::<ID_BYTES>(), Some(&token)));
        assert!(!check_csrf(&id, Some("")));
        assert!(!check_csrf(&id, None));
    }

    #[test]
    fn tokens_open_only_with_their_session() {
        let id: [u8; ID_BYTES] = random();
        let sealed = seal(&id, &random(), "ghu_secret");
        assert!(!sealed.contains("ghu_secret"));
        assert_eq!(open(&id, &sealed).as_deref(), Some("ghu_secret"));
        assert_eq!(open(&random::<ID_BYTES>(), &sealed), None);
        assert_eq!(open(&id, "AAAA"), None);
        let mut tampered = URL_SAFE_NO_PAD.decode(&sealed).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(open(&id, &b64url(tampered)), None);
    }
}
