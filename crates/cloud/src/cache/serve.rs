// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/v1/cache/*`, which the agent on a runner calls with its cache token
//! (see [`crate::cache`]).
//!
//! - `POST /v1/cache/lookup` answers a presigned GET of the entry to
//!   restore, or 404.
//! - `POST /v1/cache/uploads` starts a multipart upload into the job's
//!   scope, or answers 409 when the key exists.
//! - `POST /v1/cache/uploads/{id}/parts` answers presigned `UploadPart`
//!   URLs.
//! - `POST /v1/cache/uploads/{id}/complete` completes the upload; 409
//!   when another job wrote the key first.
//! - `DELETE /v1/cache/uploads/{id}` abandons it.
//!
//! Every call assumes the tenant's role. Its credentials are kept in the
//! isolate for [`CREDENTIALS_TTL`], so a lookup costs S3 calls and
//! not an STS call each time.

use std::cell::RefCell;
use std::collections::HashMap;

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, Env, Method, Request, Response, Result, console_warn};

use super::{
    CREDENTIALS_TTL, Claims, CompleteRequest, LookupRequest, LookupResponse, MAX_PAGES,
    MAX_RESTORE_KEYS, PartUrl, PartsRequest, PartsResponse, Scopes, URL_EXPIRES, UploadRequest,
    UploadResponse,
};
use crate::aws::s3::{self, Completed, Object, Part};
use crate::aws::{self, Credentials};
use crate::fetch::send;
use crate::http::Request as Outgoing;
use crate::launch::{
    APP_ID, APP_KEY, AWS_ENDPOINT_URL, GITHUB_API_URL, app_jwt, tenant_credentials,
};
use crate::{DB, github, int, jwt, now_ms, oidc, telemetry, webcrypto};

thread_local! {
    /// Role credentials by tenant id, with when they were fetched.
    static CREDENTIALS: RefCell<HashMap<String, (Credentials, u64)>> = RefCell::new(HashMap::new());
}

/// Handles a `/v1/cache/*` request.
pub(crate) async fn handle(req: Request, env: Env) -> Result<Response> {
    match serve(req, &env).await {
        Ok(response) => Ok(response),
        Err(Refusal { status, message }) => {
            if status >= 500 {
                console_warn!("cache: {message}");
            }
            Response::from_json(&serde_json::json!({ "error": message }))
                .map(|response| response.with_status(status))
        }
    }
}

/// An answer other than success.
struct Refusal {
    status: u16,
    message: String,
}

impl Refusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<worker::Error> for Refusal {
    fn from(err: worker::Error) -> Self {
        Self::new(500, err.to_string())
    }
}

type Answer = std::result::Result<Response, Refusal>;

/// The job and its connected tenant, by job, installation and tenant.
const JOB: &str = "SELECT jobs.repository, jobs.run_id, jobs.cache_write, jobs.cache_reads, \
     tenants.id AS tenant_id, tenants.role_arn, tenants.region, tenants.cache_bucket \
     FROM jobs JOIN tenants ON tenants.installation_id = jobs.installation_id \
     WHERE jobs.id = ?1 AND jobs.installation_id = ?2 AND tenants.id = ?3 \
     AND tenants.role_arn IS NOT NULL AND tenants.region IS NOT NULL";

/// Records a multipart upload a job started.
const START: &str = "INSERT INTO cache_uploads (id, job_id, object_key, upload_id, created_at) \
     VALUES (?1, ?2, ?3, ?4, ?5)";

/// A job's upload, by id.
const UPLOAD: &str =
    "SELECT object_key, upload_id FROM cache_uploads WHERE id = ?1 AND job_id = ?2";

/// Forgets an upload once completed or abandoned.
const FORGET: &str = "DELETE FROM cache_uploads WHERE id = ?1";

/// Stores the scopes worked out for a job.
const SCOPES: &str = "UPDATE jobs SET cache_write = ?2, cache_reads = ?3 WHERE id = ?1";

