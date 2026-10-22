// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The launcher daemon behind `hatchrun-ci`'s `Local` backend: each
//! "instance" is a `hatchrun-agent ci` process in a process group of
//! its own.
//!
//! - `POST /instances` `{"name", "job_id", "user_data"}` writes the user
//!   data to `<work-root>/<id>/user-data.json`, starts the agent and
//!   answers `{"instance_id": id}`.
//! - `POST /instances/{id}/metadata-off` turns the metadata endpoint
//!   off. Like EC2, the change applies a moment later: the first read
//!   after it answers `pending`, the next ones `applied`.
//! - `GET /instances/{id}` answers `{"state", "metadata"}`.
//! - `DELETE /instances/{id}` sends the process group `SIGTERM`, as a
//!   shutdown does, and `SIGKILL` 10 s later.
//! - `GET /instances` lists the instances still running.
//! - `GET /_events` lists, in order, `launch <id>`, `metadata-off <id>`,
//!   `metadata-read <id>` (a read that answered `applied`),
//!   `terminate <id>` and `exit <id> <code>`. A process killed by signal
//!   `n` exits with `128 + n`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::rfc3339;

/// How long a terminated process gets before `SIGKILL`.
const KILL_AFTER: Duration = Duration::from_secs(10);

/// What `POST /instances` takes.
#[derive(Debug, Deserialize)]
pub struct Launch {
    /// The runner name.
    pub name: String,
    /// The job.
    pub job_id: u64,
    /// The user data, as text.
    pub user_data: String,
}

/// The metadata endpoint's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metadata {
    Enabled,
    Pending,
    Applied,
}

impl Metadata {
    fn as_str(self) -> &'static str {
        match self {
            Metadata::Enabled => "enabled",
            Metadata::Pending => "pending",
            Metadata::Applied => "applied",
        }
    }
}

struct Instance {
    name: String,
    job_id: u64,
    launched: u64,
    pid: Option<u32>,
    exited: bool,
    metadata: Metadata,
}

#[derive(Default)]
struct Inner {
    next: u64,
    instances: BTreeMap<String, Instance>,
    events: Vec<String>,
}

/// The daemon's state.
#[derive(Clone)]
pub struct Launcher {
    agent: PathBuf,
    work_root: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl Launcher {
    /// A launcher that starts `agent` under `work_root`.
    #[must_use]
    pub fn new(agent: PathBuf, work_root: PathBuf) -> Self {
        Self {
            agent,
            work_root,
            inner: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The daemon's routes.
pub fn router(launcher: Launcher) -> Router {
    Router::new()
        .route("/instances", post(launch).get(list))
        .route("/instances/{id}", get(read).delete(terminate))
        .route("/instances/{id}/metadata-off", post(metadata_off))
        .route("/_events", get(events))
        .with_state(launcher)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

async fn launch(State(launcher): State<Launcher>, Json(launch): Json<Launch>) -> Response {
    let id = {
        let mut inner = launcher.lock();
        inner.next += 1;
        format!("i-local{:010}", inner.next)
    };
    let dir = launcher.work_root.join(&id);
    let user_data = dir.join("user-data.json");
    let prepared = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(&user_data, &launch.user_data).await?;
        std::fs::File::create(dir.join("agent.log"))
    };
    let log = match prepared.await {
        Ok(log) => log,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let stderr = match log.try_clone() {
        Ok(stderr) => stderr,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let mut command = tokio::process::Command::new(&launcher.agent);
    command
        .arg("ci")
        .arg("--user-data")
        .arg(&user_data)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .arg("--runner-user")
        .arg("")
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg("--env-file")
        .arg("/dev/null")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .process_group(0);
    // The events must show the launch before anything the agent causes.
    let mut inner = launcher.lock();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("start {}: {err}", launcher.agent.display()),
            );
        }
    };
    inner.events.push(format!("launch {id}"));
    inner.instances.insert(
        id.clone(),
        Instance {
            name: launch.name,
            job_id: launch.job_id,
            launched: now_secs(),
            pid: child.id(),
            exited: false,
            metadata: Metadata::Enabled,
        },
    );
    drop(inner);
    let waiter = launcher.clone();
    let exited_id = id.clone();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => exit_code(status),
            Err(_) => -1,
        };
        let mut inner = waiter.lock();
        if let Some(instance) = inner.instances.get_mut(&exited_id) {
            instance.exited = true;
        }
        inner.events.push(format!("exit {exited_id} {code}"));
    });
    Json(json!({ "instance_id": id })).into_response()
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(-1)
}

