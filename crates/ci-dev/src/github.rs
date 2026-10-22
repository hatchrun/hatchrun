// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A fake of the GitHub REST API calls `hatchrun-ci` makes.
//!
//! Every repository has the config in `--repo-dir`, read on each request
//! so a script may change it between pushes. Check runs are kept in
//! memory, with the latest name, status and conclusion of each and the
//! count of annotations sent to it over all its updates. `GET /_state`
//! lists them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};

/// A sha the fake answers for any ref: 40 times `e`.
pub const SHA: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

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
async fn installation(Path(id): Path<u64>) -> Json<Value> {
    Json(json!({
        "id": id,
        "account": { "id": 1, "login": "e2e", "type": "Organization" },
        "suspended_at": null,
        "permissions": {
            "contents": "write",
            "metadata": "read",
            "checks": "write",
            "pull_requests": "write",
        },
    }))
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

async fn config(State(fake): State<Fake>) -> Response {
    match tokio::fs::read(fake.repo_dir.join(".hatchrun/config.yml")).await {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/vnd.github.raw",
            )],
            body,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        )
            .into_response(),
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
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        )
            .into_response();
    };
    apply(run, &body);
    Json(json!({ "id": id })).into_response()
}

async fn state(State(fake): State<Fake>) -> Json<Value> {
    let inner = fake.lock();
    let runs: Vec<&CheckRun> = inner.check_runs.values().collect();
    Json(json!({ "check_runs": runs, "token_permissions": inner.token_permissions }))
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
        assert_eq!(state["token_permissions"], json!([{ "contents": "read" }]));
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_627_200), "2026-10-22T00:00:00Z");
    }
}