/// The job's tenant, as a cache call needs it.
#[derive(Deserialize)]
struct Row {
    repository: String,
    run_id: u64,
    cache_write: Option<String>,
    cache_reads: Option<String>,
    tenant_id: String,
    role_arn: String,
    region: String,
    cache_bucket: Option<String>,
}

/// Everything a call works with once the token checks out.
struct Context<'a> {
    env: &'a Env,
    db: D1Database,
    claims: Claims,
    row: Row,
    bucket: String,
    aws_endpoint: Option<String>,
}

async fn serve(mut req: Request, env: &Env) -> Answer {
    let path = req.path();
    let route = path
        .strip_prefix("/v1/cache/")
        .ok_or_else(|| Refusal::new(404, "not found"))?
        .to_owned();
    let claims = authorize(&req, env)?;
    let db = env.d1(DB)?;
    let row: Row = db
        .prepare(JOB)
        .bind(&[
            int(claims.job_id)?,
            int(claims.installation_id)?,
            claims.tenant.as_str().into(),
        ])?
        .first(None)
        .await?
        .ok_or_else(|| Refusal::new(403, "no such job"))?;
    let bucket = row
        .cache_bucket
        .clone()
        .ok_or_else(|| Refusal::new(403, "the connect stack has no cache bucket"))?;
    let ctx = Context {
        env,
        db,
        claims,
        row,
        bucket,
        aws_endpoint: env
            .secret(AWS_ENDPOINT_URL)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| !value.is_empty()),
    };

    let segments: Vec<&str> = route.split('/').collect();
    match (req.method(), segments.as_slice()) {
        (Method::Post, ["lookup"]) => lookup(&ctx, body(&mut req).await?).await,
        (Method::Post, ["uploads"]) => start_upload(&ctx, body(&mut req).await?).await,
        (Method::Post, ["uploads", id, "parts"]) => parts(&ctx, id, body(&mut req).await?).await,
        (Method::Post, ["uploads", id, "complete"]) => {
            complete(&ctx, id, body(&mut req).await?).await
        }
        (Method::Delete, ["uploads", id]) => abort(&ctx, id).await,
        _ => Err(Refusal::new(404, "not found")),
    }
}

/// The cache token's claims, checked against the public half of the
/// token signing key.
fn authorize(req: &Request, env: &Env) -> std::result::Result<Claims, Refusal> {
    let pem = env
        .secret(telemetry::SIGNING_KEY)
        .map(|secret| secret.to_string())
        .map_err(|_| Refusal::new(500, format!("{} is not set", telemetry::SIGNING_KEY)))?;
    let seed = jwt::ed25519_pkcs8_der(&pem)
        .and_then(|der| jwt::ed25519_seed(&der))
        .map_err(|err| Refusal::new(500, format!("{}: {err}", telemetry::SIGNING_KEY)))?;
    let key: VerifyingKey = SigningKey::from_bytes(&seed).verifying_key();
    let header = req.headers().get("Authorization")?;
    super::verify(header.as_deref(), &key, now_ms() / 1000).map_err(|err| Refusal::new(401, err))
}

async fn body<T: for<'de> Deserialize<'de>>(req: &mut Request) -> std::result::Result<T, Refusal> {
    let bytes = req.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|err| Refusal::new(400, format!("bad request: {err}")))
}

fn json(value: &impl serde::Serialize, status: u16) -> Answer {
    Ok(Response::from_json(value)?.with_status(status))
}

