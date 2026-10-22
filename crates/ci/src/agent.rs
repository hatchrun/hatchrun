// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The routes the runner agent calls: `POST /v1/agent/claim`,
//! `/v1/agent/events` and `/v1/agent/result` (see
//! [`hatchrun_workflow::agent`]).
//!
//! Every call carries the job token as a bearer token. The token names
//! the job, its run and its attempt, and the Worker answers only for the
//! job row those three match; a signature alone proves nothing but that
//! the control plane issued the token. The attempt comes from the token,
//! never from the request.
//!
//! A claim hands the job over in this order: the row must be launched,
//! the instance's metadata endpoint is turned off, a poll sees the
//! change applied, and only then is a GitHub token scoped to the job's
//! repository minted and passed to the run object, whose job document is
//! the answer. Events and results go to the run object unchanged, for the token's run, job
//! and attempt; the object checks the job is in its run, the attempt and that
//! the job is running.

use std::time::Duration;

use futures_util::StreamExt;
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::start::app_jwt;
use hatchrun_workflow::agent::{CLAIM_PATH, EVENTS_PATH, RESULT_PATH};
use serde_json::json;
use worker::{Delay, Env, Method, Request, RequestInit, Response, Result, console_error};

use crate::env::{Settings, run_stub};
use crate::launcher::Backend;
use crate::store::{self, JobRow, RepoRow, RunRow};
use crate::token::{self, Claims, TokenError};

/// The largest body of an events or result call, in bytes.
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// How long a claim waits for the metadata change to apply, in
/// milliseconds.
pub const CLAIM_WAIT_MS: u64 = 10_000;
/// The pause before the first repeat poll of the metadata change, in
/// milliseconds. It doubles up to [`POLL_MAX_MS`].
pub const POLL_FIRST_MS: u64 = 1_000;
/// The longest pause between polls, in milliseconds.
pub const POLL_MAX_MS: u64 = 5_000;

/// The pause after the `polls`-th poll (from 0) found the change not
/// applied: 1 s, 2 s, 4 s, then 5 s.
pub(crate) fn poll_pause_ms(polls: u32) -> u64 {
    (POLL_FIRST_MS << polls.min(8)).min(POLL_MAX_MS)
}

/// What the claim does next while it waits for the metadata change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimStep {
    /// The change is applied: hand the job over.
    Ready,
    /// Poll again after a pause.
    Wait,
    /// Out of time: answer `503` and let the agent ask again.
    Retry,
}

/// Decides the next step from whether the change `applied` and how long
/// the claim has `waited_ms`.
pub(crate) fn claim_step(applied: bool, waited_ms: u64) -> ClaimStep {
    if applied {
        ClaimStep::Ready
    } else if waited_ms < CLAIM_WAIT_MS {
        ClaimStep::Wait
    } else {
        ClaimStep::Retry
    }
}

/// The claims of the `Bearer <token>` in an `Authorization` header.
///
/// # Errors
///
/// [`TokenError::Malformed`] for a missing or non-bearer header, and
/// whatever [`token::verify`] says otherwise.
pub(crate) fn authorize(
    header: Option<&str>,
    key: &[u8],
    now_secs: u64,
) -> std::result::Result<Claims, TokenError> {
    let token = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(TokenError::Malformed)?;
    token::verify(key, token, now_secs)
}

/// How a token's claims relate to a job row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Identity {
    /// Same job, run and attempt.
    Match,
    /// The job is right but the attempt is not: the job was re-run.
    Stale,
    /// The token is for another job or run.
    Refused,
}

fn identity(row: &JobRow, claims: &Claims) -> Identity {
    if claims.job != row.id || claims.run != row.run_id {
        Identity::Refused
    } else if u32::try_from(claims.attempt) == Ok(row.attempt) {
        Identity::Match
    } else {
        Identity::Stale
    }
}

/// What a claim finds in the job row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimAnswer {
    /// The instance is up: turn its metadata off and hand over the job.
    Go,
    /// Not launched yet: answer `503`.
    Retry,
    /// The attempt ended, was replaced or never started: answer `409`.
    Ended,
    /// The token is for another job or run: answer `401`.
    Refused,
}

