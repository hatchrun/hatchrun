// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! OIDC tokens for jobs with `permissions: id-token: write`, from the
//! `token.ci.hatchrun.io` issuer.
//!
//! The tokens follow GitHub's: a job's steps call
//! `core.getIDToken(audience)`, which reads
//! `ACTIONS_ID_TOKEN_REQUEST_URL` and `ACTIONS_ID_TOKEN_REQUEST_TOKEN`,
//! sends `GET <url>&audience=<audience>` with the request token as a
//! bearer token and takes `value` from the answer. A cloud account
//! trusts the issuer and matches the `sub` and `aud` claims.
//!
//! This issuer has its own key ([`SIGNING_KEY`]). It is never the key of
//! `oidc.hatchrun.io`, which customer AWS accounts trust for hatchrun's
//! control plane: a job must not be able to obtain a token that role
//! accepts. The Worker serves the discovery document and the JWKS on
//! [`HOST`] and signs the tokens on its API host. Key slots work as the
//! other issuer's do (see [`hatchrun_cloud::oidc`]): a next and a
//! previous key are published, and only the signing key signs.
//!
//! The request token is an HMAC token of the job's attempt (see
//! [`crate::token::sign_oidc`]), made when the agent claims the job. It
//! is not a job token, which the agent keeps to itself, and it opens
//! nothing but this endpoint. The endpoint answers only while the job's
//! row is `running` at the token's attempt.

use hatchrun_cloud::{jwt, oidc as cloud_oidc, webcrypto};
use hatchrun_workflow::ID_TOKEN_SCOPE;
use serde_json::{Value, json};
use worker::{Env, Method, Request, Response, Result, console_error};

use crate::agent::running_attempt;
use crate::env::Settings;
use crate::github::CONFIG_PATH;
use crate::store::{self, JobRow, RepoRow, RunRow};
use crate::token::{self, TokenError};

/// The issuer URL, as a cloud account's trust names it.
pub const ISSUER: &str = "https://token.ci.hatchrun.io";
/// The issuer's hostname. Requests for it are the issuer's.
pub const HOST: &str = "token.ci.hatchrun.io";
/// Secret holding the issuer's RSA private key in PEM.
pub const SIGNING_KEY: &str = "CI_OIDC_SIGNING_KEY";
/// Variable holding the `kid` of [`SIGNING_KEY`] in the JWKS.
pub const KEY_ID: &str = "CI_OIDC_KEY_ID";
/// Secret holding the key that signs next, published but not signing.
pub const NEXT_SIGNING_KEY: &str = "CI_OIDC_NEXT_SIGNING_KEY";
/// Variable holding the `kid` of [`NEXT_SIGNING_KEY`].
pub const NEXT_KEY_ID: &str = "CI_OIDC_NEXT_KEY_ID";
/// Secret holding the key that signed last, published but not signing.
pub const PREVIOUS_SIGNING_KEY: &str = "CI_OIDC_PREVIOUS_SIGNING_KEY";
/// Variable holding the `kid` of [`PREVIOUS_SIGNING_KEY`].
pub const PREVIOUS_KEY_ID: &str = "CI_OIDC_PREVIOUS_KEY_ID";
/// The keys the JWKS lists, as (secret, `kid` variable) pairs: the
/// signing key first.
pub const SLOTS: [(&str, &str); 3] = [
    (SIGNING_KEY, KEY_ID),
    (NEXT_SIGNING_KEY, NEXT_KEY_ID),
    (PREVIOUS_SIGNING_KEY, PREVIOUS_KEY_ID),
];

/// Path of the discovery document, under [`ISSUER`].
pub const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
/// Path of the JWKS, under [`ISSUER`].
pub const JWKS_PATH: &str = "/.well-known/jwks";
/// Where jobs ask for tokens, on the Worker's API host.
pub const TOKEN_PATH: &str = "/v1/oidc/token";
/// What the toolkit appends `&audience=` to, so the URL needs a query.
pub const API_VERSION_QUERY: &str = "api-version=2.0";
/// The longest audience, in bytes.
pub const MAX_AUDIENCE_BYTES: usize = 512;
/// Seconds a token lives.
pub const LIFETIME: u64 = 5 * 60;

