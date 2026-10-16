// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The job cache proxy, for launches whose boot document has a `cache`
//! field.
//!
//! `actions/cache`, the `actions/setup-*` caches and `BuildKit`'s
//! `type=gha` backend talk to GitHub's cache v2 service: three Twirp
//! JSON RPCs, an Azure Blob upload to the URL `CreateCacheEntry` hands
//! out, and a download from the URL `GetCacheEntryDownloadURL` hands
//! out. The proxy serves all of it on the instance's private IP and
//! answers through `/v1/cache` on hatchrun-cloud, which keeps the
//! entries in the tenant's S3 bucket:
//!
//! - a lookup answers a download URL on the proxy for the presigned S3
//!   GET the cloud found. A plain GET of it is redirected to S3; a
//!   ranged one, which `BuildKit` sends with `x-ms-range`, is fetched from
//!   S3 with `Range` and passed through.
//! - a create starts an S3 multipart upload and answers an upload URL
//!   on the proxy. An existing entry answers 409 `already_exists`,
//!   which `BuildKit` relies on to number its index entries.
//! - the Azure upload to that URL goes to S3 as parts (see [`blob`]).
//! - a finalize completes the multipart upload.
//!
//! Every other Twirp call under `ACTIONS_RESULTS_URL`, such as the
//! artifact service `actions/upload-artifact` uses, is passed through to
//! GitHub's results service.
//!
//! The runner is pointed here by [`patch`]. A lookup that fails is a
//! miss, and a save that fails is an error the client turns into a
//! warning.

mod blob;
mod cloud;
pub mod patch;
mod twirp;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use self::blob::{Blocks, Chunker, Operation, Placement};
use self::cloud::{Cloud, Part};
use self::twirp::Rpc;
use crate::source;

/// GitHub's results service, the `ACTIONS_RESULTS_URL` the runner gets
/// on github.com.
const RESULTS_URL: &str = "https://results-receiver.actions.githubusercontent.com";
/// Request headers passed through to the results service.
const FORWARDED: [header::HeaderName; 4] = [
    header::AUTHORIZATION,
    header::CONTENT_TYPE,
    header::ACCEPT,
    header::USER_AGENT,
];

/// The port the proxy listens on.
pub const PORT: u16 = 7419;
/// How long the proxy gets to answer its health check.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Blocks held in memory at once, each up to [`blob::MAX_BLOCK`].
/// The toolkit sends eight 64 MiB blocks at a time; the rest wait.
const BLOCKS_IN_FLIGHT: usize = 4;
/// The largest Twirp or block list body.
const MAX_SMALL_BODY: usize = 4 << 20;
/// The Azure Blob version the proxy answers as.
const AZURE_VERSION: &str = "2024-11-04";
/// Upload URLs are `/cache/upload/<id>`, download URLs
/// `/cache/download/<n>`. Azure clients read the path of a URL on an IP
/// address as `/<account>/<container>/<blob>` and want all three.
const UPLOAD_PATH: &str = "/cache/upload/";
const DOWNLOAD_PATH: &str = "/cache/download/";

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Reply = Response<BoxBody<Bytes, BoxError>>;

/// A running proxy.
pub struct Proxy {
    state: Arc<State>,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

struct State {
    cloud: Cloud,
    /// `http://<ip>:<port>`.
    base: String,
    /// Where Twirp calls the proxy does not serve go.
    results: String,
    http: reqwest::Client,
    uploads: Mutex<Uploads>,
    /// Presigned GETs by the number in their download URL.
    downloads: Mutex<HashMap<u64, String>>,
    blocks: Semaphore,
    /// Numbers download URLs, request ids and entry ids.
    counter: AtomicU64,
}

impl State {
    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Uploads by their cloud id, and their cloud ids by key and version.
#[derive(Default)]
struct Uploads {
    by_id: HashMap<String, Arc<Upload>>,
    by_entry: HashMap<(String, String), String>,
}

struct Upload {
    id: String,
    state: Mutex<UploadState>,
}

#[derive(Default)]
struct UploadState {
    blocks: Blocks,
    /// The parts of the blob, once a Put Blob or Put Block List
    /// committed it.
    committed: Option<Vec<Part>>,
    /// A part failed to reach S3. A client retrying the block would not
    /// bring it back, so the upload cannot finish.
    failed: bool,
}

impl Upload {
    fn lock(&self) -> std::sync::MutexGuard<'_, UploadState> {
        self.state.lock().expect("upload lock")
    }
}

impl Proxy {
    /// Starts the proxy on `ip`, for the boot document's cache.
    ///
    /// # Errors
    ///
    /// When the cache URL is unusable or the port cannot be bound.
    pub async fn start(cache: source::Cache, ip: IpAddr) -> Result<Self> {
        Self::start_on(cache, SocketAddr::new(ip, PORT), RESULTS_URL).await
    }

