// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/v1/agent/cache/*`: what `actions/cache` reaches through the agent.
//! The calls are those of the hosted runners' cache service (see
//! [`hatchrun_cloud::cache`]), so the agent uses the same client:
//!
//! - `POST lookup` answers a presigned GET of the entry to restore, or
//!   404.
//! - `POST uploads` starts a multipart upload, or answers 409 when the
//!   entry exists.
//! - `POST uploads/{id}/parts` answers presigned `UploadPart` URLs.
//! - `POST uploads/{id}/complete` completes the upload; 409 when
//!   another job saved the entry first.
//! - `DELETE uploads/{id}` abandons it.
//!
//! The bytes are one S3 object per entry in the artifacts bucket of the
//! CI account, at `cache/<tenant>/<repo id>/<random>`. The rows that
//! say what an object is live in `ci_cache_entries`.
//!
//! The scope of an entry is the git ref of the run that saved it. A
//! run writes to its own ref and reads its own, then the default
//! branch's, as on GitHub, so a topic branch or a pull request can use
//! what `main` saved and never put an entry where `main` would restore
//! it. The tenant, repository and ref come from the job token's run,
//! never from a request.
//!
//! A repository keeps at most [`MAX_REPO_BYTES`]: when a save takes it
//! past that, the least recently used entries are deleted.
//!
//! Entries nobody restored for [`IDLE_DAYS`] days are deleted by the
//! reconciler (see [`evict`]), which also drops uploads that never
//! finished.

use hatchrun_cloud::aws::{Credentials, s3};
use hatchrun_cloud::cache::{
    CompleteRequest, LookupRequest, LookupResponse, MAX_RESTORE_KEYS, PartUrl, PartsRequest,
    PartsResponse, UploadRequest, UploadResponse, check_key, check_part_numbers, check_version,
    complete_parts,
};
use hatchrun_cloud::db::Db;
use hatchrun_cloud::{jwt, webcrypto};
use worker::{Env, Method, Request, Response, Result, console_warn};

use crate::artifacts::{
    Answer, Refusal, aws_failure, body, check_job, credentials, json, now_ms, sign_send,
};
use crate::env::Settings;
use crate::store::{self, CacheOwner, CacheRow, Finalized, RunRow};
use crate::token::Claims;

/// The service's base path on the control plane.
pub const PATH: &str = "/v1/agent/cache";
/// How long an entry may go without a restore before it is evicted.
pub const IDLE_DAYS: u64 = 7;
/// How long an upload may stay unfinished before it is dropped. S3
/// aborts the parts a day after they started.
pub const STALE_UPLOAD_MS: u64 = 86_400_000;
/// Entries one reconciler pass evicts at most.
pub const EVICT_LIMIT: u32 = 20;

/// What one repository may keep, as on GitHub: 10 GB. Past it the least
/// recently used entries are evicted.
pub const MAX_REPO_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// How long a presigned URL holds, in seconds.
const URL_SECS: u64 = 900;

/// What a call from a job that is no longer running is told.
const JOB_ENDED: &str = "the job is not running";

/// The S3 key of an entry of `repo_id` of `tenant`, named by `token`,
/// which is random.
pub(crate) fn object_key(tenant: &str, repo_id: u64, token: &str) -> String {
    format!("cache/{tenant}/{repo_id}/{token}")
}

/// Whether `key` is where an entry of this repository lives.
pub(crate) fn in_repo(key: &str, tenant: &str, repo_id: u64) -> bool {
    key.starts_with(&format!("cache/{tenant}/{repo_id}/")) && !key.contains("..")
}

/// The refs a run on `git_ref` restores from, in order: its own, the
/// base branch's when it is a pull request, then the default branch's.
pub(crate) fn read_scopes(
    git_ref: &str,
    base_ref: Option<&str>,
    default_branch: Option<&str>,
) -> Vec<String> {
    let mut scopes = vec![git_ref.to_owned()];
    for branch in [base_ref, default_branch].into_iter().flatten() {
        let scope = format!("refs/heads/{branch}");
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes
}

/// Everything a call works with once the token checks out.
struct Context<'a> {
    settings: &'a Settings,
    db: Db,
    run: RunRow,
    bucket: &'a str,
    /// The scopes the job reads, fixed when it was claimed.
    scopes: Vec<String>,
}

