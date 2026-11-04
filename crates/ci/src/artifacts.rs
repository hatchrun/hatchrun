// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/v1/agent/artifacts/*`: what `actions/upload-artifact` and
//! `download-artifact` reach through the agent (see
//! [`hatchrun_workflow::artifacts`] for the calls).
//!
//! The bytes are one S3 object per artifact in the CI account's bucket,
//! at `artifacts/<days>d/<tenant>/<repo id>/<run id>/<random>`, where
//! `days` is the retention tier of the run (the lifecycle rules in
//! `deploy/cloudformation/ci-account.yaml` delete by that prefix). The
//! key and every query come from the job token's run, never from a
//! request: the token names the job, the job row names the run, and the
//! run row names the tenant and repository. A job therefore reads and
//! writes the artifacts of its own run, which includes those uploaded by
//! earlier attempts, and nothing else. The agent never sees a key, only
//! presigned URLs the Worker made for rows of that run.
//!
//! Calls are refused unless the job is running at the token's attempt.

use std::cell::RefCell;

use futures_util::StreamExt;
use hatchrun_cloud::aws::{self, Credentials, s3};
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::http::{Request as Outgoing, Response as Incoming};
use hatchrun_cloud::start::role_credentials;
use hatchrun_cloud::webcrypto;
use hatchrun_workflow::artifacts::{
    Artifact, CompleteRequest, DeleteResponse, ListRequest, ListResponse, MAX_PER_RUN, NameRequest,
    PATH, StartRequest, StartResponse, UrlResponse, check_name,
};
use serde::Deserialize;
use worker::{Env, Method, Request, Response, Result, console_error, console_warn};

use crate::agent::{Capped, running_attempt, too_large};
use crate::env::Settings;
use crate::launcher::TENANT;
use crate::logs::is_tier;
use crate::store::{self, ArtifactRow, Finalized, JobRow, RunRow};
use crate::token::Claims;

/// What a call from a job that is no longer running is told.
const JOB_ENDED: &str = "the job is not running";

/// How long a presigned URL holds, in seconds.
const URL_SECS: u64 = 600;
/// How long the Worker reuses the role credentials, in milliseconds.
/// STS sessions last 15 minutes, so a URL presigned at the end of this
/// still holds for [`URL_SECS`].
const CREDENTIALS_MS: u64 = 4 * 60_000;
/// Part URLs one request may ask for.
const MAX_PARTS_PER_REQUEST: usize = 100;
/// S3's part numbers run from 1 to this.
const MAX_PART: u16 = 10_000;

thread_local! {
    static CREDENTIALS: RefCell<Option<(Credentials, u64)>> = const { RefCell::new(None) };
}

/// An answer other than success.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) status: u16,
    pub(crate) message: String,
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
        console_error!("artifacts: {err}");
        Self::new(503, "unavailable")
    }
}

type Answer = std::result::Result<Response, Refusal>;

/// Whether `job` may call the service with `claims`: the token is for
/// this job, run and attempt, and the job is running.
///
/// A job that is not running at the token's attempt gets `410`, which
/// the agent tells apart from the `409` of a name that is taken.
pub(crate) fn check_job(row: &JobRow, claims: &Claims) -> std::result::Result<(), Refusal> {
    if running_attempt(row, claims) {
        Ok(())
    } else {
        Err(Refusal::new(410, JOB_ENDED))
    }
}

/// The S3 key of an artifact of run `run` in the tier of `days`, named
/// by `token`, which is random.
pub(crate) fn object_key(
    days: u32,
    tenant: &str,
    repo_id: u64,
    run_id: u64,
    token: &str,
) -> String {
    format!("artifacts/{days}d/{tenant}/{repo_id}/{run_id}/{token}")
}

/// Whether `key` is where an artifact of this run lives.
pub(crate) fn in_run(key: &str, days: u32, tenant: &str, repo_id: u64, run_id: u64) -> bool {
    key.starts_with(&format!("artifacts/{days}d/{tenant}/{repo_id}/{run_id}/"))
        && !key.contains("..")
}

/// Whether the artifact `existing` may be replaced by an upload of
/// `name` from attempt `attempt` of job `job`: only by a later attempt
/// of the job that uploaded it.
pub(crate) fn replaceable(existing: &ArtifactRow, job: u64, attempt: u32) -> bool {
    existing.job_id == job && existing.attempt < attempt
}