    async fn start_on(cache: source::Cache, addr: SocketAddr, results: &str) -> Result<Self> {
        let cloud = Cloud::new(cache)?;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listen on {addr}"))?;
        let addr = listener.local_addr().context("cache proxy address")?;
        let base = format!("http://{addr}");
        let state = Arc::new(State {
            cloud,
            base: base.clone(),
            results: results.to_owned(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_mins(1))
                .build()
                .context("build results service client")?,
            uploads: Mutex::new(Uploads::default()),
            downloads: Mutex::new(HashMap::new()),
            blocks: Semaphore::new(BLOCKS_IN_FLIGHT),
            counter: AtomicU64::new(0),
        });
        let server = tokio::spawn(serve(listener, Arc::clone(&state)));
        Ok(Self {
            state,
            url: format!("{base}/"),
            server,
        })
    }

    /// The URL the runner passes as `ACTIONS_RESULTS_URL`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Checks that the proxy answers on its URL and the cloud takes its
    /// token, within [`HEALTH_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// When either does not.
    pub async fn check(&self) -> Result<()> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(HEALTH_TIMEOUT)
            .build()
            .context("build health check client")?;
        let response = http
            .get(format!("{}healthz", self.url))
            .send()
            .await
            .context("cache proxy health check")?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("cache proxy health check: {body}"));
        }
        Ok(())
    }

    /// Stops serving and abandons the uploads the job started and never
    /// finalized.
    pub async fn stop(self) {
        self.server.abort();
        let ids: Vec<String> = self
            .state
            .uploads
            .lock()
            .expect("uploads lock")
            .by_id
            .keys()
            .cloned()
            .collect();
        for id in ids {
            match tokio::time::timeout(Duration::from_secs(3), self.state.cloud.abort(&id)).await {
                Ok(Ok(())) => debug!(upload = id, "abandoned unfinished cache upload"),
                Ok(Err(err)) => debug!(upload = id, "abandon cache upload: {err:#}"),
                Err(_) => return,
            }
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn serve(listener: TcpListener, state: Arc<State>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                warn!("cache proxy accept: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(handle(&state, req).await) }
            });
            if let Err(err) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!("cache proxy connection: {err}");
            }
        });
    }
}

async fn handle(state: &State, req: Request<Incoming>) -> Reply {
    let path = req.uri().path().to_owned();
    if let Some(rpc) = Rpc::from_path(&path) {
        if req.method() != Method::POST {
            return twirp_error(StatusCode::NOT_FOUND, "bad_route", "use POST");
        }
        return match rpc {
            Rpc::GetCacheEntryDownloadUrl => match body(req).await {
                Ok(request) => lookup(state, request).await,
                Err(msg) => malformed(msg),
            },
            Rpc::CreateCacheEntry => match body(req).await {
                Ok(request) => create(state, request).await,
                Err(msg) => malformed(msg),
            },
            Rpc::FinalizeCacheEntryUpload => match body(req).await {
                Ok(request) => finalize(state, request).await,
                Err(msg) => malformed(msg),
            },
        };
    }
    if let Some(id) = path.strip_prefix(UPLOAD_PATH) {
        let id = id.to_owned();
        return if req.method() == Method::PUT {
            put(state, &id, req).await
        } else {
            azure_error(
                state,
                &blob::Error::new(405, "UnsupportedHttpVerb", "upload URLs take PUT"),
            )
        };
    }
    if let Some(n) = path.strip_prefix(DOWNLOAD_PATH) {
        return if req.method() == Method::GET {
            download(state, n, req.headers()).await
        } else {
            azure_error(
                state,
                &blob::Error::new(405, "UnsupportedHttpVerb", "download URLs take GET"),
            )
        };
    }
    if path == "/healthz" {
        return health(state).await;
    }
    if path.starts_with("/twirp/") {
        return forward(state, req).await;
    }
    twirp_error(StatusCode::NOT_FOUND, "bad_route", "no such route")
}