async fn metadata_off(State(launcher): State<Launcher>, Path(id): Path<String>) -> Response {
    let mut inner = launcher.lock();
    let Some(instance) = inner.instances.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, "no such instance");
    };
    if instance.metadata == Metadata::Enabled {
        instance.metadata = Metadata::Pending;
    }
    inner.events.push(format!("metadata-off {id}"));
    StatusCode::NO_CONTENT.into_response()
}

async fn read(State(launcher): State<Launcher>, Path(id): Path<String>) -> Response {
    let mut inner = launcher.lock();
    let Some(instance) = inner.instances.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, "no such instance");
    };
    let metadata = instance.metadata;
    // A pending change applies by the next read.
    if metadata == Metadata::Pending {
        instance.metadata = Metadata::Applied;
    }
    let state = if instance.exited {
        "terminated"
    } else {
        "running"
    };
    if metadata == Metadata::Applied {
        inner.events.push(format!("metadata-read {id}"));
    }
    Json(json!({ "state": state, "metadata": metadata.as_str() })).into_response()
}

async fn terminate(State(launcher): State<Launcher>, Path(id): Path<String>) -> Response {
    let pid = {
        let mut inner = launcher.lock();
        let Some(instance) = inner.instances.get(&id) else {
            return error(StatusCode::NOT_FOUND, "no such instance");
        };
        let pid = (!instance.exited).then_some(instance.pid).flatten();
        inner.events.push(format!("terminate {id}"));
        pid
    };
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let group = Pid::from_raw(pid);
        let _ = killpg(group, Signal::SIGTERM);
        let waiter = launcher.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KILL_AFTER).await;
            let alive = waiter
                .lock()
                .instances
                .get(&id)
                .is_some_and(|instance| !instance.exited);
            if alive {
                let _ = killpg(group, Signal::SIGKILL);
            }
        });
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn list(State(launcher): State<Launcher>) -> Json<Value> {
    let inner = launcher.lock();
    let listed: Vec<Value> = inner
        .instances
        .iter()
        .filter(|(_, instance)| !instance.exited)
        .map(|(id, instance)| {
            json!({
                "id": id,
                "state": "running",
                "launch_time": rfc3339(instance.launched),
                "job_id": instance.job_id.to_string(),
                "runner_name": instance.name,
            })
        })
        .collect();
    Json(Value::Array(listed))
}

async fn events(State(launcher): State<Launcher>) -> Json<Value> {
    Json(json!(launcher.lock().events))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn launcher_records_metadata_off() {
        let root = tempfile::tempdir().unwrap();
        let app = router(Launcher::new(
            PathBuf::from("/usr/bin/true"),
            root.path().to_owned(),
        ));
        let (status, started) = call(
            &app,
            "POST",
            "/instances",
            Some(json!({ "name": "ci-1-1-1", "job_id": 1, "user_data": "{\"mode\":\"ci\"}" })),
        )
        .await;
        assert_eq!(status, 200);
        let id = started["instance_id"].as_str().unwrap().to_owned();
        assert_eq!(
            std::fs::read_to_string(root.path().join(&id).join("user-data.json")).unwrap(),
            "{\"mode\":\"ci\"}"
        );

        let (_, before) = call(&app, "GET", &format!("/instances/{id}"), None).await;
        assert_eq!(before["metadata"], "enabled");
        let (status, _) = call(&app, "POST", &format!("/instances/{id}/metadata-off"), None).await;
        assert_eq!(status, 204);
        let (_, pending) = call(&app, "GET", &format!("/instances/{id}"), None).await;
        assert_eq!(pending["metadata"], "pending");
        let (_, applied) = call(&app, "GET", &format!("/instances/{id}"), None).await;
        assert_eq!(applied["metadata"], "applied");

        let exit = format!("exit {id} 0");
        let mut events = Vec::new();
        for _ in 0..100 {
            let (_, listed) = call(&app, "GET", "/_events", None).await;
            events = serde_json::from_value::<Vec<String>>(listed).unwrap();
            if events.contains(&exit) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let position = |event: &str| events.iter().position(|e| e == event);
        let launch = position(&format!("launch {id}")).unwrap();
        let off = position(&format!("metadata-off {id}")).unwrap();
        let read = position(&format!("metadata-read {id}")).unwrap();
        assert!(launch < off && off < read, "{events:?}");
        assert!(position(&exit).is_some(), "{events:?}");

        let (_, listed) = call(&app, "GET", "/instances", None).await;
        assert_eq!(listed, json!([]));
    }
}
