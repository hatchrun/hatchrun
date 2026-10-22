// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A fake of the GitHub REST API calls `hatchrun-ci` makes.
//!
//! Every repository has the config in `--repo-dir`, read on each request
//! so a script may change it between pushes. Check runs are kept in
//! memory, with the latest name, status and conclusion of each and the
//! count of annotations sent to it over all its updates. `GET /_state`
//! lists them, with the installations and the repositories whose
//! config was read and the Polar events.
//!
//! Installations 42, 43 and 44 are on organizations `e2e`, `e2e2` and
//! `e2e3` (ids 1, 2 and 3), and the signed-in user is an admin of each.
//!
//! The fake also answers the Polar calls `hatchrun-ci` makes, so
//! `POLAR_API_URL` may point at it: `POST /v1/events/ingest` keeps the
//! events (an `external_id` seen before counts as a duplicate, as Polar
//! does). It wants the bearer token [`POLAR_TOKEN`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};

/// A sha the fake answers for any ref: 40 times `e`.
pub const SHA: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// The Polar access token the fake takes.
pub const POLAR_TOKEN: &str = "e2e-polar-token";

/// The organizations the App is installed on: installation id,
/// organization id and login.
const ACCOUNTS: [(u64, u64, &str); 3] = [(42, 1, "e2e"), (43, 2, "e2e2"), (44, 3, "e2e3")];

/// A check run as its last create or update left it.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRun {
    /// The id the fake gave it.
    pub id: u64,
    /// The commit it is on.
    pub head_sha: String,
    /// The job's name.
    pub name: String,
    /// What names the job when GitHub asks for a re-run.
    pub external_id: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// Set once completed.
    pub conclusion: Option<String>,
    /// The last output's summary.
    pub summary: Option<String>,
    /// The last output's text, where a failed job's log tail goes.
    pub text: Option<String>,
    /// Annotations sent over all updates.
    pub annotations: usize,
    /// Creates and updates it got.
    pub writes: usize,
}

#[derive(Default)]
struct Inner {
    tokens: u64,
    /// The `permissions` of every access token request, in order.
    token_permissions: Vec<Value>,
    check_runs: BTreeMap<u64, CheckRun>,
    /// The id of every installation read, in order.
    installation_reads: Vec<u64>,
    /// `owner/repo` of every config read, in order.
    contents: Vec<String>,
    /// The Polar events taken, in order.
    polar_events: Vec<Value>,
}

/// The fake's state.
#[derive(Clone)]
pub struct Fake {
    repo_dir: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl Fake {
    /// A fake that serves the config in `repo_dir`.
    #[must_use]
    pub fn new(repo_dir: PathBuf) -> Self {
        Self {
            repo_dir,
            inner: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The fake's routes.
pub fn router(fake: Fake) -> Router {
    Router::new()
        .route("/app/installations/{id}", get(installation))
        .route("/user/memberships/orgs/{org}", get(membership))
        .route("/app/installations/{id}/access_tokens", post(access_token))
        .route("/installation/token", axum::routing::delete(revoke))
        .route("/repos/{owner}/{repo}", get(repository))
        .route(
            "/repos/{owner}/{repo}/contents/.hatchrun/config.yml",
            get(config),
        )
        .route("/repos/{owner}/{repo}/pulls/{number}/files", get(no_files))
        .route("/repos/{owner}/{repo}/commits/{*rest}", get(commit))
        .route("/repos/{owner}/{repo}/check-runs", post(create_check_run))
        .route(
            "/repos/{owner}/{repo}/check-runs/{id}",
            patch(update_check_run),
        )
        .route("/v1/events/ingest", post(polar_ingest))
        .route("/_state", get(state))
        .with_state(fake)
}

/// Unix seconds as RFC 3339 UTC.
#[must_use]
pub fn rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The installation, as Plan 3's fields say, holding the permissions
/// the hatchrun App has.
async fn installation(State(fake): State<Fake>, Path(id): Path<u64>) -> Response {
    fake.lock().installation_reads.push(id);
    let Some(&(_, org_id, login)) = ACCOUNTS
        .iter()
        .find(|(installation, ..)| *installation == id)
    else {
        return not_found();
    };
    Json(json!({
        "id": id,
        "account": { "id": org_id, "login": login, "type": "Organization" },
        "suspended_at": null,
        "permissions": {
            "contents": "write",
            "metadata": "read",
            "checks": "write",
            "pull_requests": "write",
        },
    }))
    .into_response()
}

/// The signed-in user's membership: an active admin of every
/// organization in [`ACCOUNTS`].
async fn membership(Path(org): Path<String>) -> Response {
    let Some(&(_, org_id, login)) = ACCOUNTS.iter().find(|(.., login)| *login == org) else {
        return not_found();
    };
    Json(json!({
        "role": "admin",
        "state": "active",
        "organization": {
            "id": org_id,
            "login": login,
            "avatar_url": "https://example.invalid/o.png",
        },
    }))
    .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Not Found" })),
    )
        .into_response()
}

async fn access_token(
    State(fake): State<Fake>,
    Path(_id): Path<u64>,
    body: axum::body::Bytes,
) -> Response {
    // The webhook's token request has no body; a job's has permissions.
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let n = {
        let mut inner = fake.lock();
        inner.tokens += 1;
        inner
            .token_permissions
            .push(body.get("permissions").cloned().unwrap_or(Value::Null));
        inner.tokens
    };
    (
        StatusCode::CREATED,
        Json(json!({
            "token": format!("ghs_e2e_{n}"),
            "expires_at": rfc3339(now_secs() + 3600),
        })),
    )
        .into_response()
}

async fn revoke() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn repository() -> Json<Value> {
    Json(
        json!({ "id": 1001, "private": false, "permissions": { "pull": true, "push": true, "admin": true } }),
    )
}

async fn config(State(fake): State<Fake>, Path((owner, repo)): Path<(String, String)>) -> Response {
    fake.lock().contents.push(format!("{owner}/{repo}"));
    match tokio::fs::read(fake.repo_dir.join(".hatchrun/config.yml")).await {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/vnd.github.raw",
            )],
            body,
        )
            .into_response(),
        Err(_) => not_found(),
    }
}

