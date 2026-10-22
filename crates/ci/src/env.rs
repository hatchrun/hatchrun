// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker's variables and secrets, and the handle on a run's
//! Durable Object.

use hatchrun_cloud::{github, jwt, oidc, polar};
use worker::{Env, Result, Stub};

/// The Durable Object namespace of runs.
pub const RUNS: &str = "RUNS";

/// What the Worker reads from its variables and secrets.
#[derive(Clone)]
pub struct Settings {
    /// Client id of the GitHub App.
    pub app_id: String,
    /// The App's private key, PKCS#8 DER.
    pub app_key: Vec<u8>,
    /// The GitHub REST API root, without a trailing slash.
    pub github_api: String,
    /// The app's origin, for links in check runs.
    pub app_url: String,
    /// This Worker's origin, which runner agents call.
    pub ci_url: String,
    /// hatchrun-broker's origin.
    pub broker_url: String,
    /// The broker's token for storing configs.
    pub broker_token: String,
    /// The HMAC key of job tokens.
    pub token_key: Vec<u8>,
    /// `production`, `staging` or `dev`.
    pub deploy_env: String,
    /// How instances start: `ec2`, or `local` in development builds.
    pub launcher: String,
    /// The local launcher daemon's URL, for the `local` launcher.
    pub local_launcher_url: Option<String>,
    /// The region of the CI AWS account.
    pub ci_region: String,
    /// The role the Worker assumes. Empty until the CI account exists.
    pub ci_role_arn: String,
    /// The key that signs the token for `AssumeRoleWithWebIdentity`,
    /// PKCS#8 DER.
    pub oidc_key: Vec<u8>,
    /// The `kid` of `oidc_key` in the JWKS.
    pub oidc_kid: String,
    /// Replaces the STS and EC2 endpoints, for development.
    pub aws_endpoint: Option<String>,
    /// The Polar API root, without a trailing slash.
    pub polar_api: String,
    /// Polar's access token. Empty until billing is set up.
    pub polar_token: String,
}

impl Settings {
    /// The settings in `env`.
    ///
    /// # Errors
    ///
    /// A required setting is missing or a key does not parse.
    pub fn read(env: &Env) -> std::result::Result<Self, String> {
        Self::from_lookup(|name| {
            env.secret(name)
                .map(|value| value.to_string())
                .ok()
                .filter(|value| !value.is_empty())
        })
    }

    /// The settings `get` finds. `get` answers `None` for a name that is
    /// not set or is empty.
    ///
    /// # Errors
    ///
    /// A required setting is missing or a key does not parse.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> std::result::Result<Self, String> {
        let need = |name: &str| get(name).ok_or_else(|| format!("{name} is not set"));
        let origin = |name: &str| need(name).map(|url| url.trim_end_matches('/').to_owned());
        let key = |name: &str| jwt::pkcs8_der(&need(name)?).map_err(|err| format!("{name}: {err}"));
        Ok(Self {
            app_id: need("GITHUB_APP_ID")?,
            app_key: key("GITHUB_APP_PRIVATE_KEY")?,
            github_api: get("GITHUB_API_URL")
                .unwrap_or_else(|| github::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            app_url: origin("APP_URL")?,
            ci_url: origin("CI_URL")?,
            broker_url: origin("BROKER_URL")?,
            broker_token: need("BROKER_TOKEN")?,
            token_key: need("CI_TOKEN_KEY")?.into_bytes(),
            deploy_env: need("DEPLOY_ENV")?,
            launcher: need("LAUNCHER")?,
            local_launcher_url: get("LOCAL_LAUNCHER_URL")
                .map(|url| url.trim_end_matches('/').to_owned()),
            ci_region: need("CI_REGION")?,
            ci_role_arn: get("CI_ROLE_ARN").unwrap_or_default(),
            oidc_key: key(oidc::SIGNING_KEY)?,
            oidc_kid: need(oidc::KEY_ID)?,
            aws_endpoint: get("AWS_ENDPOINT_URL"),
            polar_api: get("POLAR_API_URL")
                .unwrap_or_else(|| polar::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            polar_token: get("POLAR_ACCESS_TOKEN").unwrap_or_default(),
        })
    }
}

/// The Durable Object of run `run_id`, named by the id as text.
///
/// # Errors
///
/// The `RUNS` binding is missing.
pub fn run_stub(env: &Env, run_id: u64) -> Result<Stub> {
    env.durable_object(RUNS)?.get_by_name(&run_id.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // An empty DER sequence: the envelope and the first byte are all that is checked.
    const PEM: &str = "-----BEGIN PRIVATE KEY-----\nMAA=\n-----END PRIVATE KEY-----";

    fn values() -> HashMap<&'static str, String> {
        HashMap::from([
            ("GITHUB_APP_ID", "Iv1".to_owned()),
            ("GITHUB_APP_PRIVATE_KEY", PEM.to_owned()),
            ("APP_URL", "https://app.example/".to_owned()),
            ("CI_URL", "https://ci.example".to_owned()),
            ("BROKER_URL", "https://broker.example/".to_owned()),
            ("BROKER_TOKEN", "b".to_owned()),
            ("CI_TOKEN_KEY", "k".to_owned()),
            ("DEPLOY_ENV", "dev".to_owned()),
            ("LAUNCHER", "ec2".to_owned()),
            ("CI_REGION", "us-east-1".to_owned()),
            ("OIDC_SIGNING_KEY", PEM.to_owned()),
            ("OIDC_KEY_ID", "kid".to_owned()),
        ])
    }

    fn read(values: &HashMap<&'static str, String>) -> std::result::Result<Settings, String> {
        Settings::from_lookup(|name| values.get(name).cloned())
    }

    #[test]
    fn settings_read_with_the_ci_account_unset() {
        let settings = read(&values()).unwrap();
        assert_eq!(settings.app_url, "https://app.example");
        assert_eq!(settings.broker_url, "https://broker.example");
        assert_eq!(settings.github_api, github::API);
        assert_eq!(settings.token_key, b"k");
        assert_eq!(settings.ci_role_arn, "");
        assert_eq!(settings.aws_endpoint, None);
        assert_eq!(settings.local_launcher_url, None);
        assert_eq!(settings.polar_api, polar::API);
        assert_eq!(settings.polar_token, "");
    }

    #[test]
    fn development_overrides_are_read() {
        let mut v = values();
        v.insert("GITHUB_API_URL", "http://localhost:9000/".to_owned());
        v.insert("AWS_ENDPOINT_URL", "http://localhost:4566".to_owned());
        v.insert("POLAR_API_URL", "https://sandbox-api.polar.sh/".to_owned());
        let settings = read(&v).unwrap();
        assert_eq!(settings.polar_api, "https://sandbox-api.polar.sh");
        assert_eq!(settings.github_api, "http://localhost:9000");
        assert_eq!(
            settings.aws_endpoint.as_deref(),
            Some("http://localhost:4566")
        );
    }

    #[test]
    fn a_missing_secret_is_named() {
        let mut v = values();
        v.remove("CI_TOKEN_KEY");
        assert_eq!(read(&v).err().unwrap(), "CI_TOKEN_KEY is not set");
    }
}
