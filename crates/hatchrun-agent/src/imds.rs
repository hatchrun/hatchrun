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

    /// Returns the user data, or `None` when the instance has none.
    pub async fn user_data(&self) -> Result<Option<Vec<u8>>> {
        self.get("user-data").await
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
}

fn is_transient(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
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
    async fn reads_with_session_token() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/user-data"))
            .and(header("X-aws-ec2-metadata-token", "token-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(imds.user_data().await.unwrap(), Some(b"{}".to_vec()));
    }

    #[tokio::test]
    async fn missing_user_data_is_none() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/user-data"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert_eq!(imds.user_data().await.unwrap(), None);
    }

    #[tokio::test]
    async fn server_error_fails() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/latest/user-data"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let imds = Imds::new(&server.uri()).unwrap();
        assert!(imds.user_data().await.is_err());
    }
}
