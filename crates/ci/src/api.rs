// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/ci/*`: runs, jobs and logs for the app, and dispatch, cancel
//! from it.
//!
//! `hatchrun-cloud` forwards these requests unchanged, cookies and
//! WebSocket upgrades included. This Worker finds the user itself: the
//! session cookie names a row of the shared Postgres, so nothing the
//! caller sends can claim an identity. The cookie is host-only, so a
//! browser sends it to `app.hatchrun.io` and never here, and a request
//! straight to this Worker is a signed-out one.
//!
//! | Route | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /api/ci/repos/{owner}/{repo}/runs?before=ID` | read | 50 runs, newest first |
//! | `GET /api/ci/runs/{run}` | read | the run and its jobs |
//! | `GET /api/ci/jobs/{job}/log?offset=N` | read | the log text |
//! | `GET /api/ci/jobs/{job}/log/index` | read | the step index, `404` while running |
//! | `GET /api/ci/jobs/{job}/live` | read | WebSocket |
//! | `POST /api/ci/repos/{owner}/{repo}/dispatch` | write, CSRF | `202` `{"runs": [..]}` |
//! | `POST /api/ci/runs/{run}/cancel` | write, CSRF | `204` |
//!
//! The organization and repository settings routes are in
//! [`crate::settings_api`]; they go through the same guard. Every
//! method but `GET` needs the app's `Origin`, the session's CSRF token
//! and a body within [`body_cap`].
//!
//! A repository the caller may not read answers `404`, as one that does
//! not exist. Every answer carries `Cache-Control: no-store`.

use std::collections::BTreeMap;

use hatchrun_cloud::Session;
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::session::hex;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_cloud::webcrypto;
use hatchrun_workflow::{Config, Event, Parameter, ParameterKind, parse};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use worker::{
    EncodeBody, Env, Headers, Method, Request, RequestInit, Response, ResponseBuilder, Result,
    console_error, console_warn,
};

use crate::access::{self, Access};
use crate::env::{Settings, run_stub};
use crate::github;
use crate::intake::Repo;
use crate::run::RunState;
use crate::runs::{self, Config as Loaded};
use crate::settings_api::{self, SettingsRoute};
use crate::store::{self, JobRow, RepoRow, RunRow};

/// The logs bucket.
const BUCKET: &str = "LOGS";

/// Runs in one page of a repository's list.
const PAGE: u32 = 50;

/// The most bytes of a body, dispatch and settings alike.
const MAX_BODY: usize = 64 << 10;

/// The most bytes of a secret or variable body: a 48 KB value is more
/// after JSON escaping.
const MAX_VALUE_BODY: usize = 128 << 10;

/// The longest ref a dispatch names.
const MAX_REF: usize = 255;

/// Where an `/api/ci/` path goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    /// A repository's runs.
    Runs {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// Start a workflow by hand.
    Dispatch {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// One run.
    Run(u64),
    /// Stop a run.
    Cancel(u64),
    /// A job's log.
    Log(u64),
    /// A job's log index.
    LogIndex(u64),
    /// A running job's log as it grows.
    Live(u64),
}

impl Route<'_> {
    /// The method the route answers to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Route::Dispatch { .. } | Route::Cancel(_) => Method::Post,
            _ => Method::Get,
        }
    }
}

fn id(segment: &str) -> Option<u64> {
    if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse().ok()
}

/// GitHub's characters for owners and repositories.
pub(crate) fn name(segment: &str) -> Option<&str> {
    let ok = !segment.is_empty()
        && segment.len() <= 100
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    ok.then_some(segment)
}

/// The route of `path`, `None` when there is none.
#[must_use]
pub fn route(path: &str) -> Option<Route<'_>> {
    let rest = path.strip_prefix("/api/ci/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["repos", owner, repo, "runs"] => Some(Route::Runs {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["repos", owner, repo, "dispatch"] => Some(Route::Dispatch {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["runs", run] => id(run).map(Route::Run),
        ["runs", run, "cancel"] => id(run).map(Route::Cancel),
        ["jobs", job, "log"] => id(job).map(Route::Log),
        ["jobs", job, "log", "index"] => id(job).map(Route::LogIndex),
        ["jobs", job, "live"] => id(job).map(Route::Live),
        _ => None,
    }
}

/// Why a request ends before it does its work.
#[derive(Debug, PartialEq, Eq)]
/// The status, the message and, for a 503, the seconds to wait.
pub(crate) struct Stop(pub(crate) u16, pub(crate) String, pub(crate) Option<u64>);

pub(crate) fn stop<T>(status: u16, message: impl Into<String>) -> std::result::Result<T, Stop> {
    Err(Stop(status, message.into(), None))
}

/// What the answer to a request that may not go on is, by access: a
/// reader sees `404` for a repository it may not read, `403` for one
/// it may read but not change.
fn needs(have: Access, want: Access, signed_in: bool) -> std::result::Result<(), Stop> {
    if have >= want {
        Ok(())
    } else if have == Access::None {
        stop(404, "not found")
    } else if signed_in {
        stop(403, "you need write access to this repository")
    } else {
        stop(401, "sign in to change runs")
    }
}

/// The first `/log` and `/state` parameters the Worker passes to a
/// run's object.
fn offset_query(query: Option<&str>) -> Option<u64> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "offset").then(|| id(value)).flatten()
    })
}

