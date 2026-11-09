// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The artifact service's Twirp JSON messages, as `@actions/artifact`
//! (v2 and later, behind `actions/upload-artifact` and
//! `download-artifact` v4 and up) sends them.
//!
//! Requests take the proto field names the toolkit sends and the
//! lowerCamelCase JSON names. Wrapper types (`StringValue`,
//! `Int64Value`) come as the bare value or as `{"value": ...}`, and
//! int64 as a string or a number. Replies use the proto names, which
//! the toolkit's `fromJson` reads, with int64 as strings and times as
//! RFC 3339.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// The path prefix of the service's RPCs.
pub const PREFIX: &str = "/twirp/github.actions.results.api.v1.ArtifactService/";

/// One of the RPCs the proxy serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    /// Reserve an artifact name and get where to upload it.
    Create,
    /// Commit an uploaded artifact.
    Finalize,
    /// List the artifacts of the run.
    List,
    /// Get where to download an artifact.
    GetSignedUrl,
    /// Delete an artifact.
    Delete,
}

impl Rpc {
    /// The RPC a request path names, if the proxy serves it.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        match path.strip_prefix(PREFIX)? {
            "CreateArtifact" => Some(Self::Create),
            "FinalizeArtifact" => Some(Self::Finalize),
            "ListArtifacts" => Some(Self::List),
            "GetSignedArtifactURL" => Some(Self::GetSignedUrl),
            "DeleteArtifact" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// `CreateArtifactRequest`. The toolkit also sends the run and job ids,
/// the expiry and a version, none of which the proxy needs: the control
/// plane takes the run from the job token and the tenant's retention
/// sets the expiry.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct CreateRequest {
    /// The artifact's name.
    pub name: String,
}

/// `CreateArtifactResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CreateResponse {
    /// Whether the name was reserved.
    pub ok: bool,
    /// Where to upload it with the Azure Blob client.
    pub signed_upload_url: String,
}

/// `FinalizeArtifactRequest`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct FinalizeRequest {
    /// The name the artifact was created with.
    pub name: String,
    /// Bytes uploaded.
    #[serde(default, deserialize_with = "int64")]
    pub size: i64,
    /// `sha256:<hex>` of the zip.
    #[serde(default, deserialize_with = "string_value")]
    pub hash: Option<String>,
}

/// `FinalizeArtifactResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct FinalizeResponse {
    /// Whether the artifact was saved.
    pub ok: bool,
    /// The artifact's id, an int64, so a string in protobuf JSON.
    pub artifact_id: String,
}

/// `ListArtifactsRequest`.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ListRequest {
    /// The run the toolkit asks about, echoed in the answer.
    #[serde(default, alias = "workflowRunBackendId")]
    pub workflow_run_backend_id: String,
    /// The job the toolkit asks from, echoed in the answer.
    #[serde(default, alias = "workflowJobRunBackendId")]
    pub workflow_job_run_backend_id: String,
    /// Only the artifact of this name.
    #[serde(default, alias = "nameFilter", deserialize_with = "string_value")]
    pub name_filter: Option<String>,
    /// Only the artifact with this id.
    #[serde(default, alias = "idFilter", deserialize_with = "int64_value")]
    pub id_filter: Option<u64>,
}

/// An artifact of a `ListArtifactsResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Listed {
    /// The run, as the toolkit named it.
    pub workflow_run_backend_id: String,
    /// The job, as the toolkit named it.
    pub workflow_job_run_backend_id: String,
    /// The artifact's id, an int64.
    pub database_id: String,
    /// Its name.
    pub name: String,
    /// Bytes, an int64.
    pub size: String,
    /// When it was finalized, RFC 3339.
    pub created_at: String,
}

/// `ListArtifactsResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ListResponse {
    /// The artifacts.
    pub artifacts: Vec<Listed>,
}

/// `GetSignedArtifactURLRequest`, and `DeleteArtifactRequest`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct NamedRequest {
    /// The artifact's name.
    pub name: String,
}

