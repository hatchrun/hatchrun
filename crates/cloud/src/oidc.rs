// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Tokens from the `oidc.hatchrun.dev` issuer.
//!
//! A customer's `ControlPlaneRole` (deploy/cloudformation/connect.yaml)
//! trusts this issuer for the audience `sts.amazonaws.com` and the
//! subject `tenant:<id>`. The Worker signs a token for the tenant and
//! trades it for the role's credentials with `AssumeRoleWithWebIdentity`.
//! STS checks the signature against the issuer's JWKS, found through
//! `/.well-known/openid-configuration`, and matches the key by `kid`.
//!
//! The signing key is an RSA private key in PEM, held as the
//! [`SIGNING_KEY`] secret, and [`KEY_ID`] names it.

use crate::jwt;

/// The issuer URL, as the customer's IAM OIDC provider names it.
pub const ISSUER: &str = "https://oidc.hatchrun.dev";
/// The audience STS expects.
pub const AUDIENCE: &str = "sts.amazonaws.com";
/// Secret holding the issuer's RSA private key in PEM.
pub const SIGNING_KEY: &str = "OIDC_SIGNING_KEY";
/// Variable holding the `kid` of [`SIGNING_KEY`] in the JWKS.
pub const KEY_ID: &str = "OIDC_KEY_ID";
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
                "iss": "https://oidc.hatchrun.dev",
                "sub": "tenant:acme1234",
                "aud": "sts.amazonaws.com",
                "iat": 1_000_000,
                "nbf": 999_940,
                "exp": 1_000_300,
            })
        );
    }
}
