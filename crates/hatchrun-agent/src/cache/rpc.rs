// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache service Twirp RPCs.

use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Limited};
use hyper::Request;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use serde::de::DeserializeOwned;
use tracing::{debug, info, warn};

use super::twirp;
use super::{
    DOWNLOAD_PATH, Download, MAX_SMALL_BODY, Reply, State, UPLOAD_PATH, Upload, UploadState,
    download_token, json, lock, twirp_error,
};

/// A Twirp request body, or why it is malformed.
pub(super) async fn body<T: DeserializeOwned>(req: Request<Incoming>) -> Result<T, String> {
    let bytes = Limited::new(req.into_body(), MAX_SMALL_BODY)
        .collect()
        .await
        .map_err(|err| err.to_string())?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|err| err.to_string())
}

pub(super) fn malformed(msg: String) -> Reply {
    twirp_error(StatusCode::BAD_REQUEST, "malformed", msg)
}

pub(super) async fn lookup(state: &State, request: twirp::LookupRequest) -> Reply {
    let Some(cloud) = &state.cloud else {
        return json(&twirp::LookupResponse::miss());
    };
    let found = cloud
        .lookup(&request.key, &request.restore_keys, &request.version)
        .await;
    let response = match found {
        Ok(Some(hit)) => {
            info!(
                key = request.key,
                matched = hit.key,
                size = hit.size,
                "cache hit"
            );
            match download_token() {
                Ok(token) => {
                    let url = format!("{}{DOWNLOAD_PATH}{token}", state.base);
                    lock(&state.downloads).insert(
                        token,
                        Download {
                            url: hit.url,
                            size: hit.size,
                        },
                    );
                    twirp::LookupResponse {
                        ok: true,
                        signed_download_url: url,
                        matched_key: hit.key,
                    }
                }
                Err(err) => {
                    warn!(key = request.key, "answering a miss: {err:#}");
                    twirp::LookupResponse::miss()
                }
            }
        }
        Ok(None) => {
            info!(key = request.key, "cache miss");
            twirp::LookupResponse::miss()
        }
        Err(err) => {
            warn!(
                key = request.key,
                "cache lookup failed, answering a miss: {err:#}"
            );
            twirp::LookupResponse::miss()
        }
    };
    json(&response)
}

pub(super) async fn create(state: &State, request: twirp::CreateRequest) -> Reply {
    let Some(cloud) = &state.cloud else {
        return twirp_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "this job has no cache service",
        );
    };
    let id = match cloud.start(&request.key, &request.version).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            info!(key = request.key, "cache entry exists, not saving");
            return twirp_error(
                StatusCode::CONFLICT,
                "already_exists",
                format!("an entry for {} exists", request.key),
            );
        }
        Err(err) => {
            warn!(key = request.key, "cache save refused: {err:#}");
            // What the control plane refused for good is a code the
            // toolkit and `BuildKit` do not retry, with Twirp's own status
            // for it; the rest is `unavailable`.
            let (code, retry_after) = twirp::code_for(&err);
            let mut reply = twirp_error(twirp::http_status(code), code, format!("{err:#}"));
            if let Some(value) = retry_after.and_then(|v| HeaderValue::from_str(&v).ok()) {
                reply.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return reply;
        }
    };
    let url = format!("{}{UPLOAD_PATH}{id}", state.base);
    let mut uploads = lock(&state.uploads);
    if let Some(old) = uploads
        .by_entry
        .insert((request.key.clone(), request.version.clone()), id.clone())
    {
        uploads.by_id.remove(&old);
    }
    uploads.by_id.insert(
        id.clone(),
        Arc::new(Upload {
            id,
            cloud: cloud.clone(),
            state: Mutex::new(UploadState::default()),
        }),
    );
    info!(key = request.key, "cache save started");
    json(&twirp::CreateResponse {
        ok: true,
        signed_upload_url: url,
        message: String::new(),
    })
}

pub(super) async fn finalize(state: &State, request: twirp::FinalizeRequest) -> Reply {
    let entry = (request.key.clone(), request.version.clone());
    let upload = {
        let mut uploads = lock(&state.uploads);
        uploads
            .by_entry
            .remove(&entry)
            .and_then(|id| uploads.by_id.remove(&id))
    };
    let refuse = |message: String| {
        json(&twirp::FinalizeResponse {
            ok: false,
            entry_id: "0".into(),
            message,
        })
    };
    let Some(upload) = upload else {
        return refuse(format!("no upload for {}", request.key));
    };
    let parts = {
        let mut upload = upload.lock();
        if upload.failed {
            None
        } else {
            upload.committed.take()
        }
    };
    let Some(parts) = parts else {
        abandon(&upload).await;
        warn!(
            key = request.key,
            "cache save finalized without a whole upload"
        );
        return refuse("the upload did not finish".into());
    };
    match upload.cloud.complete(&upload.id, &parts).await {
        Ok(true) => {
            info!(key = request.key, size = request.size_bytes, "cache saved");
            json(&twirp::FinalizeResponse {
                ok: true,
                entry_id: state.next().to_string(),
                message: String::new(),
            })
        }
        Ok(false) => {
            info!(key = request.key, "another job saved the cache entry first");
            refuse(format!("another job saved {} first", request.key))
        }
        Err(err) => {
            warn!(key = request.key, "cache save failed: {err:#}");
            abandon(&upload).await;
            refuse(format!("{err:#}"))
        }
    }
}

pub(super) async fn abandon(upload: &Upload) {
    if let Err(err) = upload.cloud.abort(&upload.id).await {
        debug!(upload = upload.id, "abandon upload: {err:#}");
    }
}
