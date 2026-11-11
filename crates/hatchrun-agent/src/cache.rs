// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The job cache proxy, for jobs whose document has a `cache` field.
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
//!   GET the cloud found. A GET of it is fetched from S3 and streamed
//!   back, with `Range` or `BuildKit`'s `x-ms-range` sent on as `Range`.
//!   The toolkit's HTTP client is built for the proxy's `http:` URL and
//!   cannot follow a redirect to S3's `https:` one.
//! - a create starts an S3 multipart upload and answers an upload URL
//!   on the proxy. An existing entry answers 409 `already_exists`,
//!   which `BuildKit` relies on to number its index entries.
//! - the Azure upload to that URL goes to S3 as parts (see [`blob`]).
//! - a finalize completes the multipart upload.
//!
//! A job also gets the artifact service
//! `actions/upload-artifact` and `download-artifact` use (see
//! [`artifact`]): the same Azure Blob upload and download URLs, backed by
//! the control plane's `/v1/agent/artifacts`. Without an artifact
//! service, every other Twirp call under `ACTIONS_RESULTS_URL` is passed
//! through to GitHub's results service.
//!
//! A lookup that fails is a miss, and a save that fails is an error the client turns into a
//! warning.

mod artifact;
mod artifact_twirp;
mod blob;
mod cloud;
mod download;
mod rpc;
mod twirp;
mod upload;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use self::blob::Blocks;
use self::cloud::{Cloud, Part};
use self::download::{download, head};
use self::rpc::{body, create, finalize, lookup, malformed};
use self::twirp::Rpc;
use self::upload::put;
use crate::source;

/// GitHub's results service, the `ACTIONS_RESULTS_URL` on github.com.
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
/// `/cache/download/<token>`. Azure clients read the path of a URL on an IP
/// address as `/<account>/<container>/<blob>` and want all three.
const UPLOAD_PATH: &str = "/cache/upload/";
const DOWNLOAD_PATH: &str = "/cache/download/";

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Reply = Response<BoxBody<Bytes, BoxError>>;

/// The services a proxy fronts.
#[derive(Clone)]
pub struct Services {
    /// The job cache, when the job has one.
    pub cache: Option<source::Cache>,
    /// The artifact service, when the job has one.
    pub artifacts: Option<ArtifactConfig>,
    /// Who the steps' runtime token names.
    pub identity: Identity,
}

/// What the runtime token says: the toolkits read these from it.
#[derive(Clone)]
pub struct Identity {
    /// The run's id, which the artifact toolkit reads.
    pub run_id: String,
    /// The job's id, which the artifact toolkit reads.
    pub job_id: String,
    /// The cache scopes: the first is the one the job saves to. `BuildKit`
    /// reads them as the token's access controls. `None` leaves them out
    /// of the token, so `BuildKit` refuses it.
    pub scopes: Option<Vec<String>>,
    /// How many seconds the token lives, from now.
    pub expires_in: u64,
}

/// The artifact service of a job.
#[derive(Clone)]
pub struct ArtifactConfig {
    /// The control plane's artifact URL and the job token.
    pub service: source::Cache,
}

/// A running proxy.
pub struct Proxy {
    state: Arc<State>,
    url: String,
    runtime_token: String,
    server: tokio::task::JoinHandle<()>,
}

struct State {
    /// The cache service, when the job has one.
    cloud: Option<Cloud>,
    /// The artifact service, when the job has one.
    artifacts: Option<Artifacts>,
    /// `http://<ip>:<port>`.
    base: String,
    /// Where Twirp calls the proxy does not serve go.
    results: String,
    http: reqwest::Client,
    uploads: Mutex<Uploads>,
    /// Entries looked up, by the token in their download URL.
    downloads: Mutex<HashMap<String, Download>>,
    blocks: Semaphore,
    /// Numbers request ids and entry ids.
    counter: AtomicU64,
}

/// The artifact service of the control plane and the token the runner's
/// steps present to the proxy.
struct Artifacts {
    cloud: Cloud,
    /// `ACTIONS_RUNTIME_TOKEN`.
    token: String,
}