fn before_query(query: Option<&str>) -> std::result::Result<Option<u64>, Stop> {
    let Some(pair) = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("before="))
    else {
        return Ok(None);
    };
    id(pair)
        .map(Some)
        .map_or_else(|| stop(400, "before is a run id"), Ok)
}

/// The names of a ref a dispatch asks for: the full ref the run shows
/// and the form GitHub's commit endpoint takes. A bare name is a branch
/// and an empty one is the default branch.
fn ref_names(input: &str, default_branch: &str) -> std::result::Result<(String, String), String> {
    let input = input.trim();
    let full = if input.is_empty() {
        format!("refs/heads/{default_branch}")
    } else if input.starts_with("refs/") {
        input.to_owned()
    } else {
        format!("refs/heads/{input}")
    };
    let valid = full.len() <= MAX_REF
        && !full.contains("..")
        && !full.ends_with('/')
        && !full.contains("//")
        && !full.chars().any(|c| c.is_control() || c.is_whitespace());
    if !valid {
        return Err(format!("`{input}` is not a branch or tag name"));
    }
    if let Some(branch) = full.strip_prefix("refs/heads/") {
        Ok((full.clone(), format!("heads/{branch}")))
    } else if let Some(tag) = full.strip_prefix("refs/tags/") {
        Ok((full.clone(), format!("tags/{tag}")))
    } else {
        Err("a ref is a branch or a tag".to_owned())
    }
}

/// The `workflow_dispatch` trigger of `workflow`.
///
/// # Errors
///
/// The message for the person who asked: the workflow is not in the
/// config, or its `on:` has no `workflow_dispatch`.
pub fn dispatchable<'a>(
    config: &'a Config,
    workflow: &str,
) -> std::result::Result<&'a hatchrun_workflow::Dispatch, String> {
    let Some(found) = config.workflows.get(workflow) else {
        return Err(format!("the config has no workflow `{workflow}`"));
    };
    found
        .on
        .workflow_dispatch
        .as_ref()
        .ok_or_else(|| format!("workflow `{workflow}` has no `workflow_dispatch` under `on:`"))
}

/// The longest string input, in bytes.
const MAX_INPUT: usize = 4096;

/// The inputs a run starts with: what the caller gave, checked against
/// the declared ones, with declared defaults for the rest.
///
/// # Errors
///
/// The message that names the input: not declared, missing with no
/// default, the wrong type, too long, or not one of the choices.
pub fn resolve_inputs(
    declared: &BTreeMap<String, Parameter>,
    given: &Map<String, Value>,
) -> std::result::Result<Map<String, Value>, String> {
    if let Some(unknown) = given.keys().find(|key| !declared.contains_key(*key)) {
        return Err(format!("input `{unknown}` is not declared by the workflow"));
    }
    let mut out = Map::new();
    for (name, parameter) in declared {
        let Some(value) = given.get(name).or(parameter.default.as_ref()) else {
            return Err(format!("input `{name}` is required"));
        };
        let ok = match (&parameter.kind, value) {
            (ParameterKind::String, Value::String(text)) => {
                if text.len() > MAX_INPUT {
                    return Err(format!("input `{name}` is longer than {MAX_INPUT} bytes"));
                }
                true
            }
            (ParameterKind::Boolean, Value::Bool(_)) => true,
            (ParameterKind::Integer, Value::Number(number)) => number.is_i64(),
            (ParameterKind::Enum(choices), Value::String(text)) => {
                if !choices.contains(text) {
                    return Err(format!(
                        "input `{name}` must be one of: {}",
                        choices.join(", ")
                    ));
                }
                true
            }
            _ => false,
        };
        if !ok {
            return Err(format!("input `{name}` has the wrong type"));
        }
        out.insert(name.clone(), value.clone());
    }
    Ok(out)
}

/// Whether a request's `Origin` is the app's. A missing one is not.
fn origin_ok(origin: Option<&str>, app_url: &str) -> bool {
    origin.is_some_and(|origin| origin == app_url.trim_end_matches('/'))
}

/// Whether a request of `method` changes something: Origin, CSRF and
/// the body cap apply to every method but `GET`.
#[must_use]
pub(crate) fn guarded(method: &Method) -> bool {
    *method != Method::Get
}

/// The most bytes of the body of a settings request.
#[must_use]
pub(crate) fn body_cap(route: &SettingsRoute<'_>) -> usize {
    match route {
        SettingsRoute::Secret(..) | SettingsRoute::Var(..) => MAX_VALUE_BODY,
        _ => MAX_BODY,
    }
}

/// The `Origin` check: changes, and the socket (`live`), come from the
/// app's own pages.
fn origin_guard(
    method: &Method,
    live: bool,
    origin: Option<&str>,
    app_url: &str,
) -> std::result::Result<(), Stop> {
    if (live || guarded(method)) && !origin_ok(origin, app_url) {
        return stop(403, "wrong origin");
    }
    Ok(())
}

/// The CSRF check of a request whose session has id `session_id`.
fn csrf_guard(session_id: Option<&[u8]>, given: Option<&str>) -> std::result::Result<(), Stop> {
    match session_id {
        None => stop(401, "sign in to change things"),
        Some(id) if hatchrun_cloud::session::check_csrf(id, given) => Ok(()),
        Some(_) => stop(403, "missing or wrong CSRF token"),
    }
}

