// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache v2 service's Twirp JSON messages, as the toolkit and
//! `BuildKit` send them.
//!
//! The toolkit sends proto field names (`restore_keys`) with 64-bit
//! integers as strings; `BuildKit` sends the same names with numbers.
//! Requests here also take the lowerCamelCase JSON names. Replies use
//! the proto names, which both clients read: `BuildKit` decodes
//! `entry_id` into a string, so it goes out as one.

use hyper::StatusCode;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use super::cloud::{JobEnded, Refused};

/// The path prefix of the service's RPCs.
pub const PREFIX: &str = "/twirp/github.actions.results.api.v1.CacheService/";

/// One of the RPCs the proxy serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    /// Reserve an entry and get where to upload it.
    CreateCacheEntry,
    /// Commit an uploaded entry.
    FinalizeCacheEntryUpload,
    /// Look up an entry to restore.
    GetCacheEntryDownloadUrl,
}

impl Rpc {
    /// The RPC a request path names, if the proxy serves it.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        match path.strip_prefix(PREFIX)? {
            "CreateCacheEntry" => Some(Self::CreateCacheEntry),
            "FinalizeCacheEntryUpload" => Some(Self::FinalizeCacheEntryUpload),
            "GetCacheEntryDownloadURL" => Some(Self::GetCacheEntryDownloadUrl),
            _ => None,
        }
    }
}

/// `CreateCacheEntryRequest`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct CreateRequest {
    /// The key to save under.
    pub key: String,
    /// The toolkit's hash of the paths and compression.
    pub version: String,
}

/// `CreateCacheEntryResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CreateResponse {
    /// Whether the entry was reserved.
    pub ok: bool,
    /// Where to upload it with the Azure Blob client.
    pub signed_upload_url: String,
    /// Why not, when `ok` is false.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// `FinalizeCacheEntryUploadRequest`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct FinalizeRequest {
    /// The key the entry was created with.
    pub key: String,
    /// The version it was created with.
    pub version: String,
    /// Bytes uploaded.
    #[serde(default, alias = "sizeBytes", deserialize_with = "int64")]
    pub size_bytes: i64,
}

/// `FinalizeCacheEntryUploadResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct FinalizeResponse {
    /// Whether the entry was saved.
    pub ok: bool,
    /// The entry's id, an int64, so a string in protobuf JSON.
    pub entry_id: String,
    /// Why not, when `ok` is false.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// `GetCacheEntryDownloadURLRequest`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct LookupRequest {
    /// The primary key.
    pub key: String,
    /// Prefixes to try in order when the key misses.
    #[serde(default, alias = "restoreKeys")]
    pub restore_keys: Vec<String>,
    /// The toolkit's hash of the paths and compression.
    pub version: String,
}

/// `GetCacheEntryDownloadURLResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct LookupResponse {
    /// Whether an entry matched.
    pub ok: bool,
    /// Where to download it.
    pub signed_download_url: String,
    /// The key that matched.
    pub matched_key: String,
}

impl LookupResponse {
    /// A miss.
    #[must_use]
    pub fn miss() -> Self {
        Self {
            ok: false,
            signed_download_url: String::new(),
            matched_key: String::new(),
        }
    }
}

