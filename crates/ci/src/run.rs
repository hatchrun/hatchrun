// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Durable Object that holds one run: its job graph, the state of
//! each job's check run and the order of what the agents report.
//!
//! Everything the object decides is a plain function in this module
//! ([`github_context`], [`job_document`], [`apply_steps`],
//! [`next_alarm`] and the rest), tested without the Workers runtime.
//! [`RunObject`] only reads storage, calls those functions and carries
//! out what they say.
//!
//! The object has one alarm. It keeps a set of deadlines (the outbox,
//! cancel escalation, upload retries and the purge, see [`Deadlines`])
//! and every handler ends by setting the alarm to the earliest. When it
//! fires, the object runs every part that is due and sets it again.
//!
//! Routes, called only by this Worker through `run_stub` (host
//! `https://run`):
//!
//! | Route | Body | Answer |
//! | --- | --- | --- |
//! | `POST /start` | [`Start`] | `204` |
//! | `POST /claim?job=ID` | `{ "token": "<scoped GitHub token>" }` | `200` job document, `409` ended |
//! | `POST /events?job=ID` | `Batch` | `200` batch reply, `409` ended |
//! | `POST /result?job=ID` | `JobResult` | `204` |
//! | `POST /failed?job=ID` | `{ "message": "..." }` | `204` |
//! | `POST /cancel` | none | `204` |
//! | `POST /rerun?job=ID` | none | `204` |
//! | `POST /purge` | none | `204` |
//! | `GET /state` | none | `200` [`RunState`] |

use std::cell::OnceCell;
use std::time::Duration;

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::{Counted, Sender};
use hatchrun_cloud::http::Response as HttpResponse;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::agent::{AgentEvent, Batch, BatchReply};
use hatchrun_workflow::{Annotation, Conclusion, Github, JobDocument, JobResult, PlannedJob};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, Method, Request, Response, Result, State, Storage};

use crate::checks::{
    JobView, MAX_TOTAL_ANNOTATIONS, Outbox, StepView, conclusion_word, details_url, render,
};
use crate::env::Settings;
use crate::github::{
    self, CheckRun, check_run_id, create_check_run, revoke_token, update_check_run,
};
use crate::graph::{Action, Graph, Node, State as NodeState};
use crate::intake::Repo;
use crate::store;

/// The queue that launches instances.
const LAUNCHES: &str = "CI_LAUNCHES";
/// The R2 bucket of logs, which also keeps the state of ended runs.
const BUCKET: &str = "LOGS";
/// How often a job's heartbeat reaches Postgres, in milliseconds.
const HEARTBEAT_WRITE_MS: u64 = 30_000;
/// How long a cancelled job has to stop before it is told to stop hard,
/// in milliseconds.
const ESCALATE_AFTER_MS: u64 = 5 * 60 * 1000;
/// How long a failed state snapshot waits before the next try, in
/// milliseconds.
const SNAPSHOT_RETRY_MS: u64 = 60 * 1000;
/// The largest event payload kept as is, in bytes.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// The most annotations kept for one job attempt.
const MAX_STORED_ANNOTATIONS: usize = 1000;
/// The most steps kept for one job attempt.
const MAX_STEPS: usize = 1000;
/// Storage keys.
const INFO: &str = "info";
const GRAPH: &str = "graph";
const OUTBOX: &str = "outbox";
const DEADLINES: &str = "deadlines";
const EVENT: &str = "event";

fn job_key(job: u64) -> String {
    format!("job:{job}")
}

fn log_prefix(job: u64) -> String {
    format!("log:{job}:")
}

/// Where the state of an ended run is kept in R2.
#[must_use]
pub fn snapshot_key(tenant: &str, run_id: u64) -> String {
    format!("state/{tenant}/{run_id}.json")
}

/// The run, as recorded when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    /// The run's row id.
    pub run_id: u64,
    /// The run's number within the repository.
    pub run_number: u64,
    /// Which attempt of the run is current, from 1.
    pub run_attempt: u32,
    /// The tenant that owns the repository.
    pub tenant_id: String,
    /// The GitHub App installation.
    pub installation_id: u64,
    /// The repository.
    pub repo: Repo,
    /// The workflow's name.
    pub workflow: String,
    /// `push` or `pull_request`.
    pub event_name: String,
    /// The full ref.
    pub git_ref: String,
    /// The commit.
    pub sha: String,
    /// The user that triggered the run.
    pub actor: String,
    /// The pull request's head branch, empty otherwise.
    pub head_ref: String,
    /// The pull request's base branch, empty otherwise.
    pub base_ref: String,
}

/// The body of `POST /start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    /// The run.
    pub run: RunInfo,
    /// Each job's row id with its planned job, in plan order.
    pub jobs: Vec<(u64, PlannedJob)>,
    /// The webhook payload.
    pub event: Value,
}

/// What the object keeps for one job attempt, under `job:<id>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobState {
    /// The job's check run, once GitHub made it.
    pub check_run_id: Option<u64>,
    /// The steps the agent reported.
    pub steps: Vec<StepView>,
    /// Every annotation of the attempt.
    pub annotations: Vec<Annotation>,
    /// How many annotations with a file GitHub already has.
    pub file_annotations_sent: usize,
    /// When the agent claimed the job, in Unix milliseconds.
    pub started_at: Option<u64>,
    /// When the job ended, in Unix milliseconds.
    pub completed_at: Option<u64>,
    /// The sequence number of the last batch applied.
    pub last_seq: u64,
    /// The GitHub token the agent holds, until it is revoked.
    pub github_token: Option<String>,
    /// When the heartbeat last reached Postgres, in Unix milliseconds.
    pub heartbeat_written_at: u64,
}

/// Everything the object's single alarm waits for, in Unix
/// milliseconds. The outbox's own deadline is in [`Outbox`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deadlines {
    /// When cancelled jobs that have not stopped are told to stop hard.
    pub escalate_at: Option<u64>,
    /// When uploads that failed, and the state snapshot, are tried
    /// again.
    pub upload_retry_at: Option<u64>,
    /// When the object deletes its storage.
    pub purge_at: Option<u64>,
}

