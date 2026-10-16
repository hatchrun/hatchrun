// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Stores runner configs in hatchrun-broker for instances to claim.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hatchrun_scaleset::Secret;
use reqwest::{StatusCode, Url};
use tracing::warn;

/// How long a config waits for its instance to claim it. The agent gives
/// up after ten minutes, so anything left after this is never claimed.
const EXPIRES_IN: u64 = 15 * 60;
/// Attempts at storing a config when the broker is unreachable or busy.
const ATTEMPTS: u32 = 3;

/// Keeps configs until their instance claims them.
pub trait ConfigStore {
    /// The broker URL instances claim from.
    fn url(&self) -> &str;
    /// Stores the JIT config under the ticket, in a boot document.
    fn store(&self, ticket: &str, config: &Secret) -> impl Future<Output = Result<()>>;
}

/// Talks to a hatchrun-broker deployment.
pub struct Broker {
    http: reqwest::Client,
    url: String,
    token: Secret,
}

impl Broker {
    /// Connects to the broker at `url` with the token it was deployed
    /// with. The URL must be HTTPS; plain HTTP is accepted for loopback.
    pub fn new(url: &str, token: Secret) -> Result<Self> {
        let parsed = Url::parse(url).with_context(|| format!("broker URL {url:?}"))?;
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
            bail!("broker URL {url:?} must use https");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .context("build broker client")?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    async fn try_store(&self, ticket: &str, config: &Secret) -> Result<Option<StatusCode>> {
        let response = self
            .http
            .put(format!("{}/v1/tickets/{ticket}", self.url))
            .bearer_auth(self.token.expose())
            .json(&serde_json::json!({
                "config": boot_document(config.expose()),
                "expiresIn": EXPIRES_IN,
            }))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) if err.is_connect() || err.is_timeout() => {
                warn!("broker unreachable: {err}");
                return Ok(None);
            }
            Err(err) => return Err(err).context("store runner config"),
        };
        let status = response.status();
        if status.is_success() {
            return Ok(Some(status));
        }
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            warn!(%status, "broker busy");
            return Ok(None);
        }
        let body = response.text().await.unwrap_or_default();
        bail!("store runner config: broker answered {status}: {body}");
    }
}

impl ConfigStore for Broker {
    fn url(&self) -> &str {
        &self.url
    }

    async fn store(&self, ticket: &str, config: &Secret) -> Result<()> {
        for attempt in 1..=ATTEMPTS {
            if self.try_store(ticket, config).await?.is_some() {
                return Ok(());
            }
            if attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
            }
        }
        bail!("store runner config: broker unavailable after {ATTEMPTS} attempts")
    }
}

/// Returns a new ticket: 32 random bytes in unpadded base64url.
pub fn new_ticket() -> Result<String> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("generate ticket"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The user data the agent reads to find its config.
pub fn user_data(broker: &str, ticket: &str) -> String {
    serde_json::json!({ "broker": broker, "ticket": ticket }).to_string()
}

/// What the agent claims from the broker. hatchrun.io adds where to
/// report metrics; a self-hosted scaler reports nothing.
fn boot_document(jit_config: &str) -> String {
    serde_json::json!({ "jitConfig": jit_config }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_random_and_url_safe() {
        let a = new_ticket().unwrap();
        let b = new_ticket().unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn broker_url_must_be_https() {
        Broker::new("https://broker.example.com/", Secret::new("t")).unwrap();
        Broker::new("http://127.0.0.1:8787", Secret::new("t")).unwrap();
        assert!(Broker::new("http://broker.example.com", Secret::new("t")).is_err());
    }

    #[test]
    fn user_data_holds_no_config() {
        assert_eq!(
            user_data("https://b.example", "t-1"),
            r#"{"broker":"https://b.example","ticket":"t-1"}"#
        );
    }

    #[test]
    fn boot_document_has_no_telemetry() {
        assert_eq!(boot_document("jit-1"), r#"{"jitConfig":"jit-1"}"#);
    }
}