/// Part numbers of a parts request.
fn check_part_numbers(numbers: &[u16]) -> std::result::Result<(), String> {
    if numbers.is_empty() || numbers.len() > MAX_PARTS_PER_REQUEST {
        return Err(format!("ask for 1 to {MAX_PARTS_PER_REQUEST} parts"));
    }
    if numbers
        .iter()
        .any(|number| !(1..=MAX_PART).contains(number))
    {
        return Err(format!("part numbers run from 1 to {MAX_PART}"));
    }
    Ok(())
}

/// The parts of a complete request, sorted, each once.
fn complete_parts(
    mut parts: Vec<hatchrun_workflow::artifacts::Part>,
) -> std::result::Result<Vec<s3::Part>, String> {
    if parts.is_empty() || parts.len() > usize::from(MAX_PART) {
        return Err(format!("complete with 1 to {MAX_PART} parts"));
    }
    parts.sort_by_key(|part| part.number);
    if parts
        .windows(2)
        .any(|pair| pair[0].number == pair[1].number)
    {
        return Err("each part once".to_owned());
    }
    if parts
        .iter()
        .any(|part| !(1..=MAX_PART).contains(&part.number) || part.etag.is_empty())
    {
        return Err(format!(
            "parts need a number from 1 to {MAX_PART} and an etag"
        ));
    }
    Ok(parts
        .into_iter()
        .map(|part| s3::Part {
            number: part.number,
            etag: part.etag,
        })
        .collect())
}

/// Everything a call works with once the token checks out.
struct Context<'a> {
    settings: &'a Settings,
    db: Db,
    job: JobRow,
    run: RunRow,
    bucket: &'a str,
}

/// Answers a call to `/v1/agent/artifacts/*`. `claims` were checked by
/// the caller.
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
                console_warn!("artifacts: {message}");
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
            ["uploads" | "list" | "url" | "delete"] | ["uploads", _, "parts" | "complete"]
        ) | (Method::Delete, ["uploads", _])
    );
    if !known {
        return Err(Refusal::new(404, "not found"));
    }
    if settings.artifacts_bucket.is_empty() {
        return Err(Refusal::new(503, "artifacts are not configured"));
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
        job,
        run,
        bucket: &settings.artifacts_bucket,
    };
    match (req.method(), segments.as_slice()) {
        (Method::Post, ["uploads"]) => start(&ctx, body(req).await?).await,
        (Method::Post, ["uploads", id, "parts"]) => parts(&ctx, id, body(req).await?).await,
        (Method::Post, ["uploads", id, "complete"]) => complete(&ctx, id, body(req).await?).await,
        (Method::Delete, ["uploads", id]) => abort(&ctx, id).await,
        (Method::Post, ["list"]) => list(&ctx, body(req).await?).await,
        (Method::Post, ["url"]) => url(&ctx, body(req).await?).await,
        (Method::Post, ["delete"]) => delete(&ctx, body(req).await?).await,
        _ => Err(Refusal::new(404, "not found")),
    }
}

async fn body<T: for<'de> Deserialize<'de>>(req: &mut Request) -> std::result::Result<T, Refusal> {
    let declared = req
        .headers()
        .get("Content-Length")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(too_large) {
        return Err(Refusal::new(413, "body too large"));
    }
    // Stop reading at the cap: the isolate never holds more than that.
    let mut stream = req.stream()?;
    let mut capped = Capped::default();
    while let Some(chunk) = stream.next().await {
        if !capped.push(&chunk?) {
            return Err(Refusal::new(413, "body too large"));
        }
    }
    let bytes = capped.into_bytes();
    serde_json::from_slice(&bytes).map_err(|err| Refusal::new(400, format!("bad request: {err}")))
}

fn json(value: &impl serde::Serialize, status: u16) -> Answer {
    Ok(Response::from_json(value)?.with_status(status))
}

fn id_of(id: &str) -> std::result::Result<u64, Refusal> {
    id.parse().map_err(|_| Refusal::new(404, "no such upload"))
}