async fn lookup(ctx: &Context<'_>, request: LookupRequest) -> Answer {
    super::check_version(&request.version).map_err(|err| Refusal::new(400, err))?;
    super::check_key(&request.key).map_err(|err| Refusal::new(400, err))?;
    if request.restore_keys.len() > MAX_RESTORE_KEYS {
        return Err(Refusal::new(
            400,
            format!("at most {MAX_RESTORE_KEYS} restore keys"),
        ));
    }
    for key in &request.restore_keys {
        super::check_key(key).map_err(|err| Refusal::new(400, err))?;
    }
    let scopes = scopes(ctx).await?;
    let credentials = credentials(ctx).await?;
    let wanted: Vec<(&str, bool)> = std::iter::once((request.key.as_str(), true))
        .chain(request.restore_keys.iter().map(|key| (key.as_str(), false)))
        .collect();
    for scope in &scopes.reads {
        for (key, exact) in &wanted {
            let prefix = super::object_key(scope, &request.version, key);
            let objects = list(ctx, &credentials, &prefix).await?;
            if let Some(found) = super::pick(&objects, &prefix, *exact) {
                let object_url = object_url(ctx, &found.key);
                let url = s3::presign(
                    &s3::get_object(&object_url),
                    &credentials,
                    &ctx.row.region,
                    now_ms(),
                    URL_EXPIRES,
                )
                .map_err(|err| Refusal::new(500, err))?;
                let key = found.key[scope.len() + request.version.len() + 2..].to_owned();
                return json(
                    &LookupResponse {
                        key,
                        url,
                        size: found.size,
                    },
                    200,
                );
            }
        }
    }
    Err(Refusal::new(404, "no entry"))
}

async fn start_upload(ctx: &Context<'_>, request: UploadRequest) -> Answer {
    super::check_version(&request.version).map_err(|err| Refusal::new(400, err))?;
    super::check_key(&request.key).map_err(|err| Refusal::new(400, err))?;
    let scopes = scopes(ctx).await?;
    let credentials = credentials(ctx).await?;
    let key = super::object_key(&scopes.write, &request.version, &request.key);
    // Saves the upload when the entry exists; the conditional complete
    // still settles a race between two jobs.
    if super::pick(&list(ctx, &credentials, &key).await?, &key, true).is_some() {
        return Err(Refusal::new(409, "the entry exists"));
    }
    let url = object_url(ctx, &key);
    let upload_id =
        s3::upload_id(&signed(ctx, &credentials, s3::create_multipart_upload(&url)).await?)
            .map_err(aws_failure("CreateMultipartUpload"))?;
    let id = jwt::b64url(webcrypto::random_bytes(16)?);
    ctx.db
        .prepare(START)
        .bind(&[
            id.as_str().into(),
            int(ctx.claims.job_id)?,
            key.as_str().into(),
            upload_id.as_str().into(),
            int(now_ms())?,
        ])?
        .run()
        .await?;
    json(&UploadResponse { upload_id: id }, 201)
}

/// An upload of this job.
#[derive(Deserialize)]
struct Upload {
    object_key: String,
    upload_id: String,
}

async fn upload(ctx: &Context<'_>, id: &str) -> std::result::Result<Upload, Refusal> {
    ctx.db
        .prepare(UPLOAD)
        .bind(&[id.into(), int(ctx.claims.job_id)?])?
        .first(None)
        .await?
        .ok_or_else(|| Refusal::new(404, "no such upload"))
}

async fn forget(ctx: &Context<'_>, id: &str) -> std::result::Result<(), Refusal> {
    ctx.db.prepare(FORGET).bind(&[id.into()])?.run().await?;
    Ok(())
}

