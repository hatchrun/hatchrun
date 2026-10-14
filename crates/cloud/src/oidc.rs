// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Tokens from the `oidc.hatchrun.io` issuer.
//!
//! A customer's `ControlPlaneRole` (deploy/cloudformation/connect.yaml)
//! trusts this issuer for the audience `sts.amazonaws.com` and the
//! subject `tenant:<id>`. The Worker signs a token for the tenant and
//! trades it for the role's credentials with `AssumeRoleWithWebIdentity`.
//! STS checks the signature against the issuer's JWKS, found through
//! `/.well-known/openid-configuration`, and matches the key by `kid`.
//!
//! The signing key is an RSA private key in PEM, held as the
//! [`SIGNING_KEY`] secret, and [`KEY_ID`] names it. The JWKS publishes
//! its public half, read from the secret on each request, so the
//! published key cannot drift from the one that signs.
//!
//! Two more slots carry keys that are published but do not sign: the
//! next key, published ahead of signing with it so STS has it cached
//! when the first token arrives, and the previous key, kept while tokens
//! it signed may still be in use. A slot is published when its `kid`
//! variable is set. See [`SLOTS`].

use crate::jwt::{self, PublicKey};

/// The issuer URL, as the customer's IAM OIDC provider names it.
pub const ISSUER: &str = "https://oidc.hatchrun.io";
/// The issuer's hostname. Requests for it are the issuer's.
pub const HOST: &str = "oidc.hatchrun.io";
/// The audience STS expects.
pub const AUDIENCE: &str = "sts.amazonaws.com";
/// Secret holding the issuer's RSA private key in PEM.
pub const SIGNING_KEY: &str = "OIDC_SIGNING_KEY";
/// Variable holding the `kid` of [`SIGNING_KEY`] in the JWKS.
pub const KEY_ID: &str = "OIDC_KEY_ID";
/// Secret holding the key that signs next, published but not signing.
pub const NEXT_SIGNING_KEY: &str = "OIDC_NEXT_SIGNING_KEY";
/// Variable holding the `kid` of [`NEXT_SIGNING_KEY`].
pub const NEXT_KEY_ID: &str = "OIDC_NEXT_KEY_ID";
/// Secret holding the key that signed last, published but not signing.
pub const PREVIOUS_SIGNING_KEY: &str = "OIDC_PREVIOUS_SIGNING_KEY";
/// Variable holding the `kid` of [`PREVIOUS_SIGNING_KEY`].
pub const PREVIOUS_KEY_ID: &str = "OIDC_PREVIOUS_KEY_ID";
/// The keys the JWKS lists, as (secret, `kid` variable) pairs: the
/// signing key first.
pub const SLOTS: [(&str, &str); 3] = [
    (SIGNING_KEY, KEY_ID),
    (NEXT_SIGNING_KEY, NEXT_KEY_ID),
    (PREVIOUS_SIGNING_KEY, PREVIOUS_KEY_ID),
];

/// Path of the discovery document, under [`ISSUER`].
pub const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
/// Path of the JWKS, under [`ISSUER`].
pub const JWKS_PATH: &str = "/.well-known/jwks.json";
/// `Cache-Control` for both documents. STS keeps its own copy of the
/// JWKS; an hour bounds how long any cache holds a stale one, which is
/// how long a new key must be published before it signs.
pub const CACHE_CONTROL: &str = "public, max-age=3600";

/// The claims [`signing_input`] puts in a token.
const CLAIMS: [&str; 6] = ["iss", "sub", "aud", "iat", "nbf", "exp"];
/// Seconds a token lives. STS uses it at once.
pub const LIFETIME: u64 = 5 * 60;

/// The subject that names a tenant.
#[must_use]
pub fn subject(tenant: &str) -> String {
    format!("tenant:{tenant}")
}

/// The signing input of a token for `tenant`, issued at `now` (Unix
/// seconds). Sign it with the key `kid` names and pass both to
/// [`jwt::token`].
#[must_use]
pub fn signing_input(kid: &str, tenant: &str, now: u64) -> String {
    jwt::signing_input(
        Some(kid),
        &serde_json::json!({
            "iss": ISSUER,
            "sub": subject(tenant),
            "aud": AUDIENCE,
            "iat": now,
            "nbf": now.saturating_sub(60),
            "exp": now + LIFETIME,
        }),
    )
}

/// `/.well-known/openid-configuration`: where the keys are and what the
/// tokens hold. The issuer only signs tokens for its own use, so there
/// are no authorization or token endpoints.
#[must_use]
pub fn discovery() -> serde_json::Value {
    serde_json::json!({
        "issuer": ISSUER,
        "jwks_uri": format!("{ISSUER}{JWKS_PATH}"),
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "claims_supported": CLAIMS,
    })
}

/// The JWKS listing `keys`, each with its `kid`.
#[must_use]
pub fn jwks(keys: &[(String, PublicKey)]) -> serde_json::Value {
    let keys: Vec<_> = keys
        .iter()
        .map(|(kid, key)| {
            serde_json::json!({
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": kid,
                "n": jwt::b64url(&key.n),
                "e": jwt::b64url(&key.e),
            })
        })
        .collect();
    serde_json::json!({ "keys": keys })
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    #[test]
    fn names_the_tenant_for_sts() {
        let input = signing_input("2026-10", "acme1234", 1_000_000);
        let (header, claims) = input.split_once('.').unwrap();
        let decode = |part: &str| -> serde_json::Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
        };
        assert_eq!(decode(header)["kid"], "2026-10");
        assert_eq!(decode(header)["alg"], "RS256");
        assert_eq!(
            decode(claims),
            serde_json::json!({
                "iss": "https://oidc.hatchrun.io",
                "sub": "tenant:acme1234",
                "aud": "sts.amazonaws.com",
                "iat": 1_000_000,
                "nbf": 999_940,
                "exp": 1_000_300,
            })
        );
        let claims = decode(claims);
        let named: Vec<_> = claims.as_object().unwrap().keys().collect();
        assert_eq!(named.len(), CLAIMS.len());
        assert!(named.iter().all(|claim| CLAIMS.contains(&claim.as_str())));
    }

    #[test]
    fn discovery_points_at_the_jwks() {
        assert_eq!(
            discovery(),
            serde_json::json!({
                "issuer": "https://oidc.hatchrun.io",
                "jwks_uri": "https://oidc.hatchrun.io/.well-known/jwks.json",
                "response_types_supported": ["id_token"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
                "claims_supported": ["iss", "sub", "aud", "iat", "nbf", "exp"],
            })
        );
    }

    #[test]
    fn lists_keys_as_unpadded_base64url() {
        let key = PublicKey {
            n: vec![0xfb, 0xef, 0xff, 0x01],
            e: vec![1, 0, 1],
        };
        let next = PublicKey {
            n: vec![0x7f],
            e: vec![3],
        };
        assert_eq!(
            jwks(&[("2026-10".to_owned(), key), ("2027-01".to_owned(), next)]),
            serde_json::json!({ "keys": [
                {
                    "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "2026-10",
                    "n": "--__AQ", "e": "AQAB",
                },
                {
                    "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "2027-01",
                    "n": "fw", "e": "Aw",
                },
            ]})
        );
        assert_eq!(jwks(&[]), serde_json::json!({ "keys": [] }));
    }
}
