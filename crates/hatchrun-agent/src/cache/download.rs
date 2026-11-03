// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The download URLs the lookup hands out.

use bytes::Bytes;
use hyper::StatusCode;
use hyper::header::{self, HeaderMap, HeaderValue};
use tracing::warn;

use super::{Reply, State, azure_error, azure_headers, blob, lock, passthrough, reply};

/// The presigned GET and size of the entry behind a download token.
fn entry(state: &State, token: &str) -> Option<(String, u64)> {
    lock(&state.downloads)
        .get(token)
        .map(|download| (download.url.clone(), download.size))
}

fn no_entry(state: &State) -> Reply {
    azure_error(
        state,
        &blob::Error::new(404, "BlobNotFound", "no such entry"),
    )
}

/// Streams the entry from S3. A range, `x-ms-range` from the Azure SDKs
/// or `Range`, goes to S3 as `Range`, since S3 ignores `x-ms-range`.
/// S3's status, 200 or 206, and its length and range headers come back
/// with the body; the toolkit checks the file it wrote against
/// `Content-Length`.
pub(super) async fn download(state: &State, token: &str, headers: &HeaderMap) -> Reply {
    let Some((url, _)) = entry(state, token) else {
        return no_entry(state);
    };
    let range = headers
        .get("x-ms-range")
        .or_else(|| headers.get(header::RANGE))
        .and_then(|value| value.to_str().ok());
    let upstream = match state.cloud.download(&url, range).await {
        Ok(upstream) => upstream,
        Err(err) => {
            warn!("cache download: {err:#}");
            return azure_error(
                state,
                &blob::Error::new(500, "InternalError", format!("{err:#}")),
            );
        }
    };
    if !upstream.status().is_success() {
        warn!(status = %upstream.status(), range, "cache download refused by S3");
    }
    let mut response = passthrough(
        upstream,
        &[
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::CONTENT_TYPE,
            header::ETAG,
            header::LAST_MODIFIED,
            header::ACCEPT_RANGES,
        ],
    );
    let headers = response.headers_mut();
    headers.insert("x-ms-blob-type", HeaderValue::from_static("BlockBlob"));
    azure_headers(state, headers);
    response
}

/// The entry's length, from the lookup: a presigned GET does not take
/// HEAD. The toolkit's concurrent download sizes the entry this way.
pub(super) fn head(state: &State, token: &str) -> Reply {
    let Some((_, size)) = entry(state, token) else {
        return no_entry(state);
    };
    let mut response = reply(StatusCode::OK, "application/octet-stream", Bytes::new());
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert("x-ms-blob-type", HeaderValue::from_static("BlockBlob"));
    azure_headers(state, headers);
    response
}
