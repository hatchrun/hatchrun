// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Minimal `IMDSv2` client.
//!
//! aws-config ships an `IMDS` client, but it only returns UTF-8 text, and user
//! data is often gzip.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};

use crate::source::Metadata;

/// Default instance metadata endpoint.
pub const DEFAULT_ENDPOINT: &str = "http://169.254.169.254";

const TOKEN_TTL_SECONDS: &str = "300";
const ATTEMPTS: u32 = 10;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Reads instance metadata over `IMDSv2`.
pub struct Imds {
    http: Client,
    endpoint: String,
}

impl Imds {
    /// Creates a client for the given endpoint, without a trailing slash.
    pub fn new(endpoint: &str) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(5))
            .build()
            .context("build instance metadata client")?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_owned(),
        })
    }

    /// Returns the region the instance runs in.
    pub async fn region(&self) -> Result<String> {
        self.text("meta-data/placement/region")
            .await?
            .context("instance metadata has no placement/region")
    }

    /// Returns the instance type, such as `m7i.large`.
    pub async fn instance_type(&self) -> Result<String> {
        self.text("meta-data/instance-type")
            .await?
            .context("instance metadata has no instance-type")
    }

    /// Returns the instance's primary private IPv4 address.
    pub async fn private_ip(&self) -> Result<std::net::IpAddr> {
        let ip = self
            .text("meta-data/local-ipv4")
            .await?
            .context("instance metadata has no local-ipv4")?;
        ip.trim()
            .parse()
            .with_context(|| format!("instance metadata local-ipv4 {ip:?}"))
    }

    /// Fetches a path under /latest/. Returns `None` on 404. Connection
    /// errors are retried, because the network may still be coming up when
    /// the agent starts.
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let mut attempt = 1;
        loop {
            match self.try_get(path).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < ATTEMPTS && is_transient(&err) => {
                    attempt += 1;
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(err) => return Err(err).with_context(|| format!("instance metadata {path}")),
            }
        }
    }

    async fn try_get(&self, path: &str) -> reqwest::Result<Option<Vec<u8>>> {
        let token = self
            .http
            .put(format!("{}/latest/api/token", self.endpoint))
            .header("X-aws-ec2-metadata-token-ttl-seconds", TOKEN_TTL_SECONDS)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let response = self
            .http
            .get(format!("{}/latest/{path}", self.endpoint))
            .header("X-aws-ec2-metadata-token", token)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.error_for_status()?.bytes().await?;
        Ok(Some(body.to_vec()))
    }

    async fn text(&self, path: &str) -> Result<Option<String>> {
        self.get(path)
            .await?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .with_context(|| format!("instance metadata {path} is not UTF-8"))
            })
            .transpose()
    }
}

fn is_transient(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

impl Metadata for Imds {
    async fn instance_id(&self) -> Result<String> {
        self.text("meta-data/instance-id")
            .await?
            .context("instance metadata has no instance-id")
    }

    async fn user_data(&self) -> Result<Option<Vec<u8>>> {
        self.get("user-data").await
    }

    // Metadata answers 404 both for a missing tag and when the launch
    // template does not expose tags. Both read as not set.
    async fn tag(&self, key: &str) -> Result<Option<String>> {
        self.text(&format!("meta-data/tags/instance/{key}")).await
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/latest/api/token"))
            .and(header(
                "X-aws-ec2-metadata-token-ttl-seconds",
                TOKEN_TTL_SECONDS,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("token-1"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn reads_metadata_with_session_token() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/instance-id"))
            .and(header("X-aws-ec2-metadata-token", "token-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("i-0123"))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(imds.instance_id().await.unwrap(), "i-0123");
    }

    #[tokio::test]
    async fn reads_the_private_ip() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/local-ipv4"))
            .respond_with(ResponseTemplate::new(200).set_body_string("10.0.3.17"))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(
            imds.private_ip().await.unwrap(),
            std::net::IpAddr::from([10, 0, 3, 17])
        );
    }

    #[tokio::test]
    async fn missing_tag_is_none() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path(
                "/latest/meta-data/tags/instance/hatchrun:jit-parameter",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(imds.tag("hatchrun:jit-parameter").await.unwrap(), None);
    }

    #[tokio::test]
    async fn user_data_keeps_binary_bytes() {
        let server = server().await;
        let gzip_header = vec![0x1f, 0x8b, 0x08, 0x00, 0xff];
        Mock::given(method("GET"))
            .and(path("/latest/user-data"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip_header.clone()))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(imds.user_data().await.unwrap(), Some(gzip_header));
    }

    #[tokio::test]
    async fn server_error_fails() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/placement/region"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert!(imds.region().await.is_err());
    }
}
