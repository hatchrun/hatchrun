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
/// What an eviction pass may spend in subrequests: credentials and one
/// delete per entry.
pub const EVICT_COST: u32 = EVICT_LIMIT + 1;

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

/// The refs a run on `git_ref` restores from, in order: its own, then
/// the default branch's.
pub(crate) fn read_scopes(git_ref: &str, default_branch: Option<&str>) -> Vec<String> {
    let mut scopes = vec![git_ref.to_owned()];
    if let Some(branch) = default_branch {
        let default = format!("refs/heads/{branch}");
        if default != git_ref {
            scopes.push(default);
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
    let ctx = Context {
        settings,
        db,
        run,
        bucket: &settings.artifacts_bucket,
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

async fn lookup(ctx: &Context<'_>, request: LookupRequest) -> Answer {
    check_entry(&request.version, &request.key)?;
    if request.restore_keys.len() > MAX_RESTORE_KEYS {
        return Err(Refusal::new(
            400,
            format!("at most {MAX_RESTORE_KEYS} restore keys"),
        ));
    }
    for key in &request.restore_keys {
        check_key(key).map_err(|err| Refusal::new(400, err))?;
    }
    let default_branch = store::repo(&ctx.db, ctx.run.repo_id)
        .await?
        .map(|repo| repo.default_branch);
    let scopes = read_scopes(&ctx.run.git_ref, default_branch.as_deref());
    let wanted: Vec<(&str, bool)> = std::iter::once((request.key.as_str(), true))
        .chain(request.restore_keys.iter().map(|key| (key.as_str(), false)))
        .collect();
    for scope in &scopes {
        let owner = CacheOwner {
            scope,
            ..ctx.owner()
        };
        for &(key, exact) in &wanted {
            let Some(found) =
                store::cache_entry(&ctx.db, owner, &request.version, key, exact).await?
            else {
                continue;
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
            return json(
                &LookupResponse {
                    key: found.key,
                    url,
                    size: found.size.unwrap_or(0),
                },
                200,
            );
        }
    }
    Err(Refusal::new(404, "no entry"))
}

async fn start(ctx: &Context<'_>, request: UploadRequest) -> Answer {
    check_entry(&request.version, &request.key)?;
    // Saves the upload when the entry exists; finalizing still settles
    // a race between two jobs.
    if store::cache_entry(&ctx.db, ctx.owner(), &request.version, &request.key, true)
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
        store::drop_pending_cache_entry(&ctx.db, row.id, ctx.owner()).await?;
        return Err(Refusal::new(409, "another job saved the entry first"));
    }
    // The agent does not send the size, and S3 knows it.
    let size = object_size(ctx, &credentials, &row.object_key).await?;
    let done = store::finalize_cache_entry(&ctx.db, row.id, ctx.owner(), size, now_ms()).await?;
    match done {
        Finalized::Done => Ok(Response::empty()?.with_status(204)),
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

/// The size of the object at `key`, from a listing of that prefix.
async fn object_size(
    ctx: &Context<'_>,
    credentials: &Credentials,
    key: &str,
) -> std::result::Result<u64, Refusal> {
    let request = s3::list_objects(&object_url(ctx, ""), key, None);
    let listing = s3::listing(&signed(ctx, credentials, request).await?)
        .map_err(aws_failure("ListObjectsV2"))?;
    listing
        .objects
        .iter()
        .find(|object| object.key == key)
        .map(|object| object.size)
        .ok_or_else(|| Refusal::new(502, "the completed upload is not in the bucket"))
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
/// uploads that never finished, at most [`EVICT_LIMIT`] of them, and
/// returns how many went. Each object goes first and its row after, so
/// a failed delete leaves the row for the next pass. A pass costs at most
/// [`EVICT_COST`] subrequests.
///
/// # Errors
///
/// What went wrong, for the log: Postgres, or the CI role.
pub async fn evict(settings: &Settings, db: &Db, now: u64) -> std::result::Result<usize, String> {
    if settings.artifacts_bucket.is_empty() {
        return Ok(0);
    }
    let idle_ms = IDLE_DAYS * 86_400_000;
    let rows = store::idle_cache_entries(db, now, idle_ms, STALE_UPLOAD_MS, EVICT_LIMIT)
        .await
        .map_err(|err| err.to_string())?;
    if rows.is_empty() {
        return Ok(0);
    }
    let credentials = credentials(settings)
        .await
        .map_err(|err| err.message.clone())?;
    let results = futures_util::future::join_all(rows.iter().map(|row| {
        let url = s3::object_url(
            &settings.artifacts_bucket,
            &settings.ci_region,
            settings.aws_endpoint.as_deref(),
            &row.object_key,
        );
        let credentials = &credentials;
        async move {
            // Rows are trusted no further than the requests that made
            // them: only keys under cache/ are deleted here.
            if !in_repo(&row.object_key, &row.tenant_id, row.repo_id) {
                return Err(format!("entry {} has a foreign key", row.id));
            }
            let response = sign_send(settings, credentials, s3::delete_object(&url))
                .await
                .map_err(|err| err.message)?;
            s3::ok(&response).map_err(|err| err.to_string())
        }
    }))
    .await;
    let mut gone = Vec::new();
    for (row, result) in rows.iter().zip(results) {
        match result {
            Ok(()) => gone.push(row.id),
            Err(err) => worker::console_error!("cache: evict {}: {err}", row.id),
        }
    }
    store::delete_idle_cache_entries(db, &gone, now, idle_ms)
        .await
        .map_err(|err| err.to_string())?;
    Ok(gone.len())
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
    fn a_run_reads_its_own_ref_then_the_default_branch() {
        assert_eq!(
            read_scopes("refs/heads/topic", Some("main")),
            ["refs/heads/topic", "refs/heads/main"]
        );
        assert_eq!(
            read_scopes("refs/pull/4/merge", Some("main")),
            ["refs/pull/4/merge", "refs/heads/main"]
        );
        assert_eq!(
            read_scopes("refs/heads/main", Some("main")),
            ["refs/heads/main"]
        );
        assert_eq!(read_scopes("refs/tags/v1", None), ["refs/tags/v1"]);
    }

    #[test]
    fn the_cache_paths_are_the_agents_cache_calls() {
        assert_eq!(PATH, "/v1/agent/cache");
    }
}
