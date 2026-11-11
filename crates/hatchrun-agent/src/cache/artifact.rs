// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The artifact service Twirp RPCs, for jobs whose document has an
//! `artifacts` service.
//!
//! `actions/upload-artifact` creates an artifact, uploads a zip to the
//! Azure Blob URL the create hands out, and finalizes it. The upload is
//! the same Azure-over-S3-multipart mapping the cache uses (see
//! [`super::blob`] and [`super::upload`]); only the control plane calls
//! differ, so each RPC here becomes one call to
//! [`hatchrun_workflow::artifacts`] on the control plane with the job's
//! token. `download-artifact` lists, asks for a signed URL and downloads
//! it; the URL is a download URL of this proxy for the presigned S3 GET,
//! like a cache hit.

use anyhow::{Result, anyhow};
use hatchrun_workflow::artifacts as wire;
use hyper::body::Incoming;
use hyper::{Method, Request, StatusCode, header};
use serde::Deserialize;
use tracing::{info, warn};

use super::artifact_twirp::{
    self as twirp, CreateRequest, CreateResponse, DeleteResponse, FinalizeRequest,
    FinalizeResponse, ListRequest, ListResponse, Listed, NamedRequest, Rpc, UrlResponse,
};
use super::cloud::{Cloud, Part, UploadGone, answer};
use super::rpc::{abandon, body, malformed, settle};
use super::{
    Artifacts, DOWNLOAD_PATH, Download, Outcome, Reply, State, UPLOAD_PATH, Upload, download_token,
    json, lock, refusal, twirp_error,
};

/// The version an artifact upload has in the upload table, which cache
/// entries key by their own versions.
const ARTIFACT_VERSION: &str = "artifact\0";

/// Serves one artifact RPC.
pub(super) async fn serve(
    state: &State,
    artifacts: &Artifacts,
    rpc: Rpc,
    req: Request<Incoming>,
) -> Reply {
    if req.method() != Method::POST {
        return twirp_error(StatusCode::NOT_FOUND, "bad_route", "use POST");
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer != Some(artifacts.token.as_str()) {
        return twirp_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bad runtime token",
        );
    }
    let cloud = &artifacts.cloud;
    match rpc {
        Rpc::Create => match body(req).await {
            Ok(request) => create(state, cloud, request).await,
            Err(msg) => malformed(msg),
        },
        Rpc::Finalize => match body(req).await {
            Ok(request) => finalize(state, request).await,
            Err(msg) => malformed(msg),
        },
        Rpc::List => match body(req).await {
            Ok(request) => list(cloud, request).await,
            Err(msg) => malformed(msg),
        },
        Rpc::GetSignedUrl => match body(req).await {
            Ok(request) => signed_url(state, cloud, request).await,
            Err(msg) => malformed(msg),
        },
        Rpc::Delete => match body(req).await {
            Ok(request) => delete(cloud, request).await,
            Err(msg) => malformed(msg),
        },
    }
}

/// Reserves the name and hands out an upload URL for it.
async fn create(state: &State, cloud: &Cloud, request: CreateRequest) -> Reply {
    if let Err(msg) = wire::check_name(&request.name) {
        return twirp_error(StatusCode::BAD_REQUEST, "invalid_argument", msg);
    }
    let id = match cloud.start_artifact(&request.name).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            info!(name = request.name, "artifact exists, not saving");
            return twirp_error(
                StatusCode::CONFLICT,
                "already_exists",
                format!(
                    "an artifact named {} exists in this run; delete it first or upload with overwrite",
                    request.name
                ),
            );
        }
        Err(err) => {
            warn!(name = request.name, "artifact upload refused: {err:#}");
            return refusal(&err);
        }
    };
    let url = format!("{}{UPLOAD_PATH}{id}", state.base);
    let replaced = lock(&state.uploads).replace(
        (request.name.clone(), ARTIFACT_VERSION.to_owned()),
        Upload::new(id, cloud.clone()),
    );
    // A client that creates the name again has given up on the earlier
    // upload; its S3 multipart upload and pending row would stay. One
    // that is finalizing or settled is not given up: its answer may be on
    // the way or lost.
    if let Some(old) = replaced {
        warn!(
            name = request.name,
            "artifact created again, dropping the earlier upload"
        );
        abandon(&old).await;
    }
    info!(name = request.name, "artifact upload started");
    json(&CreateResponse {
        ok: true,
        signed_upload_url: url,
    })
}

/// Completes the upload of the name, once however often it is asked.
async fn finalize(state: &State, request: FinalizeRequest) -> Reply {
    let entry = (request.name.clone(), ARTIFACT_VERSION.to_owned());
    let upload = lock(&state.uploads).claim(&entry);
    let refuse = || {
        json(&FinalizeResponse {
            ok: false,
            artifact_id: "0".into(),
        })
    };
    let Some(upload) = upload else {
        warn!(name = request.name, "no artifact upload to finalize");
        return refuse();
    };
    let size = u64::try_from(request.size).unwrap_or(0);
    let lost = format!("another job saved {} first", request.name);
    let up = &*upload;
    let hash = request.hash.as_deref();
    let outcome = settle(up, &lost, |parts| async move {
        let saved = up
            .cloud
            .complete_artifact(&up.id, &parts, size, hash)
            .await?;
        Ok(saved.map(|id| id.to_string()))
    })
    .await;
    match outcome {
        Ok(Outcome::Saved(id)) => {
            info!(name = request.name, size, "artifact saved");
            json(&FinalizeResponse {
                ok: true,
                artifact_id: id,
            })
        }
        Ok(Outcome::Refused(message)) => {
            info!(name = request.name, "artifact not saved: {message}");
            refuse()
        }
        Err(err) => {
            warn!(name = request.name, "artifact save failed: {err:#}");
            refusal(&err)
        }
    }
}

