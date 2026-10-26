// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The plain functions the object decides with.

use std::cell::Cell;

use hatchrun_cloud::http::Response as HttpResponse;
use hatchrun_workflow::agent::{AgentEvent, Batch};
use hatchrun_workflow::{Github, JobDocument, JobResult};
use serde_json::{Value, json};

use crate::checks::{MAX_TOTAL_ANNOTATIONS, Outbox, StepView};
use crate::github::{self, CheckRun, Output};
use crate::graph::{Graph, Node, State as NodeState};
use crate::logs::LogWriter;

use super::{
    Deadlines, HEARTBEAT_WRITE_MS, JobState, JobSummary, MAX_EVENT_BYTES, MAX_SEND_TRIES,
    MAX_STEPS, MAX_STORED_ANNOTATIONS, RunInfo, RunState, TODO_RETRY_MS, Todo,
};

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
/// fields as the job document's, plus `event_name`, `workflow` and `event`, and no
/// token.
pub(crate) fn github_context(info: &RunInfo, event: &Value, api: &str) -> Value {
    let mut context = serde_json::to_value(github_struct(info, api, ""))
        .expect("the github context is plain strings and always serializes");
    if let Some(fields) = context.as_object_mut() {
        fields.remove("token");
        fields.insert("event_name".to_owned(), json!(info.event_name));
        fields.insert("workflow".to_owned(), json!(info.workflow));
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

/// Records why the queued job waits, from the launch consumer. Returns
/// whether the message is new, which is when the check run needs an
/// update.
pub(crate) fn apply_held(state: &mut JobState, message: &str) -> bool {
    if state.held.as_deref() == Some(message) {
        return false;
    }
    state.held = Some(message.to_owned());
    true
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

/// Writes the step starts and lines of `batch` to the job's log and
/// answers whether it wrote anything.
pub(crate) fn apply_log(writer: &mut LogWriter, batch: &Batch) -> bool {
    let mut wrote = false;
    for event in &batch.events {
        match event {
            AgentEvent::StepStarted { index, name } => {
                writer.step_started(*index, name);
                wrote = true;
            }
            AgentEvent::Line { text } => {
                writer.line(text);
                wrote = true;
            }
            _ => {}
        }
    }
    wrote
}

/// Applies a batch to the job's state and its log. A batch at or below
/// the last sequence number is ignored whole, so its lines are written
/// once. Answers what [`apply_steps`] did and whether the log changed.
pub(crate) fn apply_batch(
    state: &mut JobState,
    writer: &mut LogWriter,
    batch: &Batch,
) -> (Applied, bool) {
    let applied = apply_steps(state, batch);
    let wrote = applied.fresh && apply_log(writer, batch);
    (applied, wrote)
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
pub(super) fn attempts(graph: &Graph) -> Vec<(u64, u32)> {
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
/// failures in a row the mark is dropped, which the caller logs, unless
/// the update is `terminal` (the job is done): that one is the update
/// that completes the check run, so it keeps retrying at the backoff
/// cap and the run is not purged while it waits.
pub(crate) fn record_flush(
    state: &mut JobState,
    outbox: &mut Outbox,
    job: u64,
    generation: u64,
    flushed: &Flushed,
    terminal: bool,
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
            state.send_failures = state.send_failures.saturating_add(1);
            if !terminal && state.send_failures >= MAX_SEND_TRIES {
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

#[cfg(test)]
pub(super) mod tests {
    use crate::github::GhAnnotation;
    use crate::intake::Repo;
    use hatchrun_workflow::agent::AgentEvent;
    use hatchrun_workflow::{Annotation, AnnotationLevel, Conclusion, StepSummary, compile, parse};

    use super::super::Work;
    use super::*;

    const API: &str = "https://api.github.com";

    pub(in crate::run) fn info() -> RunInfo {
        RunInfo {
            run_id: 7,
            run_number: 3,
            run_attempt: 1,
            tenant_id: "t".repeat(16),
            retention_days: 30,
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

    pub(in crate::run) fn graph() -> Graph {
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

    pub(in crate::run) fn summary(name: &str) -> StepSummary {
        StepSummary {
            name: name.to_owned(),
            outcome: Conclusion::Success,
            conclusion: Conclusion::Success,
            duration_ms: 1000,
        }
    }

    pub(in crate::run) fn batch(seq: u64, events: Vec<AgentEvent>) -> Batch {
        Batch { seq, events }
    }

    pub(in crate::run) fn line(text: &str) -> AgentEvent {
        AgentEvent::Line {
            text: text.to_owned(),
        }
    }

    pub(in crate::run) fn written(writer: &mut LogWriter) -> String {
        let mut text = String::new();
        for (_, chunk) in writer.take_full_chunks() {
            text.push_str(&chunk);
        }
        let (last, _) = writer.finish();
        text.push_str(&last.map(|(_, chunk)| chunk).unwrap_or_default());
        text
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
    fn held_message_marks_the_outbox_once() {
        let mut state = JobState::default();
        assert!(apply_held(&mut state, "Waiting for a card."));
        assert!(!apply_held(&mut state, "Waiting for a card."));
        assert!(apply_held(&mut state, "Waiting: acme has 5 jobs running."));
        assert_eq!(
            state.held.as_deref(),
            Some("Waiting: acme has 5 jobs running.")
        );
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
                false,
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
            false,
            0
        ));
        assert!(outbox.is_empty(), "the fifth failure drops the mark");
        assert_eq!(state.send_failures, 0);
    }

    #[test]
    fn a_terminal_update_is_never_dropped_and_backs_off_at_the_cap() {
        let (mut state, mut outbox, generation) = marked(1);
        for tries in 1..=(MAX_SEND_TRIES * 4) {
            assert!(!record_flush(
                &mut state,
                &mut outbox,
                1,
                generation,
                &Flushed::Failed,
                true,
                0
            ));
            assert_eq!(state.send_failures, tries);
            assert_eq!(outbox.generation(1), Some(generation), "the mark stays");
        }
        assert_eq!(outbox.next_alarm(0), Some(900_000), "at the 900 s cap");
        assert!(!outbox.is_empty(), "so the run is not purged");
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
            false,
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
        record_flush(&mut state, &mut outbox, 1, generation, &last, false, 0);
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
            false,
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
    fn repeated_seq_is_ignored() {
        let mut state = JobState::default();
        let mut writer = LogWriter::default();
        let events = vec![
            AgentEvent::StepStarted {
                index: 0,
                name: "build".to_owned(),
            },
            line("one"),
            line("two"),
        ];
        let (first, wrote) = apply_batch(&mut state, &mut writer, &batch(1, events.clone()));
        assert!(first.fresh && wrote);
        let (again, wrote) = apply_batch(&mut state, &mut writer, &batch(1, events));
        assert!(!again.fresh && !wrote);
        let (next, wrote) = apply_batch(&mut state, &mut writer, &batch(2, vec![line("three")]));
        assert!(next.fresh && wrote);
        assert_eq!(written(&mut writer), "== build\none\ntwo\nthree\n");
    }

    #[test]
    fn a_batch_of_annotations_writes_no_log() {
        let mut state = JobState::default();
        let mut writer = LogWriter::default();
        let (applied, wrote) = apply_batch(
            &mut state,
            &mut writer,
            &batch(
                1,
                vec![AgentEvent::StepFinished {
                    index: 0,
                    summary: summary("a"),
                }],
            ),
        );
        assert!(applied.fresh && !wrote);
        assert_eq!(writer.bytes(), 0);
    }
}