/// One job in a [`RunState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
    /// The job in the graph.
    pub node: Node,
    /// Its check run.
    pub check_run_id: Option<u64>,
    /// Its steps.
    pub steps: Vec<StepView>,
    /// When the agent claimed it.
    pub started_at: Option<u64>,
    /// When it ended.
    pub completed_at: Option<u64>,
}

/// The answer to `GET /state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// The run.
    pub info: RunInfo,
    /// The run was cancelled.
    pub cancelled: bool,
    /// The jobs, in plan order.
    pub jobs: Vec<JobSummary>,
}

/// What an ended run leaves in R2 before the object deletes its
/// storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    info: RunInfo,
    graph: Graph,
    event: Value,
}

/// The first of the two parts of a ref that GitHub's contexts name:
/// the short name and whether it is a branch or a tag.
fn ref_parts(git_ref: &str) -> (&str, &'static str) {
    if let Some(name) = git_ref.strip_prefix("refs/tags/") {
        return (name, "tag");
    }
    let name = ["refs/heads/", "refs/pull/", "refs/"]
        .iter()
        .find_map(|prefix| git_ref.strip_prefix(prefix))
        .unwrap_or(git_ref);
    (name, "branch")
}

/// The `github` context of a job document, with `token` as its token.
pub(crate) fn github_struct(info: &RunInfo, api: &str, token: &str) -> Github {
    let (ref_name, ref_type) = ref_parts(&info.git_ref);
    Github {
        repository: info.repo.full_name.clone(),
        repository_owner: info
            .repo
            .full_name
            .split_once('/')
            .map_or_else(String::new, |(owner, _)| owner.to_owned()),
        sha: info.sha.clone(),
        ref_: info.git_ref.clone(),
        ref_name: ref_name.to_owned(),
        ref_type: ref_type.to_owned(),
        head_ref: info.head_ref.clone(),
        base_ref: info.base_ref.clone(),
        actor: info.actor.clone(),
        server_url: "https://github.com".to_owned(),
        api_url: api.to_owned(),
        graphql_url: format!("{api}/graphql"),
        token: token.to_owned(),
    }
}

/// The `github` context the graph's `if:` conditions read: the same
/// fields as the job document's, plus `event_name` and `event`, and no
/// token.
pub(crate) fn github_context(info: &RunInfo, event: &Value, api: &str) -> Value {
    let mut context = serde_json::to_value(github_struct(info, api, ""))
        .expect("the github context is plain strings and always serializes");
    if let Some(fields) = context.as_object_mut() {
        fields.remove("token");
        fields.insert("event_name".to_owned(), json!(info.event_name));
        fields.insert("event".to_owned(), event.clone());
    }
    context
}

/// The document an agent gets when it claims `job`, `None` when the
/// graph has no such job.
pub(crate) fn job_document(
    info: &RunInfo,
    event: &Value,
    graph: &Graph,
    job: u64,
    token: &str,
    api: &str,
) -> Option<JobDocument> {
    let node = graph.node(job)?;
    Some(JobDocument {
        run_id: info.run_id.to_string(),
        run_number: info.run_number,
        run_attempt: info.run_attempt,
        workflow: info.workflow.clone(),
        job: node.job.clone(),
        event_name: info.event_name.clone(),
        event: event.clone(),
        github: github_struct(info, api, token),
        needs: graph.needs(job),
        secrets: std::collections::BTreeMap::new(),
        vars: std::collections::BTreeMap::new(),
        timings: std::collections::BTreeMap::new(),
        cache: None,
    })
}

/// The earliest time anything waits for, `None` when nothing does.
pub(crate) fn next_alarm(deadlines: &Deadlines, outbox: &Outbox, now_ms: u64) -> Option<u64> {
    [
        deadlines.escalate_at,
        deadlines.upload_retry_at,
        deadlines.purge_at,
        outbox.next_alarm(now_ms),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// The parts of the alarm whose time has come.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    pub(crate) flush: bool,
    pub(crate) escalate: bool,
    pub(crate) upload: bool,
    pub(crate) purge: bool,
}

pub(crate) fn due(deadlines: &Deadlines, outbox: &Outbox, now_ms: u64) -> Due {
    let reached = |at: Option<u64>| at.is_some_and(|at| at <= now_ms);
    Due {
        flush: !outbox.due(now_ms).is_empty(),
        escalate: reached(deadlines.escalate_at),
        upload: reached(deadlines.upload_retry_at),
        purge: reached(deadlines.purge_at),
    }
}

/// What [`apply_steps`] did with a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The batch was new. A batch at or below `last_seq` was applied
    /// before and is ignored.
    pub(crate) fresh: bool,
    /// How many steps the batch finished.
    pub(crate) steps_finished: usize,
}

fn step_at(steps: &mut Vec<StepView>, index: usize) -> &mut StepView {
    while steps.len() <= index {
        let at = steps.len();
        steps.push(StepView {
            index: at,
            name: format!("Step {}", at + 1),
            summary: None,
        });
    }
    &mut steps[index]
}

/// Applies the step and annotation events of `batch` to `state`.
/// `Line` events are the log's, not the job state's.
pub(crate) fn apply_steps(state: &mut JobState, batch: &Batch) -> Applied {
    if batch.seq <= state.last_seq {
        return Applied {
            fresh: false,
            steps_finished: 0,
        };
    }
    state.last_seq = batch.seq;
    let mut steps_finished = 0;
    for event in &batch.events {
        match event {
            AgentEvent::StepStarted { index, name } if *index < MAX_STEPS => {
                let step = step_at(&mut state.steps, *index);
                step.name.clone_from(name);
                step.summary = None;
            }
            AgentEvent::StepFinished { index, summary } if *index < MAX_STEPS => {
                let step = step_at(&mut state.steps, *index);
                step.name.clone_from(&summary.name);
                step.summary = Some(summary.clone());
                steps_finished += 1;
            }
            AgentEvent::Annotation { annotation }
                if state.annotations.len() < MAX_STORED_ANNOTATIONS =>
            {
                state.annotations.push(annotation.clone());
            }
            _ => {}
        }
    }
    Applied {
        fresh: true,
        steps_finished,
    }
}