impl State {
    /// A client for presigned S3 downloads: either service's.
    fn s3(&self) -> Option<&Cloud> {
        self.cloud
            .as_ref()
            .or_else(|| self.artifacts.as_ref().map(|artifacts| &artifacts.cloud))
    }

    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// An entry a lookup found.
struct Download {
    /// Presigned GET of the entry.
    url: String,
    /// Bytes.
    size: u64,
}

/// The secret part of a download URL. Any process on the instance can
/// reach the proxy, so a download URL must not be guessable from the
/// ones before it: knowing one lets a process read the entry.
fn download_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("random download token: {err}"))?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

/// Uploads by their cloud id, and their cloud ids by key and version.
#[derive(Default)]
struct Uploads {
    by_id: HashMap<String, Arc<Upload>>,
    by_entry: HashMap<(String, String), String>,
}

impl Uploads {
    /// The upload of `entry`, claimed. The claim is taken under the lock
    /// that `replace` and `stop` check idleness under, so neither can
    /// abandon the upload between the lookup and the finalize.
    fn claim(&self, entry: &(String, String)) -> Option<Claim> {
        let upload = self.by_id.get(self.by_entry.get(entry)?)?;
        upload.claims.fetch_add(1, Ordering::SeqCst);
        Some(Claim(Arc::clone(upload)))
    }

    /// Makes `upload` the one for `entry`. The upload it replaces is
    /// returned to abandon when it is idle. One that is finalizing or
    /// settled stays in `by_id`: `stop` aborts it once it is idle, and
    /// until then the control plane's stale-upload sweep and the S3
    /// lifecycle rule are its cleanup.
    fn replace(&mut self, entry: (String, String), upload: Arc<Upload>) -> Option<Arc<Upload>> {
        let old = self.by_entry.insert(entry, upload.id.clone());
        self.by_id.insert(upload.id.clone(), upload);
        let old = self.by_id.get(&old?)?;
        if !old.idle() {
            return None;
        }
        let old = Arc::clone(old);
        self.by_id.remove(&old.id);
        Some(old)
    }
}

/// A finalize's hold on an upload, released when it returns or is
/// dropped.
struct Claim(Arc<Upload>);

impl std::ops::Deref for Claim {
    type Target = Upload;

    fn deref(&self) -> &Upload {
        &self.0
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.claims.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Upload {
    id: String,
    /// The service the upload goes to.
    cloud: Cloud,
    state: Mutex<UploadState>,
    /// The definitive answer of the finalize, once there is one. Held
    /// across the call to the control plane, so finalizes of one upload
    /// run one at a time and a repeat gets the same answer. Dropping a
    /// request mid-finalize releases it with no answer, so a retry runs.
    outcome: tokio::sync::Mutex<Option<Outcome>>,
    /// Finalizes that looked the upload up and have not returned.
    claims: AtomicUsize,
}

/// What a finalize came to.
#[derive(Clone)]
enum Outcome {
    /// Saved, as this id.
    Saved(String),
    /// Not saved, and why.
    Refused(String),
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
    /// The finalize settled. Nothing is kept, and a put still streaming
    /// drops its data.
    settled: bool,
}

/// Locks `m`, taking the data back from a poisoned lock. One panicking
/// request must not make every later request panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Upload {
    /// No finalize holds the upload and none has settled, so it is still
    /// the proxy's to abandon. Call under the uploads lock, which
    /// claims are taken under.
    fn idle(&self) -> bool {
        self.claims.load(Ordering::SeqCst) == 0
            && self.outcome.try_lock().is_ok_and(|done| done.is_none())
    }

    /// A new upload with nothing received.
    fn new(id: String, cloud: Cloud) -> Arc<Self> {
        Arc::new(Self {
            id,
            cloud,
            state: Mutex::new(UploadState::default()),
            outcome: tokio::sync::Mutex::new(None),
            claims: AtomicUsize::new(0),
        })
    }

    /// Locks the upload's blocks and parts.
    fn lock(&self) -> MutexGuard<'_, UploadState> {
        lock(&self.state)
    }
}

