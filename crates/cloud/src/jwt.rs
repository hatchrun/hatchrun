// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! JSON Web Tokens, short of the signature: the signing input, the
//! finished token, the private key in the form `WebCrypto` imports and
//! the public half of an RSA key, as the JWKS publishes it. The Worker
//! signs with `WebCrypto` (see [`crate::webcrypto`]).
//!
//! Two RS256 tokens use this: the GitHub App's JWT, signed with the key
//! GitHub issued for the App, and the tenant token for AWS (see
//! [`crate::oidc`]). The runner's telemetry token is `EdDSA`, signed with
//! a key of its own (see [`crate::telemetry`]).

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
    join(&header, claims)
}

/// `header.claims` of an `EdDSA` token, signed with Ed25519.
#[must_use]
pub fn eddsa_signing_input(claims: &serde_json::Value) -> String {
    join(&serde_json::json!({ "alg": "EdDSA", "typ": "JWT" }), claims)
}

fn join(header: &serde_json::Value, claims: &serde_json::Value) -> String {
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
    let (label, der) = pem_der(pem)?;
    match label {
        "RSA PRIVATE KEY" => Ok(wrap_pkcs1(&der)),
        "PRIVATE KEY" => Ok(der),
        _ => Err(KeyError("not an RSA private key in PEM")),
    }
}

/// Reads an Ed25519 private key in PKCS#8 PEM (`BEGIN PRIVATE KEY`, as
/// `openssl genpkey -algorithm ed25519` writes it) and returns its DER.
///
/// # Errors
///
/// When the text is not such a key.
pub fn ed25519_pkcs8_der(pem: &str) -> Result<Vec<u8>, KeyError> {
    const NOT_ED25519: KeyError = KeyError("not an Ed25519 private key in PEM");
    let (label, der) = pem_der(pem)?;
    if label != "PRIVATE KEY" {
        return Err(NOT_ED25519);
    }
    // PrivateKeyInfo { version, AlgorithmIdentifier { OID }, OCTET STRING }
    let (info, _) = element(&der, SEQUENCE)?;
    let (_, info) = element(info, INTEGER)?;
    let (algorithm, _) = element(info, SEQUENCE)?;
    if element(algorithm, OID)?.0 != ED25519 {
        return Err(NOT_ED25519);
    }
    Ok(der)
}

/// The 32-byte seed of an Ed25519 private key in PKCS#8 DER (see
/// [`ed25519_pkcs8_der`]), from which the key and its public half
/// follow.
///
/// # Errors
///
/// When the DER is not an Ed25519 `PrivateKeyInfo`.
pub fn ed25519_seed(pkcs8: &[u8]) -> Result<[u8; 32], KeyError> {
    const NOT_ED25519: KeyError = KeyError("not an Ed25519 private key");
    // PrivateKeyInfo { version, AlgorithmIdentifier, OCTET STRING {
    //   CurvePrivateKey ::= OCTET STRING } }
    let (info, _) = element(pkcs8, SEQUENCE)?;
    let (_, info) = element(info, INTEGER)?;
    let (algorithm, info) = element(info, SEQUENCE)?;
    if element(algorithm, OID)?.0 != ED25519 {
        return Err(NOT_ED25519);
    }
    let (wrapped, _) = element(info, OCTET_STRING)?;
    let (seed, _) = element(wrapped, OCTET_STRING)?;
    seed.try_into().map_err(|_| NOT_ED25519)
}

/// The label and DER of the first PEM block in `pem`.
fn pem_der(pem: &str) -> Result<(&str, Vec<u8>), KeyError> {
    let mut lines = pem
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    let label = lines
        .next()
        .unwrap_or_default()
        .strip_prefix("-----BEGIN ")
        .and_then(|rest| rest.strip_suffix("-----"))
        .ok_or(KeyError("private key is not PEM"))?;
    let body: String = lines
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_| KeyError("private key PEM is not valid base64"))?;
    if der.first() != Some(&SEQUENCE) {
        return Err(KeyError("private key is not DER"));
    }
    Ok((label, der))
}