async fn parts(ctx: &Context<'_>, id: &str, request: PartsRequest) -> Answer {
    super::check_part_numbers(&request.numbers).map_err(|err| Refusal::new(400, err))?;
    let upload = upload(ctx, id).await?;
    let credentials = credentials(ctx).await?;
    let url = object_url(ctx, &upload.object_key);
    let now = now_ms();
    let parts = request
        .numbers
        .iter()
        .map(|&number| {
            s3::presign(
                &s3::upload_part(&url, &upload.upload_id, number),
                &credentials,
                &ctx.row.region,
                now,
                URL_EXPIRES,
            )
            .map(|url| PartUrl { number, url })
            .map_err(|err| Refusal::new(500, err))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    json(&PartsResponse { parts }, 200)
}

async fn complete(ctx: &Context<'_>, id: &str, request: CompleteRequest) -> Answer {
    let parts = super::complete_parts(request.parts).map_err(|err| Refusal::new(400, err))?;
    let upload = upload(ctx, id).await?;
    let credentials = credentials(ctx).await?;
    let url = object_url(ctx, &upload.object_key);
    let parts: Vec<Part> = parts
        .into_iter()
        .map(|part| Part {
            number: part.number,
            etag: part.etag,
        })
        .collect();
    let response = signed(
        ctx,
        &credentials,
        s3::complete_multipart_upload(&url, &upload.upload_id, &parts),
    )
    .await?;
    let completed = s3::completed(&response).map_err(aws_failure("CompleteMultipartUpload"))?;
    forget(ctx, id).await?;
    match completed {
        Completed::Written => Ok(Response::empty()?.with_status(204)),
        Completed::Exists => {
            // The parts are of no use now; the lifecycle rule would
            // remove them in a day, this does it at once.
            let _ = signed(
                ctx,
                &credentials,
                s3::abort_multipart_upload(&url, &upload.upload_id),
            )
            .await;
            Err(Refusal::new(409, "another job saved the entry first"))
        }
    }
}

async fn abort(ctx: &Context<'_>, id: &str) -> Answer {
    let upload = upload(ctx, id).await?;
    let credentials = credentials(ctx).await?;
    let url = object_url(ctx, &upload.object_key);
    let response = signed(
        ctx,
        &credentials,
        s3::abort_multipart_upload(&url, &upload.upload_id),
    )
    .await?;
    if response.status != 404 {
        s3::ok(&response).map_err(aws_failure("AbortMultipartUpload"))?;
    }
    forget(ctx, id).await?;
    Ok(Response::empty()?.with_status(204))
}

fn object_url(ctx: &Context<'_>, key: &str) -> String {
    s3::object_url(
        &ctx.bucket,
        &ctx.row.region,
        ctx.aws_endpoint.as_deref(),
        key,
    )
}

fn aws_failure(action: &'static str) -> impl Fn(aws::Error) -> Refusal {
    move |err| {
        let status = if err.code == "AccessDenied" { 403 } else { 502 };
        Refusal::new(status, format!("{action}: {err}"))
    }
}

/// Signs and sends an S3 request.
async fn signed(
    ctx: &Context<'_>,
    credentials: &Credentials,
    mut request: Outgoing,
) -> std::result::Result<crate::http::Response, Refusal> {
    s3::sign(&mut request, credentials, &ctx.row.region, now_ms())
        .map_err(|err| Refusal::new(500, format!("sign S3 request: {err}")))?;
    send(&request)
        .await
        .map_err(|err| Refusal::new(502, format!("S3 unreachable: {err}")))
}

/// Every object under `prefix`, up to [`MAX_PAGES`] pages.
async fn list(
    ctx: &Context<'_>,
    credentials: &Credentials,
    prefix: &str,
) -> std::result::Result<Vec<Object>, Refusal> {
    let bucket_url = object_url(ctx, "");
    let mut objects = Vec::new();
    let mut continuation = None;
    for _ in 0..MAX_PAGES {
        let request = s3::list_objects(&bucket_url, prefix, continuation.as_deref());
        let listing = s3::listing(&signed(ctx, credentials, request).await?)
            .map_err(aws_failure("ListObjectsV2"))?;
        objects.extend(listing.objects);
        continuation = listing.continuation;
        if continuation.is_none() {
            break;
        }
    }
    Ok(objects)
}

/// The tenant's role credentials, from the isolate when fresh.
async fn credentials(ctx: &Context<'_>) -> std::result::Result<Credentials, Refusal> {
    let now = now_ms();
    let ttl = u64::try_from(CREDENTIALS_TTL.as_millis()).unwrap_or(u64::MAX);
    let tenant = &ctx.row.tenant_id;
    let cached = CREDENTIALS.with(|cache| {
        cache
            .borrow()
            .get(tenant)
            .filter(|(_, at)| now.saturating_sub(*at) < ttl)
            .map(|(credentials, _)| credentials.clone())
    });
    if let Some(credentials) = cached {
        return Ok(credentials);
    }
    let secret = |name: &str| {
        ctx.env
            .secret(name)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Refusal::new(500, format!("{name} is not set")))
    };
    let key = jwt::pkcs8_der(&secret(oidc::SIGNING_KEY)?)
        .map_err(|err| Refusal::new(500, format!("{}: {err}", oidc::SIGNING_KEY)))?;
    let credentials = tenant_credentials(
        &key,
        &secret(oidc::KEY_ID)?,
        ctx.aws_endpoint.as_deref(),
        tenant,
        &ctx.row.role_arn,
        &ctx.row.region,
        &format!("hatchrun-cache-{}", ctx.claims.job_id),
    )
    .await
    .map_err(|err| Refusal::new(502, format!("AssumeRoleWithWebIdentity: {err}")))?;
    CREDENTIALS.with(|cache| {
        cache
            .borrow_mut()
            .insert(tenant.clone(), (credentials.clone(), now));
    });
    Ok(credentials)
}

/// The job's scopes: from its row, or worked out from its workflow run
/// on GitHub the first time and kept on the row.
async fn scopes(ctx: &Context<'_>) -> std::result::Result<Scopes, Refusal> {
    if let (Some(write), Some(reads)) = (&ctx.row.cache_write, &ctx.row.cache_reads)
        && let Ok(reads) = serde_json::from_str(reads)
    {
        return Ok(Scopes {
            write: write.clone(),
            reads,
        });
    }
    let secret = |name: &str| {
        ctx.env
            .secret(name)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| !value.is_empty())
    };
    let api = secret(GITHUB_API_URL)
        .unwrap_or_else(|| github::API.to_owned())
        .trim_end_matches('/')
        .to_owned();
    let app_id = secret(APP_ID).ok_or_else(|| Refusal::new(500, format!("{APP_ID} is not set")))?;
    let app_key = jwt::pkcs8_der(
        &secret(APP_KEY).ok_or_else(|| Refusal::new(500, format!("{APP_KEY} is not set")))?,
    )
    .map_err(|err| Refusal::new(500, format!("{APP_KEY}: {err}")))?;
    let app = app_jwt(&app_id, &app_key)
        .await
        .map_err(|err| Refusal::new(500, err))?;
    let token = github::token(
        &github_send(&github::installation_token(
            &api,
            ctx.claims.installation_id,
            &app,
        ))
        .await?,
    )
    .map_err(|err| Refusal::new(502, err))?;
    let repository = &ctx.row.repository;
    let run = github::parse_workflow_run(
        &github_send(&github::workflow_run(
            &api,
            &token,
            repository,
            ctx.row.run_id,
        ))
        .await?,
    )
    .map_err(|err| Refusal::new(502, err))?;
    let default_branch =
        github::default_branch(&github_send(&github::repository(&api, &token, repository)).await?)
            .map_err(|err| Refusal::new(502, err))?;
    let on_branch = on_branch(&api, &token, repository, &run).await?;
    let scopes = super::scopes(&run, &default_branch, on_branch);
    let reads =
        serde_json::to_string(&scopes.reads).map_err(|err| Refusal::new(500, err.to_string()))?;
    ctx.db
        .prepare(SCOPES)
        .bind(&[
            int(ctx.claims.job_id)?,
            scopes.write.as_str().into(),
            JsValue::from(reads),
        ])?
        .run()
        .await?;
    Ok(scopes)
}