/// Decides what a claim with `claims` gets from `row`. A job still
/// `queued` or `launching` is retried, since an instance can boot before
/// the launcher records it. A repeated claim finds the row `running` and
/// goes on, and the run object answers it with the same document.
pub(crate) fn claim_answer(row: &JobRow, claims: &Claims) -> ClaimAnswer {
    match identity(row, claims) {
        Identity::Refused => ClaimAnswer::Refused,
        Identity::Stale => ClaimAnswer::Ended,
        Identity::Match => match row.status.as_str() {
            "launched" | "running" if row.instance_id.is_some() => ClaimAnswer::Go,
            "queued" | "launching" | "launched" | "running" => ClaimAnswer::Retry,
            _ => ClaimAnswer::Ended,
        },
    }
}

/// Whether the run, its repository and the job belong together. The
/// store looks runs and repositories up by id alone, so the tenant of
/// each is compared here before an installation token is minted.
pub(crate) fn consistent(job: &JobRow, run: &RunRow, repo: &RepoRow) -> bool {
    job.run_id == run.id && run.repo_id == repo.id && run.tenant_id == repo.tenant_id
}

/// A request body collected up to [`MAX_BODY_BYTES`].
#[derive(Default)]
pub(crate) struct Capped {
    bytes: Vec<u8>,
}

