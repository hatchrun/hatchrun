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

use std::cell::{Cell, OnceCell};
use std::time::Duration;

use async_lock::Mutex;
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
    self, CheckRun, Output, check_run_id, create_check_run, revoke_token, update_check_run,
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
/// Check run requests of a job that may fail in a row before its update
/// is dropped.
const MAX_SEND_TRIES: u32 = 5;
/// How long side effects that were not done wait before the alarm tries
/// them, in milliseconds.
const TODO_RETRY_MS: u64 = 30 * 1000;
/// How long an object whose alarm failed to plan waits, in milliseconds.
const ALARM_FALLBACK_MS: u64 = 60 * 1000;
/// How long an object restored for a read or a rerun waits before it
/// purges itself, in milliseconds.
const RESTORED_KEEP_MS: u64 = 10 * 60 * 1000;
/// Storage keys.
const INFO: &str = "info";
const GRAPH: &str = "graph";
const OUTBOX: &str = "outbox";
const DEADLINES: &str = "deadlines";
const EVENT: &str = "event";
const TODO: &str = "todo";

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

/// The R2 key of the state of run `run`, `None` for a tenant id that
/// is empty or could name another key.
#[must_use]
pub fn restore_key(run: Option<u64>, tenant: Option<&str>) -> Option<String> {
    let tenant = tenant.filter(|t| !t.is_empty() && !t.contains(['/', '.']))?;
    Some(snapshot_key(tenant, run?))
}

/// Whether `info` is run `run` of `tenant`.
pub(crate) fn is_run(info: &RunInfo, run: u64, tenant: &str) -> bool {
    info.run_id == run && info.tenant_id == tenant
}

/// The run and tenant a request that can reach an empty object names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ident {
    run: u64,
    tenant: String,
}

impl Ident {
    fn from_query(run: Option<u64>, tenant: Option<String>) -> Option<Self> {
        let tenant = tenant?;
        restore_key(run, Some(&tenant))?;
        Some(Self { run: run?, tenant })
    }
}

/// What an object holds when a request names a run.
enum Held {
    /// This run, or its snapshot loaded back.
    Run,
    /// Neither state nor snapshot.
    Missing,
    /// Another run.
    Other,
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
    /// Check run requests for this attempt that failed in a row.
    pub send_failures: u32,
}

/// Everything the object's single alarm waits for, in Unix
/// milliseconds. The outbox's own deadline is in [`Outbox`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deadlines {
    /// When cancelled jobs that have not stopped are told to stop hard.
    pub escalate_at: Option<u64>,
    /// When uploads that failed are tried again.
    pub upload_retry_at: Option<u64>,
    /// When the ended run's state is written to R2.
    pub snapshot_at: Option<u64>,
    /// When the object deletes its storage.
    pub purge_at: Option<u64>,
    /// When the side effects still in the [`Todo`] list are tried
    /// again.
    pub todo_retry_at: Option<u64>,
    /// Alarms in a row in which a part failed; sets the backoff.
    pub failures: u32,
}

/// A side effect on Postgres or the launch queue. Each one can be
/// repeated: the object stores the list before it acts, drops an item
/// when it is done and tries the rest again after a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Work {
    /// The run's row leaves `queued`.
    StartRun,
    /// Queue attempt `attempt` of the job, and its launch, unless the
    /// job already moved on.
    QueueLaunch {
        /// The job.
        job: u64,
        /// The attempt.
        attempt: u32,
    },
    /// The job's row ends.
    FinishJob {
        /// The job.
        job: u64,
        /// The attempt that ended.
        attempt: u32,
        /// How it ended.
        conclusion: Conclusion,
        /// The steps that ended, as the row keeps them.
        steps: Value,
    },
    /// The run's row ends.
    FinishRun {
        /// The attempt that ended.
        attempt: u32,
        /// How it ended.
        conclusion: Conclusion,
    },
    /// The job's row is set up for `attempt`.
    ResetJob {
        /// The job.
        job: u64,
        /// The new attempt.
        attempt: u32,
    },
    /// The run's row opens for `attempt`.
    ReopenRun {
        /// The new attempt.
        attempt: u32,
    },
}

/// One item of the [`Todo`] list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// Names the item, so that finishing one never drops another.
    pub id: u64,
    /// What to do.
    pub work: Work,
}

/// The side effects still to do, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    next: u64,
    /// The items, oldest first.
    pub items: Vec<Pending>,
}

impl Todo {
    /// Adds `work` at the end.
    pub fn push(&mut self, work: Work) {
        self.next += 1;
        self.items.push(Pending {
            id: self.next,
            work,
        });
    }

    /// Drops the item `id`, if it is still there.
    pub fn remove(&mut self, id: u64) {
        self.items.retain(|item| item.id != id);
    }

    /// Whether nothing is left to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Sets or clears the retry deadline of the todo list: items that are
/// left need a wake-up in case the request that added them never got to
/// do them.
pub(crate) fn arm_todo(deadlines: &mut Deadlines, todo: &Todo, now_ms: u64) {
    if todo.is_empty() {
        deadlines.todo_retry_at = None;
    } else if deadlines.todo_retry_at.is_none() {
        deadlines.todo_retry_at = Some(now_ms + TODO_RETRY_MS);
    }
}

/// What a request from an agent finds when it names a job and the
/// attempt its token was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The run has no such job.
    Missing,
    /// The token is for another attempt.
    Stale,
    /// The job is not waiting for an agent or running.
    Ended,
    /// A launch is queued, no agent has the job.
    Queued,
    /// An agent has the job.
    Running,
}

pub(crate) fn gate(node: Option<&Node>, attempt: u32) -> Gate {
    let Some(node) = node else {
        return Gate::Missing;
    };
    if node.attempt != attempt {
        return Gate::Stale;
    }
    match node.state {
        NodeState::Queued => Gate::Queued,
        NodeState::Running => Gate::Running,
        NodeState::Waiting | NodeState::Done(_) => Gate::Ended,
    }
}

/// Whether the row of a job that `start_job` did not move is already
/// running for `attempt`, which is what a repeated claim finds.
pub(crate) fn running_for(status: &str, row_attempt: u32, attempt: u32) -> bool {
    status == "running" && row_attempt == attempt
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
        deadlines.snapshot_at,
        deadlines.purge_at,
        deadlines.todo_retry_at,
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
    pub(crate) snapshot: bool,
    pub(crate) purge: bool,
    pub(crate) todo: bool,
}