async fn start(ctx: &Context<'_>, request: StartRequest) -> Answer {
    check_name(&request.name).map_err(|err| Refusal::new(400, err))?;
    if !is_tier(ctx.run.retention_days) {
        return Err(Refusal::new(500, "the run has no retention tier"));
    }
    if let Some(existing) = store::artifact_by_name(&ctx.db, ctx.run.id, &request.name).await?
        && !replaceable(&existing, ctx.job.id, ctx.job.attempt)
    {
        return Err(Refusal::new(409, "an artifact of this name exists"));
    }
    if store::artifact_count(&ctx.db, ctx.run.id).await? >= MAX_PER_RUN {
        return Err(Refusal::new(
            429,
            format!("a run holds at most {MAX_PER_RUN} artifacts"),
        ));
    }
    let token = hatchrun_cloud::jwt::b64url(webcrypto::random_bytes(16)?);
    let key = object_key(
        ctx.run.retention_days,
        &ctx.run.tenant_id,
        ctx.run.repo_id,
        ctx.run.id,
        &token,
    );
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
    let row = store::start_artifact(
        &ctx.db,
        ctx.run.id,
        ctx.job.id,
        ctx.job.attempt,
        &request.name,
        &key,
        &upload_id,
        now_ms(),
    )
    .await?;
    json(
        &StartResponse {
            upload_id: row.id.to_string(),
        },
        201,
    )
}

/// The unfinished upload `id` of the job's run.
async fn pending(ctx: &Context<'_>, id: &str) -> std::result::Result<ArtifactRow, Refusal> {
    let row = store::pending_artifact(&ctx.db, id_of(id)?, ctx.run.id)
        .await?
        .ok_or_else(|| Refusal::new(404, "no such upload"))?;
    // The row's key must be one this run would have made.
    if !in_run(
        &row.object_key,
        ctx.run.retention_days,
        &ctx.run.tenant_id,
        ctx.run.repo_id,
        ctx.run.id,
    ) {
        return Err(Refusal::new(500, "the upload has a foreign key"));
    }
    Ok(row)
}

async fn parts(ctx: &Context<'_>, id: &str, request: serde_json::Value) -> Answer {
    #[derive(Deserialize)]
    struct Parts {
        numbers: Vec<u16>,
    }
    #[derive(serde::Serialize)]
    struct PartUrl {
        number: u16,
        url: String,
    }
    let Parts { numbers } = serde_json::from_value(request)
        .map_err(|err| Refusal::new(400, format!("bad request: {err}")))?;
    check_part_numbers(&numbers).map_err(|err| Refusal::new(400, err))?;
    let row = pending(ctx, id).await?;
    let upload_id = row
        .upload_id
        .ok_or_else(|| Refusal::new(404, "no such upload"))?;
    let credentials = credentials(ctx.settings).await?;
    let url = object_url(ctx, &row.object_key);
    let now = now_ms();
    let parts = numbers
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
    json(&serde_json::json!({ "parts": parts }), 200)
}

