// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! RS256 JSON Web Tokens, short of the signature: the signing input, the
//! finished token and the private key in the form `WebCrypto` imports.
//! The Worker signs with `WebCrypto` (see [`crate::webcrypto`]).
//!
//! Two tokens use this: the GitHub App's JWT, signed with the key GitHub
//! issued for the App, and the tenant token for AWS (see [`crate::oidc`]).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Unpadded base64url, as JWTs use.
#[must_use]
pub fn b64url(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `header.claims`, the part of an RS256 token that gets signed.
#[must_use]
pub fn signing_input(kid: Option<&str>, claims: &serde_json::Value) -> String {
    let header = match kid {
        Some(kid) => serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": kid }),
        None => serde_json::json!({ "alg": "RS256", "typ": "JWT" }),
    };
    format!(
        "{}.{}",
        b64url(header.to_string()),
        b64url(claims.to_string())
    )
}

/// The token: the signing input and its signature.
#[must_use]
pub fn token(signing_input: &str, signature: &[u8]) -> String {
    format!("{signing_input}.{}", b64url(signature))
}

/// Why a private key was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct KeyError(&'static str);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Reads an RSA private key in PEM and returns it as PKCS#8 DER, the only
/// private key format `WebCrypto` imports. GitHub hands out App keys as
/// PKCS#1 (`BEGIN RSA PRIVATE KEY`); those are wrapped in a PKCS#8
/// envelope. PKCS#8 keys (`BEGIN PRIVATE KEY`) pass through.
///
/// # Errors
///
/// When the text is not one of those two PEM blocks.
pub fn pkcs8_der(pem: &str) -> Result<Vec<u8>, KeyError> {
    let mut lines = pem
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    let pkcs1 = match lines.next().unwrap_or_default() {
        "-----BEGIN RSA PRIVATE KEY-----" => true,
        "-----BEGIN PRIVATE KEY-----" => false,
        _ => return Err(KeyError("not an RSA private key in PEM")),
    };
    let body: String = lines
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_| KeyError("private key PEM is not valid base64"))?;
    if der.first() != Some(&0x30) {
        return Err(KeyError("private key is not DER"));
    }
    Ok(if pkcs1 { wrap_pkcs1(&der) } else { der })
}

/// `PrivateKeyInfo { version 0, rsaEncryption, OCTET STRING pkcs1 }`.
fn wrap_pkcs1(pkcs1: &[u8]) -> Vec<u8> {
    // INTEGER 0, then SEQUENCE { OID 1.2.840.113549.1.1.1, NULL }.
    const PREFIX: &[u8] = &[
        0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01,
        0x01, 0x05, 0x00,
    ];
    let mut octets = vec![0x04];
    octets.extend(der_length(pkcs1.len()));
    octets.extend_from_slice(pkcs1);

    let mut out = vec![0x30];
    out.extend(der_length(PREFIX.len() + octets.len()));
    out.extend_from_slice(PREFIX);
    out.extend(octets);
    out
}

fn der_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        #[allow(clippy::cast_possible_truncation)]
        return vec![len as u8];
    }
    let bytes: Vec<u8> = len
        .to_be_bytes()
        .into_iter()
        .skip_while(|byte| *byte == 0)
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let mut out = vec![0x80 | bytes.len() as u8];
    out.extend(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_input_carries_the_kid() {
        let input = signing_input(Some("k1"), &serde_json::json!({ "iss": "x" }));
        let (header, claims) = input.split_once('.').unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
        assert_eq!(
            header,
            serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": "k1" })
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(claims).unwrap(), br#"{"iss":"x"}"#);
        assert!(!signing_input(None, &serde_json::json!({})).contains('='));
        assert_eq!(token("a.b", &[0xfb, 0xff]), "a.b.-_8");
    }

    fn pem(label: &str, der: &[u8]) -> String {
        let body = STANDARD.encode(der);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        format!(
            "\n-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    }

    #[test]
    fn wraps_pkcs1_in_pkcs8() {
        // A stand-in for an RSA key: a SEQUENCE long enough to need a
        // two-byte length, as real keys do.
        let mut pkcs1 = vec![0x30, 0x82, 0x01, 0x00];
        pkcs1.extend([7; 256]);
        let der = pkcs8_der(&pem("RSA PRIVATE KEY", &pkcs1)).unwrap();
        assert_eq!(&der[..4], &[0x30, 0x82, 0x01, 0x1a]);
        assert_eq!(der.len(), 4 + 0x011a);
        // version, algorithm, then the PKCS#1 key as an OCTET STRING.
        assert_eq!(&der[4..7], &[0x02, 0x01, 0x00]);
        assert_eq!(&der[22..26], &[0x04, 0x82, 0x01, 0x04]);
        assert_eq!(&der[26..], &pkcs1[..]);
    }

    #[test]
    fn passes_pkcs8_through() {
        let pkcs8 = [0x30, 0x03, 0x02, 0x01, 0x00];
        assert_eq!(pkcs8_der(&pem("PRIVATE KEY", &pkcs8)).unwrap(), pkcs8);
    }

    #[test]
    fn refuses_other_keys() {
        assert!(pkcs8_der(&pem("EC PRIVATE KEY", &[0x30, 0x00])).is_err());
        assert!(pkcs8_der("-----BEGIN PRIVATE KEY-----\n!!!\n-----END PRIVATE KEY-----").is_err());
        assert!(pkcs8_der(&pem("PRIVATE KEY", &[0x04, 0x00])).is_err());
        assert!(pkcs8_der("").is_err());
    }

    #[test]
    fn der_lengths() {
        assert_eq!(der_length(0x7f), [0x7f]);
        assert_eq!(der_length(0x80), [0x81, 0x80]);
        assert_eq!(der_length(0x0104), [0x82, 0x01, 0x04]);
    }
}