async fn list(cloud: &Cloud, request: ListRequest) -> Reply {
    let found = cloud
        .list_artifacts(request.name_filter.as_deref(), request.id_filter)
        .await;
    match found {
        Ok(artifacts) => json(&ListResponse {
            artifacts: artifacts
                .into_iter()
                .map(|artifact| Listed {
                    workflow_run_backend_id: request.workflow_run_backend_id.clone(),
                    workflow_job_run_backend_id: request.workflow_job_run_backend_id.clone(),
                    database_id: artifact.id.to_string(),
                    name: artifact.name,
                    size: artifact.size.to_string(),
                    created_at: twirp::timestamp(artifact.created_at),
                })
                .collect(),
        }),
        Err(err) => {
            warn!("artifact list failed: {err:#}");
            refusal(&err)
        }
    }
}

async fn signed_url(state: &State, cloud: &Cloud, request: NamedRequest) -> Reply {
    let found = match cloud.artifact_url(&request.name).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return twirp_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no artifact named {}", request.name),
            );
        }
        Err(err) => {
            warn!(name = request.name, "artifact url failed: {err:#}");
            return refusal(&err);
        }
    };
    let token = match download_token() {
        Ok(token) => token,
        Err(err) => {
            return twirp_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("{err:#}"),
            );
        }
    };
    let url = format!("{}{DOWNLOAD_PATH}{token}", state.base);
    lock(&state.downloads).insert(
        token,
        Download {
            url: found.url,
            size: found.size,
        },
    );
    json(&UrlResponse { signed_url: url })
}

async fn delete(cloud: &Cloud, request: NamedRequest) -> Reply {
    match cloud.delete_artifact(&request.name).await {
        Ok(Some(id)) => {
            info!(name = request.name, "artifact deleted");
            json(&DeleteResponse {
                ok: true,
                artifact_id: id.to_string(),
            })
        }
        Ok(None) => twirp_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no artifact named {}", request.name),
        ),
        Err(err) => {
            warn!(name = request.name, "artifact delete failed: {err:#}");
            refusal(&err)
        }
    }
}

#[derive(Deserialize)]
struct Done {
    id: u64,
}

/// The calls to the control plane's artifact service.
impl Cloud {
    /// Reserves `name` and starts its upload. `None` when the run has
    /// an artifact of that name.
    async fn start_artifact(&self, name: &str) -> Result<Option<String>> {
        let request = wire::StartRequest {
            name: name.to_owned(),
        };
        let response = self.post(&["uploads"], &request).await?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(None);
        }
        let started: wire::StartResponse = answer(response, "start artifact").await?.json().await?;
        Ok(Some(started.upload_id))
    }

    /// Completes an upload and makes it an artifact; its id, or `None`
    /// when the name was taken first; a 404 is [`UploadGone`].
    async fn complete_artifact(
        &self,
        upload: &str,
        parts: &[Part],
        size: u64,
        digest: Option<&str>,
    ) -> Result<Option<u64>> {
        let request = wire::CompleteRequest {
            parts: parts
                .iter()
                .map(|part| wire::Part {
                    number: part.number,
                    etag: part.etag.clone(),
                })
                .collect(),
            size,
            digest: digest.map(str::to_owned),
        };
        let response = self
            .post(&["uploads", upload, "complete"], &request)
            .await?;
        match response.status() {
            StatusCode::CONFLICT => return Ok(None),
            StatusCode::NOT_FOUND => return Err(UploadGone.into()),
            _ => {}
        }
        let done: Done = answer(response, "complete artifact").await?.json().await?;
        Ok(Some(done.id))
    }

    /// The finalized artifacts of the run.
    async fn list_artifacts(
        &self,
        name: Option<&str>,
        id: Option<u64>,
    ) -> Result<Vec<wire::Artifact>> {
        let request = wire::ListRequest {
            name: name.map(str::to_owned),
            id,
        };
        let response = self.post(&["list"], &request).await?;
        let listed: wire::ListResponse = answer(response, "list artifacts").await?.json().await?;
        Ok(listed.artifacts)
    }

    /// A presigned GET of the artifact named `name`, `None` when there
    /// is none.
    async fn artifact_url(&self, name: &str) -> Result<Option<wire::UrlResponse>> {
        let request = wire::NameRequest {
            name: name.to_owned(),
        };
        let response = self.post(&["url"], &request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let found = answer(response, "artifact url")
            .await?
            .json()
            .await
            .map_err(|err| anyhow!("artifact url: {err}"))?;
        Ok(Some(found))
    }

    /// Deletes the artifact named `name`; its id, or `None` when there
    /// is none.
    async fn delete_artifact(&self, name: &str) -> Result<Option<u64>> {
        let request = wire::NameRequest {
            name: name.to_owned(),
        };
        let response = self.post(&["delete"], &request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let deleted: wire::DeleteResponse =
            answer(response, "delete artifact").await?.json().await?;
        Ok(Some(deleted.id))
    }
}
