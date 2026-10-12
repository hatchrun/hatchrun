// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Claims the runner config from hatchrun-broker.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tracing::debug;

use crate::source::Broker;

/// Claims tickets over HTTPS.
pub struct HttpBroker {
    http: Client,
}

impl HttpBroker {
    /// Builds the client.
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .context("build broker client")?;
        Ok(Self { http })
    }
}

#[derive(Deserialize)]
struct Claimed {
    config: String,
}

impl Broker for HttpBroker {
    async fn claim(&self, broker: &str, ticket: &str) -> Result<Option<String>> {
        let url = claim_url(broker, ticket)?;
        let response = match self.http.post(url).send().await {
            Ok(response) => response,
            // The network may still be coming up; the caller polls again.
            Err(err) if err.is_connect() || err.is_timeout() => {
                debug!("broker unreachable: {err}");
                return Ok(None);
            }
            Err(err) => return Err(err).context("claim runner config"),
        };
        match response.status() {
            StatusCode::OK => {
                let claimed: Claimed = response.json().await.context("read claimed config")?;
                Ok(Some(claimed.config))
            }
            // Not stored yet. The scaler stores the config after the
            // instance launched, so the first claims often come too early.
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
                debug!(%status, "broker busy");
                Ok(None)
            }
            status => bail!("claim runner config: broker answered {status}"),
        }
    }
}

/// Builds the claim URL. The config is a credential, so it only travels
/// over HTTPS; plain HTTP is accepted for loopback addresses, for tests.
fn claim_url(broker: &str, ticket: &str) -> Result<Url> {
    let mut url = Url::parse(broker).with_context(|| format!("broker URL {broker:?}"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("broker URL {broker:?} must use https");
    }
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("broker URL {broker:?} cannot have a path"))?
        .pop_if_empty()
        .extend(["v1", "tickets", ticket, "claim"]);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TICKET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-_0";

    #[test]
    fn claim_urls() {
        assert_eq!(
            claim_url("https://broker.example.com", "t")
                .unwrap()
                .as_str(),
            "https://broker.example.com/v1/tickets/t/claim"
        );
        assert_eq!(
            claim_url("https://example.com/hatchrun/", "t")
                .unwrap()
                .as_str(),
            "https://example.com/hatchrun/v1/tickets/t/claim"
        );
        claim_url("http://127.0.0.1:8787", "t").unwrap();
        assert!(claim_url("http://broker.example.com", "t").is_err());
        assert!(claim_url("not a url", "t").is_err());
    }

    async fn broker(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/v1/tickets/{TICKET}/claim")))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn claims_config() {
        let server = broker(200, r#"{"config":"jit-1"}"#).await;
        let config = HttpBroker::new()
            .unwrap()
            .claim(&server.uri(), TICKET)
            .await
            .unwrap();
        assert_eq!(config.as_deref(), Some("jit-1"));
    }

    #[tokio::test]
    async fn not_stored_yet_and_server_errors_mean_retry() {
        for status in [404, 429, 503] {
            let server = broker(status, "").await;
            let config = HttpBroker::new()
                .unwrap()
                .claim(&server.uri(), TICKET)
                .await
                .unwrap();
            assert_eq!(config, None, "{status}");
        }
    }

    #[tokio::test]
    async fn other_errors_fail() {
        let server = broker(400, "").await;
        let err = HttpBroker::new()
            .unwrap()
            .claim(&server.uri(), TICKET)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"), "{err:#}");
    }

    #[tokio::test]
    async fn unreachable_broker_means_retry() {
        // Nothing listens on port 9 of loopback.
        let config = HttpBroker::new()
            .unwrap()
            .claim("http://127.0.0.1:9", TICKET)
            .await
            .unwrap();
        assert_eq!(config, None);
    }
}