impl Proxy {
    /// Starts the proxy on `ip` for the services a job's document
    /// names: the cache, the artifacts, or both.
    ///
    /// # Errors
    ///
    /// When a service URL is unusable or the port cannot be bound.
    pub async fn start_services(services: Services, ip: IpAddr) -> Result<Self> {
        Self::start_with(services, SocketAddr::new(ip, PORT), RESULTS_URL).await
    }

    #[cfg(test)]
    async fn start_on(cache: source::Cache, addr: SocketAddr, results: &str) -> Result<Self> {
        let services = Services {
            cache: Some(cache),
            artifacts: None,
            identity: Identity {
                run_id: "1".into(),
                job_id: "build".into(),
                scopes: Some(vec!["refs/heads/main".into()]),
                expires_in: 3600,
            },
        };
        Self::start_with(services, addr, results).await
    }

    async fn start_with(services: Services, addr: SocketAddr, results: &str) -> Result<Self> {
        let cloud = services.cache.map(Cloud::new).transpose()?;
        let identity = &services.identity;
        let runtime_token = artifact_twirp::runtime_token(
            &identity.run_id,
            &identity.job_id,
            identity.scopes.as_deref(),
            identity.expires_in,
        )?;
        let artifacts = services
            .artifacts
            .map(|config| -> Result<Artifacts> {
                Ok(Artifacts {
                    cloud: Cloud::new(config.service)?,
                    token: runtime_token.clone(),
                })
            })
            .transpose()?;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listen on {addr}"))?;
        let addr = listener.local_addr().context("cache proxy address")?;
        let base = format!("http://{addr}");
        let state = Arc::new(State {
            cloud,
            artifacts,
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
            runtime_token,
            server,
        })
    }

    /// The `ACTIONS_RUNTIME_TOKEN` of the steps: a JWT the toolkits read
    /// and never verify (see [`artifact_twirp::runtime_token`]). The
    /// proxy accepts it on artifact calls; the cache calls go to the
    /// cloud with the job's own token.
    #[must_use]
    pub fn runtime_token(&self) -> &str {
        &self.runtime_token
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
    /// finalized. An upload with a finalize in flight or settled is left
    /// alone: the control plane's stale-upload sweep and the S3 lifecycle
    /// rule clean up after it.
    pub async fn stop(self) {
        self.server.abort();
        let uploads: Vec<(String, Cloud)> = {
            let uploads = lock(&self.state.uploads);
            uploads
                .by_id
                .values()
                .filter(|upload| upload.idle())
                .map(|upload| (upload.id.clone(), upload.cloud.clone()))
                .collect()
        };
        for (id, cloud) in uploads {
            match tokio::time::timeout(Duration::from_secs(3), cloud.abort(&id)).await {
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
    if let Some(artifacts) = &state.artifacts
        && let Some(rpc) = artifact_twirp::Rpc::from_path(&path)
    {
        return artifact::serve(state, artifacts, rpc, req).await;
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
    if let Some(token) = path.strip_prefix(DOWNLOAD_PATH) {
        return match *req.method() {
            Method::GET => download(state, token, req.headers()).await,
            Method::HEAD => head(state, token),
            _ => azure_error(
                state,
                &blob::Error::new(405, "UnsupportedHttpVerb", "download URLs take GET"),
            ),
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
    let Some(cloud) = &state.cloud else {
        return reply(StatusCode::OK, "text/plain", "ok");
    };
    match cloud.lookup("hatchrun-health", &[], "health").await {
        Ok(_) => reply(StatusCode::OK, "text/plain", "ok"),
        Err(err) => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain",
            format!("{err:#}"),
        ),
    }
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

/// The reply for a call to the control plane that failed, with the
/// Twirp code and status its answer deserves (see [`twirp::code_for`])
/// and its `Retry-After`.
fn refusal(err: &anyhow::Error) -> Reply {
    let (code, retry_after) = twirp::code_for(err);
    let mut reply = twirp_error(twirp::http_status(code), code, format!("{err:#}"));
    if let Some(value) = retry_after.and_then(|v| HeaderValue::from_str(&v).ok()) {
        reply.headers_mut().insert(header::RETRY_AFTER, value);
    }
    reply
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
mod tests;