/// The claims the issuer puts in a token.
const CLAIMS: [&str; 20] = [
    "iss",
    "sub",
    "aud",
    "iat",
    "nbf",
    "exp",
    "jti",
    "repository",
    "repository_id",
    "repository_owner",
    "ref",
    "ref_type",
    "sha",
    "workflow",
    "job_workflow_ref",
    "run_id",
    "run_attempt",
    "actor",
    "event_name",
    "environment",
];

/// The URL a job with `id-token: write` gets as
/// `ACTIONS_ID_TOKEN_REQUEST_URL`, on the Worker at `ci_url`.
#[must_use]
pub fn request_url(ci_url: &str) -> String {
    format!("{ci_url}{TOKEN_PATH}?{API_VERSION_QUERY}")
}

/// Whether a job's `token_permissions` (a JSON object) grant OIDC tokens.
#[must_use]
pub fn granted(token_permissions: &Value) -> bool {
    token_permissions
        .get(ID_TOKEN_SCOPE)
        .and_then(Value::as_str)
        == Some("write")
}

/// The audience of a token when the job names none, as GitHub's: the
/// repository owner's URL.
#[must_use]
pub fn default_audience(repo: &RepoRow) -> String {
    format!("https://github.com/{}", owner(&repo.full_name))
}

fn owner(full_name: &str) -> &str {
    full_name
        .split_once('/')
        .map_or(full_name, |(owner, _)| owner)
}

/// The `sub` claim: the environment when the job has one, else the pull
/// request, else the ref.
#[must_use]
pub fn subject(repo: &str, event: &str, git_ref: &str, environment: Option<&str>) -> String {
    if let Some(environment) = environment {
        format!("repo:{repo}:environment:{environment}")
    } else if matches!(event, "pull_request" | "pull_request_target") {
        format!("repo:{repo}:pull_request")
    } else {
        format!("repo:{repo}:ref:{git_ref}")
    }
}

/// The claims of a token for `job` of `run` in `repo`, for `audience`,
/// issued at `now` (Unix seconds) with id `jti`.
///
/// `repository_owner_id` is left out: the control plane does not record
/// the owner's numeric id.
#[must_use]
pub fn claims(
    job: &JobRow,
    run: &RunRow,
    repo: &RepoRow,
    audience: &str,
    now: u64,
    jti: &str,
) -> Value {
    let full = repo.full_name.as_str();
    let ref_type = if run.git_ref.starts_with("refs/tags/") {
        "tag"
    } else {
        "branch"
    };
    let mut claims = json!({
        "iss": ISSUER,
        "sub": subject(full, &run.event, &run.git_ref, job.environment.as_deref()),
        "aud": audience,
        "iat": now,
        "nbf": now,
        "exp": now + LIFETIME,
        "jti": jti,
        "repository": full,
        "repository_id": repo.id.to_string(),
        "repository_owner": owner(full),
        "ref": run.git_ref,
        "ref_type": ref_type,
        "sha": run.sha,
        "workflow": run.workflow,
        "job_workflow_ref": format!("{full}/{CONFIG_PATH}@{}", run.git_ref),
        "run_id": run.id.to_string(),
        "run_attempt": run.run_attempt.to_string(),
        "actor": run.actor,
        "event_name": run.event,
    });
    if let Some(environment) = &job.environment {
        claims["environment"] = json!(environment);
    }
    claims
}

/// `/.well-known/openid-configuration`: where the keys are and what the
/// tokens hold.
#[must_use]
pub fn discovery() -> Value {
    json!({
        "issuer": ISSUER,
        "jwks_uri": format!("{ISSUER}{JWKS_PATH}"),
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "claims_supported": CLAIMS,
    })
}

