// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache service Twirp RPCs.

use std::future::Future;

use http_body_util::{BodyExt, Limited};
use hyper::Request;
use hyper::StatusCode;
use hyper::body::Incoming;
use serde::de::DeserializeOwned;
use tracing::{debug, info, warn};

use super::cloud::{JobEnded, Part, UploadGone};
use super::twirp;
use super::{
    DOWNLOAD_PATH, Download, MAX_SMALL_BODY, Outcome, Reply, State, UPLOAD_PATH, Upload,
    UploadState, download_token, json, lock, refusal, twirp_error,
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

/// Starts a cache save and hands out an upload URL for it.
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
            return refusal(&err);
        }
    };
    let url = format!("{}{UPLOAD_PATH}{id}", state.base);
    let replaced = lock(&state.uploads).replace(
        (request.key.clone(), request.version.clone()),
        Upload::new(id, cloud.clone()),
    );
    if let Some(old) = replaced {
        warn!(
            key = request.key,
            "cache save created again, dropping the earlier upload"
        );
        abandon(&old).await;
    }
    info!(key = request.key, "cache save started");
    json(&twirp::CreateResponse {
        ok: true,
        signed_upload_url: url,
        message: String::new(),
    })
}

/// Completes the save of the entry, once however often it is asked.
pub(super) async fn finalize(state: &State, request: twirp::FinalizeRequest) -> Reply {
    let entry = (request.key.clone(), request.version.clone());
    let upload = lock(&state.uploads).claim(&entry);
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
    let lost = format!("another job saved {} first", request.key);
    let up = &*upload;
    let outcome = settle(up, &lost, |parts| async move {
        let saved = up.cloud.complete(&up.id, &parts).await?;
        Ok(saved.then(|| state.next().to_string()))
    })
    .await;
    match outcome {
        Ok(Outcome::Saved(id)) => {
            info!(key = request.key, size = request.size_bytes, "cache saved");
            json(&twirp::FinalizeResponse {
                ok: true,
                entry_id: id,
                message: String::new(),
            })
        }
        Ok(Outcome::Refused(message)) => {
            info!(key = request.key, "cache entry not saved: {message}");
            refuse(message)
        }
        Err(err) => {
            warn!(key = request.key, "cache save failed: {err:#}");
            refusal(&err)
        }
    }
}

/// Finalizes `upload` once: `complete` gets the parts and answers the
/// id of the saved entry, `None` when it was lost to another job.
///
/// A definitive answer is kept, and a repeat or an overlapping finalize
/// gets it again with no second call. An error to retry (the control
/// plane or the network did not answer) leaves the upload as it was and
/// comes back as `Err`, for the client's retry to complete again, and so
/// does a job that ended. `lost` is the reason for a lost race.
pub(super) async fn settle<F, Fut>(
    upload: &Upload,
    lost: &str,
    complete: F,
) -> Result<Outcome, anyhow::Error>
where
    F: FnOnce(Vec<Part>) -> Fut,
    Fut: Future<Output = Result<Option<String>, anyhow::Error>>,
{
    let mut done = upload.outcome.lock().await;
    if let Some(outcome) = &*done {
        return Ok(outcome.clone());
    }
    let parts = {
        let state = upload.lock();
        if state.failed {
            None
        } else {
            state.committed.clone()
        }
    };
    let Some(parts) = parts else {
        abandon(upload).await;
        warn!(upload = upload.id, "finalized without a whole upload");
        return Ok(settled(
            upload,
            &mut done,
            "the upload did not finish".into(),
        ));
    };
    let outcome = match complete(parts).await {
        Ok(Some(id)) => Outcome::Saved(id),
        Ok(None) => Outcome::Refused(lost.to_owned()),
        Err(err) => {
            if err.downcast_ref::<UploadGone>().is_some() {
                abandon(upload).await;
                return Ok(settled(upload, &mut done, err.to_string()));
            }
            if err.downcast_ref::<JobEnded>().is_some() {
                abandon(upload).await;
                return Err(err);
            }
            if matches!(
                twirp::code_for(&err).0,
                "unavailable" | "resource_exhausted"
            ) {
                return Err(err);
            }
            abandon(upload).await;
            Outcome::Refused(format!("{err:#}"))
        }
    };
    Ok(keep(upload, &mut done, outcome))
}

/// Records a refusal as the outcome.
fn settled(upload: &Upload, done: &mut Option<Outcome>, message: String) -> Outcome {
    keep(upload, done, Outcome::Refused(message))
}

/// Records the outcome and frees what the upload buffered: it takes no
/// more data.
fn keep(upload: &Upload, done: &mut Option<Outcome>, outcome: Outcome) -> Outcome {
    *upload.lock() = UploadState {
        settled: true,
        ..UploadState::default()
    };
    done.insert(outcome).clone()
}

/// Aborts the upload on the control plane, ignoring a failure.
pub(super) async fn abandon(upload: &Upload) {
    if let Err(err) = upload.cloud.abort(&upload.id).await {
        debug!(upload = upload.id, "abandon upload: {err:#}");
    }
}