/// `GetSignedArtifactURLResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct UrlResponse {
    /// Where to download it.
    pub signed_url: String,
}

/// `DeleteArtifactResponse`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct DeleteResponse {
    /// Whether the artifact was deleted.
    pub ok: bool,
    /// The id it had, an int64.
    pub artifact_id: String,
}

/// `millis` since the epoch as RFC 3339, as protobuf JSON writes a
/// timestamp.
#[must_use]
pub fn timestamp(millis: u64) -> String {
    humantime::format_rfc3339_millis(UNIX_EPOCH + Duration::from_millis(millis)).to_string()
}

/// The `ACTIONS_RUNTIME_TOKEN` of a job: a JWT the toolkits read and
/// never verify, so the signature is random filler; the proxy checks
/// the whole token on every artifact call.
///
/// - The artifact toolkit reads the run and job from the `scp` claim,
///   `Actions.Results:<run>:<job>`.
/// - `BuildKit`'s `type=gha` backend (go-actions-cache) refuses a token
///   without `nbf`, `exp` and `ac`, a string holding a JSON list of
///   `{Scope, Permission}` (1 read, 2 write). It writes to the scope with
///   write permission and reads each scope it may read, as GitHub issues
///   them: the first of `scopes` is read and write, the others read.
///
/// # Errors
///
/// When the system has no randomness.
pub fn runtime_token(run_id: &str, job_id: &str, scopes: &[String]) -> Result<String> {
    // The scope is `Actions.Results:<run>:<job>`, split on `:` and
    // spaces by the toolkit.
    let part = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let access: Vec<serde_json::Value> = scopes
        .iter()
        .enumerate()
        .map(|(index, scope)| {
            serde_json::json!({ "Scope": scope, "Permission": if index == 0 { 3 } else { 1 } })
        })
        .collect();
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": "hatchrun",
        "scp": format!("Actions.Results:{}:{}", part(run_id), part(job_id)),
        "ac": serde_json::Value::Array(access).to_string(),
        // A little before now for clock skew, and past any job's timeout.
        "nbf": now.saturating_sub(300),
        "exp": now + 2 * 86_400,
    });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut filler = [0u8; 32];
    getrandom::fill(&mut filler).map_err(|err| anyhow!("random runtime token: {err}"))?;
    Ok(format!(
        "{header}.{payload}.{}",
        URL_SAFE_NO_PAD.encode(filler)
    ))
}

/// A value as protobuf JSON writes a `StringValue`, bare, or in an
/// object.
#[derive(Deserialize)]
#[serde(untagged)]
enum Text {
    Plain(String),
    Wrapped { value: String },
}

/// An int64 as a string or a number, bare or wrapped.
#[derive(Deserialize)]
#[serde(untagged)]
enum Int {
    Number(i64),
    Text(String),
    Wrapped { value: Box<Int> },
}

impl Int {
    fn get<E: de::Error>(self) -> Result<i64, E> {
        match self {
            Self::Number(value) => Ok(value),
            Self::Text(text) => text.parse().map_err(E::custom),
            Self::Wrapped { value } => value.get(),
        }
    }
}

fn string_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(
        Option::<Text>::deserialize(deserializer)?.map(|text| match text {
            Text::Plain(value) | Text::Wrapped { value } => value,
        }),
    )
}

fn int64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    Int::deserialize(deserializer)?.get()
}