/// Takes what the agent's final report says about steps and, when no
/// annotation streamed in, its annotations.
pub(crate) fn apply_result(state: &mut JobState, result: &JobResult) {
    state.steps = result
        .steps
        .iter()
        .take(MAX_STEPS)
        .enumerate()
        .map(|(index, summary)| StepView {
            index,
            name: summary.name.clone(),
            summary: Some(summary.clone()),
        })
        .collect();
    if state.annotations.is_empty() {
        state.annotations = result
            .annotations
            .iter()
            .take(MAX_STORED_ANNOTATIONS)
            .cloned()
            .collect();
    }
}

/// Whether a heartbeat written at `last` is old enough to write again
/// at `now`, both in Unix milliseconds.
pub(crate) fn should_write_heartbeat(last: u64, now: u64) -> bool {
    now.saturating_sub(last) >= HEARTBEAT_WRITE_MS
}

/// `event`, with its `commits` replaced by an empty list when it is
/// over [`MAX_EVENT_BYTES`] as JSON.
pub(crate) fn bounded_event(mut event: Value) -> Value {
    let size = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len());
    if size > MAX_EVENT_BYTES
        && let Some(commits) = event.get_mut("commits")
    {
        *commits = json!([]);
    }
    event
}

/// The steps of a job as the row keeps them: one object per step that
/// ended.
pub(crate) fn steps_json(steps: &[StepView]) -> Value {
    let ended: Vec<_> = steps.iter().filter_map(|s| s.summary.as_ref()).collect();
    serde_json::to_value(ended).expect("step summaries always serialize")
}

/// Each job's row id and attempt.
fn attempts(graph: &Graph) -> Vec<(u64, u32)> {
    graph.jobs.iter().map(|n| (n.id, n.attempt)).collect()
}

/// The jobs whose attempt grew since `before`, with the new attempt.
pub(crate) fn reset_jobs(before: &[(u64, u32)], graph: &Graph) -> Vec<(u64, u32)> {
    attempts(graph)
        .into_iter()
        .filter(|now| !before.contains(now))
        .collect()
}

/// Whether the ended run may be written to R2 and purged: it ended,
/// every check run is sent, no upload waits and no purge is set.
pub(crate) fn snapshot_ready(graph: &Graph, outbox: &Outbox, deadlines: &Deadlines) -> bool {
    graph.conclusion().is_some()
        && outbox.is_empty()
        && deadlines.upload_retry_at.is_none()
        && deadlines.purge_at.is_none()
}

/// How GitHub's answer to a check run request changes what is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// It went through.
    Done,
    /// Try again after this many seconds.
    Wait(u64),
    /// Trying again cannot help (a request GitHub rejects).
    Drop,
}

/// Seconds to wait after `attempt` failed sends in a row: 60, doubling,
/// up to 900.
fn backoff(attempt: u32) -> u64 {
    (60_u64 << attempt.min(10)).min(900)
}

pub(crate) fn delivery(response: &HttpResponse, now_ms: u64, attempt: u32) -> Delivery {
    if response.ok() {
        return Delivery::Done;
    }
    if let Some(wait) = github::throttle(response, now_ms / 1000, attempt) {
        return Delivery::Wait(wait);
    }
    if response.status >= 500 || response.status == 401 {
        return Delivery::Wait(backoff(attempt));
    }
    Delivery::Drop
}