/// The `audience` of a token request's query, `None` when it has none or
/// it is empty.
fn audience_of(req: &Request) -> Option<String> {
    let url = req.url().ok()?;
    url.query_pairs()
        .find(|(name, _)| name == "audience")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// What a request for a token gets from the job's row: the claims may be
/// made only for the running job of the token's attempt, which has
/// `id-token: write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Make the token.
    Go,
    /// The request token is for another job, run or attempt, or the job
    /// is not running: `401`.
    Refused,
    /// The job was not granted `id-token: write`: `403`.
    Forbidden,
}

/// Decides what the request token's `claims` get from `row`.
#[must_use]
pub fn answer(row: &JobRow, claims: &token::Claims) -> Answer {
    if !running_attempt(row, claims) {
        Answer::Refused
    } else if !granted(&row.token_permissions) {
        Answer::Forbidden
    } else {
        Answer::Go
    }
}

fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": message }))?.with_status(status))
}

/// The issuer's discovery document and JWKS, for a request to [`HOST`].
///
/// # Errors
///
/// The Worker runtime failed to build a response.
pub fn issuer(req: &Request, env: &Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return error("method not allowed", 405);
    }
    let body = match req.path().as_str() {
        DISCOVERY_PATH => discovery(),
        JWKS_PATH => {
            let (keys, problems) = cloud_oidc::public_keys(&SLOTS, |name| {
                env.secret(name)
                    .map(|value| value.to_string())
                    .ok()
                    .filter(|value| !value.is_empty())
            });
            for problem in problems {
                console_error!("{problem}");
            }
            if keys.is_empty() {
                return error("no signing key", 500);
            }
            cloud_oidc::jwks(&keys)
        }
        _ => return error("not found", 404),
    };
    let mut response = Response::from_json(&body)?;
    response
        .headers_mut()
        .set("Cache-Control", cloud_oidc::CACHE_CONTROL)?;
    Ok(response)
}