/// Passes a Twirp call through to GitHub's results service.
async fn forward(state: &State, req: Request<Incoming>) -> Reply {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let body = match Limited::new(body, MAX_SMALL_BODY).collect().await {
        Ok(body) => body.to_bytes(),
        Err(err) => return malformed(err.to_string()),
    };
    let mut request = state
        .http
        .request(parts.method, format!("{}{path}", state.results))
        .body(body);
    for name in FORWARDED {
        if let Some(value) = parts.headers.get(&name) {
            request = request.header(name, value);
        }
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(err) => {
            warn!(path, "results service unreachable: {err}");
            return twirp_error(StatusCode::BAD_GATEWAY, "unavailable", err.to_string());
        }
    };
    passthrough(upstream, &[header::CONTENT_TYPE, header::CONTENT_LENGTH])
}

/// An upstream answer with its status, body and the named headers.
fn passthrough(upstream: reqwest::Response, names: &[header::HeaderName]) -> Reply {
    let upstream: Response<reqwest::Body> = upstream.into();
    let (parts, body) = upstream.into_parts();
    let mut response = Response::new(body.map_err(|err| Box::new(err) as BoxError).boxed());
    *response.status_mut() = parts.status;
    for name in names {
        if let Some(value) = parts.headers.get(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    response
}

/// The proxy answers, and the cloud takes the token: a lookup that
/// misses is as good as one that hits.
async fn health(state: &State) -> Reply {
    match state.cloud.lookup("hatchrun-health", &[], "health").await {
        Ok(_) => reply(StatusCode::OK, "text/plain", "ok"),
        Err(err) => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain",
            format!("{err:#}"),
        ),
    }
}

/// A Twirp request body, or why it is malformed.
async fn body<T: DeserializeOwned>(req: Request<Incoming>) -> Result<T, String> {
    let bytes = Limited::new(req.into_body(), MAX_SMALL_BODY)
        .collect()
        .await
        .map_err(|err| err.to_string())?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|err| err.to_string())
}

fn malformed(msg: String) -> Reply {
    twirp_error(StatusCode::BAD_REQUEST, "malformed", msg)
}

async fn lookup(state: &State, request: twirp::LookupRequest) -> Reply {
    let found = state
        .cloud
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
            let n = state.next();
            state
                .downloads
                .lock()
                .expect("downloads lock")
                .insert(n, hit.url);
            twirp::LookupResponse {
                ok: true,
                signed_download_url: format!("{}{DOWNLOAD_PATH}{n}", state.base),
                matched_key: hit.key,
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

async fn create(state: &State, request: twirp::CreateRequest) -> Reply {
    let id = match state.cloud.start(&request.key, &request.version).await {
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
            return twirp_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                format!("{err:#}"),
            );
        }
    };
    let url = format!("{}{UPLOAD_PATH}{id}", state.base);
    let mut uploads = state.uploads.lock().expect("uploads lock");
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

async fn finalize(state: &State, request: twirp::FinalizeRequest) -> Reply {
    let entry = (request.key.clone(), request.version.clone());
    let upload = {
        let mut uploads = state.uploads.lock().expect("uploads lock");
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
        abandon(state, &upload.id).await;
        warn!(
            key = request.key,
            "cache save finalized without a whole upload"
        );
        return refuse("the upload did not finish".into());
    };
    match state.cloud.complete(&upload.id, &parts).await {
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
            abandon(state, &upload.id).await;
            refuse(format!("{err:#}"))
        }
    }
}

