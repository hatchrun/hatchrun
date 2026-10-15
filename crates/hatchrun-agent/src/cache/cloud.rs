// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Client for `/v1/cache` on hatchrun-cloud, and for the presigned S3
//! URLs it answers with.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};

use crate::broker::https_url;
use crate::source;

/// How long a cloud call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one part upload to S3 may take: 64 MiB at about 1 MiB/s.
const PART_TIMEOUT: Duration = Duration::from_mins(1);
/// How long a download may go without a byte from S3.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The entry a lookup found.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hit {
    /// The key that matched, as the job named it.
    pub key: String,
    /// Presigned GET of the entry.
    pub url: String,
    /// Bytes.
    pub size: u64,
}

/// One uploaded part, for the complete call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Part {
    /// The S3 part number, 1 to 10,000.
    pub number: u16,
    /// The `ETag` S3 answered the part with, quotes included.
    pub etag: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LookupRequest<'a> {
    key: &'a str,
    restore_keys: &'a [String],
    version: &'a str,
}

#[derive(Serialize)]
struct UploadRequest<'a> {
    key: &'a str,
    version: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    upload_id: String,
}

#[derive(Serialize)]
struct PartsRequest<'a> {
    numbers: &'a [u16],
}

#[derive(Deserialize)]
struct PartsResponse {
    parts: Vec<PartUrl>,
}

#[derive(Deserialize)]
struct PartUrl {
    number: u16,
    url: String,
}

#[derive(Serialize)]
struct CompleteRequest<'a> {
    parts: &'a [Part],
}

/// Calls `/v1/cache` with the job's cache token.
#[derive(Clone)]
pub struct Cloud {
    http: Client,
    s3: Client,
    download: Client,
    url: Url,
    token: String,
}

impl Cloud {
    /// A client for the boot document's cache.
    pub fn new(cache: source::Cache) -> Result<Self> {
        let url = https_url(&cache.url, "cache")?;
        if url.cannot_be_a_base() {
            bail!("cache URL {:?} cannot have a path", cache.url);
        }
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(CALL_TIMEOUT)
            .build()
            .context("build cache client")?;
        let s3 = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(PART_TIMEOUT)
            .build()
            .context("build S3 client")?;
        let download = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("build S3 download client")?;
        Ok(Self {
            http,
            s3,
            download,
            url,
            token: cache.token,
        })
    }

    fn endpoint(&self, path: &[&str]) -> Url {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .expect("checked in new")
            .pop_if_empty()
            .extend(path);
        url
    }

    async fn post(&self, path: &[&str], body: &impl Serialize) -> Result<reqwest::Response> {
        let what = path.join("/");
        self.http
            .post(self.endpoint(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("cache {what}"))
    }

    /// The entry to restore: `key` exactly, else the newest entry for
    /// each restore key in turn. `None` on a miss.
    pub async fn lookup(
        &self,
        key: &str,
        restore_keys: &[String],
        version: &str,
    ) -> Result<Option<Hit>> {
        let response = self
            .post(
                &["lookup"],
                &LookupRequest {
                    key,
                    restore_keys,
                    version,
                },
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(answer(response, "lookup").await?.json().await?))
    }

    /// Starts an upload. `None` when the entry exists.
    pub async fn start(&self, key: &str, version: &str) -> Result<Option<String>> {
        let response = self
            .post(&["uploads"], &UploadRequest { key, version })
            .await?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(None);
        }
        let upload: UploadResponse = answer(response, "start upload").await?.json().await?;
        Ok(Some(upload.upload_id))
    }

    /// Uploads one part through a presigned URL and returns its `ETag`.
    pub async fn put_part(&self, upload: &str, number: u16, body: Bytes) -> Result<String> {
        let response = self
            .post(
                &["uploads", upload, "parts"],
                &PartsRequest { numbers: &[number] },
            )
            .await?;
        let parts: PartsResponse = answer(response, "part URL").await?.json().await?;
        let url = parts
            .parts
            .into_iter()
            .find(|part| part.number == number)
            .ok_or_else(|| anyhow!("the cloud sent no URL for part {number}"))?
            .url;
        let response = self
            .s3
            .put(url)
            .body(body)
            .send()
            .await
            .with_context(|| format!("upload part {number}"))?;
        let response = answer(response, &format!("upload part {number}")).await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("S3 answered part {number} without an ETag"))?;
        Ok(etag.to_owned())
    }

    /// Completes an upload. `false` when another job saved the entry
    /// first.
    pub async fn complete(&self, upload: &str, parts: &[Part]) -> Result<bool> {
        let response = self
            .post(&["uploads", upload, "complete"], &CompleteRequest { parts })
            .await?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(false);
        }
        answer(response, "complete upload").await?;
        Ok(true)
    }

    /// Starts a ranged GET of a presigned URL. The response, error or
    /// not, is the caller's to pass on.
    pub async fn download(&self, url: &str, range: &str) -> Result<reqwest::Response> {
        self.download
            .get(url)
            .header(reqwest::header::RANGE, range)
            .send()
            .await
            .context("download cache entry")
    }

    /// Abandons an upload.
    pub async fn abort(&self, upload: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.endpoint(&["uploads", upload]))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("cache abort upload")?;
        answer(response, "abort upload").await?;
        Ok(())
    }
}

/// The response when it succeeded, else an error with its status and
/// the start of its body.
async fn answer(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    bail!("{what}: {status}: {body}")
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn cloud(server: &MockServer) -> Cloud {
        Cloud::new(source::Cache {
            url: format!("{}/v1/cache/", server.uri()),
            token: "t.o.k".into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn lookup_hits_and_misses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .and(header("authorization", "Bearer t.o.k"))
            .and(body_json(serde_json::json!({
                "key": "npm-abc", "restoreKeys": ["npm-"], "version": "v1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "npm-abc", "url": "https://s3.example.com/x", "size": 12
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let cloud = cloud(&server);
        assert_eq!(
            cloud
                .lookup("npm-abc", &["npm-".into()], "v1")
                .await
                .unwrap(),
            Some(Hit {
                key: "npm-abc".into(),
                url: "https://s3.example.com/x".into(),
                size: 12
            })
        );
        assert_eq!(cloud.lookup("other", &[], "v1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_existing_entry_starts_no_upload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        assert_eq!(cloud(&server).start("k", "v").await.unwrap(), None);
    }

    #[tokio::test]
    async fn uploads_a_part_through_its_presigned_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads/up-1/parts"))
            .and(body_json(serde_json::json!({ "numbers": [3] })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parts": [{ "number": 3, "url": format!("{}/s3/part-3?sig=x", server.uri()) }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/s3/part-3"))
            .and(wiremock::matchers::body_bytes(b"part bytes".to_vec()))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"e3\""))
            .mount(&server)
            .await;

        let etag = cloud(&server)
            .put_part("up-1", 3, Bytes::from_static(b"part bytes"))
            .await
            .unwrap();
        assert_eq!(etag, "\"e3\"");
    }

    #[tokio::test]
    async fn a_lost_race_completes_as_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads/up-1/complete"))
            .and(body_json(serde_json::json!({
                "parts": [{ "number": 1, "etag": "\"e1\"" }]
            })))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let parts = [Part {
            number: 1,
            etag: "\"e1\"".into(),
        }];
        assert!(!cloud(&server).complete("up-1", &parts).await.unwrap());
    }

    #[tokio::test]
    async fn errors_carry_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{\"error\":\"bad token\"}"))
            .mount(&server)
            .await;
        let err = cloud(&server).start("k", "v").await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }
}