pub(crate) fn due(deadlines: &Deadlines, outbox: &Outbox, now_ms: u64) -> Due {
    let reached = |at: Option<u64>| at.is_some_and(|at| at <= now_ms);
    Due {
        flush: !outbox.due(now_ms).is_empty(),
        escalate: reached(deadlines.escalate_at),
        upload: reached(deadlines.upload_retry_at),
        snapshot: reached(deadlines.snapshot_at),
        purge: reached(deadlines.purge_at),
        todo: reached(deadlines.todo_retry_at),
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
    if !result.steps.is_empty() {
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
    }
    if state.annotations.is_empty() {
        state.annotations = result
            .annotations
            .iter()
            .take(MAX_STORED_ANNOTATIONS)
            .cloned()
            .collect();
    }
}

/// Takes the right to drain the todo list: `true` for the first caller,
/// `false` while another holds it.
pub(crate) fn take_drain(flag: &Cell<bool>) -> bool {
    !flag.replace(true)
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
/// every check run is sent, no side effect or upload waits and no purge
/// is set.
pub(crate) fn snapshot_ready(
    graph: &Graph,
    outbox: &Outbox,
    deadlines: &Deadlines,
    todo: &Todo,
) -> bool {
    graph.conclusion().is_some()
        && outbox.is_empty()
        && todo.is_empty()
        && deadlines.upload_retry_at.is_none()
        && deadlines.purge_at.is_none()
}

/// How GitHub's answer to a check run request changes what is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// It went through.
    Done,
    /// GitHub throttles: try again after this many seconds.
    Wait(u64),
    /// GitHub refuses the request (a 4xx that is not throttling). A
    /// smaller request may go through.
    Rejected,
    /// A server error, a refused token or no answer.
    Failed,
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
        return Delivery::Failed;
    }
    Delivery::Rejected
}

/// The characters of the summary a minimal update keeps.
const MINIMAL_SUMMARY_CHARS: usize = 500;

/// `check` with only what ends a check run: its status, conclusion and
/// times, a short summary and no annotations. Sent once when GitHub
/// refuses the whole request, so a check run does not stay
/// `in_progress` for a bad annotation or an oversized body.
pub(crate) fn minimal_check(check: &CheckRun) -> CheckRun {
    CheckRun {
        output: check.output.as_ref().map(|output| Output {
            title: output.title.clone(),
            summary: output.summary.chars().take(MINIMAL_SUMMARY_CHARS).collect(),
            text: None,
            annotations: Vec::new(),
        }),
        ..check.clone()
    }
}

/// What came of sending one job's check run request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flushed {
    /// It went through. `new_check_run` is the id when it was a create;
    /// `annotations_sent` is the count of annotations with a file that
    /// GitHub has now.
    Sent {
        new_check_run: Option<u64>,
        annotations_sent: usize,
    },
    /// GitHub throttles us for this many seconds.
    Throttled(u64),
    /// The request did not go through, however it was tried.
    Failed,
}

/// Whether the job is still at the attempt a request was planned for.
pub(crate) fn plan_is_current(node: Option<&Node>, planned: u32) -> bool {
    node.is_some_and(|node| node.attempt == planned)
}

/// Records what happened to the request of `job`, made for the mark of
/// `generation`. Returns whether the flush goes on with the next job:
/// not after a throttle or a failure that backs off.
///
/// A failure keeps the mark and backs off. After [`MAX_SEND_TRIES`]
/// failures in a row the mark is dropped, which the caller logs.
pub(crate) fn record_flush(
    state: &mut JobState,
    outbox: &mut Outbox,
    job: u64,
    generation: u64,
    flushed: &Flushed,
    now_ms: u64,
) -> bool {
    match flushed {
        Flushed::Sent {
            new_check_run,
            annotations_sent,
        } => {
            if let Some(id) = new_check_run {
                state.check_run_id = Some(*id);
            }
            state.file_annotations_sent = *annotations_sent;
            state.send_failures = 0;
            outbox.sent(job, generation);
            // More annotations wait for the next request.
            if *annotations_sent < file_annotations(state) {
                outbox.mark(job);
            }
            true
        }
        Flushed::Throttled(seconds) => {
            outbox.throttled(now_ms, *seconds);
            false
        }
        Flushed::Failed => {
            state.send_failures += 1;
            if state.send_failures >= MAX_SEND_TRIES {
                state.send_failures = 0;
                outbox.sent(job, generation);
                return true;
            }
            // Blocks every job's updates for a while, which is what a
            // GitHub that fails for one is likely doing for all.
            outbox.throttled(now_ms, backoff(state.send_failures - 1));
            false
        }
    }
}

/// How many annotations with a file the job's check run can get.
pub(crate) fn file_annotations(state: &JobState) -> usize {
    state
        .annotations
        .iter()
        .filter(|a| a.file.is_some())
        .count()
        .min(MAX_TOTAL_ANNOTATIONS)
}

/// Which parts of the alarm failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Failed {
    pub(crate) flush: bool,
    pub(crate) escalate: bool,
    pub(crate) upload: bool,
    pub(crate) todo: bool,
    pub(crate) purge: bool,
    pub(crate) snapshot: bool,
}

/// Sets the deadlines of the parts that failed to `backoff` from now
/// and counts the failure; an alarm in which nothing failed resets the
/// count.
pub(crate) fn reschedule(
    deadlines: &mut Deadlines,
    outbox: &mut Outbox,
    failed: Failed,
    now_ms: u64,
) {
    let any = failed.flush
        || failed.escalate
        || failed.upload
        || failed.todo
        || failed.purge
        || failed.snapshot;
    if !any {
        deadlines.failures = 0;
        return;
    }
    let wait = backoff(deadlines.failures);
    let at = now_ms + wait * 1000;
    if failed.escalate {
        deadlines.escalate_at = Some(at);
    }
    if failed.upload {
        deadlines.upload_retry_at = Some(at);
    }
    if failed.todo {
        deadlines.todo_retry_at = Some(at);
    }
    if failed.purge {
        deadlines.purge_at = Some(at);
    }
    if failed.snapshot {
        deadlines.snapshot_at = Some(at);
    }
    if failed.flush {
        outbox.throttled(now_ms, wait);
    }
    deadlines.failures += 1;
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
    attempt: u32,
}

#[derive(Deserialize)]
struct ClaimBody {
    token: String,
}

#[derive(Deserialize)]
struct FailedBody {
    message: String,
}

/// What a handler works on: the pieces of state every handler reads,
/// and which of them it changed.
struct Loaded {
    info: RunInfo,
    graph: Graph,
    outbox: Outbox,
    deadlines: Deadlines,
    todo: Todo,
    dirty: [bool; 5],
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

    fn todo_mut(&mut self) -> &mut Todo {
        self.dirty[4] = true;
        &mut self.todo
    }
}

// The macro that exports the object generates methods without docs.
mod object {
    #![allow(missing_docs)]

    use std::cell::{Cell, OnceCell};

    use async_lock::Mutex;
    use worker::{DurableObject, Env, Request, Response, Result, State, durable_object};

    use super::{Cx, Ident, Settings, fail};

    /// One run.
    #[durable_object]
    pub struct RunObject {
        state: State,
        env: Env,
        /// Handlers take turns on the state in storage. The lock is
        /// held to read and change it and never across a call to
        /// GitHub, Postgres or the queue.
        turn: Mutex<()>,
        settings: OnceCell<Settings>,
        /// Whether a request or the alarm is running the todo list.
        draining: Cell<bool>,
    }

