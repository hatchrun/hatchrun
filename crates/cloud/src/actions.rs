// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! GitHub Actions OIDC tokens, checked without the runtime.
//!
//! A job with `id-token: write` can ask GitHub for a token naming the
//! repository, ref, workflow file and environment it runs in, for an
//! audience it picks. GitHub signs it with RS256 and publishes the keys
//! at [`ISSUER`]`/.well-known/jwks`. [`verify`] checks the signature
//! against those keys and every claim against a [`Policy`]; the Worker
//! fetches and caches the keys (see `app::images`).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// GitHub's issuer for Actions tokens.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";
/// Path of the issuer's JWKS.
pub const JWKS_PATH: &str = "/.well-known/jwks";
/// How long fetched keys are used, in milliseconds. GitHub serves the
/// JWKS with `max-age=3600`.
pub const KEYS_MAX_AGE_MS: u64 = 60 * 60 * 1000;
/// How long a token with an unknown `kid` waits before the keys are
/// fetched again, in milliseconds, so bad tokens cannot make the Worker
/// fetch on every request.
pub const KEYS_REFETCH_MS: u64 = 60 * 1000;
/// Seconds of clock difference allowed on `exp` and `nbf`.
const LEEWAY: u64 = 60;
/// The smallest modulus accepted, in bits.
const MIN_MODULUS_BITS: usize = 2048;
/// `DigestInfo` for SHA-256, which PKCS#1 v1.5 puts in front of the hash.
const SHA256_PREFIX: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// A signing key from the issuer's JWKS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The key's `kid`, which tokens name in their header.
    pub kid: String,
    key: RsaPublicKey,
}

impl Key {
    /// A key from its `kid`, modulus and exponent as unsigned
    /// big-endian bytes.
    ///
    /// # Errors
    ///
    /// When the numbers are not an RSA public key of at least 2048 bits.
    pub fn new(kid: impl Into<String>, n: &[u8], e: &[u8]) -> Result<Self, String> {
        let key = RsaPublicKey::new(BigUint::from_bytes_be(n), BigUint::from_bytes_be(e))
            .map_err(|err| err.to_string())?;
        if key.n().bits() < MIN_MODULUS_BITS {
            return Err(format!("modulus is shorter than {MIN_MODULUS_BITS} bits"));
        }
        Ok(Self {
            kid: kid.into(),
            key,
        })
    }
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// The RS256 signing keys of a JWKS document. GitHub lists RSA keys
/// with `kid`, `n` and `e`, some with an `x5c` certificate chain and
/// `x5t` thumbprint as well, which are not needed. Keys of other types
/// or uses are left out.
///
/// # Errors
///
/// When the document is not a JWKS, or an RS256 key in it is unreadable.
pub fn parse_jwks(body: &[u8]) -> Result<Vec<Key>, String> {
    let jwks: Jwks = serde_json::from_slice(body).map_err(|err| format!("bad JWKS: {err}"))?;
    jwks.keys
        .into_iter()
        .filter(|jwk| {
            jwk.kty == "RSA"
                && jwk.alg.as_deref().is_none_or(|alg| alg == "RS256")
                && jwk.usage.as_deref().is_none_or(|usage| usage == "sig")
        })
        .map(|jwk| {
            let kid = jwk.kid.ok_or("JWKS key without a kid")?;
            let number = |value: Option<String>, name: &str| {
                URL_SAFE_NO_PAD
                    .decode(value.unwrap_or_default())
                    .ok()
                    .filter(|bytes| !bytes.is_empty())
                    .ok_or_else(|| format!("JWKS key {kid} has no readable {name}"))
            };
            let n = number(jwk.n, "n")?;
            let e = number(jwk.e, "e")?;
            Key::new(kid.clone(), &n, &e).map_err(|err| format!("JWKS key {kid}: {err}"))
        })
        .collect()
}

/// Whether cached keys fetched `age_ms` ago answer a token whose `kid`
/// they `have`. Keys are fetched again once they are old, or when they
/// lack the `kid` and were not fetched within the last minute: GitHub
/// publishes a new key before it signs with it, so a miss after an hour
/// means the cache is behind.
#[must_use]
pub fn reuse_keys(age_ms: u64, have: bool) -> bool {
    if have {
        age_ms < KEYS_MAX_AGE_MS
    } else {
        age_ms < KEYS_REFETCH_MS
    }
}

/// What a token must say to be accepted.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// `iss`, the issuer the keys came from.
    pub issuer: &'a str,
    /// `aud`, the audience the job asked for.
    pub audience: &'a str,
    /// `repository`, as `owner/name`.
    pub repository: &'a str,
    /// `repository_id`, which stays with the repository through renames.
    pub repository_id: &'a str,
    /// `ref`, the branch the run is on.
    pub git_ref: &'a str,
    /// `job_workflow_ref`, the workflow file that defines the job and
    /// its ref. It is the called workflow when the job is in a reusable
    /// one, and `workflow_ref` otherwise.
    pub job_workflow_ref: &'a str,
    /// `environment`, the job's deployment environment.
    pub environment: &'a str,
}