/// The HTTP status Twirp gives an error code.
#[must_use]
pub fn http_status(code: &str) -> StatusCode {
    match code {
        "canceled" | "deadline_exceeded" => StatusCode::REQUEST_TIMEOUT,
        "invalid_argument" | "malformed" | "out_of_range" => StatusCode::BAD_REQUEST,
        "unauthenticated" => StatusCode::UNAUTHORIZED,
        "permission_denied" => StatusCode::FORBIDDEN,
        "not_found" | "bad_route" => StatusCode::NOT_FOUND,
        "already_exists" | "aborted" => StatusCode::CONFLICT,
        "failed_precondition" => StatusCode::PRECONDITION_FAILED,
        "resource_exhausted" => StatusCode::TOO_MANY_REQUESTS,
        "unimplemented" => StatusCode::NOT_IMPLEMENTED,
        "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The Twirp code for what a call to the control plane came to, and the
/// `Retry-After` to pass on. Only a final refusal gets a code the
/// clients do not retry: a 408 and a 429 are retried, as are 5xx and
/// anything that is not an answer of the control plane.
#[must_use]
pub fn code_for(err: &anyhow::Error) -> (&'static str, Option<String>) {
    if err.downcast_ref::<JobEnded>().is_some() {
        return ("failed_precondition", None);
    }
    let Some(refused) = err.downcast_ref::<Refused>() else {
        return ("unavailable", None);
    };
    match refused.status.as_u16() {
        401 => ("unauthenticated", None),
        403 => ("permission_denied", None),
        404 => ("not_found", None),
        400 | 422 => ("invalid_argument", None),
        409 => ("already_exists", None),
        410 => ("failed_precondition", None),
        429 => ("resource_exhausted", refused.retry_after.clone()),
        400..=499 if refused.status != StatusCode::REQUEST_TIMEOUT => ("failed_precondition", None),
        _ => ("unavailable", None),
    }
}

/// A Twirp error body.
#[derive(Debug, Serialize)]
pub struct Error {
    /// A Twirp error code, such as `malformed` or `internal`.
    pub code: &'static str,
    /// What went wrong.
    pub msg: String,
}

/// An int64 as protobuf JSON writes it, a string, or as a number.
fn int64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Int64 {
        Number(i64),
        Text(String),
    }
    match Int64::deserialize(deserializer)? {
        Int64::Number(value) => Ok(value),
        Int64::Text(text) => text.parse().map_err(de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_the_three_rpcs() {
        assert_eq!(
            Rpc::from_path(
                "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL"
            ),
            Some(Rpc::GetCacheEntryDownloadUrl)
        );
        assert_eq!(
            Rpc::from_path("/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry"),
            Some(Rpc::CreateCacheEntry)
        );
        assert_eq!(
            Rpc::from_path(
                "/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload"
            ),
            Some(Rpc::FinalizeCacheEntryUpload)
        );
        assert_eq!(
            Rpc::from_path("/twirp/github.actions.results.api.v1.CacheService/ListCacheEntries"),
            None
        );
        assert_eq!(Rpc::from_path("/CreateCacheEntry"), None);
    }

    #[test]
    fn parses_the_toolkit_lookup() {
        let request: LookupRequest = serde_json::from_str(
            r#"{"key":"npm-linux-abc","restore_keys":["npm-linux-","npm-"],"version":"8e7f"}"#,
        )
        .unwrap();
        assert_eq!(
            request,
            LookupRequest {
                key: "npm-linux-abc".into(),
                restore_keys: vec!["npm-linux-".into(), "npm-".into()],
                version: "8e7f".into(),
            }
        );
    }

    #[test]
    fn parses_json_field_names() {
        let request: LookupRequest = serde_json::from_str(
            r#"{"metadata":{"repositoryId":"1"},"key":"k","restoreKeys":["a"],"version":"v"}"#,
        )
        .unwrap();
        assert_eq!(request.restore_keys, ["a"]);
        let request: FinalizeRequest =
            serde_json::from_str(r#"{"key":"k","version":"v","sizeBytes":"42"}"#).unwrap();
        assert_eq!(request.size_bytes, 42);
    }

    #[test]
    fn parses_the_buildkit_finalize() {
        let request: FinalizeRequest = serde_json::from_str(
            r#"{"key":"buildkit-blob-1-sha256:ab","size_bytes":1048577,"version":"c0ff"}"#,
        )
        .unwrap();
        assert_eq!(request.size_bytes, 1_048_577);
    }

    #[test]
    fn a_lookup_without_restore_keys_has_none() {
        let request: LookupRequest = serde_json::from_str(r#"{"key":"k","version":"v"}"#).unwrap();
        assert_eq!(request.restore_keys, Vec::<String>::new());
    }

    #[test]
    fn sizes_are_strings_or_numbers() {
        let request: FinalizeRequest =
            serde_json::from_str(r#"{"key":"k","version":"v","size_bytes":"1073741824"}"#).unwrap();
        assert_eq!(request.size_bytes, 1 << 30);
        let request: FinalizeRequest =
            serde_json::from_str(r#"{"key":"k","version":"v","size_bytes":7}"#).unwrap();
        assert_eq!(request.size_bytes, 7);
        assert!(
            serde_json::from_str::<FinalizeRequest>(
                r#"{"key":"k","version":"v","size_bytes":"x"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_request_without_a_key_is_malformed() {
        assert!(serde_json::from_str::<CreateRequest>(r#"{"version":"v"}"#).is_err());
    }

    #[test]
    fn responses_use_proto_names() {
        let response = serde_json::to_value(CreateResponse {
            ok: true,
            signed_upload_url: "http://10.0.0.5:7419/cache/upload/u1".into(),
            message: String::new(),
        })
        .unwrap();
        assert_eq!(
            response,
            serde_json::json!({ "ok": true, "signed_upload_url": "http://10.0.0.5:7419/cache/upload/u1" })
        );
        let response = serde_json::to_value(FinalizeResponse {
            ok: true,
            entry_id: "1".into(),
            message: String::new(),
        })
        .unwrap();
        assert_eq!(response, serde_json::json!({ "ok": true, "entry_id": "1" }));
        let response = serde_json::to_value(LookupResponse::miss()).unwrap();
        assert_eq!(
            response,
            serde_json::json!({ "ok": false, "signed_download_url": "", "matched_key": "" })
        );
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn refused(status: u16, retry_after: Option<&str>) -> anyhow::Error {
        Refused::new(StatusCode::from_u16(status).unwrap(), retry_after).into()
    }

    #[test]
    fn a_final_refusal_is_a_code_the_clients_do_not_retry() {
        for (upstream, code, status) in [
            (401, "unauthenticated", 401),
            (403, "permission_denied", 403),
            (404, "not_found", 404),
            (400, "invalid_argument", 400),
            (422, "invalid_argument", 400),
            (409, "already_exists", 409),
            (410, "failed_precondition", 412),
            (413, "failed_precondition", 412),
        ] {
            let (got, retry) = code_for(&refused(upstream, None));
            assert_eq!(got, code, "{upstream}");
            assert_eq!(retry, None);
            assert_eq!(http_status(got).as_u16(), status, "{upstream}");
        }
        let (code, _) = code_for(&JobEnded.into());
        assert_eq!(
            (code, http_status(code).as_u16()),
            ("failed_precondition", 412)
        );
    }

    #[test]
    fn a_timeout_a_throttle_and_a_server_error_are_retried() {
        let (code, retry) = code_for(&refused(408, None));
        assert_eq!((code, http_status(code).as_u16()), ("unavailable", 503));
        assert_eq!(retry, None);
        let (code, retry) = code_for(&refused(429, Some("7")));
        assert_eq!(
            (code, http_status(code).as_u16()),
            ("resource_exhausted", 429)
        );
        assert_eq!(retry.as_deref(), Some("7"));
        for status in [500, 502, 503] {
            assert_eq!(code_for(&refused(status, None)).0, "unavailable");
        }
        assert_eq!(
            code_for(&anyhow::anyhow!("connection reset")).0,
            "unavailable"
        );
    }
}