impl Context<'_> {
    /// Where the run writes, and where a call to an upload looks.
    fn owner(&self) -> CacheOwner<'_> {
        CacheOwner {
            tenant_id: &self.run.tenant_id,
            repo_id: self.run.repo_id,
            scope: &self.run.git_ref,
        }
    }
}

/// Answers a call to `/v1/agent/cache/*`. `claims` were checked by the
/// caller.
///
/// # Errors
///
/// The Worker runtime failed to build a response.
pub async fn handle(
    mut req: Request,
    env: &Env,
    settings: &Settings,
    claims: Claims,
) -> Result<Response> {
    match serve(&mut req, env, settings, claims).await {
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

async fn serve(req: &mut Request, env: &Env, settings: &Settings, claims: Claims) -> Answer {
    let path = req.path();
    let route = path
        .strip_prefix(PATH)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(|| Refusal::new(404, "not found"))?
        .to_owned();
    let segments: Vec<&str> = route.split('/').collect();
    let known = matches!(
        (req.method(), segments.as_slice()),
        (
            Method::Post,
            ["lookup" | "uploads"] | ["uploads", _, "parts" | "complete"]
        ) | (Method::Delete, ["uploads", _])
    );
    if !known {
        return Err(Refusal::new(404, "not found"));
    }
    if settings.artifacts_bucket.is_empty() {
        return Err(Refusal::new(503, "the cache is not configured"));
    }
    let db = Db::connect(env).await?;
    let job = store::job(&db, claims.job)
        .await?
        .ok_or_else(|| Refusal::new(410, JOB_ENDED))?;
    check_job(&job, &claims)?;
    let run = store::run(&db, job.run_id)
        .await?
        .ok_or_else(|| Refusal::new(410, JOB_ENDED))?;
    // The list the job's claim fixed. A job claimed before it was kept
    // has none, and no cache.
    let scopes = job
        .cache_scopes
        .as_deref()
        .and_then(|text| serde_json::from_str::<Vec<String>>(text).ok())
        .unwrap_or_default();
    let ctx = Context {
        settings,
        db,
        run,
        bucket: &settings.artifacts_bucket,
        scopes,
    };
    match (req.method(), segments.as_slice()) {
        (Method::Post, ["lookup"]) => lookup(&ctx, body(req).await?).await,
        (Method::Post, ["uploads"]) => start(&ctx, body(req).await?).await,
        (Method::Post, ["uploads", id, "parts"]) => parts(&ctx, id, body(req).await?).await,
        (Method::Post, ["uploads", id, "complete"]) => complete(&ctx, id, body(req).await?).await,
        (Method::Delete, ["uploads", id]) => abort(&ctx, id).await,
        _ => Err(Refusal::new(404, "not found")),
    }
}

fn id_of(id: &str) -> std::result::Result<u64, Refusal> {
    id.parse().map_err(|_| Refusal::new(404, "no such upload"))
}

fn check_entry(version: &str, key: &str) -> std::result::Result<(), Refusal> {
    check_version(version).map_err(|err| Refusal::new(400, err))?;
    check_key(key).map_err(|err| Refusal::new(400, err))
}

/// What a job with no scopes is told: it has no cache.
const NO_SCOPES: &str = "the job has no cache scopes";

async fn lookup(ctx: &Context<'_>, request: LookupRequest) -> Answer {
    check_entry(&request.version, &request.key)?;
    if ctx.scopes.is_empty() {
        // A miss, as for any entry that is not there.
        return Err(Refusal::new(404, "no entry"));
    }
    if request.restore_keys.len() > MAX_RESTORE_KEYS {
        return Err(Refusal::new(
            400,
            format!("at most {MAX_RESTORE_KEYS} restore keys"),
        ));
    }
    for key in &request.restore_keys {
        check_key(key).map_err(|err| Refusal::new(400, err))?;
    }
    let Some(found) = store::lookup_cache_entry(
        &ctx.db,
        &ctx.run.tenant_id,
        ctx.run.repo_id,
        &request.version,
        &ctx.scopes,
        &request.key,
        &request.restore_keys,
    )
    .await?
    else {
        return Err(Refusal::new(404, "no entry"));
    };
    if !in_repo(&found.object_key, &ctx.run.tenant_id, ctx.run.repo_id) {
        return Err(Refusal::new(500, "the entry has a foreign key"));
    }
    let credentials = credentials(ctx.settings).await?;
    let url = s3::presign(
        &s3::get_object(&object_url(ctx, &found.object_key)),
        &credentials,
        &ctx.settings.ci_region,
        now_ms(),
        std::time::Duration::from_secs(URL_SECS),
    )
    .map_err(|err| Refusal::new(500, err))?;
    store::touch_cache_entry(&ctx.db, found.id, &ctx.run.tenant_id, now_ms()).await?;
    json(
        &LookupResponse {
            key: found.key,
            url,
            size: found.size.unwrap_or(0),
        },
        200,
    )
}

async fn start(ctx: &Context<'_>, request: UploadRequest) -> Answer {
    check_entry(&request.version, &request.key)?;
    if ctx.scopes.is_empty() {
        return Err(Refusal::new(503, NO_SCOPES));
    }
    // Saves the upload when the entry exists; finalizing still settles
    // a race between two jobs.
    if store::cache_entry(&ctx.db, ctx.owner(), &request.version, &request.key)
        .await?
        .is_some()
    {
        return Err(Refusal::new(409, "the entry exists"));
    }
    let token = jwt::b64url(webcrypto::random_bytes(16)?);
    let key = object_key(&ctx.run.tenant_id, ctx.run.repo_id, &token);
    let credentials = credentials(ctx.settings).await?;
    let upload_id = s3::upload_id(
        &signed(
            ctx,
            &credentials,
            s3::create_multipart_upload(&object_url(ctx, &key)),
        )
        .await?,
    )
    .map_err(aws_failure("CreateMultipartUpload"))?;
    let row = store::start_cache_entry(
        &ctx.db,
        ctx.owner(),
        &request.version,
        &request.key,
        &key,
        &upload_id,
        now_ms(),
    )
    .await?;
    json(
        &UploadResponse {
            upload_id: row.id.to_string(),
        },
        201,
    )
}

/// The unfinished upload `id` of the job's repository and ref.
async fn pending(ctx: &Context<'_>, id: &str) -> std::result::Result<CacheRow, Refusal> {
    let row = store::pending_cache_entry(&ctx.db, id_of(id)?, ctx.owner())
        .await?
        .ok_or_else(|| Refusal::new(404, "no such upload"))?;
    // The row's key must be one this repository would have made.
    if !in_repo(&row.object_key, &ctx.run.tenant_id, ctx.run.repo_id) {
        return Err(Refusal::new(500, "the upload has a foreign key"));
    }
    Ok(row)
}

async fn parts(ctx: &Context<'_>, id: &str, request: PartsRequest) -> Answer {
    check_part_numbers(&request.numbers).map_err(|err| Refusal::new(400, err))?;
    let row = pending(ctx, id).await?;
    let upload_id = row
        .upload_id
        .ok_or_else(|| Refusal::new(404, "no such upload"))?;
    let credentials = credentials(ctx.settings).await?;
    let url = object_url(ctx, &row.object_key);
    let now = now_ms();
    let parts = request
        .numbers
        .iter()
        .map(|&number| {
            s3::presign(
                &s3::upload_part(&url, &upload_id, number),
                &credentials,
                &ctx.settings.ci_region,
                now,
                std::time::Duration::from_secs(URL_SECS),
            )
            .map(|url| PartUrl { number, url })
            .map_err(|err| Refusal::new(500, err))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    json(&PartsResponse { parts }, 200)
}

async fn complete(ctx: &Context<'_>, id: &str, request: CompleteRequest) -> Answer {
    let parts: Vec<s3::Part> = complete_parts(request.parts)
        .map_err(|err| Refusal::new(400, err))?
        .into_iter()
        .map(|part| s3::Part {
            number: part.number,
            etag: part.etag,
        })
        .collect();
    let row = pending(ctx, id).await?;
    let upload_id = row
        .upload_id
        .clone()
        .ok_or_else(|| Refusal::new(404, "no such upload"))?;
    let credentials = credentials(ctx.settings).await?;
    let url = object_url(ctx, &row.object_key);
    let response = signed(
        ctx,
        &credentials,
        s3::complete_multipart_upload(&url, &upload_id, &parts),
    )
    .await?;
    let written = s3::completed(&response).map_err(aws_failure("CompleteMultipartUpload"))?;
    if written == s3::Completed::Exists {
        // The parts are of no use now, and nothing else would delete
        // them or the object that stopped the upload.
        for request in [
            s3::abort_multipart_upload(&url, &upload_id),
            s3::delete_object(&url),
        ] {
            if let Err(err) = signed(ctx, &credentials, request).await {
                console_warn!("cache: clean up upload {}: {}", row.id, err.message);
            }
        }
        store::drop_pending_cache_entry(&ctx.db, row.id, ctx.owner()).await?;
        return Err(Refusal::new(409, "another job saved the entry first"));
    }
    // The agent does not send the size, and S3 knows it.
    let size = object_size(ctx, &credentials, &row.object_key).await?;
    let done = store::finalize_cache_entry(&ctx.db, row.id, ctx.owner(), size, now_ms()).await?;
    match done {
        Finalized::Done => {
            // The entry is saved; a failure to trim must not fail it.
            if let Err(err) = enforce_quota(ctx, &credentials).await {
                console_warn!("cache: quota of repository {}: {}", ctx.run.repo_id, err);
            }
            Ok(Response::empty()?.with_status(204))
        }
        Finalized::Refused => {
            // Another job finalized the same entry first: this object
            // belongs to no row.
            if let Err(err) = signed(ctx, &credentials, s3::delete_object(&url)).await {
                console_warn!("cache: delete {}: {}", row.id, err.message);
            }
            store::drop_pending_cache_entry(&ctx.db, row.id, ctx.owner()).await?;
            Err(Refusal::new(409, "another job saved the entry first"))
        }
    }
}

async fn abort(ctx: &Context<'_>, id: &str) -> Answer {
    let row = pending(ctx, id).await?;
    if let Some(upload_id) = &row.upload_id {
        let credentials = credentials(ctx.settings).await?;
        let url = object_url(ctx, &row.object_key);
        let response = signed(
            ctx,
            &credentials,
            s3::abort_multipart_upload(&url, upload_id),
        )
        .await?;
        if response.status != 404 {
            s3::ok(&response).map_err(aws_failure("AbortMultipartUpload"))?;
        }
    }
    store::drop_pending_cache_entry(&ctx.db, row.id, ctx.owner()).await?;
    Ok(Response::empty()?.with_status(204))
}

/// Deletes the least recently used entries of the run's repository while
/// it holds more than [`MAX_REPO_BYTES`], at most [`EVICT_LIMIT`] per
/// call. The rows go first and their objects after. What is left over
/// goes at the next save.
async fn enforce_quota(
    ctx: &Context<'_>,
    credentials: &Credentials,
) -> std::result::Result<(), String> {
    let rows = store::evict_over_quota_cache_entries(
        &ctx.db,
        &ctx.run.tenant_id,
        ctx.run.repo_id,
        MAX_REPO_BYTES,
        EVICT_LIMIT,
    )
    .await
    .map_err(|err| err.to_string())?;
    delete_objects(ctx.settings, credentials, &rows).await;
    Ok(())
}

/// Deletes the objects of evicted `rows`, which are already gone from
/// the table, so no lookup can find one whose object is gone. An object
/// S3 no longer has counts as deleted. One that could not be deleted is
/// logged by key. Returns how many were deleted.
async fn delete_objects(
    settings: &Settings,
    credentials: &Credentials,
    rows: &[CacheRow],
) -> usize {
    let results = futures_util::future::join_all(rows.iter().map(|row| async move {
        // Rows are trusted no further than the requests that made them:
        // only keys under cache/ are deleted here.
        if !in_repo(&row.object_key, &row.tenant_id, row.repo_id) {
            return Err(format!("entry {} has a foreign key", row.id));
        }
        let url = s3::object_url(
            &settings.artifacts_bucket,
            &settings.ci_region,
            settings.aws_endpoint.as_deref(),
            &row.object_key,
        );
        let response = sign_send(settings, credentials, s3::delete_object(&url))
            .await
            .map_err(|err| err.message)?;
        if response.status == 404 {
            return Ok(());
        }
        s3::ok(&response).map_err(|err| err.to_string())
    }))
    .await;
    let mut deleted = 0;
    for (row, result) in rows.iter().zip(results) {
        match result {
            Ok(()) => deleted += 1,
            Err(err) => worker::console_error!(
                "cache: delete object {} of evicted entry {}: {err}",
                row.object_key,
                row.id
            ),
        }
    }
    deleted
}

/// The size of the object at `key`.
async fn object_size(
    ctx: &Context<'_>,
    credentials: &Credentials,
    key: &str,
) -> std::result::Result<u64, Refusal> {
    let response = signed(ctx, credentials, s3::head_object(&object_url(ctx, key))).await?;
    s3::content_length(&response).map_err(aws_failure("HeadObject"))
}

fn object_url(ctx: &Context<'_>, key: &str) -> String {
    s3::object_url(
        ctx.bucket,
        &ctx.settings.ci_region,
        ctx.settings.aws_endpoint.as_deref(),
        key,
    )
}

async fn signed(
    ctx: &Context<'_>,
    credentials: &Credentials,
    request: hatchrun_cloud::http::Request,
) -> std::result::Result<hatchrun_cloud::http::Response, Refusal> {
    sign_send(ctx.settings, credentials, request).await
}

/// Deletes the entries nobody restored for [`IDLE_DAYS`] days and the
/// uploads that never finished, at most `limit` of them, and returns how
/// many were deleted. The rows go first and the objects after. A pass
/// costs at most `limit + 1` subrequests.
///
/// # Errors
///
/// What went wrong, for the log: Postgres, or the CI role.
pub async fn evict(
    settings: &Settings,
    db: &Db,
    now: u64,
    limit: u32,
) -> std::result::Result<usize, String> {
    if settings.artifacts_bucket.is_empty() || limit == 0 {
        return Ok(0);
    }
    let idle_ms = IDLE_DAYS * 86_400_000;
    let rows = store::evict_idle_cache_entries(db, now, idle_ms, STALE_UPLOAD_MS, limit)
        .await
        .map_err(|err| err.to_string())?;
    if rows.is_empty() {
        return Ok(0);
    }
    let credentials = credentials(settings)
        .await
        .map_err(|err| err.message.clone())?;
    Ok(delete_objects(settings, &credentials, &rows).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_the_tenant_and_repository() {
        let key = object_key("t1", 7, "abc");
        assert_eq!(key, "cache/t1/7/abc");
        assert!(in_repo(&key, "t1", 7));
        assert!(!in_repo(&key, "t1", 8));
        assert!(!in_repo(&key, "t2", 7));
        assert!(!in_repo("artifacts/30d/t1/7/9/abc", "t1", 7));
        assert!(!in_repo("cache/t1/7/../8/abc", "t1", 7));
        assert!(!in_repo("cache/t1/70/abc", "t1", 7));
    }

    #[test]
    fn a_run_reads_its_own_ref_then_the_base_then_the_default_branch() {
        assert_eq!(
            read_scopes("refs/heads/topic", None, Some("main")),
            ["refs/heads/topic", "refs/heads/main"]
        );
        assert_eq!(
            read_scopes("refs/pull/4/merge", Some("release"), Some("main")),
            ["refs/pull/4/merge", "refs/heads/release", "refs/heads/main"]
        );
        assert_eq!(
            read_scopes("refs/pull/4/merge", Some("main"), Some("main")),
            ["refs/pull/4/merge", "refs/heads/main"]
        );
        assert_eq!(
            read_scopes("refs/heads/main", None, Some("main")),
            ["refs/heads/main"]
        );
        assert_eq!(read_scopes("refs/tags/v1", None, None), ["refs/tags/v1"]);
    }

    #[test]
    fn the_cache_paths_are_the_agents_cache_calls() {
        assert_eq!(PATH, "/v1/agent/cache");
    }
}