/// Answers `GET /v1/oidc/token`: the job's identity token for the
/// requested audience.
///
/// # Errors
///
/// The Worker runtime failed to build a response.
pub async fn handle(req: Request, env: &Env) -> Result<Response> {
    if req.method() != Method::Get || req.path() != TOKEN_PATH {
        return error("not found", 404);
    }
    let settings = match Settings::read(env) {
        Ok(settings) => settings,
        Err(err) => {
            console_error!("oidc: {err}");
            return error("not configured", 500);
        }
    };
    let header = req.headers().get("Authorization").ok().flatten();
    let now = worker::Date::now().as_millis() / 1000;
    let Ok(request) = header
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(TokenError::Malformed)
        .and_then(|bearer| token::verify_oidc(&settings.token_key, bearer, now))
    else {
        return error("unauthorized", 401);
    };
    let requested = audience_of(&req);
    if requested
        .as_deref()
        .is_some_and(|audience| audience.len() > MAX_AUDIENCE_BYTES)
    {
        return error("audience too long", 400);
    }
    let db = match hatchrun_cloud::db::Db::connect(env).await {
        Ok(db) => db,
        Err(err) => {
            console_error!("oidc: database: {err}");
            return error("unavailable", 503);
        }
    };
    // A query that fails is retried by the toolkit on a 503 and not on
    // a 500.
    let job = match store::job(&db, request.job).await {
        Ok(Some(job)) => job,
        Ok(None) => return error("unauthorized", 401),
        Err(err) => {
            console_error!("oidc: job: {err}");
            return error("unavailable", 503);
        }
    };
    match answer(&job, &request) {
        Answer::Go => {}
        Answer::Refused => return error("unauthorized", 401),
        Answer::Forbidden => return error("id-token: write is not granted", 403),
    }
    let run = match store::run(&db, job.run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return error("unauthorized", 401),
        Err(err) => {
            console_error!("oidc: run: {err}");
            return error("unavailable", 503);
        }
    };
    let repo = match store::repo(&db, run.repo_id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return error("unauthorized", 401),
        Err(err) => {
            console_error!("oidc: repo: {err}");
            return error("unavailable", 503);
        }
    };
    if !crate::agent::consistent(&job, &run, &repo) {
        return error("unauthorized", 401);
    }
    let audience = requested.unwrap_or_else(|| default_audience(&repo));
    let jti = match webcrypto::random_bytes(16) {
        Ok(bytes) => jwt::b64url(bytes),
        Err(err) => {
            console_error!("oidc: random: {err}");
            return error("unavailable", 503);
        }
    };
    let input = jwt::signing_input(
        Some(&settings.ci_oidc_kid),
        &claims(&job, &run, &repo, &audience, now, &jti),
    );
    let signature = match webcrypto::sign_rs256(&settings.ci_oidc_key, input.as_bytes()).await {
        Ok(signature) => signature,
        Err(err) => {
            console_error!("oidc: sign: {err}");
            return error("signing failed", 500);
        }
    };
    let mut response = Response::from_json(&json!({ "value": jwt::token(&input, &signature) }))?;
    response.headers_mut().set("Cache-Control", "no-store")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)] // callers build the value inline
    fn job(environment: Option<&str>, permissions: Value) -> JobRow {
        serde_json::from_value(json!({
            "id": 7, "run_id": 1, "plan_id": "build", "name": "build",
            "status": "running", "conclusion": null, "instance_id": "i-1",
            "attempt": 2, "launch_attempts": 1, "next_launch_at": null,
            "timeout_minutes": 10, "updated_at": 0, "heartbeat_at": null,
            "started_at": null, "completed_at": null, "steps": "[]",
            "log_key": null, "template": "ubuntu-latest",
            "token_permissions": permissions.to_string(),
            "secret_refs": "{\"all\":false,\"names\":[]}", "held": null,
            "environment": environment, "approval_attempt": null,
            "approval_decision": null, "approval_by": null, "approval_at": null,
        }))
        .expect("a job row")
    }

    fn run(event: &str, git_ref: &str) -> RunRow {
        serde_json::from_value(json!({
            "id": 1, "run_number": 4, "run_attempt": 3, "tenant_id": "acme",
            "repo_id": 99, "workflow": "ci", "delivery": "d", "event": event,
            "git_ref": git_ref, "sha": "abc123", "actor": "octo",
            "status": "in_progress", "conclusion": null, "created_at": 0,
            "completed_at": null, "retention_days": 30,
        }))
        .expect("a run row")
    }

    fn repo() -> RepoRow {
        RepoRow {
            id: 99,
            tenant_id: "acme".into(),
            installation_id: 5,
            full_name: "acme/widgets".into(),
            default_branch: "main".into(),
            private: false,
        }
    }

    fn write() -> Value {
        json!({ "id_token": "write", "metadata": "read" })
    }

    #[test]
    fn the_subject_prefers_the_environment_then_the_pull_request() {
        let sub = |event, git_ref, env| subject("acme/widgets", event, git_ref, env);
        assert_eq!(
            sub("push", "refs/heads/main", Some("prod")),
            "repo:acme/widgets:environment:prod"
        );
        assert_eq!(
            sub("pull_request", "refs/pull/3/merge", Some("prod")),
            "repo:acme/widgets:environment:prod"
        );
        assert_eq!(
            sub("pull_request", "refs/pull/3/merge", None),
            "repo:acme/widgets:pull_request"
        );
        assert_eq!(
            sub("push", "refs/heads/main", None),
            "repo:acme/widgets:ref:refs/heads/main"
        );
        assert_eq!(
            sub("push", "refs/tags/v1", None),
            "repo:acme/widgets:ref:refs/tags/v1"
        );
    }

    #[test]
    fn a_token_carries_the_jobs_identity() {
        let got = claims(
            &job(Some("prod"), write()),
            &run("push", "refs/heads/main"),
            &repo(),
            "sts.amazonaws.com",
            1_000_000,
            "j1",
        );
        assert_eq!(
            got,
            json!({
                "iss": "https://token.ci.hatchrun.io",
                "sub": "repo:acme/widgets:environment:prod",
                "aud": "sts.amazonaws.com",
                "iat": 1_000_000,
                "nbf": 1_000_000,
                "exp": 1_000_300,
                "jti": "j1",
                "repository": "acme/widgets",
                "repository_id": "99",
                "repository_owner": "acme",
                "ref": "refs/heads/main",
                "ref_type": "branch",
                "sha": "abc123",
                "workflow": "ci",
                "job_workflow_ref": "acme/widgets/.hatchrun/config.yml@refs/heads/main",
                "run_id": "1",
                "run_attempt": "3",
                "actor": "octo",
                "event_name": "push",
                "environment": "prod",
            })
        );
    }

    #[test]
    fn every_claim_is_announced_and_none_is_left_unannounced() {
        let got = claims(
            &job(Some("prod"), write()),
            &run("push", "refs/heads/main"),
            &repo(),
            "a",
            1,
            "j",
        );
        let mut named: Vec<_> = got
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut announced = CLAIMS.to_vec();
        named.sort_unstable();
        announced.sort_unstable();
        assert_eq!(named, announced);
    }

    #[test]
    fn without_an_environment_there_is_no_environment_claim() {
        let got = claims(
            &job(None, write()),
            &run("push", "refs/tags/v1"),
            &repo(),
            "a",
            1,
            "j",
        );
        assert!(got.get("environment").is_none());
        assert_eq!(got["ref_type"], "tag");
        assert_eq!(got["sub"], "repo:acme/widgets:ref:refs/tags/v1");
    }

    #[test]
    fn the_default_audience_is_the_owners_url() {
        assert_eq!(default_audience(&repo()), "https://github.com/acme");
    }

    #[test]
    fn discovery_points_at_the_jwks() {
        let doc = discovery();
        assert_eq!(doc["issuer"], "https://token.ci.hatchrun.io");
        assert_eq!(
            doc["jwks_uri"],
            "https://token.ci.hatchrun.io/.well-known/jwks"
        );
        assert_eq!(
            doc["id_token_signing_alg_values_supported"],
            json!(["RS256"])
        );
    }

    #[test]
    fn the_issuer_and_its_keys_are_not_the_control_planes() {
        assert_ne!(ISSUER, cloud_oidc::ISSUER);
        assert_ne!(HOST, cloud_oidc::HOST);
        for ((secret, kid), (cloud_secret, cloud_kid)) in SLOTS.iter().zip(cloud_oidc::SLOTS) {
            assert_ne!(*secret, cloud_secret);
            assert_ne!(*kid, cloud_kid);
        }
    }

    #[test]
    fn the_request_url_has_a_query_for_the_toolkit() {
        assert_eq!(
            request_url("https://ci.hatchrun.io"),
            "https://ci.hatchrun.io/v1/oidc/token?api-version=2.0"
        );
    }

    #[test]
    fn only_write_grants_tokens() {
        assert!(granted(&write()));
        assert!(!granted(&json!({ "metadata": "read" })));
        assert!(!granted(&json!({ "id_token": "read" })));
    }

    #[test]
    fn a_request_is_answered_for_the_running_attempt_with_the_grant() {
        let claims = token::Claims {
            job: 7,
            run: 1,
            attempt: 2,
            exp: 10,
        };
        assert_eq!(answer(&job(None, write()), &claims), Answer::Go);
        let mut done = job(None, write());
        done.status = "completed".into();
        assert_eq!(answer(&done, &claims), Answer::Refused);
        let rerun = token::Claims {
            attempt: 1,
            ..claims
        };
        assert_eq!(answer(&job(None, write()), &rerun), Answer::Refused);
        let other = token::Claims { job: 8, ..claims };
        assert_eq!(answer(&job(None, write()), &other), Answer::Refused);
        assert_eq!(
            answer(&job(None, json!({ "metadata": "read" })), &claims),
            Answer::Forbidden
        );
    }
}