async fn no_files() -> Json<Value> {
    Json(json!([]))
}

/// `commits/{ref}` answers [`SHA`]; `commits/{sha}/check-runs` counts
/// the check runs of that name on the commit.
async fn commit(
    State(fake): State<Fake>,
    Path((_owner, _repo, rest)): Path<(String, String, String)>,
    query: axum::extract::RawQuery,
) -> Json<Value> {
    if let Some(sha) = rest.strip_suffix("/check-runs") {
        let name = query
            .0
            .unwrap_or_default()
            .split('&')
            .find_map(|pair| pair.strip_prefix("check_name=").map(decode))
            .unwrap_or_default();
        let total = fake
            .lock()
            .check_runs
            .values()
            .filter(|run| run.head_sha == sha && run.name == name)
            .count();
        return Json(json!({ "total_count": total, "check_runs": [] }));
    }
    Json(json!({ "sha": SHA }))
}

/// Percent-decodes a query value, `+` included.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn annotations(body: &Value) -> usize {
    body.pointer("/output/annotations")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn apply(run: &mut CheckRun, body: &Value) {
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        name.clone_into(&mut run.name);
    }
    if let Some(external_id) = body.get("external_id").and_then(Value::as_str) {
        external_id.clone_into(&mut run.external_id);
    }
    if let Some(status) = body.get("status").and_then(Value::as_str) {
        status.clone_into(&mut run.status);
    }
    if let Some(conclusion) = body.get("conclusion").and_then(Value::as_str) {
        run.conclusion = Some(conclusion.to_owned());
    }
    if let Some(summary) = body.pointer("/output/summary").and_then(Value::as_str) {
        run.summary = Some(summary.to_owned());
    }
    if let Some(text) = body.pointer("/output/text").and_then(Value::as_str) {
        run.text = Some(text.to_owned());
    }
    run.annotations += annotations(body);
    run.writes += 1;
}

async fn create_check_run(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let mut inner = fake.lock();
    let id = u64::try_from(inner.check_runs.len()).unwrap_or(0) + 1;
    let mut run = CheckRun {
        id,
        head_sha: body
            .get("head_sha")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: String::new(),
        external_id: String::new(),
        status: "queued".to_owned(),
        conclusion: None,
        summary: None,
        text: None,
        annotations: 0,
        writes: 0,
    };
    apply(&mut run, &body);
    inner.check_runs.insert(id, run);
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

async fn update_check_run(
    State(fake): State<Fake>,
    Path((_owner, _repo, id)): Path<(String, String, u64)>,
    Json(body): Json<Value>,
) -> Response {
    let mut inner = fake.lock();
    let Some(run) = inner.check_runs.get_mut(&id) else {
        return not_found();
    };
    apply(run, &body);
    Json(json!({ "id": id })).into_response()
}

/// Whether the request carries Polar's token.
fn polar_authorized(headers: &HeaderMap) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        == Some(POLAR_TOKEN)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "detail": "Unauthorized" })),
    )
        .into_response()
}

/// Polar's event ingest: keeps each new event and counts one whose
/// `external_id` it has as a duplicate.
async fn polar_ingest(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !polar_authorized(&headers) {
        return unauthorized();
    }
    let events = body
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut inner = fake.lock();
    let (mut inserted, mut duplicates) = (0_u32, 0_u32);
    for event in events {
        let id = event.get("external_id");
        let seen = id.is_some()
            && inner
                .polar_events
                .iter()
                .any(|kept| kept.get("external_id") == id);
        if seen {
            duplicates += 1;
        } else {
            inserted += 1;
            inner.polar_events.push(event);
        }
    }
    Json(json!({ "inserted": inserted, "duplicates": duplicates })).into_response()
}