async fn abandon(state: &State, id: &str) {
    if let Err(err) = state.cloud.abort(id).await {
        debug!(upload = id, "abandon cache upload: {err:#}");
    }
}

/// Without a range the client is sent to S3. With one, `x-ms-range`
/// from the Azure SDKs or `Range`, S3's answer is passed through, since
/// S3 ignores `x-ms-range` and would answer the whole entry.
async fn download(state: &State, n: &str, headers: &HeaderMap) -> Reply {
    let url = n.parse::<u64>().ok().and_then(|n| {
        state
            .downloads
            .lock()
            .expect("downloads lock")
            .get(&n)
            .cloned()
    });
    let Some(url) = url else {
        return azure_error(
            state,
            &blob::Error::new(404, "BlobNotFound", "no such entry"),
        );
    };
    let range = headers
        .get("x-ms-range")
        .or_else(|| headers.get(header::RANGE))
        .and_then(|value| value.to_str().ok());
    let Some(range) = range else {
        let mut response = reply(StatusCode::TEMPORARY_REDIRECT, "text/plain", Bytes::new());
        if let Ok(location) = HeaderValue::from_str(&url) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        azure_headers(state, response.headers_mut());
        return response;
    };
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

async fn put(state: &State, id: &str, req: Request<Incoming>) -> Reply {
    let upload = state
        .uploads
        .lock()
        .expect("uploads lock")
        .by_id
        .get(id)
        .cloned();
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
        Ok(Operation::Blob) => put_blob(state, &upload, req.into_body()).await,
        Ok(Operation::Block { id }) => put_block(state, &upload, &id, req.into_body()).await,
        Ok(Operation::BlockList) => put_block_list(state, &upload, req.into_body()).await,
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

/// Uploads a part, marking the upload failed when it does not arrive.
async fn put_part(
    state: &State,
    upload: &Upload,
    number: u16,
    bytes: Bytes,
) -> Result<Part, blob::Error> {
    match state.cloud.put_part(&upload.id, number, bytes).await {
        Ok(etag) => Ok(Part { number, etag }),
        Err(err) => {
            upload.lock().failed = true;
            Err(blob::Error::new(500, "InternalError", format!("{err:#}")))
        }
    }
}

/// Streams the body to S3 in [`blob::PART_SIZE`] parts, one part
/// uploading while the next arrives.
async fn put_blob(state: &State, upload: &Upload, mut body: Incoming) -> Result<(), blob::Error> {
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
            let (cloud, id, this) = (state.cloud.clone(), upload.id.clone(), number);
            sending = Some(tokio::spawn(async move {
                let etag = cloud.put_part(&id, this, chunk).await?;
                Ok(Part { number: this, etag })
            }));
        }
    }
    parts.extend(finish_part(upload, sending.take()).await?);
    if let Some(last) = chunker.finish(number == 0) {
        number = next_part(number)?;
        parts.push(put_part(state, upload, number, last).await?);
    }
    upload.lock().committed = Some(parts);
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
    let placement = upload.lock().blocks.place(block)?;
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
            let part = put_part(state, upload, number, bytes).await?;
            upload.lock().blocks.uploaded(block, part);
        }
        Placement::Append => {
            let full = upload.lock().blocks.append(block, bytes)?;
            for (number, bytes) in full {
                let part = put_part(state, upload, number, bytes).await?;
                upload.lock().blocks.joined(part);
            }
        }
    }
    Ok(())
}

async fn put_block_list(state: &State, upload: &Upload, body: Incoming) -> Result<(), blob::Error> {
    let bytes = Limited::new(body, MAX_SMALL_BODY)
        .collect()
        .await
        .map_err(|err| blob::Error::new(413, "RequestBodyTooLarge", err.to_string()))?
        .to_bytes();
    let xml = std::str::from_utf8(&bytes)
        .map_err(|_| blob::Error::new(400, "InvalidXmlDocument", "the block list is not UTF-8"))?;
    let ids = blob::block_list(xml)?;
    let blob::Commit { mut parts, tail } = upload.lock().blocks.commit(&ids)?;
    if let Some((number, bytes)) = tail {
        parts.push(put_part(state, upload, number, bytes).await?);
    }
    upload.lock().committed = Some(parts);
    Ok(())
}

