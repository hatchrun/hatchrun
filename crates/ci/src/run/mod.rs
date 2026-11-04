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
//! | `POST /held?job=ID` | `{ "message": "..." }` | `204`, `409` not queued |
//! | `POST /cancel[?job=ID&attempt=N]` | none, or `{ "message": "...", "outright": bool }` | `204` |
//! | `POST /rerun?job=ID` | none | `204` |
//! | `POST /purge` | none | `204` |
//! | `GET /state` | none | `200` [`RunState`] |
//! | `GET /live?job=ID` | WebSocket upgrade | `101`, `409` ended |
//! | `GET /log?job=ID[&offset=N]` | none | `200` text, `404` none |
//!
//! `Line` events of a batch go to the job's [`LogWriter`]: each full
//! chunk is stored as `log:<job>:<n:08>` and the rest of the writer
//! under `logw:<job>` and `logo:<job>`. A job's sockets (hibernating
//! `WebSockets` tagged with the job id) get every applied batch. When the
//! job ends the writer is sealed (`logx:<job>`), the sockets get the end
//! and close, and the log is gzipped to R2 outside the lock; a failed
//! upload stays pending in `job:<id>` and the alarm retries it.

mod decide;
mod log;
mod object;

use hatchrun_workflow::{Annotation, Conclusion, Concurrency, PlannedJob};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::checks::StepView;
use crate::github::Capped;
use crate::graph::Node;
use crate::intake::Repo;
use crate::logs::LogIndex;

/// The queue that launches instances.
pub(crate) const LAUNCHES: &str = "CI_LAUNCHES";
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
/// How long a failed log upload waits before the next try, in
/// milliseconds.
const UPLOAD_RETRY_MS: u64 = 30 * 1000;
/// How much text before the end of the failed step is read to find the
/// tail for a check run, in bytes.
const TAIL_SCAN_BYTES: u64 = 64 << 10;
/// The most bytes the failed tail of a check run keeps.
const TAIL_BYTES: usize = 24 << 10;
/// How many chunk values are read in one call.
const READ_CHUNKS: u32 = 32;
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

/// Where the state of an ended run is kept in R2. `days` is the
/// tenant's retention, which names the tier an R2 lifecycle rule
/// expires.
#[must_use]
pub fn snapshot_key(days: u32, tenant: &str, run_id: u64) -> String {
    format!("state/{days}d/{tenant}/{run_id}.json")
}

/// The R2 key of the state of run `run` kept `days` days, `None` for a
/// tenant id that is empty or could name another key, or a retention
/// that is no tier.
#[must_use]
pub fn restore_key(run: Option<u64>, tenant: Option<&str>, days: Option<u32>) -> Option<String> {
    let tenant = tenant.filter(|t| !t.is_empty() && !t.contains(['/', '.']))?;
    let days = days.filter(|d| crate::logs::is_tier(*d))?;
    Some(snapshot_key(days, tenant, run?))
}

/// Whether `info` is run `run` of `tenant`.
pub(crate) fn is_run(info: &RunInfo, run: u64, tenant: &str) -> bool {
    info.run_id == run && info.tenant_id == tenant
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
    /// How many days the run's logs and state are kept: 30 or 90, as the
    /// tenant's retention was when the run was recorded.
    pub retention_days: u32,
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
    /// The workflow's `concurrency`, as written.
    pub concurrency: Option<Concurrency>,
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
    /// The finished log is not in R2 yet. Its chunks and [`Sealed`] stay
    /// in storage until it is.
    pub upload_pending: bool,
    /// Why the queued job waits for a launch, until an agent claims it.
    pub held: Option<String>,
    /// The token scopes the App holds less of than the job asked.
    pub capped: Vec<Capped>,
}

/// A finished job's log as it waits for the upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    /// The index that goes beside the log.
    pub index: LogIndex,
    /// How many numbered chunks hold the text, from 0.
    pub chunks: u32,
    /// The size of each chunk in bytes.
    pub sizes: Vec<u32>,
    /// The job attempt the log belongs to.
    pub attempt: u32,
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
    /// When the check run flush is tried again after it failed as a
    /// whole (storage, not GitHub).
    #[serde(default)]
    pub flush_retry_at: Option<u64>,
    /// When the earliest [`PendingChain`] is tried again.
    #[serde(default)]
    pub chain_retry_at: Option<u64>,
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
        /// Where the job's log will be in R2, `None` when the job
        /// wrote none.
        log_key: Option<String>,
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

/// How long a [`PendingChain`] is retried before it is given up.
pub const CHAIN_GIVE_UP_MS: u64 = 3_600_000;

/// The longest wait between two tries of a [`PendingChain`], in
/// milliseconds.
const CHAIN_MAX_WAIT_MS: u64 = 300_000;

/// A state change of the run that `on: workflow_run` workflows still
/// have to be started for. Kept apart from the [`Work`] items, so a
/// GitHub outage that fails it never holds up the run's own side
/// effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingChain {
    /// Names the entry.
    pub id: u64,
    /// The attempt of the run the change belongs to.
    pub attempt: u32,
    /// `requested`, `in_progress` or `completed`.
    pub action: String,
    /// How the attempt ended, for `completed`.
    pub conclusion: Option<String>,
    /// When the change was recorded.
    pub first_at: u64,
    /// When it is tried next.
    pub next_at: u64,
    /// How many tries failed.
    pub tries: u32,
}