/// The claims of an accepted token that the Worker logs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Claims {
    /// `iss`.
    pub iss: String,
    /// `aud`, one audience or several.
    pub aud: Audience,
    /// Expiry, Unix seconds.
    pub exp: u64,
    /// Start of validity, Unix seconds.
    pub nbf: Option<u64>,
    /// `repository`.
    pub repository: String,
    /// `repository_id`.
    pub repository_id: String,
    /// `ref`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// `job_workflow_ref`.
    pub job_workflow_ref: String,
    /// `environment`, missing for a job without one.
    pub environment: Option<String>,
    /// `run_id`.
    pub run_id: String,
}

/// `aud`: a string, or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    /// A single audience.
    One(String),
    /// Several audiences.
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, audience: &str) -> bool {
        match self {
            Self::One(one) => one == audience,
            Self::Many(many) => many.iter().any(|one| one == audience),
        }
    }
}

/// Why a token was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Not a JWT, or not one signed with RS256.
    Malformed(String),
    /// No key with the token's `kid`.
    UnknownKey(String),
    /// The signature does not match.
    BadSignature,
    /// Past `exp`.
    Expired,
    /// Before `nbf`.
    NotYetValid,
    /// A claim other than the policy's.
    Claim {
        /// The claim's name.
        name: &'static str,
        /// What the token says.
        found: String,
    },
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed token: {why}"),
            Self::UnknownKey(kid) => write!(f, "no key with kid {kid}"),
            Self::BadSignature => f.write_str("bad signature"),
            Self::Expired => f.write_str("token expired"),
            Self::NotYetValid => f.write_str("token not valid yet"),
            Self::Claim { name, found } => write!(f, "{name} {found:?} is not allowed"),
        }
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

/// The `kid` in a token's header, to find its key.
///
/// # Errors
///
/// When the token is not an RS256 JWT with a `kid`.
pub fn key_id(token: &str) -> Result<String, Rejection> {
    let [header, _, _] = parts(token)?;
    let header: Header = decode(header)?;
    if header.alg != "RS256" {
        return Err(Rejection::Malformed(format!("alg {}", header.alg)));
    }
    header
        .kid
        .ok_or_else(|| Rejection::Malformed("no kid".to_owned()))
}

/// Checks `token`'s signature against `keys` and its claims against
/// `policy` at `now`, Unix seconds.
///
/// # Errors
///
/// When either check fails; see [`Rejection`].
pub fn verify(
    token: &str,
    keys: &[Key],
    policy: &Policy<'_>,
    now: u64,
) -> Result<Claims, Rejection> {
    let kid = key_id(token)?;
    let key = keys
        .iter()
        .find(|key| key.kid == kid)
        .ok_or(Rejection::UnknownKey(kid))?;
    let [header, payload, signature] = parts(token)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| Rejection::Malformed("signature is not base64url".to_owned()))?;
    let digest = Sha256::digest(format!("{header}.{payload}"));
    let scheme = Pkcs1v15Sign {
        hash_len: Some(digest.len()),
        prefix: SHA256_PREFIX.into(),
    };
    key.key
        .verify(scheme, &digest, &signature)
        .map_err(|_| Rejection::BadSignature)?;

    let claims: Claims = decode(payload)?;
    if now >= claims.exp.saturating_add(LEEWAY) {
        return Err(Rejection::Expired);
    }
    if claims
        .nbf
        .is_some_and(|nbf| now.saturating_add(LEEWAY) < nbf)
    {
        return Err(Rejection::NotYetValid);
    }
    let checks = [
        ("iss", claims.iss.as_str(), policy.issuer),
        ("repository", &claims.repository, policy.repository),
        ("repository_id", &claims.repository_id, policy.repository_id),
        ("ref", &claims.git_ref, policy.git_ref),
        (
            "job_workflow_ref",
            &claims.job_workflow_ref,
            policy.job_workflow_ref,
        ),
        (
            "environment",
            claims.environment.as_deref().unwrap_or_default(),
            policy.environment,
        ),
    ];
    if let Some((name, found, _)) = checks.iter().find(|(_, found, want)| found != want) {
        return Err(Rejection::Claim {
            name,
            found: (*found).to_owned(),
        });
    }
    if !claims.aud.contains(policy.audience) {
        return Err(Rejection::Claim {
            name: "aud",
            found: match &claims.aud {
                Audience::One(one) => one.clone(),
                Audience::Many(many) => many.join(" "),
            },
        });
    }
    Ok(claims)
}