/// The graph and the steps of every job, as `GET /state` answers.
pub(crate) fn run_state(info: &RunInfo, graph: &Graph, states: &[(u64, JobState)]) -> RunState {
    let jobs = graph
        .jobs
        .iter()
        .map(|node| {
            let state = states
                .iter()
                .find(|(id, _)| *id == node.id)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            JobSummary {
                node: node.clone(),
                check_run_id: state.check_run_id,
                steps: state.steps,
                started_at: state.started_at,
                completed_at: state.completed_at,
            }
        })
        .collect();
    RunState {
        info: info.clone(),
        cancelled: graph.cancelled,
        jobs,
    }
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn fail(message: impl Into<String>) -> worker::Error {
    worker::Error::RustError(message.into())
}

fn answer(status: u16) -> Result<Response> {
    Ok(Response::empty()?.with_status(status))
}

#[derive(Serialize)]
struct LaunchMessage {
    job_id: u64,
}

#[derive(Deserialize)]
struct ClaimBody {
    token: String,
}

#[derive(Deserialize)]
struct FailedBody {
    message: String,
}

/// What a handler works on: the four pieces of state every handler
/// reads, and which of them it changed.
struct Loaded {
    info: RunInfo,
    graph: Graph,
    outbox: Outbox,
    deadlines: Deadlines,
    dirty: [bool; 4],
    restored: bool,
}

impl Loaded {
    fn info_mut(&mut self) -> &mut RunInfo {
        self.dirty[0] = true;
        &mut self.info
    }

    fn graph_mut(&mut self) -> &mut Graph {
        self.dirty[1] = true;
        &mut self.graph
    }

    fn outbox_mut(&mut self) -> &mut Outbox {
        self.dirty[2] = true;
        &mut self.outbox
    }

    fn deadlines_mut(&mut self) -> &mut Deadlines {
        self.dirty[3] = true;
        &mut self.deadlines
    }
}

// The macro that exports the object generates methods without docs.
mod object {
    #![allow(missing_docs)]

    use async_lock::Mutex;
    use worker::{DurableObject, Env, Request, Response, Result, State, durable_object};

    use super::Cx;

    /// One run.
    #[durable_object]
    pub struct RunObject {
        state: State,
        env: Env,
        /// Handlers wait for GitHub and Postgres, and storage reads let
        /// another request in meanwhile; this makes them take turns.
        turn: Mutex<()>,
    }

    impl DurableObject for RunObject {
        fn new(state: State, env: Env) -> Self {
            Self {
                state,
                env,
                turn: Mutex::new(()),
            }
        }

        async fn fetch(&self, mut req: Request) -> Result<Response> {
            let _turn = self.turn.lock().await;
            let url = req.url()?;
            let job = url
                .query_pairs()
                .find(|(key, _)| key == "job")
                .and_then(|(_, value)| value.parse::<u64>().ok());
            let cx = Cx::new(&self.state, &self.env)?;
            cx.route(&mut req, url.path(), job).await
        }

        async fn alarm(&self) -> Result<Response> {
            let _turn = self.turn.lock().await;
            let cx = Cx::new(&self.state, &self.env)?;
            cx.alarm().await?;
            Response::empty()
        }
    }
}

pub use object::RunObject;

/// One request or alarm: the clock, the settings and the connections.
struct Cx {
    storage: Storage,
    env: Env,
    /// The object's name, which is the run's id as text.
    name: Option<String>,
    settings: Settings,
    now: u64,
    db: OnceCell<Db>,
    github: Counted,
}

impl Cx {
    fn new(state: &State, env: &Env) -> Result<Self> {
        Ok(Self {
            storage: state.storage(),
            env: env.clone(),
            name: state.id().name(),
            settings: Settings::read(env).map_err(fail)?,
            now: now_ms(),
            db: OnceCell::new(),
            github: Counted::default(),
        })
    }

    async fn db(&self) -> Result<&Db> {
        if self.db.get().is_none() {
            let db = Db::connect(&self.env).await?;
            // Nothing else sets it: handlers take turns.
            let _ = self.db.set(db);
        }
        self.db.get().ok_or_else(|| fail("no database"))
    }

    async fn route(&self, req: &mut Request, path: &str, job: Option<u64>) -> Result<Response> {
        match (req.method(), path) {
            (Method::Post, "/start") => {
                let Ok(start) = req.json::<Start>().await else {
                    return Response::error("bad start", 400);
                };
                return self.start(start).await;
            }
            (Method::Post, "/purge") => return self.purge().await,
            _ => {}
        }
        let (restore, gone) = match path {
            "/rerun" | "/state" => (true, 404),
            "/claim" | "/events" => (false, 409),
            _ => (false, 204),
        };
        let Some(mut run) = self.loaded(restore).await? else {
            return answer(gone);
        };
        let response = match (req.method(), path, job) {
            (Method::Get, "/state", _) => {
                let mut states = Vec::new();
                for node in &run.graph.jobs {
                    states.push((node.id, self.job_state(node.id).await?));
                }
                Response::from_json(&run_state(&run.info, &run.graph, &states))
            }
            (Method::Post, "/cancel", _) => self.cancel(&mut run).await.and_then(|()| answer(204)),
            (Method::Post, "/claim", Some(job)) => match req.json::<ClaimBody>().await {
                Ok(body) => self.claim(&mut run, job, body.token).await,
                Err(_) => Response::error("bad claim", 400),
            },
            (Method::Post, "/events", Some(job)) => match req.json::<Batch>().await {
                Ok(batch) => self.events(&mut run, job, &batch).await,
                Err(_) => Response::error("bad batch", 400),
            },
            (Method::Post, "/result", Some(job)) => match req.json::<JobResult>().await {
                Ok(result) => self.result(&mut run, job, result).await,
                Err(_) => Response::error("bad result", 400),
            },
            (Method::Post, "/failed", Some(job)) => match req.json::<FailedBody>().await {
                Ok(body) => self.failed(&mut run, job, body.message).await,
                Err(_) => Response::error("bad failure", 400),
            },
            (Method::Post, "/rerun", Some(job)) => {
                self.rerun(&mut run, job).await.and_then(|()| answer(204))
            }
            _ => Response::error("not found", 404),
        }?;
        if run.dirty.iter().any(|d| *d) || run.restored {
            self.commit(&mut run).await?;
        }
        Ok(response)
    }

    async fn alarm(&self) -> Result<()> {
        let Some(mut run) = self.loaded(false).await? else {
            return self.storage.delete_alarm().await;
        };
        let due = due(&run.deadlines, &run.outbox, self.now);
        if due.purge {
            return self.purge().await.map(|_| ());
        }
        if due.flush {
            self.flush(&mut run).await?;
        }
        if due.escalate {
            run.deadlines_mut().escalate_at = None;
            // The reply to the agent's next batch carries the new count.
            run.graph_mut().escalate();
        }
        if due.upload {
            // Uploads of finished logs retry here (Task 10); a state
            // snapshot that failed is retried by `settle`.
            run.deadlines_mut().upload_retry_at = None;
        }
        self.commit(&mut run).await
    }

    async fn loaded(&self, restore: bool) -> Result<Option<Loaded>> {
        let storage = &self.storage;
        let (Some(info), Some(graph)) = (
            storage.get::<RunInfo>(INFO).await?,
            storage.get::<Graph>(GRAPH).await?,
        ) else {
            return if restore {
                self.restore().await
            } else {
                Ok(None)
            };
        };
        Ok(Some(Loaded {
            info,
            graph,
            outbox: storage.get(OUTBOX).await?.unwrap_or_default(),
            deadlines: storage.get(DEADLINES).await?.unwrap_or_default(),
            dirty: [false; 4],
            restored: false,
        }))
    }

    /// Reads the snapshot of an ended run back into an object that was
    /// purged. The run's id is the object's name.
    async fn restore(&self) -> Result<Option<Loaded>> {
        let Some(run_id) = self
            .name
            .as_deref()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            return Ok(None);
        };
        let Some(row) = store::run(self.db().await?, run_id).await? else {
            return Ok(None);
        };
        let bucket = self.env.bucket(BUCKET)?;
        let Some(object) = bucket
            .get(snapshot_key(&row.tenant_id, run_id))
            .execute()
            .await?
        else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        let snapshot: Snapshot = serde_json::from_slice(&body.bytes().await?)
            .map_err(|err| fail(format!("run {run_id}: snapshot: {err}")))?;
        self.storage.put(INFO, &snapshot.info).await?;
        self.storage.put(GRAPH, &snapshot.graph).await?;
        self.storage.put(EVENT, &snapshot.event).await?;
        Ok(Some(Loaded {
            info: snapshot.info,
            graph: snapshot.graph,
            outbox: Outbox::default(),
            deadlines: Deadlines::default(),
            dirty: [false; 4],
            restored: true,
        }))
    }

    /// Writes what the handler changed, snapshots an ended run and sets
    /// the alarm.
    async fn commit(&self, run: &mut Loaded) -> Result<()> {
        self.settle(run).await;
        let storage = &self.storage;
        if run.dirty[0] {
            storage.put(INFO, &run.info).await?;
        }
        if run.dirty[1] {
            storage.put(GRAPH, &run.graph).await?;
        }
        if run.dirty[2] {
            storage.put(OUTBOX, &run.outbox).await?;
        }
        if run.dirty[3] {
            storage.put(DEADLINES, &run.deadlines).await?;
        }
        run.dirty = [false; 4];
        match next_alarm(&run.deadlines, &run.outbox, self.now) {
            Some(at) => {
                let wait = Duration::from_millis(at.saturating_sub(self.now));
                storage.set_alarm(wait).await
            }
            None => storage.delete_alarm().await,
        }
    }

    /// An ended run with nothing left to send is written to R2 and then
    /// purged. A snapshot that fails is tried again after a minute.
    async fn settle(&self, run: &mut Loaded) {
        if !snapshot_ready(&run.graph, &run.outbox, &run.deadlines) {
            return;
        }
        match self.snapshot(run).await {
            Ok(()) => run.deadlines_mut().purge_at = Some(self.now),
            Err(err) => {
                worker::console_error!("run {}: snapshot: {err}", run.info.run_id);
                run.deadlines_mut().upload_retry_at = Some(self.now + SNAPSHOT_RETRY_MS);
            }
        }
    }

    async fn snapshot(&self, run: &Loaded) -> Result<()> {
        let snapshot = Snapshot {
            info: run.info.clone(),
            graph: run.graph.clone(),
            event: self.event().await?,
        };
        let body = serde_json::to_vec(&snapshot).map_err(|err| fail(err.to_string()))?;
        self.env
            .bucket(BUCKET)?
            .put(snapshot_key(&run.info.tenant_id, run.info.run_id), body)
            .execute()
            .await?;
        Ok(())
    }

    async fn purge(&self) -> Result<Response> {
        self.storage.delete_alarm().await?;
        self.storage.delete_all().await?;
        answer(204)
    }

    async fn event(&self) -> Result<Value> {
        Ok(self.storage.get(EVENT).await?.unwrap_or(Value::Null))
    }

    async fn github_ctx(&self, info: &RunInfo) -> Result<Value> {
        Ok(github_context(
            info,
            &self.event().await?,
            &self.settings.github_api,
        ))
    }

    async fn job_state(&self, job: u64) -> Result<JobState> {
        Ok(self.storage.get(&job_key(job)).await?.unwrap_or_default())
    }

    async fn put_job(&self, job: u64, state: &JobState) -> Result<()> {
        self.storage.put(&job_key(job), state).await
    }

    async fn start(&self, start: Start) -> Result<Response> {
        // The graph is written last, so a start that failed halfway
        // runs again.
        if self.storage.get::<Graph>(GRAPH).await?.is_some() {
            // Delivered again; the run is already held.
            return answer(204);
        }
        let Start {
            run: info,
            jobs,
            event,
        } = start;
        let mut run = Loaded {
            graph: Graph::new(jobs),
            outbox: Outbox::default(),
            deadlines: Deadlines::default(),
            info,
            dirty: [true; 4],
            restored: false,
        };
        let ids: Vec<u64> = run.graph.jobs.iter().map(|n| n.id).collect();
        for id in ids {
            run.outbox.mark(id);
        }
        self.storage.put(EVENT, &bounded_event(event)).await?;
        store::run_started(self.db().await?, run.info.run_id, self.now).await?;
        let ctx = self.github_ctx(&run.info).await?;
        let actions = run.graph.advance(&ctx);
        self.execute(&mut run, actions).await?;
        self.commit(&mut run).await?;
        answer(204)
    }

    async fn claim(&self, run: &mut Loaded, job: u64, token: String) -> Result<Response> {
        let Some(node) = run.graph.node(job) else {
            return Response::error("no such job", 404);
        };
        let was = node.state;
        if !matches!(was, NodeState::Queued | NodeState::Running) {
            return answer(409);
        }
        if was == NodeState::Queued && !store::start_job(self.db().await?, job, self.now).await? {
            return answer(409);
        }
        let mut state = self.job_state(job).await?;
        if let Some(old) = state.github_token.replace(token.clone()) {
            self.revoke(&old).await;
        }
        if was == NodeState::Queued {
            state.started_at = Some(self.now);
            state.heartbeat_written_at = self.now;
        }
        self.put_job(job, &state).await?;
        run.graph_mut().running(job);
        let document = job_document(
            &run.info,
            &self.event().await?,
            &run.graph,
            job,
            &token,
            &self.settings.github_api,
        )
        .ok_or_else(|| fail("job vanished"))?;
        Response::from_json(&document)
    }

    async fn events(&self, run: &mut Loaded, job: u64, batch: &Batch) -> Result<Response> {
        let Some(node) = run.graph.node(job) else {
            return Response::error("no such job", 404);
        };
        if node.state != NodeState::Running {
            return answer(409);
        }
        let reply = BatchReply {
            cancel: node.cancel,
        };
        let mut state = self.job_state(job).await?;
        let applied = apply_steps(&mut state, batch);
        let mut changed = applied.fresh;
        if applied.steps_finished > 0 {
            run.outbox_mut().mark(job);
        }
        if should_write_heartbeat(state.heartbeat_written_at, self.now) {
            store::heartbeat(self.db().await?, job, self.now).await?;
            state.heartbeat_written_at = self.now;
            changed = true;
        }
        if changed {
            self.put_job(job, &state).await?;
        }
        Response::from_json(&reply)
    }

    async fn result(&self, run: &mut Loaded, job: u64, result: JobResult) -> Result<Response> {
        let mut state = self.job_state(job).await?;
        apply_result(&mut state, &result);
        let ctx = self.github_ctx(&run.info).await?;
        let actions = run.graph_mut().finished(job, result, &ctx);
        if !actions.is_empty() {
            self.put_job(job, &state).await?;
        }
        self.execute(run, actions).await?;
        answer(204)
    }

    async fn failed(&self, run: &mut Loaded, job: u64, message: String) -> Result<Response> {
        let ctx = self.github_ctx(&run.info).await?;
        let actions = run.graph_mut().failed(job, message, &ctx);
        self.execute(run, actions).await?;
        answer(204)
    }

    async fn cancel(&self, run: &mut Loaded) -> Result<()> {
        let ctx = self.github_ctx(&run.info).await?;
        let actions = run.graph_mut().cancel(&ctx);
        if run.graph.conclusion().is_none() {
            run.deadlines_mut()
                .escalate_at
                .get_or_insert(self.now + ESCALATE_AFTER_MS);
        }
        self.execute(run, actions).await
    }

    /// A rerun starts every reset job's attempt from nothing: a new
    /// state (so its batches count from 1 again), its old token revoked,
    /// its log chunks dropped and its row set up for the new attempt.
    async fn rerun(&self, run: &mut Loaded, job: u64) -> Result<()> {
        let ctx = self.github_ctx(&run.info).await?;
        let before = attempts(&run.graph);
        let actions = run.graph_mut().rerun(job, &ctx);
        let reset = reset_jobs(&before, &run.graph);
        if reset.is_empty() {
            return Ok(());
        }
        let db = self.db().await?;
        for (id, attempt) in reset {
            if let Some(token) = self.job_state(id).await?.github_token {
                self.revoke(&token).await;
            }
            self.put_job(id, &JobState::default()).await?;
            self.delete_prefix(&log_prefix(id)).await?;
            store::reset_job(db, id, attempt, self.now).await?;
            run.outbox_mut().mark(id);
        }
        let attempt = store::reopen_run(db, run.info.run_id, self.now).await?;
        run.info_mut().run_attempt = attempt;
        let deadlines = run.deadlines_mut();
        deadlines.purge_at = None;
        deadlines.escalate_at = None;
        self.execute(run, actions).await
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        loop {
            let options = worker::durable::ListOptions::new()
                .prefix(prefix)
                .limit(128);
            let found = self.storage.list_with_options(options).await?;
            let keys: Vec<String> = found
                .keys()
                .into_iter()
                .filter_map(|k| k.ok()?.as_string())
                .collect();
            if keys.is_empty() {
                return Ok(());
            }
            self.storage.delete_multiple(keys).await?;
        }
    }

    async fn execute(&self, run: &mut Loaded, actions: Vec<Action>) -> Result<()> {
        for action in actions {
            match action {
                Action::Launch(job) => self.launch(job).await?,
                Action::Finished { job, conclusion } => {
                    self.finish_job(run, job, conclusion).await?;
                }
                // The agent learns of a cancel from the reply to its
                // next batch.
                Action::Cancel { .. } => {}
                Action::RunDone(conclusion) => {
                    store::finish_run(
                        self.db().await?,
                        run.info.run_id,
                        conclusion_word(conclusion),
                        self.now,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn launch(&self, job: u64) -> Result<()> {
        store::set_job(self.db().await?, job, "queued", None, self.now).await?;
        self.env
            .queue(LAUNCHES)?
            .send(LaunchMessage { job_id: job })
            .await
    }

    async fn finish_job(&self, run: &mut Loaded, job: u64, conclusion: Conclusion) -> Result<()> {
        let mut state = self.job_state(job).await?;
        state.completed_at = Some(self.now);
        let steps = steps_json(&state.steps);
        // The key of the finished log comes with the uploads (Task 10).
        store::finish_job(
            self.db().await?,
            job,
            conclusion_word(conclusion),
            &steps,
            None,
            self.now,
        )
        .await?;
        if let Some(token) = state.github_token.take() {
            self.revoke(&token).await;
        }
        self.put_job(job, &state).await?;
        run.outbox_mut().mark(job);
        Ok(())
    }

    /// Revokes a job's GitHub token. A token that stays valid expires
    /// within the hour, so a failure is logged and not retried.
    async fn revoke(&self, token: &str) {
        let request = revoke_token(&self.settings.github_api, token);
        match self.github.send(&request).await {
            Ok(response) if response.ok() || response.status == 401 => {}
            Ok(response) => {
                worker::console_warn!("revoke token: GitHub answered {}", response.status);
            }
            Err(err) => worker::console_warn!("revoke token: {err}"),
        }
    }

    /// Sends the check run update of each job that is due, one request
    /// per job. Stops when GitHub throttles.
    async fn flush(&self, run: &mut Loaded) -> Result<()> {
        let settings = &self.settings;
        let tokens = Tokens::new(
            Kv::new(&self.env),
            App::new(
                settings.github_api.clone(),
                settings.app_id.clone(),
                settings.app_key.clone(),
            ),
            &self.github,
            self.now,
        );
        for job in run.outbox.due(self.now) {
            let Some(generation) = run.outbox.generation(job) else {
                continue;
            };
            let mut state = self.job_state(job).await?;
            let Some((check, sent_count)) = self.render_job(run, job, &state) else {
                run.outbox_mut().sent(job, generation);
                continue;
            };
            match self.deliver(&tokens, run, job, &mut state, &check).await {
                Delivery::Done => {
                    state.file_annotations_sent = sent_count;
                    let with_file = state
                        .annotations
                        .iter()
                        .filter(|a| a.file.is_some())
                        .count()
                        .min(MAX_TOTAL_ANNOTATIONS);
                    self.put_job(job, &state).await?;
                    let outbox = run.outbox_mut();
                    outbox.sent(job, generation);
                    // More annotations wait for the next request.
                    if sent_count < with_file {
                        outbox.mark(job);
                    }
                }
                Delivery::Wait(seconds) => {
                    run.outbox_mut().throttled(self.now, seconds);
                    break;
                }
                Delivery::Drop => run.outbox_mut().sent(job, generation),
            }
        }
        Ok(())
    }

    /// The check run request for `job` and the count of annotations
    /// with a file GitHub has once it is sent.
    fn render_job(&self, run: &Loaded, job: u64, state: &JobState) -> Option<(CheckRun, usize)> {
        let node = run.graph.node(job)?;
        let view = JobView {
            workflow: &run.info.workflow,
            node,
            head_sha: &run.info.sha,
            details_url: details_url(
                &self.settings.app_url,
                &run.info.repo.full_name,
                run.info.run_id,
                job,
            ),
            steps: &state.steps,
            annotations: &state.annotations,
            file_annotations_sent: state.file_annotations_sent,
            failed_tail: None,
            started_at: state.started_at,
            completed_at: state.completed_at,
        };
        Some(render(&view))
    }

    /// Creates or updates the job's check run, and keeps the id GitHub
    /// answers with in `state`.
    async fn deliver(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        run: &Loaded,
        job: u64,
        state: &mut JobState,
        check: &CheckRun,
    ) -> Delivery {
        let api = &self.settings.github_api;
        let repo = &run.info.repo.full_name;
        let installation = run.info.installation_id;
        let response = match state.check_run_id {
            None => {
                tokens
                    .send(installation, |t| create_check_run(api, repo, t, check))
                    .await
            }
            Some(id) => {
                tokens
                    .send(installation, |t| update_check_run(api, repo, id, t, check))
                    .await
            }
        };
        let attempt = run.outbox.attempt();
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                worker::console_warn!("run {}: check run of job {job}: {err}", run.info.run_id);
                return Delivery::Wait(backoff(attempt));
            }
        };
        match delivery(&response, self.now, attempt) {
            Delivery::Done if state.check_run_id.is_none() => match check_run_id(&response) {
                Ok(id) => {
                    state.check_run_id = Some(id);
                    Delivery::Done
                }
                Err(err) => {
                    worker::console_error!(
                        "run {}: check run of job {job}: {err}",
                        run.info.run_id
                    );
                    Delivery::Drop
                }
            },
            Delivery::Drop => {
                worker::console_error!(
                    "run {}: check run of job {job}: GitHub answered {}: {}",
                    run.info.run_id,
                    response.status,
                    response.text()
                );
                Delivery::Drop
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use hatchrun_workflow::agent::AgentEvent;
    use hatchrun_workflow::{AnnotationLevel, StepSummary, compile, parse};

    use super::*;

    const API: &str = "https://api.github.com";

    fn info() -> RunInfo {
        RunInfo {
            run_id: 7,
            run_number: 3,
            run_attempt: 1,
            tenant_id: "t".repeat(16),
            installation_id: 99,
            repo: Repo {
                id: 5,
                full_name: "octo/app".to_owned(),
                default_branch: "main".to_owned(),
                private: false,
            },
            workflow: "ci".to_owned(),
            event_name: "push".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            sha: "abc".to_owned(),
            actor: "octocat".to_owned(),
            head_ref: String::new(),
            base_ref: String::new(),
        }
    }

    fn graph() -> Graph {
        let config = parse(
            "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [t, { t: { name: b, requires: [t] } }] } }
",
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    fn summary(name: &str) -> StepSummary {
        StepSummary {
            name: name.to_owned(),
            outcome: Conclusion::Success,
            conclusion: Conclusion::Success,
            duration_ms: 1000,
        }
    }

    fn batch(seq: u64, events: Vec<AgentEvent>) -> Batch {
        Batch { seq, events }
    }

    #[test]
    fn github_context_for_a_push() {
        let event = json!({ "commits": [] });
        let ctx = github_context(&info(), &event, API);
        assert_eq!(ctx["ref"], "refs/heads/main");
        assert_eq!(ctx["ref_name"], "main");
        assert_eq!(ctx["ref_type"], "branch");
        assert_eq!(ctx["sha"], "abc");
        assert_eq!(ctx["event_name"], "push");
        assert_eq!(ctx["repository"], "octo/app");
        assert_eq!(ctx["event"], event);
        assert!(ctx.get("token").is_none());
    }

    #[test]
    fn tags_and_pull_requests_have_their_own_ref_names() {
        assert_eq!(ref_parts("refs/tags/v1"), ("v1", "tag"));
        assert_eq!(ref_parts("refs/pull/5/merge"), ("5/merge", "branch"));
    }

    #[test]
    fn github_struct_carries_the_token() {
        let github = github_struct(&info(), API, "t");
        assert_eq!(github.token, "t");
        assert_eq!(github.repository_owner, "octo");
        assert_eq!(github.server_url, "https://github.com");
        assert_eq!(github.api_url, API);
        assert_eq!(github.graphql_url, format!("{API}/graphql"));
    }

    #[test]
    fn job_document_for_a_claim() {
        let graph = graph();
        let event = json!({ "after": "abc" });
        let doc = job_document(&info(), &event, &graph, 1, "tok", API).unwrap();
        assert_eq!(doc.run_id, "7");
        assert_eq!(doc.run_attempt, 1);
        assert_eq!(doc.github.token, "tok");
        assert_eq!(doc.event, event);
        assert_eq!(doc.job.id, graph.node(1).unwrap().job.id);
        assert_eq!(doc.needs, graph.needs(1));
        assert!(doc.secrets.is_empty() && doc.vars.is_empty() && doc.timings.is_empty());
        assert!(doc.cache.is_none());
        assert!(job_document(&info(), &event, &graph, 404, "tok", API).is_none());
    }

    #[test]
    fn next_alarm_is_the_earliest_deadline() {
        let mut outbox = Outbox::default();
        outbox.mark(1);
        outbox.throttled(0, 50);
        let deadlines = Deadlines {
            escalate_at: Some(40_000),
            upload_retry_at: Some(60_000),
            purge_at: None,
        };
        assert_eq!(next_alarm(&deadlines, &outbox, 0), Some(40_000));
        let later = Deadlines {
            escalate_at: Some(70_000),
            ..deadlines
        };
        assert_eq!(next_alarm(&later, &outbox, 0), Some(50_000));
        assert_eq!(
            next_alarm(&Deadlines::default(), &Outbox::default(), 5),
            None
        );
    }

    #[test]
    fn due_parts_are_the_ones_whose_time_came() {
        let mut outbox = Outbox::default();
        outbox.mark(1);
        let deadlines = Deadlines {
            escalate_at: Some(100),
            upload_retry_at: Some(200),
            purge_at: Some(300),
        };
        let due = due(&deadlines, &outbox, 150);
        assert!(due.flush && due.escalate && !due.upload && !due.purge);
    }

    #[test]
    fn fresh_state_after_rerun_takes_seq_one() {
        let mut state = JobState {
            last_seq: 9,
            ..JobState::default()
        };
        assert!(!apply_steps(&mut state, &batch(1, vec![])).fresh);
        let mut fresh = JobState::default();
        assert!(apply_steps(&mut fresh, &batch(1, vec![])).fresh);
        assert_eq!(fresh.last_seq, 1);
    }

    #[test]
    fn repeated_batch_does_not_finish_a_step_twice() {
        let mut state = JobState::default();
        let b = batch(
            3,
            vec![
                AgentEvent::StepStarted {
                    index: 0,
                    name: "build".to_owned(),
                },
                AgentEvent::StepFinished {
                    index: 0,
                    summary: summary("build"),
                },
                AgentEvent::Annotation {
                    annotation: Annotation {
                        level: AnnotationLevel::Error,
                        message: "m".to_owned(),
                        title: None,
                        file: None,
                        line: None,
                        end_line: None,
                        col: None,
                        end_column: None,
                    },
                },
            ],
        );
        assert_eq!(apply_steps(&mut state, &b).steps_finished, 1);
        let again = apply_steps(&mut state, &b);
        assert_eq!((again.fresh, again.steps_finished), (false, 0));
        assert_eq!(state.steps.len(), 1);
        assert_eq!(state.annotations.len(), 1);
        assert!(state.steps[0].summary.is_some());
    }

    #[test]
    fn a_step_index_far_beyond_the_plan_is_ignored() {
        let mut state = JobState::default();
        let b = batch(
            1,
            vec![AgentEvent::StepStarted {
                index: usize::MAX / 2,
                name: "x".to_owned(),
            }],
        );
        assert!(apply_steps(&mut state, &b).fresh);
        assert_eq!(state.steps, []);
    }

    #[test]
    fn a_result_replaces_steps_and_fills_missing_annotations() {
        let mut state = JobState::default();
        let result = JobResult {
            conclusion: Conclusion::Failure,
            outputs: std::collections::BTreeMap::new(),
            steps: vec![summary("a"), summary("b")],
            annotations: vec![Annotation {
                level: AnnotationLevel::Notice,
                message: "n".to_owned(),
                title: None,
                file: None,
                line: None,
                end_line: None,
                col: None,
                end_column: None,
            }],
        };
        apply_result(&mut state, &result);
        assert_eq!(state.steps.len(), 2);
        assert_eq!(state.annotations.len(), 1);
        apply_result(&mut state, &result);
        assert_eq!(state.annotations.len(), 1);
        assert_eq!(steps_json(&state.steps).as_array().unwrap().len(), 2);
    }

    #[test]
    fn heartbeat_written_every_thirty_seconds() {
        assert!(!should_write_heartbeat(1_000, 30_999));
        assert!(should_write_heartbeat(1_000, 31_000));
        assert!(should_write_heartbeat(0, 1_792_000_000_000));
    }

    #[test]
    fn a_large_event_loses_its_commits() {
        let big = "x".repeat(MAX_EVENT_BYTES);
        let event = json!({ "ref": "r", "commits": [{ "message": big }] });
        assert_eq!(bounded_event(event), json!({ "ref": "r", "commits": [] }));
        let small = json!({ "commits": [{ "message": "m" }] });
        assert_eq!(bounded_event(small.clone()), small);
    }

    #[test]
    fn rerun_resets_the_jobs_whose_attempt_grew() {
        let mut g = graph();
        let before = attempts(&g);
        assert_eq!(reset_jobs(&before, &g), []);
        let ctx = json!({ "ref": "refs/heads/main", "event": {} });
        let result = JobResult {
            conclusion: Conclusion::Failure,
            outputs: std::collections::BTreeMap::new(),
            steps: vec![],
            annotations: vec![],
        };
        let _ = g.advance(&ctx);
        g.running(1);
        let _ = g.finished(1, result, &ctx);
        let before = attempts(&g);
        let _ = g.rerun(2, &ctx);
        assert_eq!(reset_jobs(&before, &g), vec![(2, 2)]);
    }

    #[test]
    fn snapshot_waits_for_the_outbox_and_uploads() {
        let mut g = graph();
        let ctx = json!({ "ref": "refs/heads/main", "event": {} });
        let _ = g.advance(&ctx);
        let quiet = (Outbox::default(), Deadlines::default());
        assert!(!snapshot_ready(&g, &quiet.0, &quiet.1), "jobs still run");
        let failed = |g: &mut Graph, id: u64| {
            let _ = g.failed(id, "m".to_owned(), &ctx);
        };
        failed(&mut g, 1);
        failed(&mut g, 2);
        assert!(g.conclusion().is_some());
        assert!(snapshot_ready(&g, &quiet.0, &quiet.1));
        let mut marked = Outbox::default();
        marked.mark(1);
        assert!(!snapshot_ready(&g, &marked, &quiet.1));
        let pending = Deadlines {
            upload_retry_at: Some(1),
            ..Deadlines::default()
        };
        assert!(!snapshot_ready(&g, &quiet.0, &pending));
    }

    #[test]
    fn github_answers_decide_what_happens_to_a_check_run_request() {
        let answer = |status: u16, headers: &[(&str, &str)]| HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
        };
        assert_eq!(delivery(&answer(200, &[]), 0, 0), Delivery::Done);
        assert_eq!(
            delivery(&answer(429, &[("retry-after", "7")]), 0, 0),
            Delivery::Wait(7)
        );
        assert_eq!(delivery(&answer(502, &[]), 0, 0), Delivery::Wait(60));
        assert_eq!(delivery(&answer(502, &[]), 0, 1), Delivery::Wait(120));
        assert_eq!(delivery(&answer(422, &[]), 0, 0), Delivery::Drop);
    }

    #[test]
    fn run_state_lists_each_job_with_its_steps() {
        let g = graph();
        let state = JobState {
            check_run_id: Some(44),
            steps: vec![StepView {
                index: 0,
                name: "s".to_owned(),
                summary: None,
            }],
            ..JobState::default()
        };
        let all = run_state(&info(), &g, &[(1, state)]);
        assert_eq!(all.jobs.len(), 2);
        assert_eq!(all.jobs[0].check_run_id, Some(44));
        assert_eq!(all.jobs[0].steps.len(), 1);
        assert_eq!(all.jobs[1].steps, []);
    }

    #[test]
    fn the_snapshot_key_names_tenant_and_run() {
        assert_eq!(snapshot_key("abc", 12), "state/abc/12.json");
    }
}
