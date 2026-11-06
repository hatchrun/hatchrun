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
//! | `GET /api/ci/orgs/{org}/runs?status=&repo=&branch=&actor=&q=&since=&until=&cursor=&limit=` | org member | `Page` of runs of the repositories the caller may read, last 7 days unless `since`, plus `counts` by state for the same filters |
//! | `GET /api/ci/orgs/{org}/activity?range=24h\|7d&repo=` | org member | `{"bucket_ms", "buckets": [{"at", "started", "passed", "failed"}], "concurrency": {"running", "queued", "limit"}}` for the repositories the caller may read |
//! | `GET /api/ci/orgs/{org}/needs-you` | org member | `{"approvals", "failing"}` |
//! | `GET /api/ci/repos/{owner}/{repo}/runs?status=&branch=&actor=&q=&since=&until=&cursor=&limit=` | read | `Page` of runs, newest first |
//! | `GET /api/ci/runs/{run}` | read | the run and its jobs |
//! | `GET /api/ci/jobs/{job}/log?offset=N` | read | the log text |
//! | `GET /api/ci/jobs/{job}/log?offset=N&length=M` | read | bytes `N` to `N+M` (at most 1 MiB) of a stored log, with `x-log-offset` and `x-log-bytes` |
//! | `GET /api/ci/jobs/{job}/log/index` | read | the step index with line counts and errors, `404` while running |
//! | `GET /api/ci/jobs/{job}/log/search?q=&cursor=` | read | `{"matches", "total", "truncated", "next"}` over a stored log |
//! | `GET /api/ci/jobs/{job}/live` | read | WebSocket |
//! | `GET /api/ci/runs/{run}/artifacts` | read | the run's artifacts |
//! | `GET /api/ci/runs/{run}/artifacts/{id}` | read | `302` to a presigned download |
//! | `POST /api/ci/repos/{owner}/{repo}/dispatch` | write, CSRF | `202` `{"runs": [..]}` |
//! | `POST /api/ci/runs/{run}/cancel` | write, CSRF | `204` |
//! | `POST /api/ci/jobs/{job}/approve` | org admin, CSRF | `204` |
//! | `POST /api/ci/jobs/{job}/reject` | org admin, CSRF | `204` |
//!
//! The list routes answer `{"items", "total", "next"}` (see
//! [`crate::page`]). The organization and repository settings routes,
//! and the organization's repositories, setup and environments, are in
//! [`crate::settings_api`] and [`crate::org_api`]; they go through the
//! same guard. Every
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
use crate::alerts;
use crate::env::{Settings, run_stub};
use crate::environments::{self, Decision};
use crate::github;
use crate::intake::Repo;
use crate::launches;
use crate::logs::{self, LogIndex};
use crate::org_api;
use crate::page::Query;
use crate::run::RunState;
use crate::runs::{self, Config as Loaded};
use crate::settings_api::{self, SettingsRoute, login};
use crate::store::{self, CiBilling, JobCounts, JobRow, RepoRow, RunRow};

/// The logs bucket.
const BUCKET: &str = "LOGS";

/// The most bytes of a body, dispatch and settings alike.
const MAX_BODY: usize = 64 << 10;

/// The most bytes of a secret or variable body: a 48 KB value is more
/// after JSON escaping.
const MAX_VALUE_BODY: usize = 128 << 10;

/// The longest text a log search takes, in characters.
const MAX_NEEDLE: usize = 200;

/// The longest ref a dispatch names.
const MAX_REF: usize = 255;

