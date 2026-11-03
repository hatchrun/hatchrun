// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The artifact calls between the runner agent and the control plane.
//! The agent serves GitHub's artifact service, which
//! `actions/upload-artifact` and `download-artifact` use, and turns each
//! call into one of these. Plain data, so it builds for wasm.
//!
//! All calls are `POST` to a path under [`PATH`] with the job token as a
//! bearer token, except the abort, a `DELETE`. The service answers only
//! for the run of the token's job: the run, and from it the tenant and
//! the repository, come from the job row and never from a request.
//!
//! An upload is `uploads` (start), `uploads/{id}/parts` for presigned
//! S3 part URLs as many times as needed, then `uploads/{id}/complete`.
//! Part calls have the shape of the cache service's, so the agent sends
//! them with the same client.

use serde::{Deserialize, Serialize};

/// The service's base path on the control plane.
pub const PATH: &str = "/v1/agent/artifacts";
/// The longest artifact name, in bytes.
pub const MAX_NAME: usize = 255;
/// How many artifacts and unfinished uploads one run may hold.
pub const MAX_PER_RUN: u64 = 1_000;

/// Checks an artifact name the way the toolkit does, and refuses control
/// characters too: present, at most [`MAX_NAME`] bytes, and none of
/// `" : < > | * ? \ /`.
///
/// # Errors
///
/// What is wrong, for a 400.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(format!("a name has 1 to {MAX_NAME} bytes"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| c.is_control() || "\":<>|*?\\/".contains(*c))
    {
        return Err(format!("a name cannot contain {bad:?}"));
    }
    Ok(())
}

/// `POST uploads`: reserves an artifact name and starts its S3 upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// The name the workflow gave the artifact.
    pub name: String,
}

/// A started upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResponse {
    /// Names the upload in later calls.
    pub upload_id: String,
}

/// An uploaded part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// The S3 part number, 1 to 10,000.
    pub number: u16,
    /// The `ETag` S3 answered the part with, quotes included.
    pub etag: String,
}

/// `POST uploads/{id}/complete`: puts the parts together and makes the
/// upload an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteRequest {
    /// Every part.
    pub parts: Vec<Part>,
    /// Bytes uploaded, as the toolkit counted them.
    pub size: u64,
    /// `sha256:<hex>` of the zip, when the toolkit sent one.
    #[serde(default)]
    pub digest: Option<String>,
}

/// An artifact, as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// The artifact's id.
    pub id: u64,
    /// Its name.
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// When it was finalized, in Unix milliseconds.
    pub created_at: u64,
}

/// `POST list`: the finalized artifacts of the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {
    /// Only the artifact of this name.
    #[serde(default)]
    pub name: Option<String>,
    /// Only the artifact with this id.
    #[serde(default)]
    pub id: Option<u64>,
}

/// The artifacts a list found, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResponse {
    /// The artifacts.
    pub artifacts: Vec<Artifact>,
}

/// `POST url` and `POST delete`: the artifact of this name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRequest {
    /// The artifact's name.
    pub name: String,
}

/// A presigned GET of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlResponse {
    /// Presigned GET of the artifact's bytes.
    pub url: String,
    /// Bytes.
    pub size: u64,
}

/// A deleted artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteResponse {
    /// The id the artifact had.
    pub id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_toolkit() {
        assert!(check_name("dist").is_ok());
        assert!(check_name("linux x64 (release) 1.2.3").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name(&"a".repeat(MAX_NAME)).is_ok());
        assert!(check_name(&"a".repeat(MAX_NAME + 1)).is_err());
        for bad in [
            "a/b", "a\\b", "a:b", "a\"b", "a<b", "a>b", "a|b", "a*b", "a?b", "a\nb",
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_complete_without_a_digest_parses() {
        let request: CompleteRequest =
            serde_json::from_str(r#"{"parts":[{"number":1,"etag":"\"e1\""}],"size":5}"#).unwrap();
        assert_eq!(request.digest, None);
        assert_eq!(request.parts[0].number, 1);
    }

    #[test]
    fn a_list_request_may_be_empty() {
        assert_eq!(
            serde_json::from_str::<ListRequest>("{}").unwrap(),
            ListRequest::default()
        );
    }
}