impl Capped {
    /// Adds a chunk. Returns `false`, keeping nothing of the chunk,
    /// when it would take the body past the limit.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> bool {
        if self.bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Whether a body of `len` bytes is too big to forward.
pub(crate) fn too_large(len: usize) -> bool {
    len > MAX_BODY_BYTES
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn status(code: u16) -> Result<Response> {
    Ok(Response::empty()?.with_status(code))
}

fn text(message: &str, code: u16) -> Result<Response> {
    Response::error(message, code)
}

/// Answers a call to one of the agent paths.
///
/// # Errors
///
/// The Worker runtime failed to build a response.
pub async fn handle(mut req: Request, env: &Env) -> Result<Response> {
    let path = req.path();
    if req.method() != Method::Post || ![CLAIM_PATH, EVENTS_PATH, RESULT_PATH].contains(&&*path) {
        return text("not found", 404);
    }
    let settings = match Settings::read(env) {
        Ok(settings) => settings,
        Err(err) => {
            console_error!("agent api: {err}");
            return text("not configured", 500);
        }
    };
    let header = req.headers().get("Authorization").ok().flatten();
    let Ok(claims) = authorize(header.as_deref(), &settings.token_key, now_ms() / 1000) else {
        return text("unauthorized", 401);
    };
    let body = if path == CLAIM_PATH {
        Vec::new()
    } else {
        let declared = req
            .headers()
            .get("Content-Length")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<usize>().ok());
        if declared.is_some_and(too_large) {
            return text("body too large", 413);
        }
        let mut stream = req.stream()?;
        let mut capped = Capped::default();
        while let Some(chunk) = stream.next().await {
            if !capped.push(&chunk?) {
                return text("body too large", 413);
            }
        }
        capped.into_bytes()
    };
    if path == CLAIM_PATH {
        let db = match Db::connect(env).await {
            Ok(db) => db,
            Err(err) => {
                console_error!("agent api: database: {err}");
                return text("unavailable", 503);
            }
        };
        let row = match store::job(&db, claims.job).await {
            Ok(Some(row)) => row,
            Ok(None) => return status(409),
            Err(err) => {
                console_error!("agent api: job {}: {err}", claims.job);
                return text("unavailable", 503);
            }
        };
        return claim(env, &settings, &db, &row, &claims).await;
    }
    // The run object checks that the job is in its run, the attempt and
    // that the job is running.
    let Ok(attempt) = u32::try_from(claims.attempt) else {
        return text("unauthorized", 401);
    };
    let route = if path == EVENTS_PATH {
        "events"
    } else {
        "result"
    };
    to_run(env, claims.run, route, claims.job, attempt, body).await
}

/// Hands the job over, as the module docs describe.
async fn claim(
    env: &Env,
    settings: &Settings,
    db: &Db,
    row: &JobRow,
    claims: &Claims,
) -> Result<Response> {
    match claim_answer(row, claims) {
        ClaimAnswer::Refused => return text("unauthorized", 401),
        ClaimAnswer::Ended => return status(409),
        ClaimAnswer::Retry => return retry(),
        ClaimAnswer::Go => {}
    }
    let Some(instance_id) = row.instance_id.as_deref() else {
        return retry();
    };
    let Some(run) = store::run(db, row.run_id).await? else {
        return status(409);
    };
    let Some(repo) = store::repo(db, run.repo_id).await? else {
        return status(409);
    };
    if !consistent(row, &run, &repo) {
        console_error!("job {}: run and repository disagree on the tenant", row.id);
        return text("unauthorized", 401);
    }
    match metadata_off(settings, instance_id).await {
        Ok(true) => {}
        Ok(false) => return retry(),
        Err(err) => {
            console_error!("job {}: metadata: {err}", row.id);
            return retry();
        }
    }
    let github_token = match mint(settings, &repo).await {
        Ok(token) => token,
        Err(err) => {
            console_error!("job {}: installation token: {err}", row.id);
            return retry();
        }
    };
    let body = json!({ "token": github_token }).to_string().into_bytes();
    to_run(env, run.id, "claim", row.id, row.attempt, body).await
}

fn retry() -> Result<Response> {
    let mut response = text("not ready", 503)?;
    response.headers_mut().set("Retry-After", "1")?;
    Ok(response)
}

/// Turns the instance's metadata off and polls until it is applied.
/// `Ok(false)` means the time ran out.
async fn metadata_off(settings: &Settings, instance_id: &str) -> std::result::Result<bool, String> {
    let backend = Backend::new(settings).await?;
    backend.metadata_off(instance_id).await?;
    let start = now_ms();
    let mut polls = 0_u32;
    loop {
        let applied = backend.metadata_applied(instance_id).await?;
        match claim_step(applied, now_ms().saturating_sub(start)) {
            ClaimStep::Ready => return Ok(true),
            ClaimStep::Retry => return Ok(false),
            ClaimStep::Wait => {
                Delay::from(Duration::from_millis(poll_pause_ms(polls))).await;
                polls += 1;
            }
        }
    }
}

/// A GitHub token for the job's repository.
async fn mint(settings: &Settings, repo: &RepoRow) -> std::result::Result<String, String> {
    let jwt = app_jwt(&settings.app_id, &settings.app_key).await?;
    let request =
        crate::github::scoped_token(&settings.github_api, repo.installation_id, &jwt, repo.id);
    let response = send(&request).await?;
    crate::github::installation_token(&response)
}

/// Passes `body` to `route` of the run's object for the token's job and
/// attempt, and returns its answer.
async fn to_run(
    env: &Env,
    run_id: u64,
    route: &str,
    job: u64,
    attempt: u32,
    body: Vec<u8>,
) -> Result<Response> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(js_sys::Uint8Array::from(body.as_slice()).into()));
    let url = format!("https://run/{route}?job={job}&attempt={attempt}");
    let request = Request::new_with_init(&url, &init)?;
    run_stub(env, run_id)?.fetch_with_request(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::token::tests::random_key;
    use std::sync::LazyLock;

    /// The key for this test run, drawn fresh every run.
    static KEY: LazyLock<[u8; 32]> = LazyLock::new(random_key);

    fn row(status: &str) -> JobRow {
        JobRow {
            id: 7,
            run_id: 1,
            plan_id: "build".into(),
            name: "build".into(),
            status: status.into(),
            conclusion: None,
            instance_id: Some("i-1".into()),
            attempt: 2,
            launch_attempts: 1,
            next_launch_at: None,
            timeout_minutes: 10,
            updated_at: 0,
            heartbeat_at: None,
            started_at: None,
            completed_at: None,
            steps: serde_json::json!([]),
            log_key: None,
            template: Some("ubuntu-latest".into()),
            token_permissions: serde_json::json!({}),
            secret_refs: serde_json::json!({ "all": false, "names": [] }),
        }
    }

    fn claims() -> Claims {
        Claims {
            job: 7,
            run: 1,
            attempt: 2,
            exp: 1_000,
        }
    }

    fn run_row(tenant: &str, repo: u64) -> RunRow {
        serde_json::from_value(serde_json::json!({
            "id": 1, "run_number": 1, "run_attempt": 1, "tenant_id": tenant,
            "repo_id": repo, "workflow": "ci.yml", "delivery": "d", "event": "push",
            "git_ref": "refs/heads/main", "sha": "abc", "actor": "a",
            "status": "queued", "conclusion": null, "created_at": 0, "completed_at": null
        }))
        .expect("a run row")
    }

    fn repo_row(tenant: &str, id: u64) -> RepoRow {
        RepoRow {
            id,
            tenant_id: tenant.into(),
            installation_id: 5,
            full_name: "o/r".into(),
            default_branch: "main".into(),
            private: false,
        }
    }

    #[test]
    fn the_poll_pause_doubles_up_to_the_cap() {
        let pauses: Vec<u64> = (0..6).map(poll_pause_ms).collect();
        assert_eq!(pauses, [1_000, 2_000, 4_000, 5_000, 5_000, 5_000]);
        assert_eq!(poll_pause_ms(u32::MAX), 5_000);
    }

    #[test]
    fn claim_waits_until_metadata_is_applied() {
        assert_eq!(claim_step(false, 0), ClaimStep::Wait);
        assert_eq!(claim_step(false, 9_999), ClaimStep::Wait);
        assert_eq!(claim_step(false, 10_000), ClaimStep::Retry);
        assert_eq!(claim_step(true, 0), ClaimStep::Ready);
        assert_eq!(claim_step(true, 20_000), ClaimStep::Ready);
    }

    #[test]
    fn bad_token_is_unauthorized() {
        assert!(authorize(Some("Bearer x"), &*KEY, 0).is_err());
        assert!(authorize(None, &*KEY, 0).is_err());
        let signed = token::sign(&*KEY, &claims());
        assert_eq!(
            authorize(Some(&signed), &*KEY, 0),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            authorize(Some(&format!("Bearer {signed}")), &random_key(), 0),
            Err(TokenError::BadSignature)
        );
        assert_eq!(
            authorize(Some(&format!("Bearer {signed}")), &*KEY, 1_000),
            Err(TokenError::Expired)
        );
        assert_eq!(
            authorize(Some(&format!("Bearer {signed}")), &*KEY, 999),
            Ok(claims())
        );
    }

    #[test]
    fn job_of_another_run_is_refused() {
        let other_run = Claims { run: 2, ..claims() };
        assert_eq!(
            claim_answer(&row("running"), &other_run),
            ClaimAnswer::Refused
        );
        let other_job = Claims { job: 8, ..claims() };
        assert_eq!(
            claim_answer(&row("running"), &other_job),
            ClaimAnswer::Refused
        );
    }

    #[test]
    fn a_rerun_ends_the_old_attempt() {
        let old = Claims {
            attempt: 1,
            ..claims()
        };
        assert_eq!(claim_answer(&row("launched"), &old), ClaimAnswer::Ended);
        let negative = Claims {
            attempt: -1,
            ..claims()
        };
        assert_eq!(
            claim_answer(&row("launched"), &negative),
            ClaimAnswer::Ended
        );
    }

    #[test]
    fn launching_job_is_retried() {
        assert_eq!(
            claim_answer(&row("launching"), &claims()),
            ClaimAnswer::Retry
        );
        assert_eq!(claim_answer(&row("queued"), &claims()), ClaimAnswer::Retry);
        assert_eq!(
            claim_answer(&row("completed"), &claims()),
            ClaimAnswer::Ended
        );
        assert_eq!(claim_answer(&row("waiting"), &claims()), ClaimAnswer::Ended);
    }

    #[test]
    fn launched_and_running_jobs_go_on_with_an_instance() {
        assert_eq!(claim_answer(&row("launched"), &claims()), ClaimAnswer::Go);
        assert_eq!(claim_answer(&row("running"), &claims()), ClaimAnswer::Go);
        let mut bare = row("launched");
        bare.instance_id = None;
        assert_eq!(claim_answer(&bare, &claims()), ClaimAnswer::Retry);
    }

    #[test]
    fn run_repository_and_job_must_share_a_tenant() {
        let job = row("launched");
        assert!(consistent(&job, &run_row("t1", 9), &repo_row("t1", 9)));
        assert!(!consistent(&job, &run_row("t1", 9), &repo_row("t2", 9)));
        assert!(!consistent(&job, &run_row("t1", 9), &repo_row("t1", 10)));
        let mut elsewhere = row("launched");
        elsewhere.run_id = 2;
        assert!(!consistent(
            &elsewhere,
            &run_row("t1", 9),
            &repo_row("t1", 9)
        ));
    }

    #[test]
    fn chunks_stop_at_the_limit() {
        let mut capped = Capped::default();
        assert!(capped.push(&vec![0; MAX_BODY_BYTES - 1]));
        assert!(capped.push(&[1]));
        assert!(!capped.push(&[2]));
        assert_eq!(capped.into_bytes().len(), MAX_BODY_BYTES);
        // One chunk past the limit is refused whole.
        let mut big = Capped::default();
        assert!(!big.push(&vec![0; MAX_BODY_BYTES + 1]));
        assert_eq!(big.into_bytes(), [] as [u8; 0]);
    }

    #[test]
    fn bodies_over_one_mebibyte_are_too_large() {
        assert!(!too_large(MAX_BODY_BYTES));
        assert!(too_large(MAX_BODY_BYTES + 1));
    }
}