    impl RunObject {
        fn cx(&self) -> Result<Cx<'_>> {
            if self.settings.get().is_none() {
                let settings = Settings::read(&self.env).map_err(fail)?;
                let _ = self.settings.set(settings);
            }
            let settings = self
                .settings
                .get()
                .ok_or_else(|| fail("settings are not read"))?;
            Ok(Cx::new(
                &self.state,
                &self.env,
                &self.turn,
                &self.draining,
                settings,
            ))
        }
    }

    impl DurableObject for RunObject {
        fn new(state: State, env: Env) -> Self {
            Self {
                state,
                env,
                turn: Mutex::new(()),
                settings: OnceCell::new(),
                draining: Cell::new(false),
            }
        }

        async fn fetch(&self, mut req: Request) -> Result<Response> {
            let url = req.url()?;
            let number = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, value)| value.parse::<u64>().ok())
            };
            let (job, attempt) = (number("job"), number("attempt"));
            let tenant = url
                .query_pairs()
                .find(|(key, _)| key == "tenant")
                .map(|(_, value)| value.into_owned());
            self.cx()?
                .route(
                    &mut req,
                    url.path(),
                    job,
                    attempt.and_then(|a| u32::try_from(a).ok()),
                    Ident::from_query(number("run"), tenant),
                )
                .await
        }

        async fn alarm(&self) -> Result<Response> {
            self.cx()?.alarm().await;
            Response::empty()
        }
    }
}

pub use object::RunObject;

/// One request or alarm: the clock, the settings and the connections.
struct Cx<'a> {
    storage: Storage,
    env: Env,
    turn: &'a Mutex<()>,
    draining: &'a Cell<bool>,
    settings: &'a Settings,
    now: u64,
    db: OnceCell<Db>,
    github: Counted,
}

/// One check run request, decided under the lock and sent without it.
struct Planned {
    job: u64,
    generation: u64,
    check: CheckRun,
    /// The count of annotations with a file GitHub has once the whole
    /// request is sent.
    sent_count: usize,
    /// The count it has once a minimal request is sent.
    file_total: usize,
    check_run_id: Option<u64>,
    /// The job's attempt when the request was planned.
    node_attempt: u32,
    installation: u64,
    repo: String,
    attempt: u32,
}

impl<'a> Cx<'a> {
    fn new(
        state: &State,
        env: &Env,
        turn: &'a Mutex<()>,
        draining: &'a Cell<bool>,
        settings: &'a Settings,
    ) -> Self {
        Self {
            storage: state.storage(),
            env: env.clone(),
            turn,
            draining,
            settings,
            now: now_ms(),
            db: OnceCell::new(),
            github: Counted::default(),
        }
    }

    async fn db(&self) -> Result<&Db> {
        if self.db.get().is_none() {
            let db = Db::connect(&self.env).await?;
            // Nothing else sets it: this request is the only user.
            let _ = self.db.set(db);
        }
        self.db.get().ok_or_else(|| fail("no database"))
    }

    async fn route(
        &self,
        req: &mut Request,
        path: &str,
        job: Option<u64>,
        attempt: Option<u32>,
        ident: Option<Ident>,
    ) -> Result<Response> {
        let method = req.method();
        match (&method, path) {
            (Method::Post, "/start") => {
                return match req.json::<Start>().await {
                    Ok(start) => self.start(start).await,
                    Err(_) => Response::error("bad start", 400),
                };
            }
            (Method::Post, "/purge") => return self.purge().await,
            (Method::Post, "/cancel") => return self.cancel().await,
            (Method::Get, "/state") => {
                let Some(ident) = ident else {
                    return Response::error("a run and a tenant are needed", 400);
                };
                return match self.restore_if_needed(&ident).await? {
                    Held::Run => self.state().await,
                    Held::Missing => answer(404),
                    Held::Other => answer(409),
                };
            }
            _ => {}
        }
        let Some(job) = job else {
            return Response::error("a job is needed", 400);
        };
        match (&method, path) {
            (Method::Post, "/failed") => match req.json::<FailedBody>().await {
                Ok(body) => {
                    let Some(attempt) = attempt else {
                        return Response::error("an attempt is needed", 400);
                    };
                    self.failed(job, attempt, body.message).await
                }
                Err(_) => Response::error("bad failure", 400),
            },
            (Method::Post, "/rerun") => {
                let Some(ident) = ident else {
                    return Response::error("a run and a tenant are needed", 400);
                };
                match self.restore_if_needed(&ident).await? {
                    Held::Run => self.rerun(job).await,
                    Held::Missing => answer(404),
                    Held::Other => answer(409),
                }
            }
            (Method::Post, "/claim" | "/events" | "/result") => {
                let Some(attempt) = attempt else {
                    return Response::error("an attempt is needed", 400);
                };
                match path {
                    "/claim" => match req.json::<ClaimBody>().await {
                        Ok(body) => self.claim(job, attempt, body.token).await,
                        Err(_) => Response::error("bad claim", 400),
                    },
                    "/events" => match req.json::<Batch>().await {
                        Ok(batch) => self.events(job, attempt, &batch).await,
                        Err(_) => Response::error("bad batch", 400),
                    },
                    _ => match req.json::<JobResult>().await {
                        Ok(result) => self.result(job, attempt, result).await,
                        Err(_) => Response::error("bad result", 400),
                    },
                }
            }
            _ => Response::error("not found", 404),
        }
    }

    /// Runs each due part of the alarm in its own scope, then sets the
    /// alarm for what is left. A part that fails is tried again after a
    /// backoff; planning the next alarm falls back to a fixed wait when
    /// even that fails.
    async fn alarm(&self) {
        if let Err(err) = self.run_alarm().await {
            worker::console_error!("alarm: {err}");
            if let Err(err) = self
                .storage
                .set_alarm(Duration::from_millis(ALARM_FALLBACK_MS))
                .await
            {
                worker::console_error!("alarm: fallback: {err}");
            }
        }
    }