/// Headers every Azure answer carries. The Go SDK reads
/// `x-ms-request-id` without checking it is there.
fn azure_headers(state: &State, headers: &mut HeaderMap) {
    if let Ok(id) = HeaderValue::from_str(&format!("00000000-0000-0000-0000-{:012x}", state.next()))
    {
        headers.insert(HeaderName::from_static("x-ms-request-id"), id);
    }
    headers.insert(
        HeaderName::from_static("x-ms-version"),
        HeaderValue::from_static(AZURE_VERSION),
    );
}

/// The 201 Azure answers a write with.
fn created(state: &State) -> Reply {
    let mut response = reply(
        StatusCode::CREATED,
        "application/octet-stream",
        Bytes::new(),
    );
    let headers = response.headers_mut();
    azure_headers(state, headers);
    if let Ok(etag) = HeaderValue::from_str(&format!("\"0x8DC{:012X}\"", state.next())) {
        headers.insert(header::ETAG, etag);
    }
    if let Ok(date) = HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now()))
    {
        headers.insert(header::LAST_MODIFIED, date);
    }
    headers.insert(
        HeaderName::from_static("x-ms-request-server-encrypted"),
        HeaderValue::from_static("true"),
    );
    response
}

fn azure_error(state: &State, err: &blob::Error) -> Reply {
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = reply(status, "application/xml", err.xml());
    let headers = response.headers_mut();
    azure_headers(state, headers);
    headers.insert(
        HeaderName::from_static("x-ms-error-code"),
        HeaderValue::from_static(err.code),
    );
    response
}

fn json(value: &impl Serialize) -> Reply {
    match serde_json::to_vec(value) {
        Ok(body) => reply(StatusCode::OK, "application/json", body),
        Err(err) => twirp_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            err.to_string(),
        ),
    }
}