/// The event payload of a dispatched run.
fn dispatch_event(
    git_ref: &str,
    inputs: &Map<String, Value>,
    workflow: &str,
    repo: &str,
    login: &str,
) -> Value {
    json!({
        "ref": git_ref,
        "inputs": inputs,
        "workflow": workflow,
        "repository": { "full_name": repo },
        "sender": { "login": login },
    })
}

/// Whether a log or index key belongs to this run, so a row can never
/// lead the Worker to another tenant's object.
fn owns_key(key: &str, tenant: &str, repo_id: u64, run_id: u64) -> bool {
    key.starts_with(&format!("logs/{tenant}/{repo_id}/{run_id}/")) && !key.contains("..")
}

/// The index that belongs to a log key.
fn index_of(log_key: &str) -> Option<String> {
    log_key
        .strip_suffix(".log.gz")
        .map(|stem| format!("{stem}.index.json"))
}

/// The steps the run's object holds for jobs that have not ended, by
/// job id. The rows only learn a job's steps when it ends.
fn live_steps(jobs: &[JobRow], state: Option<&RunState>) -> BTreeMap<u64, Value> {
    let Some(state) = state else {
        return BTreeMap::new();
    };
    state
        .jobs
        .iter()
        .filter(|summary| {
            jobs.iter()
                .any(|row| row.id == summary.node.id && row.status != "completed")
        })
        .filter_map(|summary| {
            serde_json::to_value(&summary.steps)
                .ok()
                .map(|steps| (summary.node.id, steps))
        })
        .collect()
}

fn run_json(run: &RunRow, repo: &RepoRow) -> Value {
    json!({
        "id": run.id,
        "run_number": run.run_number,
        "run_attempt": run.run_attempt,
        "repository": repo.full_name,
        "workflow": run.workflow,
        "event": run.event,
        "ref": run.git_ref,
        "sha": run.sha,
        "actor": run.actor,
        "status": run.status,
        "conclusion": run.conclusion,
        "created_at": run.created_at,
        "completed_at": run.completed_at,
    })
}

fn job_json(row: &JobRow, live: &BTreeMap<u64, Value>) -> Value {
    json!({
        "id": row.id,
        "plan_id": row.plan_id,
        "name": row.name,
        "status": row.status,
        "conclusion": row.conclusion,
        "attempt": row.attempt,
        "started_at": row.started_at,
        "completed_at": row.completed_at,
        "steps": live.get(&row.id).unwrap_or(&row.steps),
        "has_log": row.log_key.is_some(),
    })
}

/// The answer of `GET /api/ci/runs/{run}`.
fn run_answer(run: &RunRow, repo: &RepoRow, jobs: &[JobRow], state: Option<&RunState>) -> Value {
    let live = live_steps(jobs, state);
    json!({
        "run": run_json(run, repo),
        "jobs": jobs.iter().map(|row| job_json(row, &live)).collect::<Vec<_>>(),
    })
}

/// How the runtime treats the body of a stored object it serves. An
/// object stored compressed already is what its `Content-Encoding`
/// says; left automatic, the runtime would compress it a second time.
fn encode_body(content_encoding: Option<&str>) -> EncodeBody {
    if content_encoding.is_some_and(|encoding| !encoding.is_empty()) {
        EncodeBody::Manual
    } else {
        EncodeBody::Automatic
    }
}

fn fail(message: impl Into<String>) -> worker::Error {
    worker::Error::RustError(message.into())
}

pub(crate) fn reply(status: u16, body: &Value) -> Result<Response> {
    Ok(Response::from_json(body)?.with_status(status))
}

pub(crate) fn no_content() -> Result<Response> {
    Ok(Response::empty()?.with_status(204))
}

pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// Answers a request under `/api/ci/`.
///
/// # Errors
///
/// Never: a failure answers 500, and the message stays in the log.
pub async fn handle(req: Request, env: &Env) -> Result<Response> {
    let mut response = match serve(req, env).await {
        Ok(Ok(response)) => response,
        Ok(Err(Stop(status, message, retry))) => {
            let mut response = reply(status, &json!({ "error": message }))?;
            if let Some(seconds) = retry {
                response
                    .headers_mut()
                    .set("Retry-After", &seconds.to_string())?;
            }
            response
        }
        Err(err) => {
            console_error!("api: {err}");
            reply(500, &json!({ "error": "internal error" }))?
        }
    };
    // A WebSocket answer has fixed headers.
    let _ = response.headers_mut().set("Cache-Control", "no-store");
    Ok(response)
}

pub(crate) type Served<T = Response> = std::result::Result<T, Stop>;