fn int64_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Option::<Int>::deserialize(deserializer)?
        .map(|int| {
            let value: i64 = int.get()?;
            u64::try_from(value).map_err(de::Error::custom)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_the_five_rpcs() {
        let path = |name: &str| format!("{PREFIX}{name}");
        assert_eq!(Rpc::from_path(&path("CreateArtifact")), Some(Rpc::Create));
        assert_eq!(
            Rpc::from_path(&path("FinalizeArtifact")),
            Some(Rpc::Finalize)
        );
        assert_eq!(Rpc::from_path(&path("ListArtifacts")), Some(Rpc::List));
        assert_eq!(
            Rpc::from_path(&path("GetSignedArtifactURL")),
            Some(Rpc::GetSignedUrl)
        );
        assert_eq!(Rpc::from_path(&path("DeleteArtifact")), Some(Rpc::Delete));
        assert_eq!(Rpc::from_path(&path("MigrateArtifact")), None);
        assert_eq!(
            Rpc::from_path("/twirp/github.actions.results.api.v1.CacheService/CreateArtifact"),
            None
        );
    }

    #[test]
    fn parses_the_toolkit_finalize() {
        let request: FinalizeRequest = serde_json::from_str(
            r#"{"workflow_run_backend_id":"r","workflow_job_run_backend_id":"j",
                "name":"dist","size":"1234","hash":"sha256:abcd"}"#,
        )
        .unwrap();
        assert_eq!(
            request,
            FinalizeRequest {
                name: "dist".into(),
                size: 1234,
                hash: Some("sha256:abcd".into()),
            }
        );
        // A wrapped hash, a numeric size and no hash at all.
        let request: FinalizeRequest =
            serde_json::from_str(r#"{"name":"a","size":7,"hash":{"value":"sha256:ef"}}"#).unwrap();
        assert_eq!(
            (request.size, request.hash.as_deref()),
            (7, Some("sha256:ef"))
        );
        let request: FinalizeRequest = serde_json::from_str(r#"{"name":"a"}"#).unwrap();
        assert_eq!((request.size, request.hash), (0, None));
    }

    #[test]
    fn parses_the_toolkit_list_filters() {
        let request: ListRequest = serde_json::from_str(
            r#"{"workflowRunBackendId":"r","workflowJobRunBackendId":"j",
                "nameFilter":"dist","idFilter":"42"}"#,
        )
        .unwrap();
        assert_eq!(request.workflow_run_backend_id, "r");
        assert_eq!(request.name_filter.as_deref(), Some("dist"));
        assert_eq!(request.id_filter, Some(42));
        let request: ListRequest =
            serde_json::from_str(r#"{"name_filter":{"value":"x"},"id_filter":{"value":"7"}}"#)
                .unwrap();
        assert_eq!(
            (request.name_filter.as_deref(), request.id_filter),
            (Some("x"), Some(7))
        );
        let request: ListRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(request, ListRequest::default());
        assert!(serde_json::from_str::<ListRequest>(r#"{"id_filter":"-1"}"#).is_err());
    }

    #[test]
    fn a_request_without_a_name_is_malformed() {
        assert!(serde_json::from_str::<CreateRequest>(r#"{"version":4}"#).is_err());
        assert!(serde_json::from_str::<NamedRequest>("{}").is_err());
    }

    #[test]
    fn responses_use_proto_names_and_string_int64s() {
        assert_eq!(
            serde_json::to_value(FinalizeResponse {
                ok: true,
                artifact_id: "9".into()
            })
            .unwrap(),
            serde_json::json!({ "ok": true, "artifact_id": "9" })
        );
        assert_eq!(timestamp(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn the_runtime_token_carries_the_toolkits_scope() {
        let scopes = vec!["refs/pull/4/merge".to_owned(), "refs/heads/main".to_owned()];
        let token = runtime_token("12", "build job", &scopes).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["scp"], "Actions.Results:12:build_job");
        assert_ne!(token, runtime_token("12", "build job", &scopes).unwrap());
        // BuildKit reads a JSON list in a string, and a window around now.
        let access: serde_json::Value =
            serde_json::from_str(claims["ac"].as_str().unwrap()).unwrap();
        assert_eq!(
            access,
            serde_json::json!([
                { "Scope": "refs/pull/4/merge", "Permission": 3 },
                { "Scope": "refs/heads/main", "Permission": 1 },
            ])
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(claims["nbf"].as_u64().unwrap() < now);
        assert!(claims["exp"].as_u64().unwrap() > now + 86_400);
    }
}