/// Where an `/api/ci/` path goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    /// An organization's runs, in the repositories the caller may read.
    OrgRuns {
        /// The organization.
        org: &'a str,
    },
    /// An organization's runs and jobs over time.
    OrgActivity {
        /// The organization.
        org: &'a str,
    },
    /// What waits for the caller in an organization.
    NeedsYou {
        /// The organization.
        org: &'a str,
    },
    /// A repository, with what its page counts.
    Repo {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// The workflows that ran in a repository.
    Workflows {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// The branches that ran in a repository.
    Branches {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// The pull requests of a repository.
    Pulls {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
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
    /// Let a job held for approval run.
    Approve(u64),
    /// Fail a job held for approval.
    Reject(u64),
    /// A job's log.
    Log(u64),
    /// A job's log index.
    LogIndex(u64),
    /// A search in a job's log.
    LogSearch(u64),
    /// A running job's log as it grows.
    Live(u64),
    /// A run's artifacts.
    Artifacts(u64),
    /// One artifact of a run, as a download.
    Artifact(u64, u64),
}

impl Route<'_> {
    /// The method the route answers to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Route::Dispatch { .. } | Route::Cancel(_) | Route::Approve(_) | Route::Reject(_) => {
                Method::Post
            }
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
        ["orgs", org, "runs"] => Some(Route::OrgRuns { org: login(org)? }),
        ["orgs", org, "activity"] => Some(Route::OrgActivity { org: login(org)? }),
        ["orgs", org, "needs-you"] => Some(Route::NeedsYou { org: login(org)? }),
        ["repos", owner, repo] => Some(Route::Repo {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["repos", owner, repo, "workflows"] => Some(Route::Workflows {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["repos", owner, repo, "branches"] => Some(Route::Branches {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["repos", owner, repo, "pulls"] => Some(Route::Pulls {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
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
        ["jobs", job, "approve"] => id(job).map(Route::Approve),
        ["jobs", job, "reject"] => id(job).map(Route::Reject),
        ["jobs", job, "log"] => id(job).map(Route::Log),
        ["jobs", job, "log", "index"] => id(job).map(Route::LogIndex),
        ["jobs", job, "log", "search"] => id(job).map(Route::LogSearch),
        ["jobs", job, "live"] => id(job).map(Route::Live),
        ["runs", run, "artifacts"] => id(run).map(Route::Artifacts),
        ["runs", run, "artifacts", artifact] => Some(Route::Artifact(id(run)?, id(artifact)?)),
        _ => None,
    }
}

/// Why a request ends before it does its work.
#[derive(Debug, PartialEq, Eq)]
/// The status, the message and, for a 503, the seconds to wait.
pub struct Stop(pub(crate) u16, pub(crate) String, pub(crate) Option<u64>);

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
    } else if want == Access::Admin {
        if signed_in {
            stop(403, "you need admin access to this repository")
        } else {
            stop(401, "sign in to manage settings")
        }
    } else if signed_in {
        stop(403, "you need write access to this repository")
    } else {
        stop(401, "sign in to change runs")
    }
}

/// What a request for a job's log asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct LogRange {
    /// Where to start: a byte offset of the text.
    offset: Option<u64>,
    /// How many bytes, for a range read of a stored log.
    length: Option<u64>,
}

/// The `offset` and `length` of a log request.
fn log_range(query: Option<&str>) -> std::result::Result<LogRange, Stop> {
    let query = Query::parse(query);
    Ok(LogRange {
        offset: query
            .number("offset")
            .or_else(|_| stop(400, "offset is a number"))?,
        length: query
            .number("length")
            .or_else(|_| stop(400, "length is a number"))?,
    })
}

/// The full ref a dispatch asks for, which the run shows and GitHub's
/// commit endpoint takes. A bare name is a branch and an empty one is
/// the default branch.
fn ref_name(input: &str, default_branch: &str) -> std::result::Result<String, String> {
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
    if full.starts_with("refs/heads/") || full.starts_with("refs/tags/") {
        Ok(full)
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
fn owns_key(key: &str, days: u32, tenant: &str, repo_id: u64, run_id: u64) -> bool {
    key.starts_with(&format!("logs/{days}d/{tenant}/{repo_id}/{run_id}/")) && !key.contains("..")
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

/// A run as the app reads it: the row, its branch, how long it took
/// and how its jobs stand.
pub(crate) fn run_json(run: &RunRow, repository: &str, jobs: JobCounts) -> Value {
    json!({
        "id": run.id,
        "run_number": run.run_number,
        "run_attempt": run.run_attempt,
        "repository": repository,
        "workflow": run.workflow,
        "event": run.event,
        "ref": run.git_ref,
        "branch": run.branch(),
        "sha": run.sha,
        "actor": run.actor,
        "message": run.message,
        "pull_request": run.pull_number.map(|number| json!({
            "number": number,
            "title": run.pull_title,
            "head_ref": run.head_ref,
            "base_ref": run.base_ref,
        })),
        "status": run.status,
        "conclusion": run.conclusion,
        "created_at": run.created_at,
        "completed_at": run.completed_at,
        "duration_ms": run.completed_at.map(|done| done.saturating_sub(run.created_at)),
        "jobs": jobs,
    })
}

/// How long a presigned artifact download holds, in seconds.
const DOWNLOAD_SECS: u64 = 300;

/// An artifact as the run page lists it. It goes when its run does, at
/// the run's creation plus its retention.
fn artifact_json(row: &store::ArtifactRow, run: &RunRow) -> Value {
    let uploaded = row.finalized_at.unwrap_or(row.created_at);
    json!({
        "id": row.id,
        "name": row.name,
        "size": row.size.unwrap_or(0),
        "created_at": uploaded,
        "expires_at": run.created_at + u64::from(run.retention_days) * 86_400_000,
    })
}

/// Percent-encodes everything but the unreserved characters.
fn percent_encode(text: &str) -> String {
    use std::fmt::Write;
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

/// The `Content-Disposition` of an artifact's download: the toolkit
/// stores a zip, and the object's own key is a random token.
fn disposition(name: &str) -> String {
    format!(
        "attachment; filename*=UTF-8''{}",
        percent_encode(&format!("{name}.zip"))
    )
}

fn job_json(row: &JobRow, live: &BTreeMap<u64, Value>) -> Value {
    json!({
        "id": row.id,
        "plan_id": row.plan_id,
        "name": row.name,
        "needs": row.needs,
        "status": row.status,
        "conclusion": row.conclusion,
        "attempt": row.attempt,
        "started_at": row.started_at,
        "completed_at": row.completed_at,
        "steps": live.get(&row.id).unwrap_or(&row.steps),
        "has_log": row.log_key.is_some(),
        "held": row.held,
        "environment": row.environment,
        "environment_url": row.environment_url,
        "approval": environments::decision(row).map(|decision| {
            let (verdict, by) = match decision {
                Decision::Approved(by) => ("approved", by),
                Decision::Rejected(by) => ("rejected", by),
            };
            json!({ "decision": verdict, "by": by, "at": row.approval_at })
        }),
    })
}

/// The name the console reads an access by.
pub(crate) fn access_name(access: Access) -> &'static str {
    match access {
        Access::None | Access::Read => "read",
        Access::Write => "write",
        Access::Admin => "admin",
    }
}

/// The answer of `GET /api/ci/runs/{run}`: the caller's `access` to
/// the repository, the run and its jobs.
fn run_answer(
    run: &RunRow,
    repo: &RepoRow,
    access: Access,
    jobs: &[JobRow],
    state: Option<&RunState>,
) -> Value {
    let live = live_steps(jobs, state);
    json!({
        "access": access_name(access),
        "run": run_json(run, &repo.full_name, JobCounts::of(jobs)),
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
        Target::Settings(route) => {
            return settings_api::handle(&cx, route, &mut req, query.as_deref()).await;
        }
    };
    match route {
        Route::OrgRuns { org } => org_api::runs(&cx, org, query.as_deref()).await,
        Route::OrgActivity { org } => org_api::activity(&cx, org, query.as_deref()).await,
        Route::NeedsYou { org } => org_api::needs_you(&cx, org).await,
        Route::Repo { owner, repo } => org_api::repo(&cx, owner, repo).await,
        Route::Workflows { owner, repo } => {
            org_api::workflows(&cx, owner, repo, query.as_deref()).await
        }
        Route::Branches { owner, repo } => {
            org_api::branches(&cx, owner, repo, query.as_deref()).await
        }
        Route::Pulls { owner, repo } => org_api::pulls(&cx, owner, repo, query.as_deref()).await,
        Route::Runs { owner, repo } => cx.runs(owner, repo, query.as_deref()).await,
        Route::Run(run) => cx.run(run).await,
        Route::Log(job) => match log_range(query.as_deref()) {
            Ok(range) => cx.log(job, &range).await,
            Err(stop) => Ok(Err(stop)),
        },
        Route::LogIndex(job) => cx.log_index(job).await,
        Route::LogSearch(job) => cx.log_search(job, query.as_deref()).await,
        Route::Live(job) => cx.live(job, &req).await,
        Route::Artifacts(run) => cx.artifacts(run).await,
        Route::Artifact(run, artifact) => cx.artifact(run, artifact).await,
        Route::Cancel(run) => cx.cancel(run).await,
        Route::Approve(job) => cx.decide(job, true).await,
        Route::Reject(job) => cx.decide(job, false).await,
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
    /// What the caller may do with the repository.
    access: Access,
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
    pub(crate) async fn repo(
        &self,
        owner: &str,
        name: &str,
        want: Access,
    ) -> Result<Served<RepoRow>> {
        Ok(self
            .repo_access(owner, name, want)
            .await?
            .map(|(repo, _)| repo))
    }

    /// The repository and the caller's access to it, when that is at
    /// least `want`.
    pub(crate) async fn repo_access(
        &self,
        owner: &str,
        name: &str,
        want: Access,
    ) -> Result<Served<(RepoRow, Access)>> {
        let Some(repo) = store::repo_by_name(self.db, &format!("{owner}/{name}")).await? else {
            return Ok(stop(404, "not found"));
        };
        Ok(self.allowed(&repo, want).await?.map(|have| (repo, have)))
    }

    /// The caller's access when it is at least `want`.
    async fn allowed(&self, repo: &RepoRow, want: Access) -> Result<Served<Access>> {
        match self.access(repo).await? {
            Ok(have) => Ok(needs(have, want, self.session.is_some()).map(|()| have)),
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
            .map(|access| Found { run, repo, access }))
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
        let query = Query::parse(query);
        let repo = match self.repo(owner, name, Access::Read).await? {
            Ok(repo) => repo,
            Err(stop) => return Ok(Err(stop)),
        };
        let mut filter = match org_api::run_filter(
            &query,
            self.session.map(|s| s.user.login.as_str()),
            now_ms(),
            // A repository's list reaches back as far as its runs do.
            u64::MAX,
        ) {
            Ok(filter) => filter,
            Err(stop) => return Ok(Err(stop)),
        };
        filter.repos.clear();
        filter.repo_ids = vec![repo.id];
        match org_api::run_list(self, &filter, &query).await {
            Ok(body) => Ok(Ok(Response::from_json(&body)?)),
            Err(org_api::Fail::Stop(stop)) => Ok(Err(stop)),
            Err(org_api::Fail::Worker(err)) => Err(err),
        }
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
        let body = run_answer(&found.run, &found.repo, found.access, &jobs, state.as_ref());
        Ok(Ok(Response::from_json(&body)?))
    }

    /// The run's object's view of it. A run that is not held, or an
    /// object that does not answer, leaves the rows to speak.
    async fn state(&self, run: &RunRow) -> Option<RunState> {
        let path = format!(
            "/state?run={}&tenant={}&days={}",
            run.id, run.tenant_id, run.retention_days
        );
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

    /// The run's finalized artifacts, oldest first. An artifact goes with
    /// the run, when the run's retention ends.
    async fn artifacts(&self, run_id: u64) -> Result<Served> {
        let found = match self.found(run_id, Access::Read).await? {
            Ok(found) => found,
            Err(stop) => return Ok(Err(stop)),
        };
        let rows = store::artifacts(self.db, run_id, None, None).await?;
        let artifacts: Vec<Value> = rows
            .iter()
            .map(|row| artifact_json(row, &found.run))
            .collect();
        Ok(Ok(Response::from_json(&json!({ "artifacts": artifacts }))?))
    }

    /// Sends the caller to a presigned download of artifact `id` of the
    /// run, which holds for a few minutes. The artifact is looked up by
    /// the run the caller may read, so no other run's artifact comes
    /// out, and its key must be one this run would have made.
    async fn artifact(&self, run_id: u64, id: u64) -> Result<Served> {
        let found = match self.found(run_id, Access::Read).await? {
            Ok(found) => found,
            Err(stop) => return Ok(Err(stop)),
        };
        if self.settings.artifacts_bucket.is_empty() {
            return Ok(stop(404, "no artifacts"));
        }
        let rows = store::artifacts(self.db, run_id, None, Some(id)).await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(stop(404, "not found"));
        };
        if !crate::artifacts::in_run(
            &row.object_key,
            found.run.retention_days,
            &found.run.tenant_id,
            found.repo.id,
            found.run.id,
        ) {
            console_error!(
                "artifact {id}: key {} is outside run {run_id}",
                row.object_key
            );
            return Ok(stop(404, "not found"));
        }
        let credentials = match crate::artifacts::credentials(self.settings).await {
            Ok(credentials) => credentials,
            Err(refusal) => return Ok(stop(refusal.status, refusal.message)),
        };
        let object = hatchrun_cloud::aws::s3::object_url(
            &self.settings.artifacts_bucket,
            &self.settings.ci_region,
            self.settings.aws_endpoint.as_deref(),
            &row.object_key,
        );
        let url = format!(
            "{object}?response-content-disposition={}",
            percent_encode(&disposition(&row.name))
        );
        let url = match hatchrun_cloud::aws::s3::presign(
            &hatchrun_cloud::aws::s3::get_object(&url),
            &credentials,
            &self.settings.ci_region,
            now_ms(),
            std::time::Duration::from_secs(DOWNLOAD_SECS),
        ) {
            Ok(url) => url,
            Err(err) => return Ok(stop(500, err)),
        };
        Ok(Ok(Response::redirect_with_status(
            worker::Url::parse(&url).map_err(|err| fail(err.to_string()))?,
            302,
        )?))
    }

    /// The key of the finished log of `job`, when it has one and it is
    /// the run's own.
    fn log_key_of(job: &JobRow, found: &Found) -> Option<String> {
        let key = job
            .log_key
            .as_deref()
            .filter(|_| job.status == "completed")?;
        if owns_key(
            key,
            found.run.retention_days,
            &found.run.tenant_id,
            found.repo.id,
            found.run.id,
        ) {
            Some(key.to_owned())
        } else {
            console_error!(
                "job {}: log key {key} is outside run {}",
                job.id,
                found.run.id
            );
            None
        }
    }

    async fn log(&self, job_id: u64, range: &LogRange) -> Result<Served> {
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        if let Some(key) = Self::log_key_of(&job, &found) {
            if let Some(length) = range.length {
                let offset = range.offset.unwrap_or(0);
                if let Some(response) = self.log_window(&key, offset, length).await? {
                    return Ok(Ok(response));
                }
            } else if let Some(response) = self.stored(&key).await? {
                return Ok(Ok(response));
            }
        }
        // Running, or finished and not uploaded yet. A range of `length`
        // bytes is for a stored log; the open log answers by `offset`.
        let path = match range.offset {
            Some(offset) => format!("/log?job={job_id}&offset={offset}"),
            None => format!("/log?job={job_id}"),
        };
        let response = call(self.env, found.run.id, Method::Get, &path).await?;
        relay_text(response).await
    }

    /// The bytes `offset` to `offset + length` of the stored log at
    /// `key`, `None` when it is not there. The headers say where the
    /// bytes start (they start after a character that `offset` splits)
    /// and how long the whole log is.
    async fn log_window(&self, key: &str, offset: u64, length: u64) -> Result<Option<Response>> {
        let index = match index_of(key) {
            Some(index_key) => self.index(&index_key).await?,
            None => None,
        };
        let span = index.as_ref().map_or(logs::Span::Whole, |index| {
            logs::span(&index.chunks, offset, length)
        });
        let (bytes, start) = match span {
            logs::Span::Whole => (self.stored_bytes(key).await?, 0),
            logs::Span::Range { at, len, start } => (self.stored_range(key, at, len).await?, start),
            logs::Span::Empty => (Some(Vec::new()), offset),
        };
        let Some(gz) = bytes else {
            return Ok(None);
        };
        let window = if gz.is_empty() {
            logs::Window {
                offset,
                bytes: Vec::new(),
            }
        } else {
            logs::window(&gz, start, offset, length).map_err(|err| fail(format!("{key}: {err}")))?
        };
        let total = match &index {
            Some(index) => index.bytes,
            None => match self.stored_bytes(key).await? {
                Some(all) => logs::text_len(&all).map_err(|err| fail(format!("{key}: {err}")))?,
                None => return Ok(None),
            },
        };
        let mut response = Response::from_bytes(window.bytes)?;
        let headers = response.headers_mut();
        headers.set("Content-Type", "text/plain; charset=utf-8")?;
        headers.set("x-log-offset", &window.offset.to_string())?;
        headers.set("x-log-bytes", &total.to_string())?;
        Ok(Some(response))
    }

    /// `length` bytes of the stored object at `key` from `at`.
    async fn stored_range(&self, key: &str, at: u64, length: u64) -> Result<Option<Vec<u8>>> {
        let Some(object) = self
            .env
            .bucket(BUCKET)?
            .get(key)
            .range(worker::Range::OffsetWithLength { offset: at, length })
            .execute()
            .await?
        else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        Ok(Some(body.bytes().await?))
    }

    /// The index stored at `key`; one that does not read is as if it
    /// were not there.
    async fn index(&self, key: &str) -> Result<Option<LogIndex>> {
        let Some(bytes) = self.stored_bytes(key).await? else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(index) => Ok(Some(index)),
            Err(err) => {
                console_error!("{key}: {err}");
                Ok(None)
            }
        }
    }

    /// The index of the job's stored log.
    async fn job_index(&self, job: &JobRow, found: &Found) -> Result<Option<LogIndex>> {
        match Self::log_key_of(job, found).as_deref().and_then(index_of) {
            Some(key) => self.index(&key).await,
            None => Ok(None),
        }
    }

    async fn log_index(&self, job_id: u64) -> Result<Served> {
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        let Some(index) = self.job_index(&job, &found).await? else {
            return Ok(stop(404, "no index"));
        };
        reply(
            200,
            &json!({
                "bytes": index.bytes,
                "lines": index.lines,
                "dropped_lines": index.dropped_lines,
                "steps": index.steps,
                "errors": index.errors,
                "first_error": index.errors.first(),
            }),
        )
        .map(Ok)
    }

    /// The lines of the job's stored log that hold `q`, ignoring case.
    async fn log_search(&self, job_id: u64, query: Option<&str>) -> Result<Served> {
        let query = Query::parse(query);
        let Some(needle) = query.get("q") else {
            return Ok(stop(400, "q is the text to search for"));
        };
        if needle.chars().count() > MAX_NEEDLE {
            return Ok(stop(400, format!("q is at most {MAX_NEEDLE} characters")));
        }
        let from = match query.cursor() {
            Ok(None) => 0,
            Ok(Some(parts)) if parts.len() == 1 => parts[0],
            Ok(Some(_)) | Err(_) => return Ok(stop(400, "the cursor is not valid")),
        };
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        let Some(key) = Self::log_key_of(&job, &found) else {
            return Ok(stop(404, "no stored log"));
        };
        let index = match index_of(&key) {
            Some(index_key) => self.index(&index_key).await?,
            None => None,
        };
        let (steps, chunks) = index.map_or_else(Default::default, |i| (i.steps, i.chunks));
        let mut searcher = logs::Searcher::new(needle, &steps, from);
        if chunks.is_empty() {
            let Some(gz) = self.stored_bytes(&key).await? else {
                return Ok(stop(404, "no stored log"));
            };
            searcher
                .scan(&gz)
                .map_err(|err| fail(format!("{key}: {err}")))?;
        }
        // One member at a time: a log is never held whole.
        for chunk in &chunks {
            if searcher.done() {
                break;
            }
            let Some(member) = self.stored_range(&key, chunk.at, chunk.len).await? else {
                return Ok(stop(404, "no stored log"));
            };
            searcher
                .scan(&member)
                .map_err(|err| fail(format!("{key}: {err}")))?;
        }
        let result = searcher.finish();
        let matches: Vec<Value> = result
            .hits
            .iter()
            .map(|hit| json!({ "offset": hit.offset, "line": hit.line, "step": hit.step }))
            .collect();
        reply(
            200,
            &json!({
                "matches": matches,
                "total": result.total,
                "truncated": result.next.is_some() || result.capped,
                "next": result.next.map(|at| crate::page::encode(&[at])),
            }),
        )
        .map(Ok)
    }

    /// An object of the logs bucket, whole.
    async fn stored_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(object) = self.env.bucket(BUCKET)?.get(key).execute().await? else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        Ok(Some(body.bytes().await?))
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

    /// Records an organization admin's decision on a job held for
    /// approval. The decision applies to the attempt the job is on and
    /// only once; a repeat answers `409`. Waking or ending the job can
    /// fail after the decision is saved: that is logged and the answer
    /// stays `204`, since the reconciler brings the job back.
    async fn decide(&self, job_id: u64, approved: bool) -> Result<Served> {
        let Some(session) = self.session else {
            return Ok(stop(401, "sign in to approve or reject jobs"));
        };
        let (job, found) = match self.job(job_id).await? {
            Ok(pair) => pair,
            Err(stop) => return Ok(Err(stop)),
        };
        let org_login = found.repo.full_name.split('/').next().unwrap_or_default();
        let org_id = match access::org_admin(self.settings, session, org_login).await {
            Ok(org_id) => org_id,
            Err(access::Failure::Retry(wait)) => {
                return Ok(Err(Stop(
                    503,
                    "GitHub is rate limiting; try again shortly".to_owned(),
                    Some(wait),
                )));
            }
            Err(access::Failure::Worker(err)) => return Err(err),
        };
        let admin = match org_id {
            Some(org_id) => store::tenant_by_org_id(self.db, org_id)
                .await?
                .is_some_and(|tenant| tenant.id == found.repo.tenant_id),
            None => false,
        };
        if !admin {
            return Ok(stop(
                403,
                format!("Only admins of {org_login} can approve or reject jobs"),
            ));
        }
        let login = session.user.login.as_str();
        if !store::decide_approval(self.db, job.id, job.attempt, approved, login, now_ms()).await? {
            return Ok(stop(409, "This job is not waiting for approval"));
        }
        let ended = if approved {
            let woken = [hatchrun_cloud::ci_wake::Woken {
                job_id: job.id,
                attempt: job.attempt,
            }];
            launches::send_wakes(self.env, &woken).await
        } else {
            launches::end_job(
                self.env,
                self.db,
                &job,
                &environments::rejected_message(login),
            )
            .await
        };
        if let Err(err) = ended {
            console_error!("job {}: after the decision: {err}", job.id);
        }
        no_content().map(Ok)
    }

    async fn dispatch(&self, owner: &str, name: &str, req: &mut Request) -> Result<Served> {
        let github = Counted::default();
        let served = self.dispatch_with(owner, name, req, &github).await;
        // The calls made are noted even when the dispatch was refused.
        alerts::note_rate_limits(self.db, &github, now_ms()).await;
        served
    }

    #[allow(clippy::too_many_lines)] // One request, read top to bottom.
    async fn dispatch_with(
        &self,
        owner: &str,
        name: &str,
        req: &mut Request,
        github: &Counted,
    ) -> Result<Served> {
        let repo = match self.repo(owner, name, Access::Write).await? {
            Ok(repo) => repo,
            Err(stop) => return Ok(Err(stop)),
        };
        let Some(session) = self.session else {
            return Ok(stop(401, "sign in to change runs"));
        };
        if !store::repo_enabled(self.db, &repo.tenant_id, repo.id).await? {
            return Ok(stop(422, "this repository is not enabled for CI"));
        }
        // Only a writer's body is read, and a chunked one is bounded here.
        let body = req.bytes().await?;
        if body.len() > MAX_BODY {
            return Ok(stop(413, "the request is too large"));
        }
        let Ok(asked) = serde_json::from_slice::<Dispatch>(&body) else {
            return Ok(stop(400, "expected {\"workflow\", \"ref\", \"inputs\"}"));
        };
        let git_ref = match ref_name(&asked.r#ref, &repo.default_branch) {
            Ok(full) => full,
            Err(message) => return Ok(stop(422, message)),
        };
        let now = now_ms();
        let settings = self.settings;
        let tokens = Tokens::new(
            Kv::new(self.env),
            App::new(
                settings.github_api.clone(),
                settings.app_id.clone(),
                settings.app_key.clone(),
            ),
            github,
            now,
        );
        let response = tokens
            .send(repo.installation_id, |token| {
                github::commit_sha(&settings.github_api, &repo.full_name, &git_ref, token)
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
        if let Some(tenant) = store::tenant(self.db, &repo.tenant_id).await?
            && tenant.ci_billing != CiBilling::Active
        {
            return Ok(stop(
                402,
                format!(
                    "Add a card for {org} at {app}/o/{org}/ci to run workflows.",
                    org = tenant.org_login,
                    app = settings.app_url
                ),
            ));
        }
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
                "no run started: the workflow has no job for this ref",
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
        assert_eq!(
            route("/api/ci/jobs/7/log/search"),
            Some(Route::LogSearch(7))
        );
        for (path, want) in [
            (
                "/api/ci/repos/acme/app",
                Route::Repo {
                    owner: "acme",
                    repo: "app",
                },
            ),
            (
                "/api/ci/repos/acme/app/workflows",
                Route::Workflows {
                    owner: "acme",
                    repo: "app",
                },
            ),
            (
                "/api/ci/repos/acme/app/branches",
                Route::Branches {
                    owner: "acme",
                    repo: "app",
                },
            ),
            (
                "/api/ci/repos/acme/app/pulls",
                Route::Pulls {
                    owner: "acme",
                    repo: "app",
                },
            ),
        ] {
            assert_eq!(route(path), Some(want), "{path}");
        }
        assert_eq!(
            route("/api/ci/orgs/acme/runs"),
            Some(Route::OrgRuns { org: "acme" })
        );
        assert_eq!(
            route("/api/ci/orgs/acme/activity"),
            Some(Route::OrgActivity { org: "acme" })
        );
        assert_eq!(
            route("/api/ci/orgs/acme/needs-you"),
            Some(Route::NeedsYou { org: "acme" })
        );
        assert_eq!(route("/api/ci/runs/12"), Some(Route::Run(12)));
        assert_eq!(
            route("/api/ci/runs/12/artifacts"),
            Some(Route::Artifacts(12))
        );
        assert_eq!(
            route("/api/ci/runs/12/artifacts/5"),
            Some(Route::Artifact(12, 5))
        );
        assert_eq!(route("/api/ci/runs/12/cancel"), Some(Route::Cancel(12)));
        assert_eq!(route("/api/ci/jobs/5/approve"), Some(Route::Approve(5)));
        assert_eq!(route("/api/ci/jobs/5/reject"), Some(Route::Reject(5)));
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
            "/api/ci/runs/1/artifacts/",
            "/api/ci/runs/1/artifacts/x",
            "/api/ci/runs/1/artifacts/2/more",
            "/api/ci/jobs/7/logs",
            "/api/ci/repos/acme",
            "/api/ci/repos/acme/app/pulls/3",
            "/api/ci/repos/acme/app/",
            "/api/ci/repos/a%2Fb/app",
            "/api/ci/repos/a%2Fb/app/runs",
            "/api/ci/repos//app/runs",
            "/api/ci/runs/99999999999999999999999",
            "/api/session",
        ] {
            assert_eq!(route(path), None, "{path}");
        }
    }

    #[test]
    fn posts_are_dispatch_cancel_and_decisions() {
        assert_eq!(
            route("/api/ci/runs/1/cancel").unwrap().method(),
            Method::Post
        );
        assert_eq!(
            route("/api/ci/repos/a/b/dispatch").unwrap().method(),
            Method::Post
        );
        for path in ["/api/ci/jobs/1/approve", "/api/ci/jobs/1/reject"] {
            assert_eq!(route(path).unwrap().method(), Method::Post);
        }
        assert_eq!(route("/api/ci/runs/1").unwrap().method(), Method::Get);
        assert_eq!(route("/api/ci/jobs/1/live").unwrap().method(), Method::Get);
    }

    #[test]
    fn access_is_named_for_the_console() {
        assert_eq!(access_name(Access::Read), "read");
        assert_eq!(access_name(Access::Write), "write");
        assert_eq!(access_name(Access::Admin), "admin");
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
        let range = |offset, length| LogRange { offset, length };
        assert_eq!(log_range(Some("offset=40")), Ok(range(Some(40), None)));
        assert_eq!(
            log_range(Some("a=1&offset=7&length=9")),
            Ok(range(Some(7), Some(9)))
        );
        assert!(log_range(Some("offset=-1")).is_err());
        assert!(log_range(Some("length=x")).is_err());
        assert_eq!(log_range(None), Ok(LogRange::default()));
    }

    #[test]
    fn refs_resolve_to_branches_and_tags() {
        let names = |input| ref_name(input, "main");
        assert_eq!(names(""), Ok("refs/heads/main".into()));
        assert_eq!(names("feat/x"), Ok("refs/heads/feat/x".into()));
        assert_eq!(names("refs/tags/v1"), Ok("refs/tags/v1".into()));
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
        let id = crate::token::tests::random_key();
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
    fn artifacts_expire_with_the_run_retention() {
        let row = store::ArtifactRow {
            id: 3,
            run_id: 9,
            job_id: 1,
            attempt: 1,
            name: "dist".into(),
            object_key: "k".into(),
            upload_id: None,
            size: Some(12),
            digest: None,
            created_at: 100,
            finalized_at: Some(200),
        };
        // Uploaded a day into the run, which expires 30 days after it
        // was created.
        let run: RunRow = serde_json::from_value(json!({
            "id": 9, "run_number": 1, "run_attempt": 1, "tenant_id": "t1",
            "repo_id": 7, "workflow": "ci", "delivery": "d", "event": "push",
            "git_ref": "refs/heads/main", "sha": "abc", "actor": "a",
            "status": "completed", "conclusion": "success", "created_at": 1000,
            "completed_at": null, "retention_days": 30
        }))
        .unwrap();
        let row = store::ArtifactRow {
            created_at: 1000 + 86_400_000,
            finalized_at: Some(1000 + 86_400_000),
            ..row
        };
        assert_eq!(
            artifact_json(&row, &run),
            json!({
                "id": 3, "name": "dist", "size": 12, "created_at": 1000 + 86_400_000_u64,
                "expires_at": 1000 + 30 * 86_400_000_u64,
            })
        );
    }

    #[test]
    fn downloads_are_named_after_the_artifact() {
        assert_eq!(
            disposition("my dist"),
            "attachment; filename*=UTF-8''my%20dist.zip"
        );
        assert_eq!(percent_encode("a b/c%"), "a%20b%2Fc%25");
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("Az09-_.~"), "Az09-_.~");
    }

    #[test]
    fn log_keys_have_the_tier() {
        assert_eq!(
            logs::log_key(30, "t1", 5, 7, 9, 1),
            "logs/30d/t1/5/7/9/1.log.gz"
        );
    }

    #[test]
    fn log_keys_stay_inside_the_run() {
        let key = logs::log_key(30, "t1", 5, 7, 9, 2);
        assert!(owns_key(&key, 30, "t1", 5, 7));
        assert!(!owns_key(&key, 90, "t1", 5, 7));
        assert!(!owns_key(&key, 30, "t2", 5, 7));
        assert!(!owns_key(&key, 30, "t1", 6, 7));
        assert!(!owns_key(&key, 30, "t1", 5, 70));
        assert!(!owns_key(
            "logs/30d/t1/5/7/../../8/9/1.log.gz",
            30,
            "t1",
            5,
            7
        ));
        assert_eq!(index_of(&key), Some(logs::index_key(30, "t1", 5, 7, 9, 2)));
        assert_eq!(index_of("logs/30d/t1/5/7/9/2.index.json"), None);
    }

    fn row(id: u64, status: &str, steps: &Value) -> JobRow {
        serde_json::from_value(json!({
            "id": id, "run_id": 7, "plan_id": "t", "name": "t", "status": status,
            "conclusion": null, "instance_id": null, "attempt": 1, "launch_attempts": 0, "capacity_attempts": 0, "capacity_since": null,
            "next_launch_at": null, "timeout_minutes": 10, "updated_at": 0,
            "heartbeat_at": null, "started_at": null, "completed_at": null,
            "steps": steps.to_string(), "log_key": null, "template": "ubuntu-latest",
            "token_permissions": "{}", "secret_refs": "{\"all\":false,\"names\":[]}", "needs": "[]"
        }))
        .unwrap()
    }

    #[test]
    fn job_json_shows_the_approval() {
        let mut job = row(1, "queued", &json!([]));
        job.held = Some("approval".into());
        job.environment = Some("production".into());
        let shown = job_json(&job, &BTreeMap::new());
        assert_eq!(shown["held"], "approval");
        assert_eq!(shown["environment"], "production");
        assert_eq!(shown["approval"], Value::Null);
        job.approval_attempt = Some(1);
        job.approval_decision = Some("approved".into());
        job.approval_by = Some("octo".into());
        job.approval_at = Some(70);
        assert_eq!(
            job_json(&job, &BTreeMap::new())["approval"],
            json!({ "decision": "approved", "by": "octo", "at": 70 })
        );
        job.approval_attempt = Some(0);
        assert_eq!(job_json(&job, &BTreeMap::new())["approval"], Value::Null);
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