/// What an `/api/ci/` path is: one of the run routes or a settings
/// route.
enum Target<'a> {
    Ci(Route<'a>),
    Settings(SettingsRoute<'a>),
}

async fn serve(mut req: Request, env: &Env) -> Result<Served> {
    let path = req.path();
    let target = match (route(&path), path.strip_prefix("/api/ci/")) {
        (Some(route), _) => Target::Ci(route),
        (None, Some(rest)) => match settings_api::find(rest) {
            Some(route) => Target::Settings(route),
            None => return Ok(stop(404, "not found")),
        },
        (None, None) => return Ok(stop(404, "not found")),
    };
    let method = req.method();
    let allowed = match &target {
        Target::Ci(route) => route.method() == method,
        Target::Settings(route) => route.allows(&method),
    };
    if !allowed {
        return Ok(stop(405, "method not allowed"));
    }
    let settings = Settings::read(env).map_err(fail)?;
    let live = matches!(target, Target::Ci(Route::Live(_)));
    let origin = req.headers().get("Origin")?;
    if let Err(stop) = origin_guard(&method, live, origin.as_deref(), &settings.app_url) {
        return Ok(Err(stop));
    }
    let db = Db::connect(env).await?;
    let session = Session::find(&req, &db).await?;
    let cx = Cx {
        env,
        db: &db,
        settings: &settings,
        session: session.as_ref(),
    };
    let query = req.url()?.query().map(str::to_owned);
    if guarded(&method) {
        // Before any call to GitHub, so a cross-site request costs the
        // victim nothing.
        let given = req.headers().get(hatchrun_cloud::session::CSRF_HEADER)?;
        if let Err(stop) = csrf_guard(cx.session.map(|s| s.id.as_slice()), given.as_deref()) {
            return Ok(Err(stop));
        }
        let cap = match &target {
            Target::Ci(_) => MAX_BODY,
            Target::Settings(route) => body_cap(route),
        };
        let length = req
            .headers()
            .get("Content-Length")?
            .and_then(|value| value.trim().parse::<usize>().ok());
        if length.is_some_and(|length| length > cap) {
            return Ok(stop(413, "the request is too large"));
        }
    }
    let route = match target {
        Target::Ci(route) => route,
        Target::Settings(route) => return settings_api::handle(&cx, route, &mut req).await,
    };
    match route {
        Route::Runs { owner, repo } => cx.runs(owner, repo, query.as_deref()).await,
        Route::Run(run) => cx.run(run).await,
        Route::Log(job) => cx.log(job, offset_query(query.as_deref())).await,
        Route::LogIndex(job) => cx.log_index(job).await,
        Route::Live(job) => cx.live(job, &req).await,
        Route::Cancel(run) => cx.cancel(run).await,
        Route::Dispatch { owner, repo } => cx.dispatch(owner, repo, &mut req).await,
    }
}

pub(crate) struct Cx<'a> {
    pub(crate) env: &'a Env,
    pub(crate) db: &'a Db,
    pub(crate) settings: &'a Settings,
    pub(crate) session: Option<&'a Session>,
}

/// A run with the repository it belongs to.
struct Found {
    run: RunRow,
    repo: RepoRow,
}