/// An RSA public key: modulus and exponent as unsigned big-endian bytes
/// with no leading zeros, the form a JWK encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// The modulus, `n`.
    pub n: Vec<u8>,
    /// The public exponent, `e`.
    pub e: Vec<u8>,
}

/// The public half of a PKCS#8 RSA private key (see [`pkcs8_der`]).
///
/// # Errors
///
/// When the DER is not an RSA `PrivateKeyInfo`.
pub fn public_key(pkcs8: &[u8]) -> Result<PublicKey, KeyError> {
    // PrivateKeyInfo { version, AlgorithmIdentifier, OCTET STRING pkcs1 }
    let (info, _) = element(pkcs8, SEQUENCE)?;
    let (_, info) = element(info, INTEGER)?;
    let (algorithm, info) = element(info, SEQUENCE)?;
    if element(algorithm, OID)?.0 != RSA_ENCRYPTION {
        return Err(KeyError("private key is not an RSA key"));
    }
    let (pkcs1, _) = element(info, OCTET_STRING)?;
    // RSAPrivateKey { version, n, e, d, ... }
    let (key, _) = element(pkcs1, SEQUENCE)?;
    let (_, key) = element(key, INTEGER)?;
    let (n, key) = element(key, INTEGER)?;
    let (e, _) = element(key, INTEGER)?;
    Ok(PublicKey {
        n: unsigned(n)?,
        e: unsigned(e)?,
    })
}

const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
const SEQUENCE: u8 = 0x30;
/// 1.2.840.113549.1.1.1, rsaEncryption.
const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// 1.3.101.112, id-Ed25519.
const ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// Splits the DER element at the front of `der`, which must have tag
/// `tag`, into its contents and whatever follows it.
fn element(der: &[u8], tag: u8) -> Result<(&[u8], &[u8]), KeyError> {
    const TRUNCATED: KeyError = KeyError("private key DER is truncated");
    let [found, first, rest @ ..] = der else {
        return Err(TRUNCATED);
    };
    if *found != tag {
        return Err(KeyError("private key DER is not an RSA private key"));
    }
    let (len, rest) = if *first < 0x80 {
        (usize::from(*first), rest)
    } else {
        let size = usize::from(first & 0x7f);
        if size == 0 || size > std::mem::size_of::<usize>() || rest.len() < size {
            return Err(TRUNCATED);
        }
        let (bytes, rest) = rest.split_at(size);
        let len = bytes
            .iter()
            .fold(0, |len, byte| len << 8 | usize::from(*byte));
        (len, rest)
    };
    if rest.len() < len {
        return Err(TRUNCATED);
    }
    Ok(rest.split_at(len))
}

/// A positive DER INTEGER as unsigned bytes. DER adds a zero byte in
/// front of a value whose top bit is set; JWKs leave it out.
fn unsigned(integer: &[u8]) -> Result<Vec<u8>, KeyError> {
    if integer.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(KeyError("private key has a negative integer"));
    }
    let start = integer
        .iter()
        .position(|byte| *byte != 0)
        .ok_or(KeyError("private key has a zero integer"))?;
    Ok(integer[start..].to_vec())
}