async fn complete(ctx: &Context<'_>, id: &str, request: serde_json::Value) -> Answer {
    let request: CompleteRequest = serde_json::from_value(request)
        .map_err(|err| Refusal::new(400, format!("bad request: {err}")))?;
    let parts = complete_parts(request.parts).map_err(|err| Refusal::new(400, err))?;
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
    s3::completed(&response).map_err(aws_failure("CompleteMultipartUpload"))?;
    let replaced = store::replaced_artifact(&ctx.db, row.id, ctx.run.id).await?;
    let done = store::finalize_artifact(
        &ctx.db,
        row.id,
        ctx.run.id,
        request.size,
        request.digest.as_deref(),
        now_ms(),
    )
    .await?;
    match done {
        Finalized::Done => {
            // The row of the earlier attempt's artifact went with the
            // finalize; its object goes now, as in `delete`.
            if let Some(old) = replaced {
                let url = object_url(ctx, &old.object_key);
                if let Err(err) = signed(ctx, &credentials, s3::delete_object(&url)).await {
                    console_warn!("artifacts: delete replaced {}: {}", old.id, err.message);
                }
            }
            json(&serde_json::json!({ "id": row.id }), 200)
        }
        Finalized::Refused => {
            // Another artifact took the name first. The object is of no
            // use; the lifecycle rule would remove it, this does it now.
            if let Err(err) = signed(ctx, &credentials, s3::delete_object(&url)).await {
                console_warn!("artifacts: delete {}: {}", row.id, err.message);
            }
            store::drop_pending_artifact(&ctx.db, row.id, ctx.run.id).await?;
            Err(Refusal::new(409, "an artifact of this name exists"))
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
    store::drop_pending_artifact(&ctx.db, row.id, ctx.run.id).await?;
    Ok(Response::empty()?.with_status(204))
}

async fn list(ctx: &Context<'_>, request: ListRequest) -> Answer {
    let rows = store::artifacts(&ctx.db, ctx.run.id, request.name.as_deref(), request.id).await?;
    let artifacts = rows
        .into_iter()
        .map(|row| Artifact {
            id: row.id,
            name: row.name,
            size: row.size.unwrap_or(0),
            created_at: row.finalized_at.unwrap_or(row.created_at),
        })
        .collect();
    json(&ListResponse { artifacts }, 200)
}

async fn url(ctx: &Context<'_>, request: NameRequest) -> Answer {
    let row = store::artifact_by_name(&ctx.db, ctx.run.id, &request.name)
        .await?
        .ok_or_else(|| Refusal::new(404, "no such artifact"))?;
    if !in_run(
        &row.object_key,
        ctx.run.retention_days,
        &ctx.run.tenant_id,
        ctx.run.repo_id,
        ctx.run.id,
    ) {
        return Err(Refusal::new(500, "the artifact has a foreign key"));
    }
    let credentials = credentials(ctx.settings).await?;
    let url = s3::presign(
        &s3::get_object(&object_url(ctx, &row.object_key)),
        &credentials,
        &ctx.settings.ci_region,
        now_ms(),
        std::time::Duration::from_secs(URL_SECS),
    )
    .map_err(|err| Refusal::new(500, err))?;
    json(
        &UrlResponse {
            url,
            size: row.size.unwrap_or(0),
        },
        200,
    )
}

async fn delete(ctx: &Context<'_>, request: NameRequest) -> Answer {
    let row = store::delete_artifact(&ctx.db, ctx.run.id, &request.name)
        .await?
        .ok_or_else(|| Refusal::new(404, "no such artifact"))?;
    // The row is gone, so the artifact is as good as deleted; a failed
    // S3 delete leaves the object to the lifecycle rule.
    match credentials(ctx.settings).await {
        Ok(credentials) => {
            let url = object_url(ctx, &row.object_key);
            if let Err(err) = signed(ctx, &credentials, s3::delete_object(&url)).await {
                console_warn!("artifacts: delete {}: {}", row.id, err.message);
            }
        }
        Err(err) => console_warn!("artifacts: delete {}: {}", row.id, err.message),
    }
    json(&DeleteResponse { id: row.id }, 200)
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn object_url(ctx: &Context<'_>, key: &str) -> String {
    s3::object_url(
        ctx.bucket,
        &ctx.settings.ci_region,
        ctx.settings.aws_endpoint.as_deref(),
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
) -> std::result::Result<Incoming, Refusal> {
    s3::sign(&mut request, credentials, &ctx.settings.ci_region, now_ms())
        .map_err(|err| Refusal::new(500, format!("sign S3 request: {err}")))?;
    send(&request)
        .await
        .map_err(|err| Refusal::new(502, format!("S3 unreachable: {err}")))
}

/// The CI role's credentials, from the isolate when fresh.
async fn credentials(settings: &Settings) -> std::result::Result<Credentials, Refusal> {
    let now = now_ms();
    let cached = CREDENTIALS.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|(_, at)| now >= *at && now - *at < CREDENTIALS_MS)
            .map(|(credentials, _)| credentials.clone())
    });
    if let Some(credentials) = cached {
        return Ok(credentials);
    }
    if settings.ci_role_arn.is_empty() {
        return Err(Refusal::new(503, "CI_ROLE_ARN is not set"));
    }
    let credentials = role_credentials(
        &settings.oidc_key,
        &settings.oidc_kid,
        settings.aws_endpoint.as_deref(),
        TENANT,
        &settings.ci_role_arn,
        &settings.ci_region,
        "hatchrun-ci-artifacts",
    )
    .await
    .map_err(|err| Refusal::new(502, format!("AssumeRoleWithWebIdentity: {err}")))?;
    CREDENTIALS.with(|cell| *cell.borrow_mut() = Some((credentials.clone(), now)));
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatchrun_workflow::artifacts::Part;

    fn job(status: &str, attempt: u32) -> JobRow {
        serde_json::from_value(serde_json::json!({
            "id": 5, "run_id": 9, "plan_id": "build", "name": "build", "status": status,
            "conclusion": null, "instance_id": "i-1", "attempt": attempt, "launch_attempts": 1,
            "next_launch_at": null, "timeout_minutes": 10, "updated_at": 1,
            "heartbeat_at": null, "started_at": null, "completed_at": null, "steps": "[]",
            "log_key": null, "template": "ubuntu-latest", "token_permissions": "{}",
            "secret_refs": "{}", "held": null, "environment": null, "approval_attempt": null,
            "approval_decision": null, "approval_by": null, "approval_at": null
        }))
        .unwrap()
    }

    fn claims(job: u64, run: u64, attempt: i32) -> Claims {
        Claims {
            job,
            run,
            attempt,
            exp: 1,
        }
    }

    #[test]
    fn only_the_running_attempt_of_the_job_may_call() {
        let running = job("running", 2);
        assert_eq!(check_job(&running, &claims(5, 9, 2)), Ok(()));
        // Every refusal is a 410, never the 409 of a taken name.
        for stale in [claims(6, 9, 2), claims(5, 8, 2), claims(5, 9, 1)] {
            let err = check_job(&running, &stale).unwrap_err();
            assert_eq!((err.status, err.message.as_str()), (410, JOB_ENDED));
        }
        for status in ["launched", "completed", "queued"] {
            let row = job(status, 2);
            assert_eq!(check_job(&row, &claims(5, 9, 2)).unwrap_err().status, 410);
        }
    }

    #[test]
    fn keys_name_the_tier_tenant_repository_and_run() {
        let path = object_key(90, "t1", 7, 9, "abc");
        assert_eq!(path, "artifacts/90d/t1/7/9/abc");
        assert!(in_run(&path, 90, "t1", 7, 9));
        // Another run, repository, tenant or tier is not this run's.
        assert!(!in_run(&path, 90, "t1", 7, 10));
        assert!(!in_run(&path, 90, "t1", 8, 9));
        assert!(!in_run(&path, 90, "t2", 7, 9));
        assert!(!in_run(&path, 30, "t1", 7, 9));
        assert!(!in_run("artifacts/90d/t1/7/9/../10/x", 90, "t1", 7, 9));
        // A run id that only starts with this one's.
        assert!(!in_run("artifacts/90d/t1/7/90/abc", 90, "t1", 7, 9));
    }

    fn artifact(job_id: u64, attempt: u32) -> ArtifactRow {
        ArtifactRow {
            id: 1,
            run_id: 9,
            job_id,
            attempt,
            name: "dist".into(),
            object_key: "k".into(),
            upload_id: None,
            size: Some(1),
            digest: None,
            created_at: 1,
            finalized_at: Some(2),
        }
    }

    #[test]
    fn only_a_later_attempt_of_the_same_job_replaces() {
        assert!(replaceable(&artifact(5, 1), 5, 2));
        assert!(!replaceable(&artifact(5, 2), 5, 2));
        assert!(!replaceable(&artifact(5, 2), 5, 1));
        assert!(!replaceable(&artifact(6, 1), 5, 2));
    }

    #[test]
    fn part_numbers_are_checked() {
        assert!(check_part_numbers(&[1, 2, 10_000]).is_ok());
        assert!(check_part_numbers(&[]).is_err());
        assert!(check_part_numbers(&[0]).is_err());
        assert!(check_part_numbers(&[10_001]).is_err());
        assert!(check_part_numbers(&vec![1; MAX_PARTS_PER_REQUEST + 1]).is_err());
    }

    #[test]
    fn complete_parts_sort_and_refuse_repeats() {
        let part = |number, etag: &str| Part {
            number,
            etag: etag.into(),
        };
        let sorted = complete_parts(vec![part(2, "\"b\""), part(1, "\"a\"")]).unwrap();
        assert_eq!(sorted.iter().map(|p| p.number).collect::<Vec<_>>(), [1, 2]);
        assert!(complete_parts(vec![]).is_err());
        assert!(complete_parts(vec![part(1, "a"), part(1, "b")]).is_err());
        assert!(complete_parts(vec![part(1, "")]).is_err());
        assert!(complete_parts(vec![part(0, "a")]).is_err());
    }
}