    async fn run_alarm(&self) -> Result<()> {
        let due = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return self.storage.delete_alarm().await;
            };
            due(&run.deadlines, &run.outbox, self.now)
        };
        let mut failed = Failed::default();
        if due.purge {
            match self.purge_due().await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => {
                    worker::console_error!("alarm: purge: {err}");
                    failed.purge = true;
                }
            }
        }
        if due.flush
            && let Err(err) = self.flush().await
        {
            worker::console_error!("alarm: check runs: {err}");
            failed.flush = true;
        }
        if due.escalate {
            let done = self
                .update(|run| {
                    run.deadlines_mut().escalate_at = None;
                    // The reply to the agent's next batch carries the
                    // new count.
                    run.graph_mut().escalate();
                })
                .await;
            if let Err(err) = done {
                worker::console_error!("alarm: escalate: {err}");
                failed.escalate = true;
            }
        }
        if due.todo
            && let Err(err) = self.drain().await
        {
            worker::console_error!("alarm: side effects: {err}");
            failed.todo = true;
        }
        if due.snapshot
            && let Err(err) = self.snapshot_now().await
        {
            worker::console_error!("alarm: snapshot: {err}");
            failed.snapshot = true;
        }
        if due.upload {
            // Uploads of finished logs retry here (Task 10).
            let done = self
                .update(|run| run.deadlines_mut().upload_retry_at = None)
                .await;
            if let Err(err) = done {
                worker::console_error!("alarm: uploads: {err}");
                failed.upload = true;
            }
        }
        let now = self.now;
        self.update(|run| {
            let Loaded {
                deadlines, outbox, ..
            } = run;
            reschedule(deadlines, outbox, failed, now);
            run.dirty[2] = true;
            run.dirty[3] = true;
        })
        .await
    }

    /// Reads the state, lets `change` change it and writes it, with the
    /// lock held throughout.
    async fn update(&self, change: impl FnOnce(&mut Loaded)) -> Result<()> {
        let _turn = self.turn.lock().await;
        let Some(mut run) = self.loaded().await? else {
            return Ok(());
        };
        change(&mut run);
        self.commit(&mut run).await
    }

    /// Reads the state. The caller holds the lock.
    async fn loaded(&self) -> Result<Option<Loaded>> {
        let storage = &self.storage;
        let (Some(info), Some(graph)) = (
            storage.get::<RunInfo>(INFO).await?,
            storage.get::<Graph>(GRAPH).await?,
        ) else {
            return Ok(None);
        };
        Ok(Some(Loaded {
            info,
            graph,
            outbox: storage.get(OUTBOX).await?.unwrap_or_default(),
            deadlines: storage.get(DEADLINES).await?.unwrap_or_default(),
            todo: storage.get(TODO).await?.unwrap_or_default(),
            dirty: [false; 5],
        }))
    }

    /// Reads the snapshot of an ended run back into an object that was
    /// purged, unless it holds the run. `ident` comes with the request;
    /// the snapshot is read from R2 without the lock, and what the
    /// object holds, or the snapshot, must be that run.
    async fn restore_if_needed(&self, ident: &Ident) -> Result<Held> {
        {
            let _turn = self.turn.lock().await;
            if let Some(info) = self.storage.get::<RunInfo>(INFO).await?
                && self.storage.get::<Graph>(GRAPH).await?.is_some()
            {
                return Ok(if is_run(&info, ident.run, &ident.tenant) {
                    Held::Run
                } else {
                    Held::Other
                });
            }
        }
        let Some(snapshot) = self.fetch_snapshot(ident).await? else {
            return Ok(Held::Missing);
        };
        if !is_run(&snapshot.info, ident.run, &ident.tenant) {
            return Ok(Held::Other);
        }
        let _turn = self.turn.lock().await;
        if self.storage.get::<Graph>(GRAPH).await?.is_some() {
            return Ok(Held::Run);
        }
        self.storage.put(EVENT, &snapshot.event).await?;
        let mut run = Loaded {
            info: snapshot.info,
            graph: snapshot.graph,
            outbox: Outbox::default(),
            deadlines: Deadlines {
                purge_at: Some(self.now + RESTORED_KEEP_MS),
                ..Deadlines::default()
            },
            todo: Todo::default(),
            dirty: [true; 5],
        };
        self.commit(&mut run).await?;
        Ok(Held::Run)
    }

    async fn fetch_snapshot(&self, ident: &Ident) -> Result<Option<Snapshot>> {
        let Some(key) = restore_key(Some(ident.run), Some(&ident.tenant)) else {
            return Ok(None);
        };
        let Some(object) = self.env.bucket(BUCKET)?.get(key).execute().await? else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        let snapshot = serde_json::from_slice(&body.bytes().await?)
            .map_err(|err| fail(format!("run {}: snapshot: {err}", ident.run)))?;
        Ok(Some(snapshot))
    }

    /// Writes what the handler changed and sets the alarm, which also
    /// wakes the object to snapshot an ended run. The caller holds the lock.
    async fn commit(&self, run: &mut Loaded) -> Result<()> {
        arm_todo(&mut run.deadlines, &run.todo, self.now);
        if run.deadlines.snapshot_at.is_none()
            && snapshot_ready(&run.graph, &run.outbox, &run.deadlines, &run.todo)
        {
            run.deadlines.snapshot_at = Some(self.now);
        }
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
        if run.dirty[4] {
            storage.put(TODO, &run.todo).await?;
        }
        // Not conditional: the code above changes the deadlines.
        storage.put(DEADLINES, &run.deadlines).await?;
        run.dirty = [false; 5];
        match next_alarm(&run.deadlines, &run.outbox, self.now) {
            Some(at) => {
                let wait = Duration::from_millis(at.saturating_sub(self.now));
                storage.set_alarm(wait).await
            }
            None => storage.delete_alarm().await,
        }
    }

    /// Writes the state of an ended run to R2 and sets the purge. The
    /// state is read under the lock, written without it and recorded
    /// under it again. A write that fails is tried again after a minute.
    async fn snapshot_now(&self) -> Result<()> {
        let (key, body, attempt) = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return Ok(());
            };
            if run.deadlines.snapshot_at.is_none()
                || !snapshot_ready(&run.graph, &run.outbox, &run.deadlines, &run.todo)
            {
                return Ok(());
            }
            let snapshot = Snapshot {
                info: run.info,
                graph: run.graph,
                event: self.event().await?,
            };
            let body = serde_json::to_vec(&snapshot).map_err(|err| fail(err.to_string()))?;
            (
                snapshot_key(&snapshot.info.tenant_id, snapshot.info.run_id),
                body,
                snapshot.info.run_attempt,
            )
        };
        let written = async { self.env.bucket(BUCKET)?.put(key, body).execute().await }.await;
        let now = self.now;
        match written {
            Ok(_) => {
                self.update(|run| {
                    // A rerun that came meanwhile, even one that ended
                    // again, made this snapshot stale.
                    if run.graph.conclusion().is_some() && run.info.run_attempt == attempt {
                        let deadlines = run.deadlines_mut();
                        deadlines.snapshot_at = None;
                        deadlines.purge_at = Some(now);
                    }
                })
                .await
            }
            Err(err) => {
                worker::console_error!("snapshot: {err}");
                self.update(|run| run.deadlines_mut().snapshot_at = Some(now + SNAPSHOT_RETRY_MS))
                    .await
            }
        }
    }

    async fn purge(&self) -> Result<Response> {
        let _turn = self.turn.lock().await;
        self.storage.delete_alarm().await?;
        self.storage.delete_all().await?;
        answer(204)
    }

    /// Purges when the deadline is still there. Answers whether it did.
    async fn purge_due(&self) -> Result<bool> {
        let _turn = self.turn.lock().await;
        let deadlines: Deadlines = self.storage.get(DEADLINES).await?.unwrap_or_default();
        if deadlines.purge_at.is_none_or(|at| at > self.now) {
            return Ok(false);
        }
        self.storage.delete_alarm().await?;
        self.storage.delete_all().await?;
        Ok(true)
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

    async fn state(&self) -> Result<Response> {
        let _turn = self.turn.lock().await;
        let Some(run) = self.loaded().await? else {
            return answer(404);
        };
        let mut states = Vec::new();
        for node in &run.graph.jobs {
            states.push((node.id, self.job_state(node.id).await?));
        }
        Response::from_json(&run_state(&run.info, &run.graph, &states))
    }

    /// Does what the todo list says and drops each item once done. The
    /// lock is taken to read and to drop an item, not while one runs.
    ///
    /// One drain runs at a time. A caller that finds one running does
    /// nothing: the running one loops until the list is empty, and the
    /// alarm comes back to anything it missed.
    async fn drain(&self) -> Result<()> {
        if !take_drain(self.draining) {
            return Ok(());
        }
        let done = self.drain_all().await;
        self.draining.set(false);
        done
    }

    async fn drain_all(&self) -> Result<()> {
        loop {
            let (info, pending) = {
                let _turn = self.turn.lock().await;
                let todo: Todo = self.storage.get(TODO).await?.unwrap_or_default();
                let Some(first) = todo.items.first().cloned() else {
                    // Under the lock, so an item added after this check
                    // finds the flag clear.
                    self.draining.set(false);
                    return Ok(());
                };
                let info: RunInfo = self
                    .storage
                    .get(INFO)
                    .await?
                    .ok_or_else(|| fail("side effects without a run"))?;
                (info, first)
            };
            self.perform(&info, &pending.work).await?;
            self.update(|run| run.todo_mut().remove(pending.id)).await?;
        }
    }

    /// Does one side effect. Each can be repeated.
    async fn perform(&self, info: &RunInfo, work: &Work) -> Result<()> {
        let db = self.db().await?;
        match work {
            Work::StartRun => store::run_started(db, info.run_id, self.now).await,
            Work::QueueLaunch { job, attempt } => {
                // A job that is launching or launched is not queued
                // again, and no second launch is sent for it.
                if store::queue_job(db, *job, *attempt, self.now).await? {
                    self.env
                        .queue(LAUNCHES)?
                        .send(LaunchMessage {
                            job_id: *job,
                            attempt: *attempt,
                        })
                        .await?;
                }
                Ok(())
            }
            Work::FinishJob {
                job,
                attempt,
                conclusion,
                steps,
            } => {
                // The key of the finished log comes with the uploads
                // (Task 10).
                store::finish_job(
                    db,
                    *job,
                    *attempt,
                    conclusion_word(*conclusion),
                    steps,
                    None,
                    self.now,
                )
                .await
                .map(|_| ())
            }
            Work::FinishRun {
                attempt,
                conclusion,
            } => {
                store::finish_run(
                    db,
                    info.run_id,
                    *attempt,
                    conclusion_word(*conclusion),
                    self.now,
                )
                .await
            }
            Work::ResetJob { job, attempt } => store::reset_job(db, *job, *attempt, self.now).await,
            Work::ReopenRun { attempt } => {
                store::reopen_run_at(db, info.run_id, *attempt, self.now).await
            }
        }
    }

    /// Turns what the graph decided into storage changes now and side
    /// effects on the todo list, and returns the GitHub tokens to
    /// revoke once the lock is released. The caller holds the lock.
    async fn apply_actions(&self, run: &mut Loaded, actions: Vec<Action>) -> Result<Vec<String>> {
        let mut revoke = Vec::new();
        for action in actions {
            match action {
                Action::Launch(job) => {
                    let attempt = run.graph.node(job).map_or(1, |node| node.attempt);
                    run.todo_mut().push(Work::QueueLaunch { job, attempt });
                }
                Action::Finished { job, conclusion } => {
                    let mut state = self.job_state(job).await?;
                    state.completed_at = Some(self.now);
                    let steps = steps_json(&state.steps);
                    revoke.extend(state.github_token.take());
                    self.put_job(job, &state).await?;
                    run.outbox_mut().mark(job);
                    let attempt = run.graph.node(job).map_or(1, |node| node.attempt);
                    run.todo_mut().push(Work::FinishJob {
                        job,
                        attempt,
                        conclusion,
                        steps,
                    });
                }
                // The agent learns of a cancel from the reply to its
                // next batch.
                Action::Cancel { .. } => {}
                Action::RunDone(conclusion) => {
                    let attempt = run.info.run_attempt;
                    run.todo_mut().push(Work::FinishRun {
                        attempt,
                        conclusion,
                    });
                }
            }
        }
        Ok(revoke)
    }

    /// Carries out what a handler decided, after it released the lock:
    /// the side effects, then the revokes. A failure is logged; the
    /// todo list keeps what is left and the alarm comes back to it.
    async fn after(&self, revoke: Vec<String>) {
        if let Err(err) = self.drain().await {
            worker::console_error!("side effects: {err}");
        }
        if let Err(err) = self.snapshot_now().await {
            worker::console_error!("snapshot: {err}");
        }
        for token in revoke {
            self.revoke(&token).await;
        }
    }

    async fn start(&self, start: Start) -> Result<Response> {
        let Start {
            run: info,
            jobs,
            event,
        } = start;
        let revoke = {
            let _turn = self.turn.lock().await;
            // The graph is written last, so a start that failed halfway
            // runs again.
            if self.storage.get::<Graph>(GRAPH).await?.is_some() {
                let held = self.storage.get::<RunInfo>(INFO).await?;
                return answer(
                    if held.is_some_and(|held| is_run(&held, info.run_id, &info.tenant_id)) {
                        204
                    } else {
                        409
                    },
                );
            }
            let mut run = Loaded {
                graph: Graph::new(jobs),
                outbox: Outbox::default(),
                deadlines: Deadlines::default(),
                todo: Todo::default(),
                info,
                dirty: [true; 5],
            };
            let ids: Vec<u64> = run.graph.jobs.iter().map(|n| n.id).collect();
            for id in ids {
                run.outbox.mark(id);
            }
            self.storage.put(EVENT, &bounded_event(event)).await?;
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph.advance(&ctx);
            run.todo.push(Work::StartRun);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    async fn claim(&self, job: u64, attempt: u32, token: String) -> Result<Response> {
        let first = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return answer(409);
            };
            gate(run.graph.node(job), attempt)
        };
        match first {
            Gate::Missing => return Response::error("no such job", 404),
            Gate::Stale | Gate::Ended => return answer(409),
            Gate::Queued => {
                // The row moves first; a claim that comes again finds it
                // running for this attempt.
                let db = self.db().await?;
                if !store::start_job(db, job, self.now).await? {
                    let running = store::job(db, job)
                        .await?
                        .is_some_and(|row| running_for(&row.status, row.attempt, attempt));
                    if !running {
                        return answer(409);
                    }
                }
            }
            Gate::Running => {}
        }
        let (document, old) = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            let was = run.graph.node(job).map(|node| node.state);
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                Gate::Stale | Gate::Ended => return answer(409),
            }
            run.graph_mut().running(job);
            let mut state = self.job_state(job).await?;
            let old = state
                .github_token
                .replace(token.clone())
                .filter(|old| *old != token);
            if was == Some(NodeState::Queued) || state.started_at.is_none() {
                state.started_at = Some(self.now);
                state.heartbeat_written_at = self.now;
            }
            self.put_job(job, &state).await?;
            let document = job_document(
                &run.info,
                &self.event().await?,
                &run.graph,
                job,
                &token,
                &self.settings.github_api,
            )
            .ok_or_else(|| fail("job vanished"))?;
            self.commit(&mut run).await?;
            (document, old)
        };
        if let Some(old) = old {
            self.revoke(&old).await;
        }
        Response::from_json(&document)
    }

    async fn events(&self, job: u64, attempt: u32, batch: &Batch) -> Result<Response> {
        let (reply, heartbeat) = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                _ => return answer(409),
            }
            let cancel = run.graph.node(job).map_or(0, |node| node.cancel);
            let mut state = self.job_state(job).await?;
            let applied = apply_steps(&mut state, batch);
            let mut changed = applied.fresh;
            if applied.steps_finished > 0 {
                run.outbox_mut().mark(job);
            }
            let heartbeat = should_write_heartbeat(state.heartbeat_written_at, self.now);
            if heartbeat {
                state.heartbeat_written_at = self.now;
                changed = true;
            }
            if changed {
                self.put_job(job, &state).await?;
            }
            if run.dirty.iter().any(|dirty| *dirty) {
                self.commit(&mut run).await?;
            }
            (BatchReply { cancel }, heartbeat)
        };
        if heartbeat {
            // The next one is 30 s away; a miss is not worth failing
            // the batch.
            let written = async { store::heartbeat(self.db().await?, job, self.now).await }.await;
            if let Err(err) = written {
                worker::console_warn!("job {job}: heartbeat: {err}");
            }
        }
        Response::from_json(&reply)
    }

    async fn result(&self, job: u64, attempt: u32, result: JobResult) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                _ => return answer(409),
            }
            let mut state = self.job_state(job).await?;
            apply_result(&mut state, &result);
            self.put_job(job, &state).await?;
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().finished(job, result, &ctx);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    async fn failed(&self, job: u64, attempt: u32, message: String) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(204);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                Gate::Stale | Gate::Ended => return answer(409),
            }
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().failed(job, message, &ctx);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    async fn cancel(&self) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(204);
            };
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().cancel(&ctx);
            if run.graph.conclusion().is_none() {
                let at = self.now + ESCALATE_AFTER_MS;
                run.deadlines_mut().escalate_at.get_or_insert(at);
            }
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    /// A rerun starts every reset job's attempt from nothing: a new
    /// state (so its batches count from 1 again), its old token revoked,
    /// its log chunks dropped and its row set up for the new attempt.
    async fn rerun(&self, job: u64) -> Result<Response> {
        let mut revoke = Vec::new();
        {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(404);
            };
            let ctx = self.github_ctx(&run.info).await?;
            let before = attempts(&run.graph);
            let actions = run.graph_mut().rerun(job, &ctx);
            let reset = reset_jobs(&before, &run.graph);
            if reset.is_empty() {
                return answer(204);
            }
            for (id, attempt) in reset {
                revoke.extend(self.job_state(id).await?.github_token);
                self.put_job(id, &JobState::default()).await?;
                self.delete_prefix(&log_prefix(id)).await?;
                run.todo_mut().push(Work::ResetJob { job: id, attempt });
                run.outbox_mut().mark(id);
            }
            let attempt = run.info.run_attempt + 1;
            run.todo_mut().push(Work::ReopenRun { attempt });
            run.info_mut().run_attempt = attempt;
            let deadlines = run.deadlines_mut();
            deadlines.purge_at = None;
            deadlines.escalate_at = None;
            revoke.extend(self.apply_actions(&mut run, actions).await?);
            self.commit(&mut run).await?;
        }
        self.after(revoke).await;
        answer(204)
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
    /// per job. The requests are planned under the lock, sent without
    /// it, and each answer is recorded under it again. Stops when GitHub
    /// throttles.
    async fn flush(&self) -> Result<()> {
        let plans = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return Ok(());
            };
            let plans = self.plan_flush(&mut run).await?;
            if run.dirty.iter().any(|dirty| *dirty) {
                self.commit(&mut run).await?;
            }
            plans
        };
        let settings = self.settings;
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
        for plan in plans {
            let flushed = self.send(&tokens, &plan).await;
            let go_on = {
                let _turn = self.turn.lock().await;
                let Some(mut run) = self.loaded().await? else {
                    return Ok(());
                };
                // A rerun that came while the request was out has a
                // new state and its own mark; this answer is not its.
                if !plan_is_current(run.graph.node(plan.job), plan.node_attempt) {
                    continue;
                }
                let mut state = self.job_state(plan.job).await?;
                if matches!(flushed, Flushed::Failed) && state.send_failures + 1 >= MAX_SEND_TRIES {
                    worker::console_error!(
                        "run {}: check run of job {}: update dropped after {MAX_SEND_TRIES} tries",
                        run.info.run_id,
                        plan.job
                    );
                }
                let now = self.now;
                let go_on = record_flush(
                    &mut state,
                    run.outbox_mut(),
                    plan.job,
                    plan.generation,
                    &flushed,
                    now,
                );
                self.put_job(plan.job, &state).await?;
                self.commit(&mut run).await?;
                go_on
            };
            if !go_on {
                break;
            }
        }
        Ok(())
    }

    /// The request of each job that is due. A job the graph does not
    /// know has nothing to send and its mark goes.
    async fn plan_flush(&self, run: &mut Loaded) -> Result<Vec<Planned>> {
        let mut plans = Vec::new();
        for job in run.outbox.due(self.now) {
            let Some(generation) = run.outbox.generation(job) else {
                continue;
            };
            let state = self.job_state(job).await?;
            let Some(node) = run.graph.node(job) else {
                run.outbox_mut().sent(job, generation);
                continue;
            };
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
            let (check, sent_count) = render(&view);
            plans.push(Planned {
                job,
                generation,
                check,
                sent_count,
                file_total: file_annotations(&state),
                check_run_id: state.check_run_id,
                node_attempt: node.attempt,
                installation: run.info.installation_id,
                repo: run.info.repo.full_name.clone(),
                attempt: run.outbox.attempt(),
            });
        }
        Ok(plans)
    }

    /// Sends the request of `plan`. GitHub refusing it is answered with
    /// a minimal request once.
    async fn send(&self, tokens: &Tokens<'_, Kv, App, Counted>, plan: &Planned) -> Flushed {
        let Some(response) = self.request(tokens, plan, &plan.check).await else {
            return Flushed::Failed;
        };
        match delivery(&response, self.now, plan.attempt) {
            Delivery::Done => Self::sent(plan, &response, plan.sent_count),
            Delivery::Wait(seconds) => Flushed::Throttled(seconds),
            Delivery::Failed => Flushed::Failed,
            Delivery::Rejected => {
                worker::console_warn!(
                    "check run of job {}: GitHub answered {}: {}",
                    plan.job,
                    response.status,
                    response.text()
                );
                let minimal = minimal_check(&plan.check);
                let Some(response) = self.request(tokens, plan, &minimal).await else {
                    return Flushed::Failed;
                };
                match delivery(&response, self.now, plan.attempt) {
                    Delivery::Done => Self::sent(plan, &response, plan.file_total),
                    Delivery::Wait(seconds) => Flushed::Throttled(seconds),
                    Delivery::Failed | Delivery::Rejected => Flushed::Failed,
                }
            }
        }
    }

    /// A request that went through: the id of a check run just made
    /// must be readable, or the next request would make another.
    fn sent(plan: &Planned, response: &HttpResponse, annotations_sent: usize) -> Flushed {
        if plan.check_run_id.is_some() {
            return Flushed::Sent {
                new_check_run: None,
                annotations_sent,
            };
        }
        match check_run_id(response) {
            Ok(id) => Flushed::Sent {
                new_check_run: Some(id),
                annotations_sent,
            },
            Err(err) => {
                worker::console_error!("check run of job {}: {err}", plan.job);
                Flushed::Failed
            }
        }
    }

    /// Creates or updates the job's check run. `None` when no answer
    /// came.
    async fn request(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        plan: &Planned,
        check: &CheckRun,
    ) -> Option<HttpResponse> {
        let api = &self.settings.github_api;
        let repo = &plan.repo;
        let response = match plan.check_run_id {
            None => {
                tokens
                    .send(plan.installation, |t| create_check_run(api, repo, t, check))
                    .await
            }
            Some(id) => {
                tokens
                    .send(plan.installation, |t| {
                        update_check_run(api, repo, id, t, check)
                    })
                    .await
            }
        };
        match response {
            Ok(response) => Some(response),
            Err(err) => {
                worker::console_warn!("check run of job {}: {err}", plan.job);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::github::GhAnnotation;
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
            ..Deadlines::default()
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
            ..Deadlines::default()
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
        let no_todo = Todo::default();
        let ready = |g: &Graph, o: &Outbox, d: &Deadlines| snapshot_ready(g, o, d, &no_todo);
        assert!(!ready(&g, &quiet.0, &quiet.1), "jobs still run");
        let failed = |g: &mut Graph, id: u64| {
            let _ = g.failed(id, "m".to_owned(), &ctx);
        };
        failed(&mut g, 1);
        failed(&mut g, 2);
        assert!(g.conclusion().is_some());
        assert!(ready(&g, &quiet.0, &quiet.1));
        let mut marked = Outbox::default();
        marked.mark(1);
        assert!(!ready(&g, &marked, &quiet.1));
        let pending = Deadlines {
            upload_retry_at: Some(1),
            ..Deadlines::default()
        };
        assert!(!ready(&g, &quiet.0, &pending));
        let mut busy = Todo::default();
        busy.push(Work::StartRun);
        assert!(!snapshot_ready(&g, &quiet.0, &quiet.1, &busy));
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
        assert_eq!(delivery(&answer(502, &[]), 0, 0), Delivery::Failed);
        assert_eq!(delivery(&answer(401, &[]), 0, 0), Delivery::Failed);
        assert_eq!(delivery(&answer(422, &[]), 0, 0), Delivery::Rejected);
    }

    fn marked(job: u64) -> (JobState, Outbox, u64) {
        let mut outbox = Outbox::default();
        outbox.mark(job);
        let generation = outbox.generation(job).unwrap();
        (JobState::default(), outbox, generation)
    }

    fn file_annotation() -> Annotation {
        Annotation {
            level: AnnotationLevel::Error,
            message: "m".to_owned(),
            title: None,
            file: Some("a.rs".to_owned()),
            line: Some(1),
            end_line: None,
            col: None,
            end_column: None,
        }
    }

    #[test]
    fn a_rejected_update_is_sent_again_without_annotations() {
        let check = CheckRun {
            name: "n".to_owned(),
            head_sha: "s".to_owned(),
            external_id: "e".to_owned(),
            details_url: "u".to_owned(),
            status: "completed".to_owned(),
            started_at: None,
            conclusion: Some("failure".to_owned()),
            completed_at: Some("t".to_owned()),
            output: Some(Output {
                title: "Failed".to_owned(),
                summary: "x".repeat(5000),
                text: Some("tail".to_owned()),
                annotations: vec![GhAnnotation {
                    path: "a".to_owned(),
                    start_line: 1,
                    end_line: 1,
                    start_column: None,
                    end_column: None,
                    annotation_level: "failure".to_owned(),
                    message: "m".to_owned(),
                    title: None,
                }],
            }),
        };
        let minimal = minimal_check(&check);
        assert_eq!(minimal.status, "completed");
        assert_eq!(minimal.conclusion.as_deref(), Some("failure"));
        let output = minimal.output.unwrap();
        assert_eq!(output.summary.chars().count(), MINIMAL_SUMMARY_CHARS);
        assert_eq!((output.text, output.annotations), (None, vec![]));
    }

    #[test]
    fn a_failed_send_keeps_the_mark_and_backs_off_until_the_fifth_try() {
        let (mut state, mut outbox, generation) = marked(1);
        for tries in 1..MAX_SEND_TRIES {
            assert!(!record_flush(
                &mut state,
                &mut outbox,
                1,
                generation,
                &Flushed::Failed,
                0
            ));
            assert_eq!(state.send_failures, tries);
            assert_eq!(outbox.generation(1), Some(generation), "the mark stays");
            assert!(outbox.due(0).is_empty(), "and the outbox backs off");
        }
        assert!(record_flush(
            &mut state,
            &mut outbox,
            1,
            generation,
            &Flushed::Failed,
            0
        ));
        assert!(outbox.is_empty(), "the fifth failure drops the mark");
        assert_eq!(state.send_failures, 0);
    }

    #[test]
    fn a_sent_update_records_the_check_run_and_marks_again_while_annotations_wait() {
        let (mut state, mut outbox, generation) = marked(1);
        state.annotations = vec![file_annotation(); 60];
        state.send_failures = 3;
        let sent = Flushed::Sent {
            new_check_run: Some(77),
            annotations_sent: 50,
        };
        assert!(record_flush(
            &mut state,
            &mut outbox,
            1,
            generation,
            &sent,
            0
        ));
        assert_eq!(state.check_run_id, Some(77));
        assert_eq!((state.file_annotations_sent, state.send_failures), (50, 0));
        assert!(outbox.generation(1).is_some_and(|g| g != generation));
        let last = Flushed::Sent {
            new_check_run: None,
            annotations_sent: 60,
        };
        let generation = outbox.generation(1).unwrap();
        record_flush(&mut state, &mut outbox, 1, generation, &last, 0);
        assert!(outbox.is_empty());
        assert_eq!(state.check_run_id, Some(77));
    }

    #[test]
    fn a_throttled_send_stops_the_flush() {
        let (mut state, mut outbox, generation) = marked(1);
        let go_on = record_flush(
            &mut state,
            &mut outbox,
            1,
            generation,
            &Flushed::Throttled(30),
            1_000,
        );
        assert!(!go_on);
        assert_eq!(outbox.generation(1), Some(generation));
        assert_eq!(outbox.next_alarm(1_000), Some(31_000));
    }

    #[test]
    fn a_claim_or_batch_must_name_the_current_attempt() {
        let mut g = graph();
        let ctx = json!({ "ref": "refs/heads/main", "event": {} });
        let _ = g.advance(&ctx);
        assert_eq!(gate(g.node(1), 1), Gate::Queued);
        assert_eq!(gate(g.node(1), 2), Gate::Stale);
        assert_eq!(gate(g.node(404), 1), Gate::Missing);
        assert_eq!(gate(g.node(2), 1), Gate::Ended, "still waiting");
        g.running(1);
        assert_eq!(gate(g.node(1), 1), Gate::Running);
        let _ = g.finished(
            1,
            JobResult {
                conclusion: Conclusion::Failure,
                outputs: std::collections::BTreeMap::new(),
                steps: vec![],
                annotations: vec![],
            },
            &ctx,
        );
        assert_eq!(gate(g.node(1), 1), Gate::Ended);
        // After a rerun the old attempt's token no longer fits.
        let _ = g.rerun(1, &ctx);
        assert_eq!(gate(g.node(1), 1), Gate::Stale);
        assert_eq!(gate(g.node(1), 2), Gate::Queued);
    }

    #[test]
    fn a_repeated_claim_finds_the_row_running_for_its_attempt() {
        assert!(running_for("running", 2, 2));
        assert!(!running_for("running", 1, 2));
        assert!(!running_for("completed", 2, 2));
    }

    #[test]
    fn the_todo_list_keeps_order_and_drops_by_id() {
        let mut todo = Todo::default();
        todo.push(Work::StartRun);
        todo.push(Work::QueueLaunch { job: 1, attempt: 1 });
        let first = todo.items[0].id;
        todo.remove(first);
        todo.remove(first);
        assert_eq!(todo.items.len(), 1);
        todo.push(Work::ReopenRun { attempt: 2 });
        assert_ne!(todo.items[0].id, todo.items[1].id);
        assert_eq!(todo.items[0].work, Work::QueueLaunch { job: 1, attempt: 1 });
    }

    #[test]
    fn side_effects_left_over_arm_a_retry() {
        let mut deadlines = Deadlines::default();
        let mut todo = Todo::default();
        arm_todo(&mut deadlines, &todo, 5);
        assert_eq!(deadlines.todo_retry_at, None);
        todo.push(Work::StartRun);
        arm_todo(&mut deadlines, &todo, 1_000);
        assert_eq!(deadlines.todo_retry_at, Some(1_000 + TODO_RETRY_MS));
        arm_todo(&mut deadlines, &todo, 9_000);
        assert_eq!(deadlines.todo_retry_at, Some(1_000 + TODO_RETRY_MS));
        todo.remove(todo.items[0].id);
        arm_todo(&mut deadlines, &todo, 9_000);
        assert_eq!(deadlines.todo_retry_at, None);
    }

    #[test]
    fn a_failed_part_is_rescheduled_with_backoff_and_the_rest_is_not() {
        let mut deadlines = Deadlines {
            escalate_at: Some(1),
            purge_at: Some(2),
            ..Deadlines::default()
        };
        let mut outbox = Outbox::default();
        let failed = Failed {
            escalate: true,
            todo: true,
            ..Failed::default()
        };
        reschedule(&mut deadlines, &mut outbox, failed, 10_000);
        assert_eq!(deadlines.escalate_at, Some(70_000));
        assert_eq!(deadlines.todo_retry_at, Some(70_000));
        assert_eq!(deadlines.purge_at, Some(2));
        assert_eq!(deadlines.failures, 1);
        reschedule(&mut deadlines, &mut outbox, failed, 10_000);
        assert_eq!(deadlines.escalate_at, Some(130_000));
        assert_eq!(
            next_alarm(&deadlines, &outbox, 10_000),
            Some(2),
            "the earliest deadline still wins"
        );
        reschedule(&mut deadlines, &mut outbox, Failed::default(), 10_000);
        assert_eq!(deadlines.failures, 0);
    }

    #[test]
    fn a_failed_flush_blocks_the_outbox_for_the_backoff() {
        let mut deadlines = Deadlines::default();
        let mut outbox = Outbox::default();
        outbox.mark(1);
        let failed = Failed {
            flush: true,
            ..Failed::default()
        };
        reschedule(&mut deadlines, &mut outbox, failed, 0);
        assert_eq!(outbox.due(59_999), Vec::<u64>::new());
        assert_eq!(outbox.due(60_000), vec![1]);
    }

    #[test]
    fn a_result_without_steps_keeps_the_streamed_ones() {
        let mut state = JobState {
            steps: vec![StepView {
                index: 0,
                name: "s".to_owned(),
                summary: None,
            }],
            ..JobState::default()
        };
        let result = JobResult {
            conclusion: Conclusion::Success,
            outputs: std::collections::BTreeMap::new(),
            steps: vec![],
            annotations: vec![],
        };
        apply_result(&mut state, &result);
        assert_eq!(state.steps.len(), 1);
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
    fn the_restore_key_is_built_from_the_run_and_tenant_of_the_request() {
        assert_eq!(
            restore_key(Some(12), Some("abc")).as_deref(),
            Some("state/abc/12.json")
        );
        assert_eq!(restore_key(None, Some("abc")), None);
        assert_eq!(restore_key(Some(12), None), None);
        assert_eq!(restore_key(Some(12), Some("")), None);
        assert_eq!(restore_key(Some(12), Some("a/../b")), None);
        assert_eq!(Ident::from_query(Some(1), Some("t".into())).unwrap().run, 1);
        assert_eq!(Ident::from_query(Some(1), Some("x/y".into())), None);
        assert_eq!(Ident::from_query(None, Some("t".into())), None);
    }

    #[test]
    fn a_flush_answer_belongs_to_the_attempt_it_was_planned_for() {
        let mut g = graph();
        let ctx = json!({ "ref": "refs/heads/main", "event": {} });
        let _ = g.advance(&ctx);
        assert!(plan_is_current(g.node(1), 1));
        g.running(1);
        let result = JobResult {
            conclusion: Conclusion::Failure,
            outputs: std::collections::BTreeMap::new(),
            steps: vec![],
            annotations: vec![],
        };
        let _ = g.finished(1, result, &ctx);
        let _ = g.rerun(1, &ctx);
        assert!(!plan_is_current(g.node(1), 1), "the rerun moved on");
        assert!(plan_is_current(g.node(1), 2));
        assert!(!plan_is_current(g.node(404), 1));
    }

    #[test]
    fn only_one_caller_drains_the_todo_list_at_a_time() {
        let flag = Cell::new(false);
        assert!(take_drain(&flag));
        assert!(!take_drain(&flag));
        flag.set(false);
        assert!(take_drain(&flag));
    }

    #[test]
    fn a_failed_snapshot_is_rescheduled_with_backoff() {
        let mut deadlines = Deadlines {
            snapshot_at: Some(1),
            ..Deadlines::default()
        };
        let failed = Failed {
            snapshot: true,
            ..Failed::default()
        };
        reschedule(&mut deadlines, &mut Outbox::default(), failed, 1_000);
        assert_eq!(deadlines.snapshot_at, Some(61_000));
    }

    #[test]
    fn a_request_must_name_the_run_the_object_holds() {
        let held = info();
        assert!(is_run(&held, 7, &held.tenant_id));
        assert!(!is_run(&held, 8, &held.tenant_id));
        assert!(!is_run(&held, 7, "other"));
    }

    #[test]
    fn the_snapshot_deadline_wakes_the_alarm() {
        let deadlines = Deadlines {
            snapshot_at: Some(5),
            ..Deadlines::default()
        };
        assert_eq!(next_alarm(&deadlines, &Outbox::default(), 9), Some(5));
        assert!(due(&deadlines, &Outbox::default(), 5).snapshot);
        assert!(!due(&deadlines, &Outbox::default(), 4).snapshot);
    }

    #[test]
    fn the_snapshot_key_names_tenant_and_run() {
        assert_eq!(snapshot_key("abc", 12), "state/abc/12.json");
    }
}