/// Whether the run's commit is on its head branch in the repository:
/// the branch exists and its tip is the commit or a descendant of it.
/// Asks GitHub only for runs that could write to a branch's scope.
async fn on_branch(
    api: &str,
    token: &str,
    repository: &str,
    run: &super::Run,
) -> std::result::Result<bool, Refusal> {
    let Some(branch) = run
        .head_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
    else {
        return Ok(false);
    };
    if super::from_outside(run) {
        return Ok(false);
    }
    let tip = github::branch_sha(
        &github_send(&github::branch_ref(api, token, repository, branch)).await?,
    )
    .map_err(|err| Refusal::new(502, err))?;
    match tip {
        None => Ok(false),
        Some(tip) if tip == run.head_sha => Ok(true),
        Some(tip) => github::contained(
            &github_send(&github::compare(
                api,
                token,
                repository,
                &tip,
                &run.head_sha,
            ))
            .await?,
        )
        .map_err(|err| Refusal::new(502, err)),
    }
}

async fn github_send(request: &Outgoing) -> std::result::Result<crate::http::Response, Refusal> {
    send(request)
        .await
        .map_err(|err| Refusal::new(502, format!("GitHub unreachable: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;

    fn seed(role: Option<&str>) -> Db {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             cache_bucket, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            &params![
                "t1",
                9u64,
                "acme",
                42u64,
                role,
                "ap-south-1",
                "bucket",
                1u64
            ],
        );
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            &params![
                5u64,
                42u64,
                "acme/app",
                77u64,
                "hatchrun",
                "in_progress",
                NOW
            ],
        );
        db
    }

    #[test]
    fn a_job_reads_with_its_tenant() {
        let db = seed(Some("arn:aws:iam::1:role/r"));
        let row: Row = db.first(JOB, &params![5u64, 42u64, "t1"]).unwrap();
        assert_eq!(row.repository, "acme/app");
        assert_eq!(row.run_id, 77);
        assert_eq!(row.tenant_id, "t1");
        assert_eq!(row.role_arn, "arn:aws:iam::1:role/r");
        assert_eq!(row.region, "ap-south-1");
        assert_eq!(row.cache_bucket.as_deref(), Some("bucket"));
        assert!(row.cache_write.is_none() && row.cache_reads.is_none());

        let reads = serde_json::to_string(&["refs/heads/main"]).unwrap();
        assert_eq!(
            db.run(SCOPES, &params![5u64, "refs/heads/dev", reads.clone()]),
            1
        );
        let row: Row = db.first(JOB, &params![5u64, 42u64, "t1"]).unwrap();
        assert_eq!(row.cache_write.as_deref(), Some("refs/heads/dev"));
        assert_eq!(row.cache_reads, Some(reads));

        assert!(db.first::<Row>(JOB, &params![5u64, 42u64, "t2"]).is_none());
        assert!(db.first::<Row>(JOB, &params![5u64, 43u64, "t1"]).is_none());
    }

    #[test]
    fn a_tenant_without_a_role_has_no_job() {
        let db = seed(None);
        assert!(db.first::<Row>(JOB, &params![5u64, 42u64, "t1"]).is_none());
    }

    #[test]
    fn uploads_belong_to_their_job() {
        let db = seed(Some("arn:aws:iam::1:role/r"));
        db.run(START, &params!["u1", 5u64, "scope/v/key", "s3-upload", NOW]);
        let upload: Upload = db.first(UPLOAD, &params!["u1", 5u64]).unwrap();
        assert_eq!(upload.object_key, "scope/v/key");
        assert_eq!(upload.upload_id, "s3-upload");
        assert!(db.first::<Upload>(UPLOAD, &params!["u1", 6u64]).is_none());
        assert_eq!(db.run(FORGET, &params!["u1"]), 1);
        assert!(db.first::<Upload>(UPLOAD, &params!["u1", 5u64]).is_none());
    }
}