impl Cx<'_> {
    /// The caller's access, or the answer that asks to try again.
    async fn access(&self, repo: &RepoRow) -> Result<Served<Access>> {
        match access::resolve(self.session, self.settings, repo).await {
            Ok(have) => Ok(Ok(have)),
            Err(access::Failure::Retry(wait)) => Ok(Err(Stop(
                503,
                "GitHub is rate limiting; try again shortly".to_owned(),
                Some(wait),
            ))),
            Err(access::Failure::Worker(err)) => Err(err),
        }
    }

    /// The repository, when the caller may use it as `want`.
    async fn repo(&self, owner: &str, name: &str, want: Access) -> Result<Served<RepoRow>> {
        let Some(repo) = store::repo_by_name(self.db, &format!("{owner}/{name}")).await? else {
            return Ok(stop(404, "not found"));
        };
        Ok(self.allowed(&repo, want).await?.map(|()| repo))
    }

    async fn allowed(&self, repo: &RepoRow, want: Access) -> Result<Served<()>> {
        match self.access(repo).await? {
            Ok(have) => Ok(needs(have, want, self.session.is_some())),
            Err(stop) => Ok(Err(stop)),
        }
    }

    /// The run, with its repository, when the caller may use it as
    /// `want`. The run's tenant and repository must agree, so a row
    /// that points across tenants is not there.
    async fn found(&self, run_id: u64, want: Access) -> Result<Served<Found>> {
        let Some(run) = store::run(self.db, run_id).await? else {
            return Ok(stop(404, "not found"));
        };
        let Some(repo) = store::repo(self.db, run.repo_id).await? else {
            return Ok(stop(404, "not found"));
        };
        if repo.tenant_id != run.tenant_id {
            return Ok(stop(404, "not found"));
        }
        Ok(self
            .allowed(&repo, want)
            .await?
            .map(|()| Found { run, repo }))
    }

    /// The job and its run, when the caller may read them.
    async fn job(&self, job_id: u64) -> Result<Served<(JobRow, Found)>> {
        let Some(job) = store::job(self.db, job_id).await? else {
            return Ok(stop(404, "not found"));
        };
        Ok(self
            .found(job.run_id, Access::Read)
            .await?
            .map(|found| (job, found)))
    }

    async fn runs(&self, owner: &str, name: &str, query: Option<&str>) -> Result<Served> {
        let before = match before_query(query) {
            Ok(before) => before,
            Err(stop) => return Ok(Err(stop)),
        };
        let repo = match self.repo(owner, name, Access::Read).await? {
            Ok(repo) => repo,
            Err(stop) => return Ok(Err(stop)),
        };
        let rows = store::runs_for_repo(self.db, repo.id, before, PAGE).await?;
        let runs: Vec<Value> = rows.iter().map(|run| run_json(run, &repo)).collect();
        Ok(Ok(Response::from_json(&json!({ "runs": runs }))?))
    }

    async fn run(&self, run_id: u64) -> Result<Served> {
        let found = match self.found(run_id, Access::Read).await? {
            Ok(found) => found,
            Err(stop) => return Ok(Err(stop)),
        };
        let jobs = store::jobs_for_run(self.db, run_id).await?;
        let state = if jobs.iter().any(|row| row.status != "completed") {
            self.state(&found.run).await
        } else {
            None
        };
        let body = run_answer(&found.run, &found.repo, &jobs, state.as_ref());
        Ok(Ok(Response::from_json(&body)?))
    }

    /// The run's object's view of it. A run that is not held, or an
    /// object that does not answer, leaves the rows to speak.
    async fn state(&self, run: &RunRow) -> Option<RunState> {
        let path = format!("/state?run={}&tenant={}", run.id, run.tenant_id);
        let mut response = match call(self.env, run.id, Method::Get, &path).await {
            Ok(response) => response,
            Err(err) => {
                console_warn!("run {}: state: {err}", run.id);
                return None;
            }
        };
        if response.status_code() != 200 {
            return None;
        }
        match response.json::<RunState>().await {
            Ok(state) => Some(state),
            Err(err) => {
                console_warn!("run {}: state: {err}", run.id);
                None
            }
        }
    }

    async fn log(&self, job_id: u64, offset: Option<u64>) -> Result<Served> {
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        if job.status == "completed"
            && let Some(key) = job.log_key.as_deref()
        {
            if !owns_key(key, &found.run.tenant_id, found.repo.id, found.run.id) {
                console_error!(
                    "job {job_id}: log key {key} is outside run {}",
                    found.run.id
                );
            } else if let Some(response) = self.stored(key).await? {
                return Ok(Ok(response));
            }
        }
        // Running, or finished and not uploaded yet.
        let path = match offset {
            Some(offset) => format!("/log?job={job_id}&offset={offset}"),
            None => format!("/log?job={job_id}"),
        };
        let response = call(self.env, found.run.id, Method::Get, &path).await?;
        relay_text(response).await
    }

    async fn log_index(&self, job_id: u64) -> Result<Served> {
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        let key = job
            .log_key
            .as_deref()
            .filter(|_| job.status == "completed")
            .and_then(index_of)
            .filter(|key| owns_key(key, &found.run.tenant_id, found.repo.id, found.run.id));
        let Some(key) = key else {
            return Ok(stop(404, "no index"));
        };
        Ok(self
            .stored(&key)
            .await?
            .map_or_else(|| stop(404, "no index"), Ok))
    }

    /// An object of the logs bucket as an answer, with the headers it
    /// was stored with (type and encoding). `None` when it is not there.
    async fn stored(&self, key: &str) -> Result<Option<Response>> {
        let Some(object) = self.env.bucket(BUCKET)?.get(key).execute().await? else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        let headers = Headers::new();
        object.write_http_metadata(Headers(headers.0.clone()))?;
        headers.set("ETag", &object.http_etag())?;
        let encoding = object.http_metadata().content_encoding;
        Ok(Some(
            ResponseBuilder::new()
                .with_headers(headers)
                .with_encode_body(encode_body(encoding.as_deref()))
                .body(body.response_body()?),
        ))
    }

    async fn live(&self, job_id: u64, req: &Request) -> Result<Served> {
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        let upgrade = req.headers().get("Upgrade")?;
        if !upgrade.is_some_and(|value| value.eq_ignore_ascii_case("websocket")) {
            return Ok(stop(426, "a WebSocket upgrade is needed"));
        }
        if job.status == "completed" {
            return Ok(stop(409, "the job ended; read its log"));
        }
        // The socket is checked once, when it opens: a reader whose access
        // ends keeps it until the job ends, which is bounded by the
        // job's timeout.
        let headers = Headers::new();
        headers.set("Upgrade", "websocket")?;
        let mut init = RequestInit::new();
        init.with_method(Method::Get).with_headers(headers);
        let request = Request::new_with_init(&format!("https://run/live?job={job_id}"), &init)?;
        let response = run_stub(self.env, found.run.id)?
            .fetch_with_request(request)
            .await?;
        Ok(Ok(response))
    }

    async fn cancel(&self, run_id: u64) -> Result<Served> {
        let found = match self.found(run_id, Access::Write).await? {
            Ok(found) => found,
            Err(stop) => return Ok(Err(stop)),
        };
        let response = call(self.env, found.run.id, Method::Post, "/cancel").await?;
        if !(200..300).contains(&response.status_code()) {
            return Err(fail(format!(
                "run {run_id}: cancel answered {}",
                response.status_code()
            )));
        }
        no_content().map(Ok)
    }

    #[allow(clippy::too_many_lines)] // One request, read top to bottom.
    async fn dispatch(&self, owner: &str, name: &str, req: &mut Request) -> Result<Served> {
        let repo = match self.repo(owner, name, Access::Write).await? {
            Ok(repo) => repo,
            Err(stop) => return Ok(Err(stop)),
        };
        let Some(session) = self.session else {
            return Ok(stop(401, "sign in to change runs"));
        };
        // Only a writer's body is read, and a chunked one is bounded here.
        let body = req.bytes().await?;
        if body.len() > MAX_BODY {
            return Ok(stop(413, "the request is too large"));
        }
        let Ok(asked) = serde_json::from_slice::<Dispatch>(&body) else {
            return Ok(stop(400, "expected {\"workflow\", \"ref\", \"inputs\"}"));
        };
        let (git_ref, api_ref) = match ref_names(&asked.r#ref, &repo.default_branch) {
            Ok(names) => names,
            Err(message) => return Ok(stop(422, message)),
        };
        let now = now_ms();
        let settings = self.settings;
        let github = Counted::default();
        let tokens = Tokens::new(
            Kv::new(self.env),
            App::new(
                settings.github_api.clone(),
                settings.app_id.clone(),
                settings.app_key.clone(),
            ),
            &github,
            now,
        );
        let response = tokens
            .send(repo.installation_id, |token| {
                github::commit_sha(&settings.github_api, &repo.full_name, &api_ref, token)
            })
            .await
            .map_err(|err| fail(err.to_string()))?;
        let sha = match github::sha_of(&response) {
            Ok(sha) => sha,
            Err(_) if matches!(response.status, 404 | 422) => {
                return Ok(stop(422, format!("`{git_ref}` does not exist")));
            }
            Err(message) => return Err(fail(message)),
        };
        let config = runs::read_config(
            &tokens,
            settings,
            repo.installation_id,
            &repo.full_name,
            &sha,
        )
        .await?;
        let source = match &config {
            Loaded::Missing => {
                return Ok(stop(
                    422,
                    format!("`{git_ref}` has no {}", github::CONFIG_PATH),
                ));
            }
            Loaded::Text(Err(diagnostic)) => return Ok(stop(422, diagnostic.message.clone())),
            Loaded::Text(Ok(source)) => source,
        };
        let parsed = match parse(source) {
            Ok(parsed) => parsed,
            Err(diagnostics) => {
                let message = diagnostics
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Ok(stop(422, message));
            }
        };
        let inputs = match dispatchable(&parsed, &asked.workflow)
            .and_then(|trigger| resolve_inputs(&trigger.inputs, &asked.inputs))
        {
            Ok(inputs) => inputs,
            Err(message) => return Ok(stop(422, message)),
        };
        let event = dispatch_event(
            &git_ref,
            &inputs,
            &asked.workflow,
            &repo.full_name,
            &session.user.login,
        );
        let delivery = format!("dispatch:{}", hex(&webcrypto::random_bytes(16)?));
        let started = Repo {
            id: repo.id,
            full_name: repo.full_name.clone(),
            default_branch: repo.default_branch.clone(),
            private: repo.private,
        };
        let ids = runs::create(
            self.env,
            self.db,
            settings,
            &tokens,
            &runs::Request {
                tenant_id: &repo.tenant_id,
                installation_id: repo.installation_id,
                repo: &started,
                delivery: &delivery,
                event_name: "workflow_dispatch",
                event: &event,
                trigger: &Event::Dispatch,
                compile_ref: &git_ref,
                git_ref: &git_ref,
                sha: &sha,
                actor: &session.user.login,
                head_ref: "",
                base_ref: "",
                workflow: Some(&asked.workflow),
            },
            &config,
            now,
        )
        .await?;
        if ids.is_empty() {
            return Ok(stop(
                422,
                "no run started: CI is off for this organization, or the workflow \
                 has no job for this ref",
            ));
        }
        reply(202, &json!({ "runs": ids })).map(Ok)
    }
}