/// The side effects still to do, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    next: u64,
    /// The items, oldest first.
    pub items: Vec<Pending>,
    /// The `workflow_run` changes still to send. Absent in state written
    /// before they were kept.
    #[serde(default)]
    pub chains: Vec<PendingChain>,
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

    /// Records a change of attempt `attempt` for `on: workflow_run`,
    /// due now. A change recorded already is not added again.
    pub fn push_chain(&mut self, attempt: u32, action: &str, conclusion: Option<&str>, now: u64) {
        if self
            .chains
            .iter()
            .any(|c| c.attempt == attempt && c.action == action)
        {
            return;
        }
        self.next += 1;
        self.chains.push(PendingChain {
            id: self.next,
            attempt,
            action: action.to_owned(),
            conclusion: conclusion.map(str::to_owned),
            first_at: now,
            next_at: now,
            tries: 0,
        });
    }

    /// The changes whose time has come, oldest first.
    #[must_use]
    pub fn due_chains(&self, now: u64) -> Vec<PendingChain> {
        self.chains
            .iter()
            .filter(|c| c.next_at <= now)
            .cloned()
            .collect()
    }

    /// Drops the change `id`, which was sent.
    pub fn chain_sent(&mut self, id: u64) {
        self.chains.retain(|c| c.id != id);
    }

    /// Records a failed try of the change `id`: it waits twice as long
    /// as the try before, up to five minutes, and is dropped once it is
    /// an hour old. Returns whether it was dropped.
    pub fn chain_failed(&mut self, id: u64, now: u64) -> bool {
        let Some(chain) = self.chains.iter_mut().find(|c| c.id == id) else {
            return false;
        };
        chain.tries += 1;
        if now.saturating_sub(chain.first_at) >= CHAIN_GIVE_UP_MS {
            self.chains.retain(|c| c.id != id);
            return true;
        }
        let wait = (5_000_u64 << chain.tries.min(10)).min(CHAIN_MAX_WAIT_MS);
        chain.next_at = now + wait;
        false
    }

    /// When the earliest change is tried, `None` with none left.
    #[must_use]
    pub fn next_chain_at(&self) -> Option<u64> {
        self.chains.iter().map(|c| c.next_at).min()
    }
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

pub use object::RunObject;

#[cfg(test)]
mod tests {
    use super::decide::tests::info;
    use super::*;

    #[test]
    fn deadlines_stored_before_the_flush_retry_still_load() {
        let old = r#"{"escalate_at":null,"upload_retry_at":5,"snapshot_at":null,"purge_at":null,"todo_retry_at":null,"failures":1}"#;
        let deadlines: Deadlines = serde_json::from_str(old).unwrap();
        assert_eq!(deadlines.upload_retry_at, Some(5));
        assert_eq!(deadlines.flush_retry_at, None);
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
    fn chains_are_kept_once_and_retried_with_a_capped_backoff() {
        let mut todo = Todo::default();
        todo.push_chain(1, "completed", Some("success"), 1_000);
        todo.push_chain(1, "completed", Some("success"), 2_000);
        todo.push_chain(2, "completed", Some("failure"), 2_000);
        assert_eq!(todo.chains.len(), 2);
        assert_eq!(todo.due_chains(1_000).len(), 1);
        let id = todo.chains[0].id;
        assert!(!todo.chain_failed(id, 1_000));
        assert_eq!(todo.chains[0].next_at, 1_000 + 10_000);
        assert!(!todo.chain_failed(id, 11_000));
        assert_eq!(todo.chains[0].next_at, 11_000 + 20_000);
        for _ in 0..12 {
            todo.chain_failed(id, 31_000);
        }
        assert_eq!(todo.chains[0].next_at, 31_000 + 300_000);
        assert_eq!(todo.next_chain_at(), Some(2_000));
        assert!(todo.chain_failed(id, 1_000 + CHAIN_GIVE_UP_MS));
        assert_eq!(todo.chains.len(), 1);
        todo.chain_sent(todo.chains[0].id);
        assert_eq!(todo.next_chain_at(), None);
    }

    #[test]
    fn state_written_before_chains_still_loads() {
        let todo: Todo = serde_json::from_str(r#"{"next":2,"items":[]}"#).unwrap();
        assert_eq!(todo.chains.len(), 0);
        let deadlines: Deadlines = serde_json::from_str(
            r#"{"escalate_at":null,"upload_retry_at":null,"snapshot_at":null,"purge_at":null,"todo_retry_at":null,"failures":0}"#,
        )
        .unwrap();
        assert_eq!(deadlines.chain_retry_at, None);
    }

    #[test]
    fn a_request_must_name_the_run_the_object_holds() {
        let held = info();
        assert!(is_run(&held, 7, &held.tenant_id));
        assert!(!is_run(&held, 8, &held.tenant_id));
        assert!(!is_run(&held, 7, "other"));
    }

    #[test]
    fn snapshot_key_has_the_tier() {
        assert_eq!(snapshot_key(90, "abc", 12), "state/90d/abc/12.json");
    }
}
