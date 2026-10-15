// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! What a hosted runner needs to report its CPU and memory to
//! hatchrun-telemetry (crates/telemetry).
//!
//! The launch stores a boot document in the broker instead of the bare
//! JIT config: the config, the telemetry URL and a token naming the
//! runner's tenant, installation, job and runner. The telemetry Worker
//! takes those from the token alone, so a runner can only report for
//! its own job.
//!
//! The token is signed with Ed25519 by its own key, [`SIGNING_KEY`].
//! The OIDC key buys credentials in customer accounts and never signs
//! anything a runner holds.

use crate::jwt;

/// Secret holding the token signing key, Ed25519 in PKCS#8 PEM.
pub const SIGNING_KEY: &str = "TELEMETRY_SIGNING_KEY";
/// Variable holding the telemetry Worker's URL.
pub const URL: &str = "TELEMETRY_URL";
/// Seconds a token lives: longer than the config waits in the broker
/// and the longest a runner instance runs
/// (`RECONCILE_MAX_INSTANCE_AGE_SECS`, eight hours) together.
pub const LIFETIME: u64 = 9 * 60 * 60;

/// Who a token reports for.
#[derive(Debug, Clone, Copy)]
pub struct Runner<'a> {
    /// The tenant id.
    pub tenant: &'a str,
    /// The App installation, which the app queries metrics by.
    pub installation_id: u64,
    /// The workflow job.
    pub job_id: u64,
    /// The runner's name.
    pub runner_name: &'a str,
}

/// The signing input of a token for `runner`, issued at `now` (Unix
/// seconds). Sign it with [`SIGNING_KEY`] and pass both to
/// [`jwt::token`].
#[must_use]
pub fn signing_input(runner: Runner<'_>, now: u64) -> String {
    jwt::eddsa_signing_input(&serde_json::json!({
        "tenant": runner.tenant,
        "installation_id": runner.installation_id,
        "job_id": runner.job_id,
        "runner_name": runner.runner_name,
        "exp": now + LIFETIME,
    }))
}

/// The document the broker hands the runner.
#[must_use]
pub fn boot_document(jit_config: &str, url: &str, token: &str) -> String {
    serde_json::json!({
        "jitConfig": jit_config,
        "telemetry": { "url": url, "token": token },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    #[test]
    fn token_names_the_runner() {
        let runner = Runner {
            tenant: "acme1234",
            installation_id: 42,
            job_id: 7,
            runner_name: "hatchrun-7-ab0100ff",
        };
        let input = signing_input(runner, 1_000_000);
        let (header, claims) = input.split_once('.').unwrap();
        let decode = |part: &str| -> serde_json::Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
        };
        assert_eq!(decode(header)["alg"], "EdDSA");
        assert_eq!(
            decode(claims),
            serde_json::json!({
                "tenant": "acme1234",
                "installation_id": 42,
                "job_id": 7,
                "runner_name": "hatchrun-7-ab0100ff",
                "exp": 1_032_400,
            })
        );
    }

    #[test]
    fn boot_document_carries_config_and_telemetry() {
        let doc: serde_json::Value = serde_json::from_str(&boot_document(
            "jit",
            "https://telemetry.hatchrun.io",
            "a.b.c",
        ))
        .unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "jitConfig": "jit",
                "telemetry": { "url": "https://telemetry.hatchrun.io", "token": "a.b.c" },
            })
        );
    }
}