/// The body of a dispatch.
#[derive(Deserialize)]
struct Dispatch {
    workflow: String,
    #[serde(default)]
    r#ref: String,
    #[serde(default)]
    inputs: Map<String, Value>,
}

/// Calls the object of run `run_id`.
async fn call(env: &Env, run_id: u64, method: Method, path: &str) -> Result<Response> {
    let mut init = RequestInit::new();
    init.with_method(method);
    let request = Request::new_with_init(&format!("https://run{path}"), &init)?;
    run_stub(env, run_id)?.fetch_with_request(request).await
}

/// The text of the object's `/log` with its offsets, or its error
/// status.
async fn relay_text(mut response: Response) -> Result<Served> {
    let status = response.status_code();
    if status != 200 {
        let message = if status == 404 {
            "no log"
        } else {
            "log unavailable"
        };
        return Ok(stop(if status == 404 { 404 } else { 502 }, message));
    }
    let offset = response.headers().get("x-log-offset")?;
    let next = response.headers().get("x-log-next")?;
    let mut out = Response::ok(response.text().await?)?;
    for (name, value) in [("x-log-offset", offset), ("x-log-next", next)] {
        if let Some(value) = value {
            out.headers_mut().set(name, &value)?;
        }
    }
    Ok(Ok(out))
}

#[cfg(test)]
mod tests {
    use hatchrun_workflow::parse;

    use super::*;

    #[test]
    fn a_stored_gzip_log_is_served_as_stored() {
        assert!(matches!(encode_body(Some("gzip")), EncodeBody::Manual));
        assert!(matches!(encode_body(None), EncodeBody::Automatic));
        assert!(matches!(encode_body(Some("")), EncodeBody::Automatic));
    }
    use crate::logs;

    #[test]
    fn routes_parse() {
        assert_eq!(route("/api/ci/jobs/7/log"), Some(Route::Log(7)));
        assert_eq!(route("/api/ci/jobs/7/log/index"), Some(Route::LogIndex(7)));
        assert_eq!(route("/api/ci/jobs/7/live"), Some(Route::Live(7)));
        assert_eq!(route("/api/ci/runs/12"), Some(Route::Run(12)));
        assert_eq!(route("/api/ci/runs/12/cancel"), Some(Route::Cancel(12)));
        assert_eq!(
            route("/api/ci/repos/acme/app.git-x/runs"),
            Some(Route::Runs {
                owner: "acme",
                repo: "app.git-x"
            })
        );
        assert_eq!(
            route("/api/ci/repos/acme/app/dispatch"),
            Some(Route::Dispatch {
                owner: "acme",
                repo: "app"
            })
        );
    }