fn twirp_error(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Reply {
    let body = serde_json::to_vec(&twirp::Error {
        code,
        msg: msg.into(),
    })
    .unwrap_or_default();
    reply(status, "application/json", body)
}

fn reply(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(
        Full::new(body.into())
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use wiremock::matchers::{body_json, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const RPC: &str = "/twirp/github.actions.results.api.v1.CacheService/";

    /// A cloud that hands out part URLs on itself, and an S3 that
    /// answers each part with the `ETag` `"e<number>"`.
    async fn cloud() -> MockServer {
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/cache/uploads/[^/]+/parts$"))
            .respond_with(move |req: &wiremock::Request| {
                let numbers: Value = serde_json::from_slice(&req.body).unwrap();
                let parts: Vec<Value> = numbers["numbers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| json!({ "number": n, "url": format!("{uri}/s3/part/{n}") }))
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({ "parts": parts }))
            })
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/s3/part/\d+$"))
            .respond_with(|req: &wiremock::Request| {
                let n = req.url.path().rsplit('/').next().unwrap().to_owned();
                ResponseTemplate::new(200).insert_header("ETag", format!("\"e{n}\""))
            })
            .mount(&server)
            .await;
        server
    }

    async fn proxy(server: &MockServer) -> Proxy {
        Proxy::start_on(
            source::Cache {
                url: format!("{}/v1/cache", server.uri()),
                token: "t.o.k".into(),
            },
            SocketAddr::from(([127, 0, 0, 1], 0)),
            &server.uri(),
        )
        .await
        .unwrap()
    }

    async fn rpc(proxy: &Proxy, name: &str, body: Value) -> (u16, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}{}{name}", proxy.url(), &RPC[1..]))
            .header("Authorization", "Bearer runtime-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    async fn expect_upload(server: &MockServer, key: &str) {
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads"))
            .and(body_json(json!({ "key": key, "version": "v1" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": "up-1" })))
            .mount(server)
            .await;
    }

    async fn expect_complete(server: &MockServer, parts: Value) {
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads/up-1/complete"))
            .and(body_json(json!({ "parts": parts })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn create(proxy: &Proxy, key: &str) -> String {
        let (status, body) = rpc(
            proxy,
            "CreateCacheEntry",
            json!({ "key": key, "version": "v1" }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
        body["signed_upload_url"].as_str().unwrap().to_owned()
    }

    async fn finalize(proxy: &Proxy, key: &str) -> Value {
        let (status, body) = rpc(
            proxy,
            "FinalizeCacheEntryUpload",
            json!({ "key": key, "version": "v1", "size_bytes": "5" }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn azure_put(url: &str, query: &str, body: &'static [u8]) -> reqwest::Response {
        let url = if query.is_empty() {
            url.to_owned()
        } else {
            format!("{url}?{query}")
        };
        reqwest::Client::new()
            .put(url)
            .header("x-ms-blob-type", "BlockBlob")
            .header("x-ms-version", AZURE_VERSION)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn saves_a_blob_put_in_one_go() {
        let server = cloud().await;
        expect_upload(&server, "npm-abc").await;
        expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
        let proxy = proxy(&server).await;

        let url = create(&proxy, "npm-abc").await;
        assert!(
            url.starts_with(&format!("{}cache/upload/", proxy.url())),
            "{url}"
        );
        let response = azure_put(&url, "", b"hello").await;
        assert_eq!(response.status(), 201);
        assert!(response.headers().contains_key("x-ms-request-id"));
        assert!(response.headers().contains_key("last-modified"));

        let body = finalize(&proxy, "npm-abc").await;
        assert_eq!(body["ok"], true, "{body}");
        assert!(body["entry_id"].is_string());
    }

    #[tokio::test]
    async fn saves_toolkit_blocks_sent_out_of_order() {
        let server = cloud().await;
        expect_upload(&server, "big").await;
        expect_complete(
            &server,
            json!([{ "number": 1, "etag": "\"e1\"" }, { "number": 2, "etag": "\"e2\"" }]),
        )
        .await;
        let proxy = proxy(&server).await;
        let url = create(&proxy, "big").await;

        let id = |i: u32| STANDARD.encode(format!("{:>36}{i:012}", "prefix"));
        for (i, body) in [(1, &b"second"[..]), (0, &b"first"[..])] {
            let query = format!("comp=block&blockid={}", urlencode(&id(i)));
            assert_eq!(azure_put(&url, &query, body).await.status(), 201);
        }
        let list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{}</Latest><Latest>{}</Latest></BlockList>",
            id(0),
            id(1)
        );
        let response = reqwest::Client::new()
            .put(format!("{url}?comp=blocklist"))
            .body(list)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);

        assert_eq!(finalize(&proxy, "big").await["ok"], true);
    }

    #[tokio::test]
    async fn saves_buildkit_blocks_as_one_part() {
        let server = cloud().await;
        expect_upload(&server, "buildkit-blob-1-sha256:ab").await;
        expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
        let proxy = proxy(&server).await;
        let url = create(&proxy, "buildkit-blob-1-sha256:ab").await;

        let id = |i: u32| {
            let mut raw = [0u8; 64];
            raw[16..20].copy_from_slice(&i.to_be_bytes());
            STANDARD.encode(raw)
        };
        for i in 0..3 {
            let query = format!("comp=block&blockid={}", urlencode(&id(i)));
            assert_eq!(azure_put(&url, &query, b"layer").await.status(), 201);
        }
        let list: String = (0..3)
            .map(|i| ["<Latest>", &id(i), "</Latest>"].concat())
            .collect();
        let response = reqwest::Client::new()
            .put(format!("{url}?comp=blocklist"))
            .body(format!("<BlockList>{list}</BlockList>"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        assert_eq!(
            finalize(&proxy, "buildkit-blob-1-sha256:ab").await["ok"],
            true
        );

        let received = server.received_requests().await.unwrap();
        let part = received
            .iter()
            .find(|req| req.url.path() == "/s3/part/1")
            .unwrap();
        assert_eq!(part.body, b"layerlayerlayer");
    }

    #[tokio::test]
    async fn an_existing_entry_is_already_exists() {
        let server = cloud().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        let (status, body) = rpc(
            &proxy,
            "CreateCacheEntry",
            json!({ "key": "k", "version": "v1" }),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(body["code"], "already_exists");
    }

    #[tokio::test]
    async fn a_failed_save_is_an_error_not_already_exists() {
        let server = cloud().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        let (status, body) = rpc(
            &proxy,
            "CreateCacheEntry",
            json!({ "key": "k", "version": "v1" }),
        )
        .await;
        assert_eq!(status, 503);
        assert_eq!(body["code"], "unavailable");
    }

    #[tokio::test]
    async fn finalizing_an_unfinished_upload_abandons_it() {
        let server = cloud().await;
        expect_upload(&server, "k").await;
        Mock::given(method("DELETE"))
            .and(path("/v1/cache/uploads/up-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;
        create(&proxy, "k").await;

        assert_eq!(finalize(&proxy, "k").await["ok"], false);
    }

    #[tokio::test]
    async fn restores_from_s3() {
        let server = cloud().await;
        let entry = format!("{}/s3/entry?X-Amz-Signature=abc", server.uri());
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .and(body_json(json!({
                "key": "npm-abc", "restoreKeys": ["npm-"], "version": "v1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "key": "npm-old", "url": entry, "size": 10
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/s3/entry"))
            .and(wiremock::matchers::header("range", "bytes=4-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 4-9/10")
                    .set_body_bytes(b"456789".to_vec()),
            )
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        let (status, body) = rpc(
            &proxy,
            "GetCacheEntryDownloadURL",
            json!({ "key": "npm-abc", "restore_keys": ["npm-"], "version": "v1" }),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["matched_key"], "npm-old");
        let url = body["signed_download_url"].as_str().unwrap();

        // A plain GET goes to S3.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = http.get(url).send().await.unwrap();
        assert_eq!(response.status(), 307);
        assert_eq!(response.headers()["location"], entry.as_str());

        // A ranged one, as the Go Azure SDK sends it, comes through.
        let response = http
            .get(url)
            .header("x-ms-range", "bytes=4-")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["content-range"], "bytes 4-9/10");
        assert!(response.headers().contains_key("x-ms-request-id"));
        assert_eq!(response.bytes().await.unwrap(), &b"456789"[..]);
    }

    #[tokio::test]
    async fn a_failed_lookup_is_a_miss() {
        let server = cloud().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        let (status, body) = rpc(
            &proxy,
            "GetCacheEntryDownloadURL",
            json!({ "key": "k", "version": "v1" }),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], false);
    }

    #[tokio::test]
    async fn health_needs_the_cloud_to_take_the_token() {
        let server = cloud().await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/cache/lookup"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        proxy.check().await.unwrap();
        let err = proxy.check().await.unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
    }

    #[tokio::test]
    async fn malformed_rpcs_are_twirp_errors() {
        let server = cloud().await;
        let proxy = proxy(&server).await;
        let (status, body) = rpc(&proxy, "CreateCacheEntry", json!({ "version": "v1" })).await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "malformed");
    }

    #[tokio::test]
    async fn passes_other_twirp_calls_to_github() {
        let server = cloud().await;
        Mock::given(method("POST"))
            .and(path(
                "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
            ))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer runtime-token",
            ))
            .and(body_json(json!({ "name": "dist" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "signed_upload_url": "https://x.blob.core.windows.net/a"
            })))
            .mount(&server)
            .await;
        let proxy = proxy(&server).await;

        let response = reqwest::Client::new()
            .post(format!(
                "{}twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
                proxy.url()
            ))
            .header("Authorization", "Bearer runtime-token")
            .json(&json!({ "name": "dist" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["signed_upload_url"],
            "https://x.blob.core.windows.net/a"
        );

        // Unknown to the proxy, so wiremock's 404 rather than the
        // proxy's bad_route.
        let response = reqwest::Client::new()
            .post(format!("{}{}ListCacheEntries", proxy.url(), &RPC[1..]))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(response.text().await.unwrap(), "");
    }

    fn urlencode(text: &str) -> String {
        text.replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    }
}