/// `PrivateKeyInfo { version 0, rsaEncryption, OCTET STRING pkcs1 }`.
fn wrap_pkcs1(pkcs1: &[u8]) -> Vec<u8> {
    // INTEGER 0, then SEQUENCE { OID rsaEncryption, NULL }.
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

    #[test]
    fn eddsa_signing_input_names_the_algorithm() {
        let input = eddsa_signing_input(&serde_json::json!({ "exp": 1 }));
        let (header, claims) = input.split_once('.').unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(header).unwrap(),
            br#"{"alg":"EdDSA","typ":"JWT"}"#
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(claims).unwrap(), br#"{"exp":1}"#);
    }

    #[test]
    fn reads_ed25519_keys() {
        // The layout openssl genpkey -algorithm ed25519 writes, with a
        // stand-in for the 32-byte seed.
        let mut key = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        key.extend([7; 32]);
        assert_eq!(ed25519_pkcs8_der(&pem("PRIVATE KEY", &key)).unwrap(), key);
        assert_eq!(ed25519_seed(&key).unwrap(), [7; 32]);
        // An RSA key in the same envelope is refused, and so is PKCS#1.
        let mut algorithm = der(OID, RSA_ENCRYPTION);
        algorithm.extend([0x05, 0x00]);
        let mut info = der(INTEGER, &[0]);
        info.extend(der(SEQUENCE, &algorithm));
        info.extend(der(OCTET_STRING, &der(SEQUENCE, &[])));
        let rsa = der(SEQUENCE, &info);
        assert!(ed25519_pkcs8_der(&pem("PRIVATE KEY", &rsa)).is_err());
        assert!(ed25519_seed(&rsa).is_err());
        assert!(ed25519_pkcs8_der(&pem("RSA PRIVATE KEY", &[0x30, 0x00])).is_err());
        assert!(ed25519_pkcs8_der("").is_err());
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

    /// A DER element with tag `tag` around `contents`.
    fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend(der_length(contents.len()));
        out.extend_from_slice(contents);
        out
    }

    /// PKCS#1 `RSAPrivateKey` with modulus `n`, exponent `e` and stand-ins
    /// for the private fields.
    fn pkcs1(n: &[u8], e: &[u8]) -> Vec<u8> {
        let mut fields = der(INTEGER, &[0]);
        fields.extend(der(INTEGER, n));
        fields.extend(der(INTEGER, e));
        for _ in 0..6 {
            fields.extend(der(INTEGER, &[0x55; 128]));
        }
        der(SEQUENCE, &fields)
    }

    #[test]
    fn finds_the_public_key() {
        // A 2048-bit modulus has its top bit set, so DER puts a zero byte
        // in front of it.
        let mut modulus = vec![0xc3];
        modulus.extend([0xa5; 255]);
        let mut n = vec![0];
        n.extend(&modulus);
        let key = pkcs8_der(&pem("RSA PRIVATE KEY", &pkcs1(&n, &[1, 0, 1]))).unwrap();
        assert_eq!(
            public_key(&key).unwrap(),
            PublicKey {
                n: modulus,
                e: vec![1, 0, 1],
            }
        );
    }

    #[test]
    fn refuses_keys_without_rsa_numbers() {
        let wrap = |pkcs1: &[u8]| pkcs8_der(&pem("RSA PRIVATE KEY", pkcs1)).unwrap();
        // A negative modulus, a zero exponent and a key cut short.
        assert!(public_key(&wrap(&pkcs1(&[0x80, 1], &[1, 0, 1]))).is_err());
        assert!(public_key(&wrap(&pkcs1(&[0x7f, 1], &[0]))).is_err());
        let whole = wrap(&pkcs1(&[0x7f, 1], &[1, 0, 1]));
        assert!(public_key(&whole[..40]).is_err());
        // An EC key in a PKCS#8 envelope.
        let mut algorithm = der(OID, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]);
        algorithm.extend(der(OID, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]));
        let mut info = der(INTEGER, &[0]);
        info.extend(der(SEQUENCE, &algorithm));
        info.extend(der(OCTET_STRING, &der(SEQUENCE, &[])));
        assert_eq!(
            public_key(&der(SEQUENCE, &info)),
            Err(KeyError("private key is not an RSA key"))
        );
    }

    #[test]
    fn der_lengths() {
        assert_eq!(der_length(0x7f), [0x7f]);
        assert_eq!(der_length(0x80), [0x81, 0x80]);
        assert_eq!(der_length(0x0104), [0x82, 0x01, 0x04]);
    }
}