    #[test]
    fn bad_paths_have_no_route() {
        for path in [
            "/api/ci/",
            "/api/ci/runs",
            "/api/ci/runs/x",
            "/api/ci/runs/+1",
            "/api/ci/runs/1/",
            "/api/ci/runs/1/cancel/more",
            "/api/ci/jobs/7/logs",
            "/api/ci/repos/acme/runs",
            "/api/ci/repos/a%2Fb/app/runs",
            "/api/ci/repos//app/runs",
            "/api/ci/runs/99999999999999999999999",
            "/api/session",
        ] {
            assert_eq!(route(path), None, "{path}");
        }
    }

    #[test]
    fn only_dispatch_and_cancel_post() {
        assert_eq!(
            route("/api/ci/runs/1/cancel").unwrap().method(),
            Method::Post
        );
        assert_eq!(
            route("/api/ci/repos/a/b/dispatch").unwrap().method(),
            Method::Post
        );
        assert_eq!(route("/api/ci/runs/1").unwrap().method(), Method::Get);
        assert_eq!(route("/api/ci/jobs/1/live").unwrap().method(), Method::Get);
    }

    #[test]
    fn missing_access_hides_the_repository() {
        assert_eq!(needs(Access::Read, Access::Read, false), Ok(()));
        assert_eq!(needs(Access::Write, Access::Write, true), Ok(()));
        assert_eq!(
            needs(Access::None, Access::Read, true),
            stop(404, "not found")
        );
        assert_eq!(
            needs(Access::None, Access::Write, false),
            stop(404, "not found")
        );
        assert!(matches!(
            needs(Access::Read, Access::Write, true),
            Err(Stop(403, _, _))
        ));
        assert!(matches!(
            needs(Access::Read, Access::Write, false),
            Err(Stop(401, _, _))
        ));
    }

    #[test]
    fn query_values_are_numbers() {
        assert_eq!(offset_query(Some("offset=40")), Some(40));
        assert_eq!(offset_query(Some("a=1&offset=7")), Some(7));
        assert_eq!(offset_query(Some("offset=-1")), None);
        assert_eq!(offset_query(None), None);
        assert_eq!(before_query(None), Ok(None));
        assert_eq!(before_query(Some("before=9")), Ok(Some(9)));
        assert!(before_query(Some("before=x")).is_err());
    }

    #[test]
    fn refs_resolve_to_branches_and_tags() {
        let names = |input| ref_names(input, "main");
        assert_eq!(
            names(""),
            Ok(("refs/heads/main".into(), "heads/main".into()))
        );
        assert_eq!(
            names("feat/x"),
            Ok(("refs/heads/feat/x".into(), "heads/feat/x".into()))
        );
        assert_eq!(
            names("refs/tags/v1"),
            Ok(("refs/tags/v1".into(), "tags/v1".into()))
        );
        assert!(names("refs/pull/1/merge").is_err());
        assert!(names("a..b").is_err());
        assert!(names("a b").is_err());
        assert!(names("a//b").is_err());
        assert!(names(&"x".repeat(300)).is_err());
    }

    const CONFIG: &str = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs: [t]
  manual:
    on: { workflow_dispatch: {} }
    jobs: [t]
";

    #[test]
    fn dispatch_needs_workflow_dispatch() {
        let config = parse(CONFIG).unwrap();
        let err = dispatchable(&config, "ci").unwrap_err();
        assert!(err.contains("workflow_dispatch"), "{err}");
        assert!(dispatchable(&config, "manual").is_ok());
        let err = dispatchable(&config, "nope").unwrap_err();
        assert!(err.contains("no workflow `nope`"), "{err}");
    }

    const INPUTS: &str = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  manual:
    on:
      workflow_dispatch:
        inputs:
          env: { type: enum, enum: [staging, prod], default: staging }
          note: { type: string }
          dry: { type: boolean, default: false }
          count: { type: integer, default: 3 }
    jobs: [t]
";

    fn resolved(given: &Value) -> std::result::Result<Map<String, Value>, String> {
        let config = parse(INPUTS).unwrap();
        let trigger = dispatchable(&config, "manual").unwrap();
        resolve_inputs(&trigger.inputs, given.as_object().unwrap())
    }

    #[test]
    fn inputs_get_their_defaults() {
        let out = resolved(&json!({ "note": "hi", "env": "prod" })).unwrap();
        assert_eq!(
            Value::Object(out),
            json!({ "env": "prod", "note": "hi", "dry": false, "count": 3 })
        );
    }

    #[test]
    fn inputs_that_do_not_fit_are_named() {
        let err = |given| resolved(&given).unwrap_err();
        assert!(err(json!({})).contains("`note` is required"));
        assert!(err(json!({ "note": "x", "zzz": 1 })).contains("`zzz` is not declared"));
        assert!(err(json!({ "note": "x", "env": "dev" })).contains("`env` must be one of"));
        assert!(err(json!({ "note": 5 })).contains("`note` has the wrong type"));
        assert!(err(json!({ "note": "x", "dry": "true" })).contains("`dry` has the wrong type"));
        assert!(err(json!({ "note": "x", "count": 1.5 })).contains("`count` has the wrong type"));
        assert!(err(json!({ "note": "x", "count": [1] })).contains("`count`"));
        assert!(err(json!({ "note": "y".repeat(5000) })).contains("`note` is longer than"));
    }

