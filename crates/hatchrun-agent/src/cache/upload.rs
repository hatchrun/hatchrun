// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Azure Blob upload to the URL a create hands out.

use std::sync::MutexGuard;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::Request;
use hyper::body::Incoming;
use tracing::warn;

use super::blob::{Chunker, Operation, Placement};
use super::cloud::Part;
use super::{MAX_SMALL_BODY, Reply, State, Upload, UploadState, azure_error, blob, created, lock};

pub(super) async fn put(state: &State, id: &str, req: Request<Incoming>) -> Reply {
    // An upload that is finalizing or settled takes no more data.
    let upload = {
        let uploads = lock(&state.uploads);
        uploads
            .by_id
            .get(id)
            .filter(|upload| upload.idle())
            .cloned()
    };
    let Some(upload) = upload else {
        return azure_error(
            state,
            &blob::Error::new(404, "BlobNotFound", "no such upload"),
        );
    };
    let result = match blob::operation(req.uri().query()) {
        Ok(_) if upload.lock().failed => Err(blob::Error::new(
            500,
            "InternalError",
            "an earlier part of this upload failed",
        )),
        Ok(Operation::Blob) => put_blob(&upload, req.into_body()).await,
        Ok(Operation::Block { id }) => put_block(state, &upload, &id, req.into_body()).await,
        Ok(Operation::BlockList) => put_block_list(&upload, req.into_body()).await,
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => created(state),
        Err(err) => {
            warn!(upload = upload.id, "cache upload: {}", err.message);
            azure_error(state, &err)
        }
    }
}

/// The upload's state for a write, or the 404 of an upload that has
/// settled meanwhile: the data is dropped.
fn open(upload: &Upload) -> Result<MutexGuard<'_, UploadState>, blob::Error> {
    let state = upload.lock();
    if state.settled {
        return Err(blob::Error::new(404, "BlobNotFound", "no such upload"));
    }
    Ok(state)
}

/// Uploads a part, marking the upload failed when it does not arrive.
async fn put_part(upload: &Upload, number: u16, bytes: Bytes) -> Result<Part, blob::Error> {
    match upload.cloud.put_part(&upload.id, number, bytes).await {
        Ok(etag) => Ok(Part { number, etag }),
        Err(err) => {
            upload.lock().failed = true;
            Err(blob::Error::new(500, "InternalError", format!("{err:#}")))
        }
    }
}

/// Streams the body to S3 in [`blob::PART_SIZE`] parts, one part
/// uploading while the next arrives.
async fn put_blob(upload: &Upload, mut body: Incoming) -> Result<(), blob::Error> {
    let mut chunker = Chunker::new(blob::PART_SIZE);
    let mut parts = Vec::new();
    let mut sending: Option<tokio::task::JoinHandle<Result<Part>>> = None;
    let mut number: u16 = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| blob::Error::new(400, "InvalidInput", err.to_string()))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        for chunk in chunker.push(data) {
            parts.extend(finish_part(upload, sending.take()).await?);
            number = next_part(number)?;
            let (cloud, id, this) = (upload.cloud.clone(), upload.id.clone(), number);
            sending = Some(tokio::spawn(async move {
                let etag = cloud.put_part(&id, this, chunk).await?;
                Ok(Part { number: this, etag })
            }));
        }
    }
    parts.extend(finish_part(upload, sending.take()).await?);
    if let Some(last) = chunker.finish(number == 0) {
        number = next_part(number)?;
        parts.push(put_part(upload, number, last).await?);
    }
    open(upload)?.committed = Some(parts);
    Ok(())
}

fn next_part(number: u16) -> Result<u16, blob::Error> {
    if number >= blob::MAX_PART {
        return Err(blob::Error::new(
            413,
            "RequestBodyTooLarge",
            "the blob is too large",
        ));
    }
    Ok(number + 1)
}

/// Waits for the part in flight, if any.
async fn finish_part(
    upload: &Upload,
    sending: Option<tokio::task::JoinHandle<Result<Part>>>,
) -> Result<Option<Part>, blob::Error> {
    let Some(sending) = sending else {
        return Ok(None);
    };
    match sending
        .await
        .map_err(anyhow::Error::from)
        .and_then(|part| part)
    {
        Ok(part) => Ok(Some(part)),
        Err(err) => {
            upload.lock().failed = true;
            Err(blob::Error::new(500, "InternalError", format!("{err:#}")))
        }
    }
}

/// Uploads an indexed block as its part, or appends a sequential one
/// and uploads the parts it filled.
async fn put_block(
    state: &State,
    upload: &Upload,
    block: &str,
    body: Incoming,
) -> Result<(), blob::Error> {
    let placement = open(upload)?.blocks.place(block)?;
    let _permit = state
        .blocks
        .acquire()
        .await
        .map_err(|_| blob::Error::new(503, "ServerBusy", "shutting down"))?;
    let bytes = Limited::new(body, blob::MAX_BLOCK)
        .collect()
        .await
        .map_err(|err| blob::Error::new(413, "RequestBodyTooLarge", err.to_string()))?
        .to_bytes();
    match placement {
        Placement::Part(number) => {
            let part = put_part(upload, number, bytes).await?;
            open(upload)?.blocks.uploaded(block, part);
        }
        Placement::Append => {
            let full = open(upload)?.blocks.append(block, bytes)?;
            for (number, bytes) in full {
                let part = put_part(upload, number, bytes).await?;
                open(upload)?.blocks.joined(part);
            }
        }
    }
    Ok(())
}

async fn put_block_list(upload: &Upload, body: Incoming) -> Result<(), blob::Error> {
    let bytes = Limited::new(body, MAX_SMALL_BODY)
        .collect()
        .await
        .map_err(|err| blob::Error::new(413, "RequestBodyTooLarge", err.to_string()))?
        .to_bytes();
    let xml = std::str::from_utf8(&bytes)
        .map_err(|_| blob::Error::new(400, "InvalidXmlDocument", "the block list is not UTF-8"))?;
    let ids = blob::block_list(xml)?;
    let blob::Commit { mut parts, tail } = open(upload)?.blocks.commit(&ids)?;
    if let Some((number, bytes)) = tail {
        parts.push(put_part(upload, number, bytes).await?);
    }
    open(upload)?.committed = Some(parts);
    Ok(())
}