async fn state(State(fake): State<Fake>) -> Json<Value> {
    let inner = fake.lock();
    let runs: Vec<&CheckRun> = inner.check_runs.values().collect();
    Json(json!({
        "check_runs": runs,
        "token_permissions": inner.token_permissions,
        "installation_reads": inner.installation_reads,
        "contents": inner.contents,
        "polar_events": inner.polar_events,
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn fake_github_serves_the_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".hatchrun")).unwrap();
        std::fs::write(dir.path().join(".hatchrun/config.yml"), "jobs: {}\n").unwrap();
        let app = router(Fake::new(dir.path().to_owned()));

        let (status, body) = call(
            &app,
            "GET",
            "/repos/e2e/demo/contents/.hatchrun/config.yml?ref=abc",
            None,
        )
        .await;
        assert_eq!((status, body.as_slice()), (200, b"jobs: {}\n".as_slice()));

        let (status, body) = call(&app, "GET", "/repos/e2e/demo/commits/heads/main", None).await;
        assert_eq!(status, 200);
        let commit: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(commit["sha"], SHA);

        let (status, body) = call(
            &app,
            "POST",
            "/app/installations/42/access_tokens",
            Some(json!({ "permissions": { "contents": "read" } })),
        )
        .await;
        assert_eq!(status, 201);
        let token: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(token["token"], "ghs_e2e_1");

        let (status, body) = call(&app, "GET", "/app/installations/42", None).await;
        assert_eq!(status, 200);
        let installation: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(installation["permissions"]["contents"], "write");

        let create = json!({
            "name": "ci / build",
            "head_sha": "abc",
            "external_id": "hatchrun-ci:7",
            "status": "queued",
            "output": { "title": "t", "summary": "s", "annotations": [] },
        });
        let (status, _) = call(&app, "POST", "/repos/e2e/demo/check-runs", Some(create)).await;
        assert_eq!(status, 201);
        let update = json!({
            "name": "ci / build",
            "status": "completed",
            "conclusion": "success",
            "output": { "title": "t", "summary": "s", "annotations": [{ "path": "README.md" }] },
        });
        let (status, _) = call(&app, "PATCH", "/repos/e2e/demo/check-runs/1", Some(update)).await;
        assert_eq!(status, 200);

        let (_, body) = call(
            &app,
            "GET",
            "/repos/e2e/demo/commits/abc/check-runs?check_name=ci+%2F+build&per_page=1",
            None,
        )
        .await;
        let listed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed["total_count"], 1);

        let (_, body) = call(&app, "GET", "/_state", None).await;
        let state: Value = serde_json::from_slice(&body).unwrap();
        let run = &state["check_runs"][0];
        assert_eq!(run["name"], "ci / build");
        assert_eq!(run["status"], "completed");
        assert_eq!(run["conclusion"], "success");
        assert_eq!(run["annotations"], 1);
        assert_eq!(run["external_id"], "hatchrun-ci:7");
        assert_eq!(run["summary"], "s");
        assert_eq!(state["contents"], json!(["e2e/demo"]));
        assert_eq!(state["installation_reads"], json!([42]));
        assert_eq!(state["token_permissions"], json!([{ "contents": "read" }]));
    }

    async fn polar(app: &Router, uri: &str, token: &str, body: Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn installations_name_their_organizations() {
        let app = router(Fake::new(PathBuf::new()));
        let (_, body) = call(&app, "GET", "/app/installations/43", None).await;
        let installation: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(installation["account"]["login"], "e2e2");
        assert_eq!(installation["account"]["id"], 2);
        let (status, _) = call(&app, "GET", "/app/installations/45", None).await;
        assert_eq!(status, 404);

        let (_, body) = call(&app, "GET", "/user/memberships/orgs/e2e3", None).await;
        let membership: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(membership["role"], "admin");
        assert_eq!(membership["organization"]["id"], 3);
        let (status, _) = call(&app, "GET", "/user/memberships/orgs/other", None).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn fake_polar_keeps_events_once_and_wants_the_token() {
        let app = router(Fake::new(PathBuf::new()));
        let event = |id: &str| json!({ "name": "ci.usage", "external_id": id });
        let body = json!({ "events": [event("ci:1:1"), event("ci:2:1")] });
        let (status, _) = polar(&app, "/v1/events/ingest", "wrong", body.clone()).await;
        assert_eq!(status, 401);
        let (status, answer) = polar(&app, "/v1/events/ingest", POLAR_TOKEN, body).await;
        assert_eq!(status, 200);
        assert_eq!(answer, json!({ "inserted": 2, "duplicates": 0 }));
        let again = json!({ "events": [event("ci:2:1"), event("ci:3:1")] });
        let (_, answer) = polar(&app, "/v1/events/ingest", POLAR_TOKEN, again).await;
        assert_eq!(answer, json!({ "inserted": 1, "duplicates": 1 }));

        let (_, body) = call(&app, "GET", "/_state", None).await;
        let state: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<_> = state["polar_events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["external_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["ci:1:1", "ci:2:1", "ci:3:1"]);
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_627_200), "2026-10-22T00:00:00Z");
    }
}