fn parts(token: &str) -> Result<[&str; 3], Rejection> {
    let parts: Vec<&str> = token.split('.').collect();
    parts
        .try_into()
        .map_err(|_| Rejection::Malformed("not three parts".to_owned()))
}

fn decode<T: serde::de::DeserializeOwned>(part: &str) -> Result<T, Rejection> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| Rejection::Malformed("part is not base64url".to_owned()))?;
    serde_json::from_slice(&bytes).map_err(|err| Rejection::Malformed(err.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use rsa::RsaPrivateKey;
    use rsa::rand_core::{CryptoRng, Error, RngCore};

    use super::*;

    const NOW: u64 = 1_800_000_000;

    /// `SplitMix64`, seeded, so the test key needs no system randomness.
    struct TestRng(u64);

    impl RngCore for TestRng {
        fn next_u32(&mut self) -> u32 {
            #[allow(clippy::cast_possible_truncation)]
            let low = self.next_u64() as u32;
            low
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for chunk in dest.chunks_mut(8) {
                chunk.copy_from_slice(&self.next_u64().to_le_bytes()[..chunk.len()]);
            }
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    impl CryptoRng for TestRng {}

    /// Two 2048-bit keys made once for all tests: the issuer's and a
    /// stranger's.
    fn private_keys() -> &'static (RsaPrivateKey, RsaPrivateKey) {
        static KEYS: OnceLock<(RsaPrivateKey, RsaPrivateKey)> = OnceLock::new();
        KEYS.get_or_init(|| {
            let mut rng = TestRng(7);
            (
                RsaPrivateKey::new(&mut rng, 2048).unwrap(),
                RsaPrivateKey::new(&mut rng, 2048).unwrap(),
            )
        })
    }

    fn public(kid: &str, key: &RsaPrivateKey) -> Key {
        Key::new(kid, &key.n().to_bytes_be(), &key.e().to_bytes_be()).unwrap()
    }

    const POLICY: Policy<'static> = Policy {
        issuer: ISSUER,
        audience: "https://app.hatchrun.io",
        repository: "hatchrun/images",
        repository_id: "1411692062",
        git_ref: "refs/heads/main",
        job_workflow_ref: "hatchrun/images/.github/workflows/register.yml@refs/heads/main",
        environment: "image-build",
    };

    /// Claims as GitHub sends them for a job of `register.yml` that
    /// `build.yml` calls.
    fn claims() -> serde_json::Value {
        serde_json::json!({
            "jti": "6f4d9c1e-0000-4000-8000-000000000000",
            "sub": "repo:hatchrun/images:environment:image-build",
            "environment": "image-build",
            "aud": "https://app.hatchrun.io",
            "ref": "refs/heads/main",
            "sha": "a9a9f82000000000000000000000000000000000",
            "repository": "hatchrun/images",
            "repository_owner": "hatchrun",
            "repository_owner_id": "340033788",
            "run_id": "18000000000",
            "run_number": "41",
            "run_attempt": "1",
            "repository_visibility": "public",
            "repository_id": "1411692062",
            "actor_id": "1",
            "actor": "github-actions[bot]",
            "workflow": "build",
            "head_ref": "",
            "base_ref": "",
            "event_name": "schedule",
            "ref_protected": "true",
            "ref_type": "branch",
            "workflow_ref": "hatchrun/images/.github/workflows/build.yml@refs/heads/main",
            "workflow_sha": "a9a9f82000000000000000000000000000000000",
            "job_workflow_ref": "hatchrun/images/.github/workflows/register.yml@refs/heads/main",
            "job_workflow_sha": "a9a9f82000000000000000000000000000000000",
            "runner_environment": "github-hosted",
            "iss": ISSUER,
            "nbf": NOW - 5,
            "exp": NOW + 295,
            "iat": NOW,
        })
    }

    fn sign_with(
        key: &RsaPrivateKey,
        header: &serde_json::Value,
        claims: &serde_json::Value,
    ) -> String {
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let scheme = Pkcs1v15Sign {
            hash_len: Some(32),
            prefix: SHA256_PREFIX.into(),
        };
        let signature = key.sign(scheme, &Sha256::digest(&input)).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn sign(claims: &serde_json::Value) -> String {
        let header = serde_json::json!({ "typ": "JWT", "alg": "RS256", "kid": "k1", "x5t": "abc" });
        sign_with(&private_keys().0, &header, claims)
    }

    fn keys() -> Vec<Key> {
        vec![
            public("k0", &private_keys().1),
            public("k1", &private_keys().0),
        ]
    }

    fn with(name: &str, value: serde_json::Value) -> serde_json::Value {
        let mut claims = claims();
        claims[name] = value;
        claims
    }

    fn check(claims: &serde_json::Value) -> Result<Claims, Rejection> {
        verify(&sign(claims), &keys(), &POLICY, NOW)
    }

    #[test]
    fn accepts_the_release_job() {
        let claims = check(&claims()).unwrap();
        assert_eq!(claims.run_id, "18000000000");
        assert_eq!(claims.repository, "hatchrun/images");
        // A list of audiences works too.
        let many = with(
            "aud",
            serde_json::json!(["other", "https://app.hatchrun.io"]),
        );
        assert!(check(&many).is_ok());
    }

    #[test]
    fn refuses_other_callers() {
        let refused = |name: &str, value: serde_json::Value| {
            let Err(Rejection::Claim { name: claim, .. }) = check(&with(name, value)) else {
                panic!("{name} passed");
            };
            claim
        };
        assert_eq!(
            refused("repository", "hatchrun/hatchrun".into()),
            "repository"
        );
        assert_eq!(refused("repository_id", "1".into()), "repository_id");
        assert_eq!(refused("ref", "refs/heads/feature".into()), "ref");
        assert_eq!(refused("ref", "refs/pull/1/merge".into()), "ref");
        assert_eq!(
            refused(
                "job_workflow_ref",
                "hatchrun/images/.github/workflows/lint.yml@refs/heads/main".into()
            ),
            "job_workflow_ref"
        );
        assert_eq!(
            refused(
                "job_workflow_ref",
                "hatchrun/images/.github/workflows/register.yml@refs/heads/other".into()
            ),
            "job_workflow_ref"
        );
        assert_eq!(refused("environment", "production".into()), "environment");
        assert_eq!(
            refused("environment", serde_json::Value::Null),
            "environment"
        );
        assert_eq!(refused("aud", "https://github.com/hatchrun".into()), "aud");
        assert_eq!(
            refused("aud", serde_json::json!(["sts.amazonaws.com"])),
            "aud"
        );
        assert_eq!(refused("iss", "https://oidc.hatchrun.io".into()), "iss");
    }

    #[test]
    fn refuses_tokens_out_of_time() {
        assert_eq!(
            check(&with("exp", (NOW - LEEWAY).into())),
            Err(Rejection::Expired)
        );
        assert!(check(&with("exp", (NOW - LEEWAY + 1).into())).is_ok());
        assert_eq!(
            check(&with("nbf", (NOW + LEEWAY + 1).into())),
            Err(Rejection::NotYetValid)
        );
        assert!(check(&with("nbf", (NOW + LEEWAY).into())).is_ok());
    }

    #[test]
    fn refuses_bad_signatures() {
        let token = sign(&claims());
        // Claims swapped under the signature.
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let (header, _) = signed.split_once('.').unwrap();
        let other = URL_SAFE_NO_PAD.encode(with("repository", "x/y".into()).to_string());
        let forged = format!("{header}.{other}.{signature}");
        assert_eq!(
            verify(&forged, &keys(), &POLICY, NOW),
            Err(Rejection::BadSignature)
        );
        // Signed by a key the issuer does not have under that kid.
        let header = serde_json::json!({ "alg": "RS256", "kid": "k1" });
        let stranger = sign_with(&private_keys().1, &header, &claims());
        assert_eq!(
            verify(&stranger, &keys(), &POLICY, NOW),
            Err(Rejection::BadSignature)
        );
        // A kid the JWKS does not list.
        let header = serde_json::json!({ "alg": "RS256", "kid": "k9" });
        let unknown = sign_with(&private_keys().0, &header, &claims());
        assert_eq!(
            verify(&unknown, &keys(), &POLICY, NOW),
            Err(Rejection::UnknownKey("k9".to_owned()))
        );
        // A signature a byte short.
        let mut bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
        bytes.pop();
        let short = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(bytes));
        assert_eq!(
            verify(&short, &keys(), &POLICY, NOW),
            Err(Rejection::BadSignature)
        );
    }

    #[test]
    fn refuses_other_algorithms() {
        let claims = URL_SAFE_NO_PAD.encode(claims().to_string());
        for header in [
            serde_json::json!({ "alg": "none", "kid": "k1" }),
            serde_json::json!({ "alg": "HS256", "kid": "k1" }),
            serde_json::json!({ "alg": "RS256" }),
        ] {
            let header = URL_SAFE_NO_PAD.encode(header.to_string());
            let token = format!("{header}.{claims}.");
            assert!(matches!(
                verify(&token, &keys(), &POLICY, NOW),
                Err(Rejection::Malformed(_))
            ));
        }
        for token in ["", "a.b", "a.b.c.d", "!.!.!"] {
            assert!(matches!(
                verify(token, &keys(), &POLICY, NOW),
                Err(Rejection::Malformed(_))
            ));
        }
    }

    #[test]
    fn reads_githubs_jwks() {
        // GitHub's JWKS as published in October 2026: RSA keys with and
        // without an x5c chain, and kids in two forms.
        let keys = parse_jwks(include_bytes!("../testdata/actions-jwks.json")).unwrap();
        let kids: Vec<&str> = keys.iter().map(|key| key.kid.as_str()).collect();
        assert_eq!(kids.len(), 4);
        assert!(kids.contains(&"38E9B30B3A023A1B72309921A69A42FCC496C42C"));
        assert!(
            keys.iter()
                .all(|key| key.key.e() == &BigUint::from(65_537_u32))
        );
    }

    #[test]
    fn reads_jwks_keys_it_can_use() {
        let key = &private_keys().0;
        let n = URL_SAFE_NO_PAD.encode(key.n().to_bytes_be());
        let jwks = serde_json::json!({ "keys": [
            { "kty": "EC", "kid": "ec", "crv": "P-256", "x": "AA", "y": "AA" },
            { "kty": "RSA", "kid": "enc", "use": "enc", "n": n, "e": "AQAB" },
            { "kty": "RSA", "kid": "ps", "alg": "PS256", "n": n, "e": "AQAB" },
            { "kty": "RSA", "kid": "k1", "n": n, "e": "AQAB" },
        ]});
        let keys = parse_jwks(jwks.to_string().as_bytes()).unwrap();
        assert_eq!(keys, vec![public("k1", key)]);

        let short = URL_SAFE_NO_PAD.encode([0xc5; 128]);
        for bad in [
            serde_json::json!({ "keys": [{ "kty": "RSA", "n": n, "e": "AQAB" }] }),
            serde_json::json!({ "keys": [{ "kty": "RSA", "kid": "k", "e": "AQAB" }] }),
            serde_json::json!({ "keys": [{ "kty": "RSA", "kid": "k", "n": "%%", "e": "AQAB" }] }),
            serde_json::json!({ "keys": [{ "kty": "RSA", "kid": "k", "n": short, "e": "AQAB" }] }),
            serde_json::json!({ "nokeys": [] }),
        ] {
            assert!(parse_jwks(bad.to_string().as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn refetches_stale_or_missing_keys() {
        assert!(reuse_keys(0, true));
        assert!(reuse_keys(KEYS_MAX_AGE_MS - 1, true));
        assert!(!reuse_keys(KEYS_MAX_AGE_MS, true));
        assert!(reuse_keys(KEYS_REFETCH_MS - 1, false));
        assert!(!reuse_keys(KEYS_REFETCH_MS, false));
    }
}