    #[test]
    fn only_the_apps_origin_may_change_things() {
        let app = "https://app.hatchrun.io/";
        assert!(origin_ok(Some("https://app.hatchrun.io"), app));
        assert!(!origin_ok(Some("https://evil.hatchrun.io"), app));
        assert!(!origin_ok(Some("https://app.hatchrun.io.evil.test"), app));
        assert!(!origin_ok(Some("null"), app));
        assert!(!origin_ok(None, app));
    }

    const APP: &str = "https://app.hatchrun.io";

    #[test]
    fn every_mutating_method_is_guarded() {
        assert!(guarded(&Method::Post));
        assert!(guarded(&Method::Put));
        assert!(guarded(&Method::Delete));
        assert!(!guarded(&Method::Get));
    }

    #[test]
    fn put_without_csrf_is_refused() {
        let id = [7_u8; 32];
        let token = hatchrun_cloud::session::csrf_token(&id);
        assert_eq!(
            csrf_guard(Some(&id), None),
            Err(Stop(403, "missing or wrong CSRF token".into(), None))
        );
        assert_eq!(csrf_guard(Some(&id), Some("wrong")).unwrap_err().0, 403);
        assert_eq!(csrf_guard(None, Some(&token)).unwrap_err().0, 401);
        assert_eq!(csrf_guard(Some(&id), Some(&token)), Ok(()));
    }

    #[test]
    fn delete_from_another_origin_is_refused() {
        let evil = Some("https://evil.example");
        for method in [Method::Delete, Method::Put, Method::Post] {
            assert_eq!(
                origin_guard(&method, false, evil, APP),
                Err(Stop(403, "wrong origin".into(), None))
            );
            assert_eq!(origin_guard(&method, false, None, APP).unwrap_err().0, 403);
            assert_eq!(
                origin_guard(&method, false, Some("https://app.hatchrun.io"), APP),
                Ok(())
            );
        }
        // A plain read needs no origin, the socket does.
        assert_eq!(origin_guard(&Method::Get, false, evil, APP), Ok(()));
        assert_eq!(
            origin_guard(&Method::Get, true, evil, APP).unwrap_err().0,
            403
        );
    }

    #[test]
    fn secret_bodies_get_more_room() {
        let scope = settings_api::Scope::Org("acme");
        assert_eq!(body_cap(&SettingsRoute::Secret(scope, "TOKEN")), 128 << 10);
        assert_eq!(body_cap(&SettingsRoute::Var(scope, "NAME")), 128 << 10);
        assert_eq!(body_cap(&SettingsRoute::Cap { org: "acme" }), 64 << 10);
        assert_eq!(body_cap(&SettingsRoute::Secrets(scope)), 64 << 10);
    }

    #[test]
    fn the_dispatch_event_names_who_and_what() {
        let mut inputs = Map::new();
        inputs.insert("env".into(), json!("prod"));
        assert_eq!(
            dispatch_event("refs/heads/main", &inputs, "manual", "acme/app", "octo"),
            json!({
                "ref": "refs/heads/main",
                "inputs": { "env": "prod" },
                "workflow": "manual",
                "repository": { "full_name": "acme/app" },
                "sender": { "login": "octo" },
            })
        );
    }

    #[test]
    fn log_keys_stay_inside_the_run() {
        let key = logs::log_key("t1", 5, 7, 9, 2);
        assert!(owns_key(&key, "t1", 5, 7));
        assert!(!owns_key(&key, "t2", 5, 7));
        assert!(!owns_key(&key, "t1", 6, 7));
        assert!(!owns_key(&key, "t1", 5, 70));
        assert!(!owns_key("logs/t1/5/7/../../8/9/1.log.gz", "t1", 5, 7));
        assert_eq!(index_of(&key), Some(logs::index_key("t1", 5, 7, 9, 2)));
        assert_eq!(index_of("logs/t1/5/7/9/2.index.json"), None);
    }

    fn row(id: u64, status: &str, steps: &Value) -> JobRow {
        serde_json::from_value(json!({
            "id": id, "run_id": 7, "plan_id": "t", "name": "t", "status": status,
            "conclusion": null, "instance_id": null, "attempt": 1, "launch_attempts": 0,
            "next_launch_at": null, "timeout_minutes": 10, "updated_at": 0,
            "heartbeat_at": null, "started_at": null, "completed_at": null,
            "steps": steps.to_string(), "log_key": null, "template": "ubuntu-latest",
            "token_permissions": "{}", "secret_refs": "{\"all\":false,\"names\":[]}"
        }))
        .unwrap()
    }

    #[test]
    fn running_jobs_show_the_objects_steps() {
        let jobs = [
            row(1, "completed", &json!([{ "name": "done" }])),
            row(2, "running", &json!([])),
        ];
        assert!(live_steps(&jobs, None).is_empty());
        let live = BTreeMap::from([(2, json!([{ "index": 0, "name": "build" }]))]);
        assert_eq!(
            job_json(&jobs[0], &live)["steps"],
            json!([{ "name": "done" }])
        );
        assert_eq!(
            job_json(&jobs[1], &live)["steps"],
            json!([{ "index": 0, "name": "build" }])
        );
        assert_eq!(job_json(&jobs[1], &live)["has_log"], false);
    }
}
