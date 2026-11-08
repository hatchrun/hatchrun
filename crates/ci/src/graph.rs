// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The jobs of one run and the order they start in.
//!
//! A [`Graph`] is plain data: it holds each job's state and, when a
//! job changes state, returns the [`Action`]s the caller must carry out
//! (launch an instance, update a check run and a row, tell an agent to
//! stop). It does no I/O and reads no clock.

use std::collections::BTreeMap;

use hatchrun_workflow::{
    CallFrame, Conclusion, ExprError, Functions, JobKind, JobResult, JobStatus, Leg, Need, Outcome,
    Plan, PlannedJob, Ready, Siblings, Statuses, checks_status, continue_on_error, entry_in,
    expand, hatchrun_context, interpolate, is_expression, job_condition, may_hold_without,
    read_value, reads_context, ready, resolve_frame, shared_depth, valid_environment_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The message of an approval job, which ends as skipped for now.
const APPROVAL_MESSAGE: &str = "Approval jobs come in a later release.";

/// Why a job that failed ends as a success.
const CONTINUED: &str = "The job failed, and continue-on-error counts it as a success.";

/// Why a job a rerun dropped from its matrix ends as skipped.
const NOT_IN_MATRIX: &str = "Not in the matrix of this attempt.";

/// Why a job of a matrix ends as cancelled when another one failed.
const FAIL_FAST: &str = "Canceled because another job of its matrix failed and fail-fast is on.";

/// The jobs of a run, in plan order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    /// The jobs, in plan order.
    pub jobs: Vec<Node>,
    /// The run was cancelled.
    pub cancelled: bool,
    /// `RunDone` was emitted and no rerun has reset it.
    run_done: bool,
    /// The repository and organization variables `continue-on-error`
    /// reads. Not stored; [`Graph::set_vars`] fills it before a job ends.
    #[serde(skip)]
    vars: BTreeMap<String, String>,
    /// The jobs that were read into several, by plan id, as they were
    /// before: a rerun of what they read makes them again.
    #[serde(default)]
    stand_ins: BTreeMap<String, PlannedJob>,
    /// The jobs a rerun took out of the graph to read their matrix
    /// again, by the plan id of the job that made them, with their row
    /// ids: those the new matrix leaves out end as skipped.
    #[serde(default)]
    previous: BTreeMap<String, Vec<(u64, PlannedJob)>>,
    /// Whether each call instance was let in, by its id, once decided:
    /// a variable that changes while the call runs does not stop it
    /// half way. A rerun forgets them.
    #[serde(default)]
    admitted: BTreeMap<String, bool>,
}

/// What reading the scope of a job found: its `inputs`.
struct Walked {
    inputs: Value,
}

/// One job of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The job's row id.
    pub id: u64,
    /// The job as planned.
    pub job: PlannedJob,
    /// Where the job is.
    pub state: State,
    /// Starts at 1 and grows with each rerun.
    pub attempt: u32,
    /// What the agent reported, when it reported.
    pub result: Option<JobResult>,
    /// Why the control plane ended the job, when it did.
    pub message: Option<String>,
    /// The environment the job runs in, once known: the name as planned,
    /// or the one its expression gave when the job became ready.
    pub environment: Option<String>,
    /// 0 until the job is asked to stop, 1 once asked, 2 once told to
    /// stop hard.
    pub cancel: u32,
    /// The plan id of the job that read its matrix or `runs-on` and made
    /// this one.
    #[serde(default)]
    pub expanded_from: Option<String>,
    /// The job is one a rerun left out of its matrix: it ended as
    /// skipped for this attempt and is no part of the run, its
    /// conclusion or what requires jobs.
    #[serde(default)]
    pub stale: bool,
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for the jobs it requires.
    Waiting,
    /// A launch is queued.
    Queued,
    /// An agent has taken the job.
    Running,
    /// The job ended.
    Done(Conclusion),
}

/// What the caller must do after a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Queue a launch for this job.
    Launch(u64),
    /// The job ended without the agent reporting it (or the agent just
    /// did): the check run and the row must follow.
    Finished {
        /// The job.
        job: u64,
        /// How it ended.
        conclusion: Conclusion,
    },
    /// Tell the agent to stop, through the reply to its next batch.
    Cancel {
        /// The job.
        job: u64,
        /// 1 asks, 2 insists.
        count: u32,
    },
    /// The job reads its matrix or `runs-on` now that it can start: the
    /// caller reads [`Graph::expansion`], records the jobs it made and
    /// hands them to [`Graph::expanded`], or ends the job with
    /// [`Graph::failed`].
    Expand(u64),
    /// Every job ended.
    RunDone(Conclusion),
}

/// The functions an `if:` sees on the control plane.
struct Status(JobStatus);

impl Functions for Status {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Err(ExprError {
            message: "hashFiles runs on the runner".to_owned(),
            offset: 0,
        })
    }

    fn status(&self) -> JobStatus {
        self.0
    }
}

impl Graph {
    /// A graph whose jobs all wait. `jobs` pairs each row id with its
    /// planned job, in plan order.
    #[must_use]
    pub fn new(jobs: Vec<(u64, PlannedJob)>) -> Self {
        Self {
            jobs: jobs
                .into_iter()
                .map(|(id, job)| Node {
                    environment: job.environment.clone().filter(|name| !is_expression(name)),
                    id,
                    job,
                    state: State::Waiting,
                    attempt: 1,
                    result: None,
                    message: None,
                    cancel: 0,
                    expanded_from: None,
                    stale: false,
                })
                .collect(),
            cancelled: false,
            run_done: false,
            vars: BTreeMap::new(),
            stand_ins: BTreeMap::new(),
            previous: BTreeMap::new(),
            admitted: BTreeMap::new(),
        }
    }

    /// Whether the run reads `vars` when it decides: an environment name
    /// does, and so can the `if` and `with` of a call, which every job of
    /// a called workflow reads. The caller then fills them with
    /// [`Graph::set_vars`] before it decides anything.
    #[must_use]
    pub fn reads_vars(&self) -> bool {
        self.jobs.iter().any(|n| {
            !n.job.scope.is_empty()
                || n.job
                    .environment
                    .as_deref()
                    .is_some_and(|name| is_expression(name) && name.contains("vars"))
        })
    }

    /// Gives the graph the variables the `continue-on-error` of a job
    /// that ends reads as `vars.NAME`.
    pub fn set_vars(&mut self, vars: BTreeMap<String, String>) {
        self.vars = vars;
    }

    /// Starts every waiting job whose requirements ended, ends the ones
    /// that cannot run, and emits `RunDone` once when nothing is left.
    pub fn advance(&mut self, github_ctx: &Value) -> Vec<Action> {
        let mut actions = Vec::new();
        loop {
            let mut changed = false;
            for at in 0..self.jobs.len() {
                if self.jobs[at].state == State::Waiting && self.requirements_done(at) {
                    self.start_or_end(at, github_ctx, &mut actions);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.emit_run_done(&mut actions);
        actions
    }

    /// An agent took the job.
    pub fn running(&mut self, job: u64) {
        if let Some(node) = self.node_mut(job)
            && node.state == State::Queued
        {
            node.state = State::Running;
        }
    }

    /// The agent reported a result. A job that already ended, or was
    /// never queued, is left alone.
    pub fn finished(&mut self, job: u64, result: JobResult, github_ctx: &Value) -> Vec<Action> {
        let Some(at) = self.position(job) else {
            return Vec::new();
        };
        if !matches!(self.jobs[at].state, State::Queued | State::Running) {
            return Vec::new();
        }
        let (conclusion, note) = self.forgive(at, result.conclusion, github_ctx);
        self.jobs[at].result = Some(result);
        self.jobs[at].state = State::Done(conclusion);
        if note.is_some() {
            self.jobs[at].message = note;
        }
        let mut actions = vec![Action::Finished { job, conclusion }];
        self.fail_fast(at, &mut actions);
        actions.extend(self.advance(github_ctx));
        actions
    }

    /// The control plane ends the job as failed, for a launch that did
    /// not work or an agent that went silent.
    pub fn failed(&mut self, job: u64, message: String, github_ctx: &Value) -> Vec<Action> {
        let Some(at) = self.position(job) else {
            return Vec::new();
        };
        if !matches!(self.jobs[at].state, State::Queued | State::Running) {
            return Vec::new();
        }
        let (conclusion, note) = self.forgive(at, Conclusion::Failure, github_ctx);
        self.jobs[at].message = Some(match note {
            Some(note) => format!("{message} {note}"),
            None => message,
        });
        self.jobs[at].state = State::Done(conclusion);
        let mut actions = vec![Action::Finished { job, conclusion }];
        self.leave_out_previous(at, &mut actions);
        self.fail_fast(at, &mut actions);
        actions.extend(self.advance(github_ctx));
        actions
    }

    /// Cancels the run: asks running jobs to stop (once each) and ends
    /// queued jobs whose `if:` no longer holds. Waiting jobs meet the
    /// same rule in [`Graph::advance`].
    pub fn cancel(&mut self, github_ctx: &Value) -> Vec<Action> {
        self.cancelled = true;
        let mut actions = Vec::new();
        for at in 0..self.jobs.len() {
            match self.jobs[at].state {
                State::Running if self.jobs[at].cancel == 0 => {
                    self.jobs[at].cancel = 1;
                    actions.push(Action::Cancel {
                        job: self.jobs[at].id,
                        count: 1,
                    });
                }
                State::Queued => match self.decide(at, github_ctx, false) {
                    Ok(true) => {}
                    Ok(false) => self.end(at, Conclusion::Cancelled, None, &mut actions),
                    Err(e) => self.end(at, Conclusion::Failure, Some(if_error(&e)), &mut actions),
                },
                _ => {}
            }
        }
        actions.extend(self.advance(github_ctx));
        actions
    }

    /// Cancels the run because of a concurrency group, and says why on
    /// every job it stops. Without `outright` this is [`Graph::cancel`]:
    /// jobs whose `if:` still holds (`always()`) run on. With it the run
    /// never started, so every job that has not started ends as
    /// cancelled whatever its `if:`.
    pub fn cancel_for_group(
        &mut self,
        message: &str,
        outright: bool,
        github_ctx: &Value,
    ) -> Vec<Action> {
        let open: Vec<bool> = self
            .jobs
            .iter()
            .map(|n| !matches!(n.state, State::Done(_)))
            .collect();
        let mut actions = Vec::new();
        if outright {
            self.cancelled = true;
            for at in 0..self.jobs.len() {
                match self.jobs[at].state {
                    State::Running if self.jobs[at].cancel == 0 => {
                        self.jobs[at].cancel = 1;
                        actions.push(Action::Cancel {
                            job: self.jobs[at].id,
                            count: 1,
                        });
                    }
                    State::Waiting | State::Queued => {
                        self.end(at, Conclusion::Cancelled, None, &mut actions);
                    }
                    _ => {}
                }
            }
            actions.extend(self.advance(github_ctx));
        } else {
            actions = self.cancel(github_ctx);
        }
        for (node, was_open) in self.jobs.iter_mut().zip(open) {
            let stopped = node.cancel > 0 || node.state == State::Done(Conclusion::Cancelled);
            if was_open && stopped && node.message.is_none() {
                node.message = Some(message.to_owned());
            }
        }
        actions
    }

    /// Cancels one job because of a concurrency group, and says why. A
    /// job that waits for a launch ends as cancelled at once; a running
    /// one is asked to stop. Another attempt, or a job that ended, is
    /// left alone.
    pub fn cancel_job(
        &mut self,
        job: u64,
        attempt: u32,
        message: &str,
        github_ctx: &Value,
    ) -> Vec<Action> {
        let Some(at) = self.position(job) else {
            return Vec::new();
        };
        if self.jobs[at].attempt != attempt {
            return Vec::new();
        }
        let mut actions = Vec::new();
        match self.jobs[at].state {
            State::Running if self.jobs[at].cancel == 0 => {
                self.jobs[at].cancel = 1;
                self.jobs[at].message = Some(message.to_owned());
                actions.push(Action::Cancel { job, count: 1 });
            }
            State::Waiting | State::Queued => {
                self.end(
                    at,
                    Conclusion::Cancelled,
                    Some(message.to_owned()),
                    &mut actions,
                );
                actions.extend(self.advance(github_ctx));
            }
            _ => {}
        }
        actions
    }

    /// Raises `cancel` from 1 to 2 on running jobs.
    pub fn escalate(&mut self) -> Vec<Action> {
        self.jobs
            .iter_mut()
            .filter(|n| n.state == State::Running && n.cancel == 1)
            .map(|n| {
                n.cancel = 2;
                Action::Cancel {
                    job: n.id,
                    count: 2,
                }
            })
            .collect()
    }

    /// Runs the job again, and every job that requires it, directly or
    /// not. Does nothing until the run has a conclusion. Jobs outside the
    /// reset set keep their result, so a cancelled job must be rerun too
    /// for the run to stop ending as cancelled.
    pub fn rerun(&mut self, job: u64, github_ctx: &Value) -> Vec<Action> {
        let Some(root) = self.position(job) else {
            return Vec::new();
        };
        // A job a rerun left out of its matrix is no part of the run; its
        // producer is what runs again.
        if self.jobs[root].stale {
            return Vec::new();
        }
        // Like GitHub, only a run that has ended can be rerun: a running
        // dependent would otherwise run twice.
        if self.conclusion().is_none() {
            return Vec::new();
        }
        let mut reset = vec![false; self.jobs.len()];
        reset[root] = true;
        // Plan order need not put a job after its requirements, so
        // repeat until no job is added.
        loop {
            let mut grew = false;
            for at in 0..self.jobs.len() {
                if !reset[at]
                    && self.jobs[at]
                        .job
                        .requires
                        .iter()
                        .any(|r| self.position_of(r).is_some_and(|p| reset[p]))
                {
                    reset[at] = true;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        for (node, reset) in self.jobs.iter_mut().zip(reset) {
            if reset {
                node.state = State::Waiting;
                node.attempt += 1;
                node.result = None;
                node.message = None;
                node.cancel = 0;
            }
        }
        self.collapse_rerun();
        self.admitted.clear();
        self.cancelled = false;
        self.run_done = false;
        self.advance(github_ctx)
    }

    /// Puts back, as the job it was, each job that read its matrix or
    /// `runs-on` from a job that is run again: it reads them again when
    /// that job ends, so entries the new output adds run and the ones it
    /// drops do not. The first job it made keeps its row.
    fn collapse_rerun(&mut self) {
        let mut again: Vec<String> = Vec::new();
        for node in &self.jobs {
            let Some(from) = &node.expanded_from else {
                continue;
            };
            let waits = node.state == State::Waiting
                && node.job.requires.iter().any(|r| {
                    self.position_of(r)
                        .is_some_and(|p| self.jobs[p].state == State::Waiting)
                });
            if waits && !again.contains(from) && self.stand_ins.contains_key(from) {
                again.push(from.clone());
            }
        }
        for from in again {
            let Some(stand_in) = self.stand_ins.get(&from).cloned() else {
                continue;
            };
            let made: Vec<usize> = (0..self.jobs.len())
                .filter(|&i| {
                    !self.jobs[i].stale
                        && self.jobs[i].expanded_from.as_deref() == Some(from.as_str())
                })
                .collect();
            let (Some(&first), attempt) = (
                made.first(),
                made.iter()
                    .map(|&i| self.jobs[i].attempt)
                    .max()
                    .unwrap_or(1),
            ) else {
                continue;
            };
            let ids: Vec<String> = made.iter().map(|&i| self.jobs[i].job.id.clone()).collect();
            let row = self.jobs[first].id;
            // A job the matrix no longer makes is left out, whichever row
            // the new jobs take.
            self.previous.insert(
                from.clone(),
                made.iter()
                    .map(|&i| (self.jobs[i].id, self.jobs[i].job.clone()))
                    .collect(),
            );
            for &i in made.iter().rev() {
                self.jobs.remove(i);
            }
            self.jobs.insert(
                first,
                Node {
                    id: row,
                    environment: stand_in
                        .environment
                        .clone()
                        .filter(|name| !is_expression(name)),
                    job: stand_in.clone(),
                    state: State::Waiting,
                    attempt,
                    result: None,
                    message: None,
                    cancel: 0,
                    expanded_from: None,
                    stale: false,
                },
            );
            let join = |requires: &mut Vec<String>| {
                let Some(p) = requires.iter().position(|r| ids.contains(r)) else {
                    return;
                };
                requires.retain(|r| !ids.contains(r));
                let p = p.min(requires.len());
                requires.insert(p, from.clone());
            };
            for node in &mut self.jobs {
                if node.job.id != from {
                    join(&mut node.job.requires);
                }
            }
            for other in self.stand_ins.values_mut() {
                join(&mut other.requires);
            }
            self.number_groups();
        }
    }

    /// The jobs a job that asked to expand makes, read from its
    /// context. `vars` are the repository and organization variables.
    /// `None` for a job the graph does not hold or that has nothing
    /// deferred.
    ///
    /// # Errors
    ///
    /// A message when the matrix cannot be made, or a job it makes
    /// would have the id of another job of the run.
    #[must_use]
    pub fn expansion(
        &self,
        job: u64,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Option<Result<Vec<Leg>, String>> {
        let at = self.position(job)?;
        self.jobs[at].job.deferred.as_ref()?;
        if self.jobs[at].state != State::Queued {
            return None;
        }
        let ctx = match self.job_context(job, github_ctx, vars)? {
            Ok(ctx) => ctx,
            Err(message) => return Some(Err(message)),
        };
        let group = self.jobs[at].job.strategy.as_ref().map(|s| &s.group);
        let in_group = |i: usize| {
            i != at
                && !self.jobs[i].stale
                && group.is_some()
                && self.jobs[i].job.strategy.as_ref().map(|s| &s.group) == group
        };
        let count = |range: std::ops::Range<usize>| {
            u32::try_from(range.filter(|i| in_group(*i)).count()).unwrap_or(u32::MAX)
        };
        let siblings = Siblings {
            jobs: self.jobs.iter().filter(|n| !n.stale).count() - 1,
            group_before: count(0..at),
            group_others: count(0..self.jobs.len()),
        };
        Some(expand(&self.jobs[at].job, &ctx, siblings).and_then(|legs| {
            let clash = legs.iter().find(|leg| {
                self.jobs
                    .iter()
                    .enumerate()
                    .any(|(i, n)| i != at && !n.stale && n.job.id == leg.job.id)
            });
            match clash {
                Some(leg) => Err(format!(
                    "the matrix would make a job `{}`, which another job of the run has",
                    leg.job.id
                )),
                None => Ok(legs),
            }
        }))
    }

    /// Ends job `job`, which asked to expand and whose matrix has no
    /// entry, as skipped, as GitHub does.
    pub fn no_jobs(&mut self, job: u64, github_ctx: &Value) -> Vec<Action> {
        let Some(at) = self.position(job) else {
            return Vec::new();
        };
        if self.jobs[at].state != State::Queued {
            return Vec::new();
        }
        let mut actions = Vec::new();
        self.end(
            at,
            Conclusion::Skipped,
            Some("The matrix has no entries.".to_owned()),
            &mut actions,
        );
        actions.extend(self.advance(github_ctx));
        actions
    }

    /// Replaces job `job`, which asked to expand, with its `legs`, each
    /// with the row id it was recorded under. The first one is normally
    /// the job's own row. Jobs that required it now require every leg.
    /// A leg that has an error ends as failed with it; the others
    /// start as soon as their requirements allow.
    pub fn expanded(&mut self, job: u64, legs: Vec<(u64, Leg)>, github_ctx: &Value) -> Vec<Action> {
        let Some(at) = self.position(job) else {
            return Vec::new();
        };
        if self.jobs[at].state != State::Queued || self.jobs[at].job.deferred.is_none() {
            return Vec::new();
        }
        let old = self.jobs.remove(at);
        let ids: Vec<String> = legs.iter().map(|(_, l)| l.job.id.clone()).collect();
        let mut failed = Vec::new();
        let nodes: Vec<Node> = legs
            .into_iter()
            .enumerate()
            .map(|(i, (id, leg))| {
                if let Some(message) = leg.error {
                    failed.push((i, message));
                }
                Node {
                    environment: leg
                        .job
                        .environment
                        .clone()
                        .filter(|name| !is_expression(name)),
                    id,
                    job: leg.job,
                    state: State::Waiting,
                    attempt: old.attempt,
                    result: None,
                    message: None,
                    cancel: 0,
                    expanded_from: Some(old.job.id.clone()),
                    stale: false,
                }
            })
            .collect();
        self.stand_ins.insert(old.job.id.clone(), old.job.clone());
        // A job that was left out earlier and is made again is current.
        self.jobs.retain(|n| !(n.stale && ids.contains(&n.job.id)));
        let at = at.min(self.jobs.len());
        let made = nodes.len();
        let rows: Vec<u64> = nodes.iter().map(|n| n.id).collect();
        let dropped = self.left_out(&old.job.id, old.attempt, &ids, &rows);
        let left_out: Vec<u64> = dropped.iter().map(|n| n.id).collect();
        self.jobs.splice(at..at, nodes);
        self.jobs.splice(at + made..at + made, dropped);
        for node in &mut self.jobs {
            let Some(p) = node.job.requires.iter().position(|r| *r == old.job.id) else {
                continue;
            };
            node.job.requires.remove(p);
            for (k, id) in ids.iter().enumerate() {
                if !node.job.requires.contains(id) {
                    node.job.requires.insert(p + k, id.clone());
                }
            }
        }
        self.number_groups();
        let mut actions: Vec<Action> = left_out
            .into_iter()
            .map(|job| Action::Finished {
                job,
                conclusion: Conclusion::Skipped,
            })
            .collect();
        for (i, message) in failed {
            let at = at + i;
            // `fail-fast` of an earlier leg may have ended it already.
            if matches!(self.jobs[at].state, State::Done(_)) {
                continue;
            }
            // The job's `if:` comes first: a job that does not run has
            // no use for a runner, so an unreadable label is no failure.
            let by_statuses = self.jobs[at]
                .job
                .statuses
                .values()
                .any(|s| *s != Statuses::SUCCESS);
            match self.decide(at, github_ctx, by_statuses) {
                Ok(true) => {
                    let (conclusion, note) = self.forgive(at, Conclusion::Failure, github_ctx);
                    let message = match note {
                        Some(note) => format!("{message} {note}"),
                        None => message,
                    };
                    self.end(at, conclusion, Some(message), &mut actions);
                    self.fail_fast(at, &mut actions);
                }
                Ok(false) if self.cancelled => {
                    self.end(at, Conclusion::Cancelled, None, &mut actions);
                }
                Ok(false) => self.end(at, Conclusion::Skipped, None, &mut actions),
                Err(e) => {
                    let (conclusion, note) = self.forgive(at, Conclusion::Failure, github_ctx);
                    let message = match note {
                        Some(note) => format!("{} {note}", if_error(&e)),
                        None => if_error(&e),
                    };
                    self.end(at, conclusion, Some(message), &mut actions);
                    self.fail_fast(at, &mut actions);
                }
            }
        }
        actions.extend(self.advance(github_ctx));
        actions
    }

    /// The jobs a rerun made before from `stand_in`, the job that read its
    /// matrix, that the new jobs `ids` (in `rows`) leave out: each ends
    /// as skipped for this attempt, and is no part of the run.
    fn left_out(
        &mut self,
        stand_in: &str,
        attempt: u32,
        ids: &[String],
        rows: &[u64],
    ) -> Vec<Node> {
        let mut dropped = Vec::new();
        for (row, job) in self.previous.remove(stand_in).unwrap_or_default() {
            if ids.contains(&job.id) || rows.contains(&row) {
                continue;
            }
            dropped.push(Node {
                id: row,
                job: PlannedJob {
                    requires: Vec::new(),
                    statuses: BTreeMap::new(),
                    deferred: None,
                    ..job
                },
                state: State::Done(Conclusion::Skipped),
                attempt,
                result: None,
                message: Some(NOT_IN_MATRIX.to_owned()),
                environment: None,
                cancel: 0,
                expanded_from: Some(stand_in.to_owned()),
                stale: true,
            });
        }
        dropped
    }

    /// Numbers the jobs of each strategy group in graph order and counts
    /// them, for `strategy.job-index` and `strategy.job-total`.
    fn number_groups(&mut self) {
        let mut totals: BTreeMap<String, u32> = BTreeMap::new();
        for strategy in self
            .jobs
            .iter()
            .filter(|n| !n.stale)
            .filter_map(|n| n.job.strategy.as_ref())
        {
            *totals.entry(strategy.group.clone()).or_default() += 1;
        }
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        for strategy in self
            .jobs
            .iter_mut()
            .filter(|n| !n.stale)
            .filter_map(|n| n.job.strategy.as_mut())
        {
            let next = seen.entry(strategy.group.clone()).or_default();
            strategy.job_index = *next;
            strategy.job_total = totals[&strategy.group];
            *next += 1;
        }
    }

    /// What `job` reads as `needs`, grouped by the name of the jobs it
    /// requires. Jobs that have not ended are left out.
    #[must_use]
    pub fn needs(&self, job: u64) -> BTreeMap<String, Need> {
        self.position(job)
            .map(|at| self.needs_at(at))
            .unwrap_or_default()
    }

    /// How the run ended, or `None` while a job has not.
    #[must_use]
    pub fn conclusion(&self) -> Option<Conclusion> {
        let mut conclusions = Vec::with_capacity(self.jobs.len());
        for node in self.jobs.iter().filter(|n| !n.stale) {
            let State::Done(c) = node.state else {
                return None;
            };
            conclusions.push(c);
        }
        Some(combine(conclusions))
    }

    /// The job with this row id.
    #[must_use]
    pub fn node(&self, job: u64) -> Option<&Node> {
        self.jobs.iter().find(|n| n.id == job)
    }

    fn node_mut(&mut self, job: u64) -> Option<&mut Node> {
        self.jobs.iter_mut().find(|n| n.id == job)
    }

    fn position(&self, job: u64) -> Option<usize> {
        self.jobs.iter().position(|n| n.id == job)
    }

    fn position_of(&self, id: &str) -> Option<usize> {
        self.jobs.iter().position(|n| n.job.id == id && !n.stale)
    }

    fn requirements_done(&self, at: usize) -> bool {
        self.jobs[at].job.requires.iter().all(|r| {
            self.position_of(r)
                .is_some_and(|p| matches!(self.jobs[p].state, State::Done(_)))
        })
    }

    fn needs_at(&self, at: usize) -> BTreeMap<String, Need> {
        let job = &self.jobs[at].job;
        // A job of a called workflow reads the jobs of its own call: what
        // the call waits for reaches it as inputs, not as `needs`.
        let inside: Vec<String> = job
            .requires
            .iter()
            .filter(|id| {
                self.position_of(id)
                    .is_none_or(|p| shared_depth(&job.scope, &self.jobs[p].job) == job.scope.len())
            })
            .cloned()
            .collect();
        self.needs_for(&job.scope, &inside)
    }

    /// What a job that runs in `scope` and requires `requires` reads as
    /// `needs`: the jobs of its own workflow by name, and each call it
    /// requires as one entry, with the outputs of the call.
    fn needs_for(&self, scope: &[CallFrame], requires: &[String]) -> BTreeMap<String, Need> {
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for id in requires {
            if let Some(p) = self.position_of(id)
                && matches!(self.jobs[p].state, State::Done(_))
            {
                groups
                    .entry(entry_in(scope, &self.jobs[p].job).to_owned())
                    .or_default()
                    .push(p);
            }
        }
        groups
            .into_iter()
            .map(|(name, members)| {
                let depth = shared_depth(scope, &self.jobs[members[0]].job);
                (name, self.group_need(depth, members))
            })
            .collect()
    }

    /// The result and outputs of the jobs `members` (positions) that one
    /// entry made, read from `depth` frames in. A job of the workflow
    /// itself, or its matrix entries, merge in plan order. The jobs of a
    /// call give the outputs the callee declares.
    fn group_need(&self, depth: usize, mut members: Vec<usize>) -> Need {
        members.sort_unstable();
        members.dedup();
        let result = combine(members.iter().filter_map(|&p| match self.jobs[p].state {
            State::Done(c) => Some(c),
            _ => None,
        }));
        let mut outputs: BTreeMap<String, String> = BTreeMap::new();
        let mut merge = |from: &BTreeMap<String, String>| {
            for (k, v) in from {
                match outputs.get_mut(k) {
                    Some(slot) if !v.is_empty() => slot.clone_from(v),
                    Some(_) => {}
                    None => {
                        outputs.insert(k.clone(), v.clone());
                    }
                }
            }
        };
        if self.jobs[members[0]].job.scope.len() > depth {
            let mut seen: Vec<&str> = Vec::new();
            for &p in &members {
                let id = self.jobs[p].job.scope[depth].id.as_str();
                if !seen.contains(&id) {
                    seen.push(id);
                    merge(&self.call_outputs(depth, id));
                }
            }
        } else {
            for &p in &members {
                if let Some(r) = &self.jobs[p].result {
                    merge(&r.outputs);
                }
            }
        }
        Need { result, outputs }
    }

    /// What the call `instance`, at frame `depth`, gives back: each
    /// `on.call.outputs` of its workflow read over the jobs it ran.
    fn call_outputs(&self, depth: usize, instance: &str) -> BTreeMap<String, String> {
        let inside: Vec<usize> = (0..self.jobs.len())
            .filter(|&p| {
                matches!(self.jobs[p].state, State::Done(_))
                    && !self.jobs[p].stale
                    && self.jobs[p]
                        .job
                        .scope
                        .get(depth)
                        .is_some_and(|f| f.id == instance)
            })
            .collect();
        let Some(&first) = inside.first() else {
            return BTreeMap::new();
        };
        let frame = &self.jobs[first].job.scope[depth];
        if frame.outputs.is_empty() {
            return BTreeMap::new();
        }
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for &p in &inside {
            let job = &self.jobs[p].job;
            let name = job
                .scope
                .get(depth + 1)
                .map_or(&job.name, |f| &f.name)
                .clone();
            groups.entry(name).or_default().push(p);
        }
        let jobs: BTreeMap<String, Need> = groups
            .into_iter()
            .map(|(name, members)| (name, self.group_need(depth + 1, members)))
            .collect();
        let ctx = json!({ "jobs": jobs });
        frame
            .outputs
            .iter()
            .map(|(name, text)| {
                let value = read_value(&Value::String(text.clone()), &ctx, JobStatus::Success)
                    .map(|v| match v {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                (name.clone(), value)
            })
            .collect()
    }

    /// The requires of the call at frame `depth` of the job at `at`
    /// that point outside the call, and whether any job of the call that
    /// has one asks for a status other than `success`.
    fn call_edges(&self, at: usize, depth: usize) -> (Vec<String>, bool) {
        let instance = &self.jobs[at].job.scope[depth].id;
        let inside: Vec<&PlannedJob> = self
            .jobs
            .iter()
            .filter(|n| !n.stale)
            .map(|n| &n.job)
            .filter(|j| j.scope.get(depth).is_some_and(|f| f.id == *instance))
            .collect();
        let mut out: Vec<String> = Vec::new();
        let mut statuses = false;
        for job in &inside {
            let before = out.len();
            for id in &job.requires {
                if !inside.iter().any(|j| j.id == *id) && !out.contains(id) {
                    out.push(id.clone());
                }
            }
            let outside = job
                .requires
                .iter()
                .any(|id| !inside.iter().any(|j| j.id == *id));
            if outside || out.len() > before {
                statuses |= job.statuses.values().any(|s| *s != Statuses::SUCCESS);
            }
        }
        // What an outer call waits for is not this call's own: it is read
        // by the outer call's `if` and `with`, not passed on.
        let parent = &self.jobs[at].job.scope[..depth];
        out.retain(|id| {
            self.position_of(id)
                .is_none_or(|p| shared_depth(parent, &self.jobs[p].job) == depth)
        });
        (out, statuses)
    }

    /// The `strategy` context of the call at frame `depth` of the job at
    /// `at`: its place among the instances of one call matrix.
    fn frame_strategy(&self, at: usize, depth: usize) -> Value {
        let scope = &self.jobs[at].job.scope;
        let frame = &scope[depth];
        let mut instances: Vec<&str> = Vec::new();
        for node in self.jobs.iter().filter(|n| !n.stale) {
            let other = &node.job.scope;
            let same = other.len() > depth
                && other[depth].name == frame.name
                && other[..depth]
                    .iter()
                    .zip(&scope[..depth])
                    .all(|(a, b)| a.id == b.id)
                && other[..depth].len() == depth;
            if same && !instances.contains(&other[depth].id.as_str()) {
                instances.push(&other[depth].id);
            }
        }
        let index = instances
            .iter()
            .position(|id| *id == frame.id)
            .unwrap_or_default();
        json!({
            "fail-fast": true,
            "job-index": index,
            "job-total": instances.len().max(1),
            "max-parallel": instances.len().max(1),
        })
    }

    /// The `inputs` of the job at `at` (those of the innermost call it
    /// runs in, each call's `with` read in the contexts of the workflow
    /// that made it; a manual run's for a job of the workflow that was
    /// started) and the contexts of each call it runs in, for reading
    /// the call's `if`. Read once for each evaluation.
    ///
    /// # Errors
    ///
    /// A message when an input cannot be read.
    fn walk(
        &self,
        at: usize,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Result<Walked, String> {
        let walked = self.walk_gated(at, github_ctx, vars, &mut |_, _| Ok(true))?;
        walked.ok_or_else(|| "a call stopped the walk".to_owned())
    }

    /// [`Graph::walk`] that asks `gate` about each call, with the call's
    /// index and contexts, before it reads the inputs of that call. A
    /// call that is skipped gives its workflow no inputs to read, so one
    /// that could not be read is no failure: the walk ends with `None`.
    fn walk_gated(
        &self,
        at: usize,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
        gate: &mut dyn FnMut(usize, &Value) -> Result<bool, String>,
    ) -> Result<Option<Walked>, String> {
        let mut inputs = github_ctx
            .get("event")
            .and_then(|e| e.get("inputs"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let frames = &self.jobs[at].job.scope;
        for (i, frame) in frames.iter().enumerate() {
            let ctx = json!({
                "hatchrun": hatchrun_context(github_ctx),
                "github": github_ctx,
                "needs": self.needs_for(&frames[..i], &self.call_edges(at, i).0),
                "matrix": frame.matrix,
                "strategy": self.frame_strategy(at, i),
                "vars": vars,
                "inputs": inputs,
            });
            if !gate(i, &ctx)? {
                return Ok(None);
            }
            inputs = resolve_frame(frame, &ctx)?;
        }
        Ok(Some(Walked { inputs }))
    }

    /// The `inputs` of a job that runs in a call, `None` for a job of
    /// the workflow that was started.
    ///
    /// # Errors
    ///
    /// A message when an input cannot be read.
    pub fn call_inputs(
        &self,
        job: u64,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Result<Option<Value>, String> {
        let Some(at) = self.position(job) else {
            return Ok(None);
        };
        if self.jobs[at].job.scope.is_empty() {
            return Ok(None);
        }
        self.walk(at, github_ctx, vars).map(|w| Some(w.inputs))
    }

    /// The status `success()` and `failure()` see: over every ancestor
    /// through `requires`, not only the direct ones. `Failure` when any
    /// ancestor failed (or was cancelled in a run that is not cancelled),
    /// `Success` when all succeeded, else `Skipped`.
    fn ancestor_status(&self, at: usize) -> JobStatus {
        self.ancestors(&self.jobs[at].job.requires, &self.jobs[at].job.scope)
    }

    /// [`Graph::ancestor_status`] from the jobs `seeds` name, looking
    /// only at jobs that run in `within`, a call or the whole run: a
    /// job of a called workflow does not see the jobs the call waits
    /// for, which the call's own `if` reads.
    fn ancestors(&self, seeds: &[String], within: &[CallFrame]) -> JobStatus {
        let mut seen = vec![false; self.jobs.len()];
        let mut skipped = false;
        for seed in seeds {
            let Some(root) = self.position_of(seed) else {
                continue;
            };
            // A seed that is a job of a call counts as the call: the walk
            // from it stays inside that call.
            let job = &self.jobs[root].job;
            if shared_depth(within, job) < within.len() {
                continue;
            }
            let limit = &job.scope[..job.scope.len().min(shared_depth(within, job) + 1)];
            let mut stack: Vec<usize> = vec![root];
            while let Some(p) = stack.pop() {
                if std::mem::replace(&mut seen[p], true) {
                    continue;
                }
                match self.jobs[p].state {
                    State::Done(Conclusion::Success) => {}
                    State::Done(Conclusion::Skipped) => skipped = true,
                    _ => return JobStatus::Failure,
                }
                for r in &self.jobs[p].job.requires {
                    let Some(next) = self.position_of(r) else {
                        continue;
                    };
                    if shared_depth(limit, &self.jobs[next].job) >= limit.len() {
                        stack.push(next);
                    }
                }
            }
        }
        if skipped {
            JobStatus::Skipped
        } else {
            JobStatus::Success
        }
    }

    /// How the job with this plan id ended, as a `requires` entry sees
    /// it, or `None` while it has not.
    fn outcome_of(&self, id: &str) -> Option<Outcome> {
        match self.jobs[self.position_of(id)?].state {
            State::Done(Conclusion::Success) => Some(Outcome::Success),
            State::Done(Conclusion::Skipped) => Some(Outcome::NotRun),
            State::Done(Conclusion::Failure) => Some(Outcome::Failed),
            State::Done(Conclusion::Cancelled) => Some(Outcome::Canceled),
            _ => None,
        }
    }

    /// Whether the job at `at` can start by its `requires` statuses. A
    /// job whose statuses are all the default, `success`, always can:
    /// its `if:` decides alone, so that `always()` and `failure()` keep
    /// working there.
    fn ready_at(&self, at: usize) -> Ready {
        let job = &self.jobs[at].job;
        if job.statuses.values().all(|s| *s == Statuses::SUCCESS) {
            return Ready::Start;
        }
        let plan = Plan {
            workflow: String::new(),
            concurrency: None,
            run_name: None,
            jobs: self.jobs.iter().map(|n| n.job.clone()).collect(),
        };
        let job = &plan.jobs[at];
        ready(&plan, job, |id| self.outcome_of(id))
    }

    /// Whether the job at `at` may run, by its `if:`. `statuses_ok`: its
    /// `requires` statuses were met and are not the default, so they
    /// stand in for the implied `success()` of an `if:` that calls no
    /// status function.
    fn decide(&self, at: usize, github_ctx: &Value, statuses_ok: bool) -> Result<bool, ExprError> {
        self.decide_recording(at, github_ctx, statuses_ok, &mut Vec::new())
    }

    /// [`Graph::decide`], that also tells which call instances it let in
    /// or kept out for the first time, for [`Graph::admitted`].
    fn decide_recording(
        &self,
        at: usize,
        github_ctx: &Value,
        statuses_ok: bool,
        decided: &mut Vec<(String, bool)>,
    ) -> Result<bool, ExprError> {
        let node = &self.jobs[at];
        let status = if self.cancelled {
            JobStatus::Cancelled
        } else if statuses_ok && !node.job.if_.as_deref().map_or(Ok(false), checks_status)? {
            JobStatus::Success
        } else {
            self.ancestor_status(at)
        };
        // Each call's `if` is read once, against the call's own needs,
        // before the inputs of that call are read.
        let walked = self
            .walk_gated(at, github_ctx, &self.vars, &mut |i, ctx| {
                let frame = &node.job.scope[i];
                if let Some(held) = self.admitted.get(&frame.id) {
                    return Ok(*held);
                }
                let held = (|| {
                    let (roots, by_statuses) = self.call_edges(at, i);
                    let status = if self.cancelled {
                        JobStatus::Cancelled
                    } else if by_statuses
                        && !frame.if_.as_deref().map_or(Ok(false), checks_status)?
                    {
                        JobStatus::Success
                    } else {
                        self.ancestors(&roots, &node.job.scope[..i])
                    };
                    job_condition(frame.if_.as_deref(), ctx, &Status(status))
                })();
                let held = held.map_err(|e| e.to_string())?;
                if !self.cancelled {
                    decided.push((frame.id.clone(), held));
                }
                Ok(held)
            })
            .map_err(|message| ExprError { message, offset: 0 })?;
        let Some(walked) = walked else {
            return Ok(false);
        };
        let ctx = self.context_with(at, github_ctx, &self.vars, &walked);
        // A job whose matrix is read when it is ready sees an empty matrix
        // now: an `if` that reads the matrix is read by each job the
        // matrix makes. Only the implicit `success()` is read here.
        let own = node.job.if_.as_deref();
        let waits_for_legs = node
            .job
            .deferred
            .as_ref()
            .is_some_and(|d| !d.matrix.is_empty())
            && own.is_some_and(|cond| reads_context(cond, "matrix"));
        if waits_for_legs {
            return may_hold_without(own.unwrap_or_default(), "matrix", &ctx, &Status(status));
        }
        job_condition(own, &ctx, &Status(status))
    }

    /// The context of job `job`'s own expressions, as the graph builds it
    /// (`needs`, `matrix`, `strategy`, `inputs`), with the repository
    /// and organization variables `vars` it was not given. `None` for a
    /// job the graph does not hold.
    ///
    /// # Errors
    ///
    /// A message when the `inputs` of a job in a call cannot be read.
    #[must_use]
    pub fn job_context(
        &self,
        job: u64,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Option<Result<Value, String>> {
        let at = self.position(job)?;
        Some(self.context(at, github_ctx, vars))
    }

    /// The contexts the control plane gives an expression of the job at
    /// `at`: what it knows without a runner.
    ///
    /// # Errors
    ///
    /// A message when the `inputs` of a job in a call cannot be read.
    fn context(
        &self,
        at: usize,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Result<Value, String> {
        let walked = self.walk(at, github_ctx, vars)?;
        Ok(self.context_with(at, github_ctx, vars, &walked))
    }

    fn context_with(
        &self,
        at: usize,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
        walked: &Walked,
    ) -> Value {
        let node = &self.jobs[at];
        json!({
            "hatchrun": hatchrun_context(github_ctx),
            "github": github_ctx,
            "needs": self.needs_at(at),
            "matrix": node.job.matrix,
            "strategy": node.job.strategy_context(),
            "vars": vars,
            "inputs": walked.inputs,
        })
    }

    /// What a job that ended as `conclusion` counts as: a failed job
    /// whose `continue-on-error` holds is a success, for the jobs that
    /// require it and for the run. The note says so on the job, or why
    /// the expression could not be read.
    fn forgive(
        &self,
        at: usize,
        conclusion: Conclusion,
        github_ctx: &Value,
    ) -> (Conclusion, Option<String>) {
        let spec = match self.jobs[at].job.continue_on_error.as_deref() {
            Some(spec) if conclusion == Conclusion::Failure => spec,
            _ => return (conclusion, None),
        };
        let ctx = match self.context(at, github_ctx, &self.vars) {
            Ok(ctx) => ctx,
            Err(message) => return (conclusion, Some(format!("continue-on-error: {message}"))),
        };
        match continue_on_error(spec, &ctx, &Status(JobStatus::Failure)) {
            Ok(true) => (Conclusion::Success, Some(CONTINUED.to_owned())),
            Ok(false) => (conclusion, None),
            Err(e) => (conclusion, Some(format!("continue-on-error: {e}"))),
        }
    }

    /// The job at `at` failed. When it has `fail-fast`, the jobs of its
    /// matrix that have not ended stop: waiting and queued ones end as
    /// cancelled, running ones are asked to stop. A job that
    /// [`Graph::forgive`] made a success does not call this.
    fn fail_fast(&mut self, at: usize, actions: &mut Vec<Action>) {
        if self.jobs[at].state != State::Done(Conclusion::Failure) {
            return;
        }
        let Some(group) = self.jobs[at]
            .job
            .strategy
            .as_ref()
            .filter(|s| s.fail_fast)
            .map(|s| s.group.clone())
        else {
            return;
        };
        for other in 0..self.jobs.len() {
            let same = self.jobs[other]
                .job
                .strategy
                .as_ref()
                .is_some_and(|s| s.group == group);
            if other == at || !same {
                continue;
            }
            match self.jobs[other].state {
                State::Waiting | State::Queued => {
                    self.end(
                        other,
                        Conclusion::Cancelled,
                        Some(FAIL_FAST.to_owned()),
                        actions,
                    );
                }
                State::Running if self.jobs[other].cancel == 0 => {
                    self.jobs[other].cancel = 1;
                    self.jobs[other].message = Some(FAIL_FAST.to_owned());
                    actions.push(Action::Cancel {
                        job: self.jobs[other].id,
                        count: 1,
                    });
                }
                _ => {}
            }
        }
    }

    /// Decides what becomes of a waiting job whose requirements ended.
    fn start_or_end(&mut self, at: usize, github_ctx: &Value, actions: &mut Vec<Action>) {
        if matches!(self.jobs[at].job.kind, JobKind::Approval) {
            self.end(
                at,
                Conclusion::Skipped,
                Some(APPROVAL_MESSAGE.to_owned()),
                actions,
            );
            return;
        }
        let wait = self.ready_at(at);
        if wait == Ready::NotRun {
            let conclusion = if self.cancelled {
                Conclusion::Cancelled
            } else {
                Conclusion::Skipped
            };
            self.end(at, conclusion, None, actions);
            return;
        }
        let statuses_ok = self.jobs[at]
            .job
            .statuses
            .values()
            .any(|s| *s != Statuses::SUCCESS);
        let mut decided = Vec::new();
        let verdict = self.decide_recording(at, github_ctx, statuses_ok, &mut decided);
        self.admitted.extend(decided);
        let problem = match verdict {
            Ok(true) if self.jobs[at].job.deferred.is_some() => {
                // The job's own `if:` held. Each job it makes meets its
                // `if:` again with its matrix entry, and settles its own
                // environment, which can read that entry.
                self.jobs[at].state = State::Queued;
                actions.push(Action::Expand(self.jobs[at].id));
                return;
            }
            Ok(true) => match self.resolve_environment(at, github_ctx) {
                Ok(()) => {
                    self.jobs[at].state = State::Queued;
                    actions.push(Action::Launch(self.jobs[at].id));
                    return;
                }
                Err(message) => message,
            },
            Ok(false) if self.cancelled => {
                self.end(at, Conclusion::Cancelled, None, actions);
                return;
            }
            Ok(false) => {
                self.end(at, Conclusion::Skipped, None, actions);
                return;
            }
            Err(e) => if_error(&e),
        };
        let (conclusion, note) = self.forgive(at, Conclusion::Failure, github_ctx);
        let message = match note {
            Some(note) => format!("{problem} {note}"),
            None => problem,
        };
        self.end(at, conclusion, Some(message), actions);
        self.fail_fast(at, actions);
    }

    /// Settles the environment of the job at `at`: an expression in its
    /// name is evaluated against the job's contexts, now that the jobs it
    /// needs ended, and the name must be a valid one.
    fn resolve_environment(&mut self, at: usize, github_ctx: &Value) -> Result<(), String> {
        let Some(template) = self.jobs[at].job.environment.clone() else {
            return Ok(());
        };
        let name = if is_expression(&template) {
            let ctx = self.context(at, github_ctx, &self.vars)?;
            interpolate(&template, &ctx, &Status(JobStatus::Success))
                .map_err(|e| format!("environment: {e}"))?
                .trim()
                .to_owned()
        } else {
            template
        };
        valid_environment_name(&name).map_err(|rule| format!("environment `{name}`: {rule}"))?;
        self.jobs[at].environment = Some(name);
        Ok(())
    }

    fn end(
        &mut self,
        at: usize,
        conclusion: Conclusion,
        message: Option<String>,
        actions: &mut Vec<Action>,
    ) {
        let node = &mut self.jobs[at];
        node.state = State::Done(conclusion);
        node.message = message;
        actions.push(Action::Finished {
            job: node.id,
            conclusion,
        });
        self.leave_out_previous(at, actions);
    }

    /// The job at `at` ended without making jobs from its matrix: the
    /// jobs a rerun took out of the graph for it end as skipped.
    fn leave_out_previous(&mut self, at: usize, actions: &mut Vec<Action>) {
        let (id, attempt) = (self.jobs[at].job.id.clone(), self.jobs[at].attempt);
        let dropped = self.left_out(&id, attempt, &[], &[]);
        for node in &dropped {
            actions.push(Action::Finished {
                job: node.id,
                conclusion: Conclusion::Skipped,
            });
        }
        let after = at + 1;
        self.jobs.splice(after..after, dropped);
    }

    fn emit_run_done(&mut self, actions: &mut Vec<Action>) {
        if self.run_done {
            return;
        }
        if let Some(conclusion) = self.conclusion() {
            self.run_done = true;
            actions.push(Action::RunDone(conclusion));
        }
    }
}

fn if_error(e: &ExprError) -> String {
    format!("if: {e}")
}

/// `Failure` if any failed, else `Cancelled` if any was cancelled, else
/// `Success` if any succeeded, else `Skipped`.
fn combine(conclusions: impl IntoIterator<Item = Conclusion>) -> Conclusion {
    let all: Vec<Conclusion> = conclusions.into_iter().collect();
    [
        Conclusion::Failure,
        Conclusion::Cancelled,
        Conclusion::Success,
    ]
    .into_iter()
    .find(|c| all.contains(c))
    .unwrap_or(Conclusion::Skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatchrun_workflow::{compile, parse};

    const HEAD: &str = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
  t2: { runs-on: ubuntu-latest, steps: [], if: always() }
  huge: { runs-on: ubuntu-26.04-16core-arm, steps: [] }
  never: { runs-on: ubuntu-latest, steps: [], if: 'false' }
";

    fn graph_of(workflow_jobs: &str) -> Graph {
        let yaml = format!(
            "{HEAD}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: {workflow_jobs} }} }}\n"
        );
        let config = parse(&yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    fn ctx() -> Value {
        json!({ "ref": "refs/heads/dev", "event": {} })
    }

    fn id(g: &Graph, name: &str) -> u64 {
        g.jobs
            .iter()
            .find(|n| n.job.id == name)
            .unwrap_or_else(|| panic!("no job {name}"))
            .id
    }

    fn ok() -> JobResult {
        done(Conclusion::Success, &[])
    }

    fn done(conclusion: Conclusion, outputs: &[(&str, &str)]) -> JobResult {
        JobResult {
            conclusion,
            outputs: outputs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            steps: Vec::new(),
            annotations: Vec::new(),
            environment_url: None,
        }
    }

    fn run(g: &mut Graph, job: u64) {
        g.running(job);
    }

    #[test]
    fn launches_roots_first() {
        let mut g = graph_of("[t, { t: { name: b, requires: [t] } }]");
        let a = id(&g, "t");
        assert_eq!(g.advance(&ctx()), [Action::Launch(a)]);
        assert_eq!(g.node(a).unwrap().state, State::Queued);
    }

    #[test]
    fn success_starts_dependents() {
        let mut g = graph_of("[t, { t: { name: b, requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "b"));
        g.advance(&ctx());
        run(&mut g, a);
        assert_eq!(
            g.finished(a, ok(), &ctx()),
            [
                Action::Finished {
                    job: a,
                    conclusion: Conclusion::Success
                },
                Action::Launch(b)
            ]
        );
    }

    #[test]
    fn failure_skips_dependents() {
        let mut g = graph_of("[t, { t: { name: b, requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "b"));
        g.advance(&ctx());
        run(&mut g, a);
        assert_eq!(
            g.finished(a, done(Conclusion::Failure, &[]), &ctx()),
            [
                Action::Finished {
                    job: a,
                    conclusion: Conclusion::Failure
                },
                Action::Finished {
                    job: b,
                    conclusion: Conclusion::Skipped
                },
                Action::RunDone(Conclusion::Failure),
            ]
        );
        assert_eq!(g.node(b).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn always_runs_after_failure() {
        let mut g = graph_of("[t, { t2: { requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "t2"));
        g.advance(&ctx());
        let actions = g.finished(a, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(actions.last(), Some(&Action::Launch(b)));
    }

    #[test]
    fn job_if_reads_github() {
        let yaml = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
  m: { runs-on: ubuntu-latest, if: github.ref == 'refs/heads/main', steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [t, { m: { requires: [t] } }] } }
";
        let plan = compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        g.advance(&ctx());
        let actions = g.finished(1, ok(), &ctx());
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert!(actions.contains(&Action::RunDone(Conclusion::Success)));
        // On main it launches.
        let mut g = Graph::new(
            compile(&parse(yaml).unwrap(), "ci", "refs/heads/main")
                .unwrap()
                .jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        let main = json!({ "ref": "refs/heads/main", "event": {} });
        g.advance(&main);
        assert_eq!(g.finished(1, ok(), &main).last(), Some(&Action::Launch(2)));
    }

    #[test]
    fn job_if_reads_hatchrun() {
        let yaml = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
  m: { runs-on: ubuntu-latest, if: hatchrun.event_name == 'push', steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [t, { m: { requires: [t] } }] } }
";
        let graph = |event: &str| {
            let mut g = Graph::new(
                compile(&parse(yaml).unwrap(), "ci", "refs/heads/main")
                    .unwrap()
                    .jobs
                    .into_iter()
                    .zip(1u64..)
                    .map(|(j, i)| (i, j))
                    .collect(),
            );
            let github = json!({ "ref": "refs/heads/main", "event_name": event, "event": {} });
            g.advance(&github);
            g.finished(1, ok(), &github)
        };
        assert_eq!(graph("push").last(), Some(&Action::Launch(2)));
        assert!(!graph("schedule").contains(&Action::Launch(2)));
    }

    #[test]
    fn matrix_needs_merge_in_plan_order() {
        let mut g = graph_of(
            "[{ t: { name: build, matrix: { n: [1, 2, 3] }, strategy: { fail-fast: false } } }, { t: { name: after, requires: [build] } }]",
        );
        let ids: Vec<u64> = g.jobs.iter().take(3).map(|n| n.id).collect();
        let after = g.jobs[3].id;
        g.advance(&ctx());
        g.finished(ids[0], done(Conclusion::Success, &[("v", "1")]), &ctx());
        g.finished(ids[1], done(Conclusion::Failure, &[("v", "")]), &ctx());
        g.finished(ids[2], done(Conclusion::Success, &[("v", "3")]), &ctx());
        let needs = g.needs(after);
        assert_eq!(
            needs["build"],
            Need {
                result: Conclusion::Failure,
                outputs: BTreeMap::from([("v".to_owned(), "3".to_owned())]),
            }
        );
    }

    #[test]
    fn approval_is_skipped_with_a_message() {
        let mut g = graph_of("[{ hold: { type: approval } }]");
        let hold = g.jobs[0].id;
        assert_eq!(
            g.advance(&ctx()),
            [
                Action::Finished {
                    job: hold,
                    conclusion: Conclusion::Skipped
                },
                Action::RunDone(Conclusion::Skipped)
            ]
        );
        assert_eq!(
            g.node(hold).unwrap().message.as_deref(),
            Some("Approval jobs come in a later release.")
        );
    }

    #[test]
    fn every_template_launches() {
        let mut g = graph_of("[huge]");
        let job = g.jobs[0].id;
        assert_eq!(g.advance(&ctx()), [Action::Launch(job)]);
    }

    #[test]
    fn cancel_ends_waiting_and_signals_running() {
        let mut g = graph_of("[t, { t: { name: b, requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "b"));
        g.advance(&ctx());
        run(&mut g, a);
        assert_eq!(g.cancel(&ctx()), [Action::Cancel { job: a, count: 1 }]);
        assert_eq!(g.node(a).unwrap().cancel, 1);
        // A second cancel does not repeat the signal.
        assert_eq!(g.cancel(&ctx()), Vec::<Action>::new());
        let actions = g.finished(a, done(Conclusion::Cancelled, &[]), &ctx());
        assert_eq!(
            actions,
            [
                Action::Finished {
                    job: a,
                    conclusion: Conclusion::Cancelled
                },
                Action::Finished {
                    job: b,
                    conclusion: Conclusion::Cancelled
                },
                Action::RunDone(Conclusion::Cancelled),
            ]
        );
    }

    #[test]
    fn cancel_ends_queued_jobs() {
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        let actions = g.cancel(&ctx());
        assert_eq!(
            actions,
            [
                Action::Finished {
                    job: a,
                    conclusion: Conclusion::Cancelled
                },
                Action::RunDone(Conclusion::Cancelled)
            ]
        );
    }

    #[test]
    fn always_job_still_runs_after_cancel() {
        let mut g = graph_of("[t, { t2: { requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "t2"));
        g.advance(&ctx());
        run(&mut g, a);
        g.cancel(&ctx());
        let actions = g.finished(a, done(Conclusion::Cancelled, &[]), &ctx());
        assert_eq!(actions.last(), Some(&Action::Launch(b)));
    }

    #[test]
    fn rerun_emits_run_done_again() {
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        let first = g.finished(a, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(first.last(), Some(&Action::RunDone(Conclusion::Failure)));
        let again = g.rerun(a, &ctx());
        assert_eq!(again, [Action::Launch(a)]);
        let last = g.finished(a, ok(), &ctx());
        assert_eq!(last.last(), Some(&Action::RunDone(Conclusion::Success)));
    }

    #[test]
    fn escalate_raises_cancel_to_two() {
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        run(&mut g, a);
        assert_eq!(g.escalate(), Vec::<Action>::new());
        g.cancel(&ctx());
        assert_eq!(g.escalate(), [Action::Cancel { job: a, count: 2 }]);
        assert_eq!(g.node(a).unwrap().cancel, 2);
        assert_eq!(g.escalate(), Vec::<Action>::new());
    }

    #[test]
    fn rerun_resets_dependents() {
        let mut g =
            graph_of("[t, { t: { name: b, requires: [t] } }, { t: { name: c, requires: [b] } }]");
        let (a, b, c) = (id(&g, "t"), id(&g, "b"), id(&g, "c"));
        g.advance(&ctx());
        g.finished(a, ok(), &ctx());
        g.finished(b, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(g.node(c).unwrap().state, State::Done(Conclusion::Skipped));
        let actions = g.rerun(a, &ctx());
        assert_eq!(actions, [Action::Launch(a)]);
        let (na, nb, nc) = (g.node(a).unwrap(), g.node(b).unwrap(), g.node(c).unwrap());
        assert_eq!((na.attempt, na.state), (2, State::Queued));
        assert_eq!((nb.attempt, nb.state), (2, State::Waiting));
        assert_eq!((nc.attempt, nc.state), (2, State::Waiting));
        assert!(nb.result.is_none() && nb.message.is_none());
    }

    #[test]
    fn rerun_waits_for_the_run_to_end() {
        let mut g = graph_of("[t, { t: { name: b, requires: [t] } }]");
        let (a, b) = (id(&g, "t"), id(&g, "b"));
        g.advance(&ctx());
        g.finished(a, ok(), &ctx());
        run(&mut g, b);
        assert_eq!(g.rerun(a, &ctx()), Vec::<Action>::new());
        assert_eq!(g.node(b).unwrap().state, State::Running);
        assert_eq!(g.node(a).unwrap().attempt, 1);
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        assert_eq!(g.rerun(a, &ctx()), Vec::<Action>::new());
        assert_eq!(g.node(a).unwrap().attempt, 1);
    }

    #[test]
    fn run_done_is_emitted_once() {
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        let actions = g.finished(a, ok(), &ctx());
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::RunDone(_)))
                .count(),
            1
        );
        assert_eq!(g.advance(&ctx()), Vec::<Action>::new());
        assert_eq!(g.finished(a, ok(), &ctx()), Vec::<Action>::new());
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn a_failed_launch_ends_the_job_with_its_message() {
        let mut g = graph_of("[t]");
        let a = g.jobs[0].id;
        g.advance(&ctx());
        let actions = g.failed(a, "no capacity".into(), &ctx());
        assert_eq!(actions.last(), Some(&Action::RunDone(Conclusion::Failure)));
        assert_eq!(g.node(a).unwrap().message.as_deref(), Some("no capacity"));
    }

    #[test]
    fn a_bad_if_fails_the_job_with_the_error() {
        let yaml = "
jobs: { t: { runs-on: ubuntu-latest, if: 'true', steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
";
        // The compiler rejects it; build the node by hand to reach the
        // evaluation error.
        let mut plan = compile(
            &parse(yaml.replace("github.ref ==", "true").as_str()).unwrap(),
            "ci",
            "r",
        )
        .unwrap();
        plan.jobs[0].if_ = Some("github.ref ==".into());
        let mut g = Graph::new(vec![(1, plan.jobs.remove(0))]);
        g.advance(&ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Failure));
        assert!(
            g.node(1)
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .starts_with("if: ")
        );
    }

    #[test]
    fn failure_does_not_see_a_skipped_need() {
        // deploy is skipped by its if:, notify runs only on failure().
        let yaml = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
  deploy: { runs-on: ubuntu-latest, if: github.ref == 'refs/heads/main', steps: [] }
  notify: { runs-on: ubuntu-latest, if: failure(), steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [t, { deploy: { requires: [t] } }, { notify: { requires: [deploy] } }] } }
";
        let plan = compile(&parse(yaml).unwrap(), "ci", "r").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        g.advance(&ctx());
        let actions = g.finished(1, ok(), &ctx());
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(actions.last(), Some(&Action::RunDone(Conclusion::Success)));
    }

    #[test]
    fn failure_sees_a_failed_grandparent_behind_an_always_job() {
        let mut g = graph_of("[t, { t2: { requires: [t] } }, { t: { name: c, requires: [t2], } }]");
        // c has no if: so it needs every ancestor to succeed.
        let (a, b, c) = (id(&g, "t"), id(&g, "t2"), id(&g, "c"));
        g.advance(&ctx());
        g.finished(a, done(Conclusion::Failure, &[]), &ctx());
        g.finished(b, ok(), &ctx());
        assert_eq!(g.node(c).unwrap().state, State::Done(Conclusion::Skipped));

        let yaml = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
  mid: { runs-on: ubuntu-latest, if: always(), steps: [] }
  last: { runs-on: ubuntu-latest, if: failure(), steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [t, { mid: { requires: [t] } }, { last: { requires: [mid] } }] } }
";
        let plan = compile(&parse(yaml).unwrap(), "ci", "r").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        let actions = g.finished(2, ok(), &ctx());
        assert_eq!(actions.last(), Some(&Action::Launch(3)));
    }

    /// A graph of `a`, `b` (requires `a`) and `c` (requires `b` with
    /// `statuses`), rows 1 to 3.
    fn chain(statuses: &str) -> Graph {
        graph_of(&format!(
            "[{{ t: {{ name: a }} }}, {{ t: {{ name: b, requires: [a] }} }}, \
             {{ t: {{ name: c, requires: [{{ b: {statuses} }}] }} }}]"
        ))
    }

    fn state_of(g: &Graph, row: u64) -> State {
        g.node(row).unwrap().state
    }

    #[test]
    fn requires_default_status_is_success() {
        let mut g = chain("success");
        g.advance(&ctx());
        g.finished(1, ok(), &ctx());
        g.finished(2, ok(), &ctx());
        assert_eq!(state_of(&g, 3), State::Queued);

        let mut g = chain("success");
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(state_of(&g, 2), State::Done(Conclusion::Skipped));
        assert_eq!(state_of(&g, 3), State::Done(Conclusion::Skipped));
    }

    #[test]
    fn requires_failed_runs_after_an_ancestor_fails() {
        // B fails itself.
        let mut g = chain("failed");
        g.advance(&ctx());
        g.finished(1, ok(), &ctx());
        g.finished(2, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(state_of(&g, 3), State::Queued);

        // A fails, B does not run: B's ancestry is still failed.
        let mut g = chain("failed");
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(state_of(&g, 3), State::Queued);

        // Everything passed: C has nothing to run after.
        let mut g = chain("failed");
        g.advance(&ctx());
        g.finished(1, ok(), &ctx());
        g.finished(2, ok(), &ctx());
        assert_eq!(state_of(&g, 3), State::Done(Conclusion::Skipped));
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn requires_terminal_always_runs() {
        for first in [Conclusion::Success, Conclusion::Failure] {
            let mut g = chain("terminal");
            g.advance(&ctx());
            g.finished(1, done(first, &[]), &ctx());
            g.finished(2, ok(), &ctx());
            assert_eq!(state_of(&g, 3), State::Queued, "{first:?}");
        }
    }

    #[test]
    fn requires_not_run_follows_a_skipped_ancestor() {
        let mut g = graph_of(
            "[{ never: { name: a } }, { t: { name: b, requires: [a] } }, \
             { t: { name: c, requires: [{ b: not_run }] } }]",
        );
        g.advance(&ctx());
        // A does not run, so B does not either, and C starts on that.
        assert_eq!(state_of(&g, 1), State::Done(Conclusion::Skipped));
        assert_eq!(state_of(&g, 2), State::Done(Conclusion::Skipped));
        assert_eq!(state_of(&g, 3), State::Queued);

        // C needs success where B ran: not_run does not match.
        let mut g = chain("not_run");
        g.advance(&ctx());
        g.finished(1, ok(), &ctx());
        g.finished(2, ok(), &ctx());
        assert_eq!(state_of(&g, 3), State::Done(Conclusion::Skipped));
    }

    #[test]
    fn requires_statuses_keep_an_explicit_if() {
        let mut g = graph_of(
            "[{ t: { name: a } }, { t2: { name: b, requires: [{ a: failed }] } }, \
             { never: { name: c, requires: [{ a: failed }] } }]",
        );
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        // t2 has `if: always()`; c's own `if:` still decides.
        assert_eq!(state_of(&g, 2), State::Queued);
        assert_eq!(state_of(&g, 3), State::Done(Conclusion::Skipped));
    }

    const STRATEGY_JOBS: &str = "
jobs:
  m: { runs-on: ubuntu-latest, steps: [] }
  soft: { runs-on: ubuntu-latest, steps: [], continue-on-error: true }
  maybe: { runs-on: ubuntu-latest, steps: [], continue-on-error: '${{ matrix.experimental }}' }
  varsoft: { runs-on: ubuntu-latest, steps: [], continue-on-error: '${{ vars.SOFT }}' }
  first: { runs-on: ubuntu-latest, steps: [], if: strategy.job-index == 0 }
  after: { runs-on: ubuntu-latest, steps: [] }
";

    fn strategy_graph(workflow_jobs: &str) -> Graph {
        let yaml = format!(
            "{STRATEGY_JOBS}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: {workflow_jobs} }} }}\n"
        );
        let config = parse(&yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    fn cancelled(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Finished {
                    job,
                    conclusion: Conclusion::Cancelled,
                } => Some(*job),
                _ => None,
            })
            .collect()
    }

    fn asked_to_stop(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Cancel { job, count: 1 } => Some(*job),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_failed_leg_cancels_the_rest_of_its_matrix() {
        let mut g = strategy_graph("[{ m: { matrix: { os: [a, b, c, d] } } }]");
        g.advance(&ctx());
        g.running(1);
        g.running(2);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        // Queued legs end as cancelled, the running one is asked to stop.
        assert_eq!(cancelled(&actions), [3, 4]);
        assert_eq!(asked_to_stop(&actions), [2]);
        assert_eq!(g.node(2).unwrap().state, State::Running);
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Cancelled));
        assert!(
            g.node(3)
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("fail-fast")
        );
        assert!(!actions.iter().any(|a| matches!(a, Action::RunDone(_))));
        // The run ends once the running leg reports.
        let actions = g.finished(2, done(Conclusion::Cancelled, &[]), &ctx());
        assert!(actions.contains(&Action::RunDone(Conclusion::Failure)));
    }

    #[test]
    fn a_leg_that_fails_while_another_waits_for_launch_still_ends_it() {
        let mut g = strategy_graph("[{ m: { matrix: { os: [a, b] } } }]");
        g.advance(&ctx());
        g.running(1);
        let actions = g.failed(1, "the agent went silent".into(), &ctx());
        assert_eq!(cancelled(&actions), [2]);
        assert!(actions.contains(&Action::RunDone(Conclusion::Failure)));
    }

    #[test]
    fn fail_fast_false_lets_the_other_legs_run() {
        let mut g = strategy_graph(
            "[{ m: { matrix: { os: [a, b, c] }, strategy: { fail-fast: false } } }]",
        );
        g.advance(&ctx());
        g.running(1);
        g.running(2);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(cancelled(&actions), Vec::<u64>::new());
        assert_eq!(asked_to_stop(&actions), Vec::<u64>::new());
        assert_eq!(g.node(2).unwrap().state, State::Running);
        assert_eq!(g.node(3).unwrap().state, State::Queued);
    }

    #[test]
    fn fail_fast_does_not_reach_other_matrices_or_jobs() {
        let mut g = strategy_graph(
            "[{ m: { name: x, matrix: { os: [a, b] } } }, { m: { name: y, matrix: { os: [a, b] } } }, soft]",
        );
        g.advance(&ctx());
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(cancelled(&actions), [2]);
        for other in 3..=5 {
            assert_eq!(g.node(other).unwrap().state, State::Queued, "{other}");
        }
    }

    #[test]
    fn entries_of_one_group_cancel_each_other() {
        let mut g = strategy_graph(
            "[{ m: { name: x, matrix: { os: [a] }, strategy: { group: g } } }, \
              { m: { name: y, matrix: { os: [b] }, strategy: { group: g } } }]",
        );
        g.advance(&ctx());
        let actions = g.finished(2, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(cancelled(&actions), [1]);
    }

    #[test]
    fn a_waiting_leg_ends_as_cancelled_too() {
        let mut g = strategy_graph(
            "[{ m: { name: x, matrix: { os: [a, b] }, strategy: { group: g } } }, \
              { m: { name: y, requires: [after], matrix: { os: [c] }, strategy: { group: g } } }, \
              after]",
        );
        g.advance(&ctx());
        assert_eq!(
            g.node(4).unwrap().state,
            State::Waiting,
            "y waits for after"
        );
        assert_eq!(g.node(3).unwrap().state, State::Queued);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(cancelled(&actions), [2, 4]);
    }

    #[test]
    fn a_failed_job_with_continue_on_error_counts_as_a_success() {
        let mut g = strategy_graph("[soft, { after: { requires: [soft] } }]");
        g.advance(&ctx());
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(
            actions[0],
            Action::Finished {
                job: 1,
                conclusion: Conclusion::Success
            }
        );
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Success));
        assert!(
            g.node(1)
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("continue-on-error")
        );
        assert_eq!(
            g.node(1).unwrap().result.as_ref().unwrap().conclusion,
            Conclusion::Failure,
            "the report is kept as the agent sent it"
        );
        // The job that requires it starts, and sees a success.
        assert_eq!(state_of(&g, 2), State::Queued);
        assert_eq!(g.needs(2)["soft"].result, Conclusion::Success);
        g.finished(2, ok(), &ctx());
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn continue_on_error_reads_the_matrix_entry() {
        let mut g = strategy_graph(
            "[{ maybe: { matrix: { experimental: [false, true] }, strategy: { fail-fast: false } } }]",
        );
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        g.finished(2, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Failure));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Success));
        assert_eq!(g.conclusion(), Some(Conclusion::Failure));
    }

    #[test]
    fn continue_on_error_reads_the_variables_the_graph_was_given() {
        let mut g = strategy_graph("[varsoft]");
        g.advance(&ctx());
        g.set_vars(BTreeMap::from([("SOFT".to_owned(), "true".to_owned())]));
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Success));
        let mut g = strategy_graph("[varsoft]");
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Failure));
    }

    #[test]
    fn a_forgiven_leg_does_not_trigger_fail_fast() {
        let mut g = strategy_graph("[{ maybe: { matrix: { experimental: [true, false] } } }]");
        g.advance(&ctx());
        g.running(2);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(cancelled(&actions), Vec::<u64>::new());
        assert_eq!(asked_to_stop(&actions), Vec::<u64>::new());
        g.finished(2, ok(), &ctx());
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn a_forgiven_if_error_does_not_trigger_fail_fast() {
        let mut plan = compile(
            &parse(&format!(
                "{STRATEGY_JOBS}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ maybe: {{ matrix: {{ experimental: [true, false] }} }} }}] }} }}\n"
            ))
            .unwrap(),
            "ci",
            "refs/heads/main",
        )
        .unwrap();
        // The compiler rejects a bad `if:`; set it by hand on the
        // experimental leg to reach the evaluation error.
        plan.jobs[0].if_ = Some("github.ref ==".into());
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        );
        let actions = g.advance(&ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Success));
        let message = g.node(1).unwrap().message.clone().unwrap();
        assert!(message.starts_with("if: ") && message.contains("continue-on-error"));
        assert_eq!(cancelled(&actions), Vec::<u64>::new());
        assert_eq!(g.node(2).unwrap().state, State::Queued);
    }

    #[test]
    fn a_job_the_control_plane_fails_can_be_forgiven_too() {
        let mut g = strategy_graph("[soft]");
        g.advance(&ctx());
        g.running(1);
        let actions = g.failed(1, "no capacity".into(), &ctx());
        assert_eq!(
            actions[0],
            Action::Finished {
                job: 1,
                conclusion: Conclusion::Success
            }
        );
        let message = g.node(1).unwrap().message.clone().unwrap();
        assert!(message.contains("no capacity") && message.contains("continue-on-error"));
    }

    #[test]
    fn a_cancelled_job_is_not_forgiven() {
        let mut g = strategy_graph("[soft]");
        g.advance(&ctx());
        g.finished(1, done(Conclusion::Cancelled, &[]), &ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Cancelled));
    }

    #[test]
    fn a_continue_on_error_that_cannot_be_read_leaves_the_failure() {
        let mut g = strategy_graph("[{ maybe: {} }]");
        g.advance(&ctx());
        // `matrix.experimental` is not set: the expression is null, not true.
        g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert_eq!(g.node(1).unwrap().state, State::Done(Conclusion::Failure));
    }

    /// The graph of the workflow `ci` of `yaml`, jobs numbered from 1.
    fn graph_from(yaml: &str) -> Graph {
        let config = parse(yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    const ENVIRONMENTS: &str = r#"
jobs:
  pick: { runs-on: ubuntu-latest, outputs: { stage: "${{ steps.s.outputs.v }}" }, steps: [] }
  deploy:
    runs-on: ubuntu-latest
    parameters: { region: { type: string } }
    environment: "${{ needs.pick.outputs.stage }}-${{ matrix.zone }}"
    steps: []
  fixed: { runs-on: ubuntu-latest, environment: production, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - pick
      - fixed
      - deploy: { requires: [pick], matrix: { region: [x], zone: [a, b] } }
"#;

    #[test]
    fn an_environment_expression_is_evaluated_when_the_job_is_ready() {
        let mut g = graph_from(ENVIRONMENTS);
        // A name that is plain text is known from the start; an
        // expression is not.
        assert_eq!(
            g.node(2).unwrap().environment.as_deref(),
            Some("production")
        );
        assert_eq!(g.node(3).unwrap().environment, None);
        g.advance(&ctx());
        assert_eq!(g.node(3).unwrap().state, State::Waiting);
        assert_eq!(g.node(3).unwrap().environment, None);
        let finished = JobResult {
            outputs: BTreeMap::from([("stage".to_owned(), "prod".to_owned())]),
            ..ok()
        };
        let actions = g.finished(1, finished, &ctx());
        assert!(actions.contains(&Action::Launch(3)));
        assert!(actions.contains(&Action::Launch(4)));
        assert_eq!(g.node(3).unwrap().environment.as_deref(), Some("prod-a"));
        assert_eq!(g.node(4).unwrap().environment.as_deref(), Some("prod-b"));
        // The plan keeps the expression, for a rerun to read again.
        assert!(
            g.node(3)
                .unwrap()
                .job
                .environment
                .as_deref()
                .unwrap()
                .contains("${{")
        );
    }

    #[test]
    fn an_environment_name_that_is_not_valid_ends_the_job_as_failed() {
        let mut g = graph_from(ENVIRONMENTS);
        g.advance(&ctx());
        let finished = JobResult {
            outputs: BTreeMap::from([("stage".to_owned(), "prod eu".to_owned())]),
            ..ok()
        };
        let actions = g.finished(1, finished, &ctx());
        assert!(!actions.contains(&Action::Launch(3)));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Failure));
        let message = g.node(3).unwrap().message.clone().unwrap();
        assert!(message.contains("environment `prod eu-a`"), "{message}");
    }

    #[test]
    fn an_environment_name_that_cannot_be_evaluated_ends_the_job_as_failed() {
        let yaml = ENVIRONMENTS.replace(
            "${{ needs.pick.outputs.stage }}-${{ matrix.zone }}",
            "${{ fromJSON(needs.pick.outputs.stage) }}",
        );
        let mut g = graph_from(&yaml);
        g.advance(&ctx());
        let finished = JobResult {
            outputs: BTreeMap::from([("stage".to_owned(), "{".to_owned())]),
            ..ok()
        };
        g.finished(1, finished, &ctx());
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Failure));
        let message = g.node(3).unwrap().message.clone().unwrap();
        assert!(message.starts_with("environment: "), "{message}");
    }

    #[test]
    fn an_environment_name_can_read_vars() {
        let yaml = ENVIRONMENTS.replace(
            "${{ needs.pick.outputs.stage }}-${{ matrix.zone }}",
            "${{ vars.STAGE }}-${{ matrix.zone }}",
        );
        let mut g = graph_from(&yaml);
        assert!(g.reads_vars());
        g.set_vars(BTreeMap::from([("STAGE".to_owned(), "qa".to_owned())]));
        g.advance(&ctx());
        g.finished(1, ok(), &ctx());
        assert_eq!(g.node(3).unwrap().environment.as_deref(), Some("qa-a"));
        assert!(!graph_from(ENVIRONMENTS).reads_vars());
    }

    #[test]
    fn if_reads_the_strategy_context() {
        let mut g = strategy_graph("[{ first: { matrix: { os: [a, b, c] } } }]");
        g.advance(&ctx());
        assert_eq!(g.node(1).unwrap().state, State::Queued);
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn a_pending_run_cancelled_outright_ends_whatever_its_if() {
        let mut g = graph_of("[t, { t2: { name: b, requires: [t] } }]");
        g.advance(&ctx());
        let actions = g.cancel_for_group("replaced", true, &ctx());
        assert_eq!(state_of(&g, 1), State::Done(Conclusion::Cancelled));
        // `if: always()` does not keep it: the run never started.
        assert_eq!(state_of(&g, 2), State::Done(Conclusion::Cancelled));
        assert_eq!(g.node(2).unwrap().message.as_deref(), Some("replaced"));
        assert!(actions.contains(&Action::RunDone(Conclusion::Cancelled)));
    }

    #[test]
    fn cancelling_a_running_run_for_a_group_keeps_always_jobs() {
        let mut g = graph_of("[t, { t2: { name: b, requires: [t] } }]");
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.cancel_for_group("newer run", false, &ctx());
        assert!(actions.contains(&Action::Cancel { job: 1, count: 1 }));
        assert_eq!(g.node(1).unwrap().message.as_deref(), Some("newer run"));
        g.finished(1, done(Conclusion::Cancelled, &[]), &ctx());
        // The job with `if: always()` still runs after the cancel.
        assert_eq!(state_of(&g, 2), State::Queued);
        assert_eq!(g.node(1).unwrap().message.as_deref(), Some("newer run"));
    }

    #[test]
    fn cancel_job_ends_a_queued_job_and_asks_a_running_one_to_stop() {
        let mut g = graph_of("[t, { t: { name: b } }]");
        g.advance(&ctx());
        run(&mut g, 1);
        assert!(
            g.cancel_job(1, 2, "x", &ctx()).is_empty(),
            "another attempt"
        );
        assert_eq!(
            g.cancel_job(1, 1, "x", &ctx()),
            [Action::Cancel { job: 1, count: 1 }]
        );
        assert!(g.cancel_job(1, 1, "x", &ctx()).is_empty(), "once");
        let actions = g.cancel_job(2, 1, "newer job", &ctx());
        assert!(actions.contains(&Action::Finished {
            job: 2,
            conclusion: Conclusion::Cancelled
        }));
        assert_eq!(g.node(2).unwrap().message.as_deref(), Some("newer job"));
        // The run goes on until job 1 stops.
        assert_eq!(g.conclusion(), None);
    }

    const DYNAMIC: &str = "
jobs:
  setup: { runs-on: ubuntu-latest, steps: [], outputs: { os: x } }
  test: { runs-on: '${{ matrix.os }}', steps: [], if: \"matrix.os != 'skipme'\" }
  after: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - setup
      - test:
          requires: [setup]
          strategy: { fail-fast: false }
          matrix: { os: '${{ fromJSON(needs.setup.outputs.os) }}' }
      - after: { requires: [test] }
";

    fn dynamic() -> Graph {
        let config = parse(DYNAMIC).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap();
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    /// Runs setup with `os` as its output and returns the actions.
    fn after_setup(g: &mut Graph, os: &str) -> Vec<Action> {
        assert_eq!(g.advance(&ctx()), [Action::Launch(1)]);
        run(g, 1);
        g.finished(1, done(Conclusion::Success, &[("os", os)]), &ctx())
    }

    #[test]
    fn a_dynamic_job_expands_when_its_requirements_ended() {
        let mut g = dynamic();
        let actions = after_setup(
            &mut g,
            r#"["ubuntu-latest","ubuntu-26.04-arm","skipme","windows"]"#,
        );
        assert_eq!(actions.last(), Some(&Action::Expand(2)));
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        assert_eq!(legs.len(), 4);
        let rows = vec![
            (2, legs[0].clone()),
            (10, legs[1].clone()),
            (11, legs[2].clone()),
            (12, legs[3].clone()),
        ];
        let actions = g.expanded(2, rows, &ctx());
        assert_eq!(
            actions,
            [
                // A label that names no template fails that job alone,
                // unless its `if:` skips it first.
                Action::Finished {
                    job: 11,
                    conclusion: Conclusion::Skipped
                },
                Action::Finished {
                    job: 12,
                    conclusion: Conclusion::Failure
                },
                Action::Launch(2),
                Action::Launch(10),
            ]
        );
        assert!(
            g.node(12)
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("names no runner template")
        );
        let ids: Vec<&str> = g.jobs.iter().map(|n| n.job.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "setup",
                "test (ubuntu-latest)",
                "test (ubuntu-26.04-arm)",
                "test (skipme)",
                "test (windows)",
                "after"
            ]
        );
        // The job after it waits for every leg.
        assert_eq!(g.node(3).unwrap().job.requires.len(), 4);
        let strategy = g.node(10).unwrap().job.strategy.as_ref().unwrap();
        assert_eq!((strategy.job_index, strategy.job_total), (1, 4));
        run(&mut g, 2);
        run(&mut g, 10);
        g.finished(2, ok(), &ctx());
        let actions = g.finished(10, ok(), &ctx());
        // Two legs failed, so the job after them is skipped.
        assert!(actions.contains(&Action::Finished {
            job: 3,
            conclusion: Conclusion::Skipped
        }));
    }

    #[test]
    fn a_matrix_that_cannot_be_read_fails_the_job_and_skips_its_dependents() {
        let mut g = dynamic();
        let actions = after_setup(&mut g, "[]");
        assert_eq!(actions.last(), Some(&Action::Expand(2)));
        let message = g
            .expansion(2, &ctx(), &BTreeMap::new())
            .unwrap()
            .unwrap_err();
        let actions = g.failed(2, message, &ctx());
        assert!(actions.contains(&Action::Finished {
            job: 2,
            conclusion: Conclusion::Failure
        }));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Skipped));
        assert!(
            g.node(2)
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("no values")
        );
    }

    #[test]
    fn a_cancelled_run_makes_no_jobs() {
        let mut g = dynamic();
        g.advance(&ctx());
        run(&mut g, 1);
        g.cancel(&ctx());
        let actions = g.finished(1, done(Conclusion::Cancelled, &[]), &ctx());
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Cancelled));
    }

    #[test]
    fn a_failed_setup_skips_the_dynamic_job_without_expanding() {
        let mut g = dynamic();
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
    }

    fn dynamic_with(job_if: &str, matrix: &str, fail_fast: bool) -> Graph {
        let yaml = format!(
            "
jobs:
  setup: {{ runs-on: ubuntu-latest, steps: [], outputs: {{ os: x }} }}
  test: {{ runs-on: '${{{{ matrix.os }}}}', steps: [], if: \"{job_if}\" }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - setup
      - test:
          requires: [setup]
          strategy: {{ fail-fast: {fail_fast} }}
          matrix: {matrix}
"
        );
        let config = parse(&yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        )
    }

    const FROM: &str = "'${{ fromJSON(needs.setup.outputs.os) }}'";

    #[test]
    fn the_job_if_is_read_before_the_matrix() {
        let mut g = dynamic_with("needs.setup.outputs.os != '[]'", FROM, true);
        let actions = after_setup(&mut g, "[]");
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn an_empty_matrix_skips_the_job() {
        for empty in ["[]", "{}"] {
            let mut g = dynamic_with("true", FROM, true);
            let actions = after_setup(&mut g, empty);
            assert_eq!(actions.last(), Some(&Action::Expand(2)));
            let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
            assert!(legs.is_empty(), "{empty}");
            let actions = g.no_jobs(2, &ctx());
            assert!(actions.contains(&Action::Finished {
                job: 2,
                conclusion: Conclusion::Skipped
            }));
            assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        }
    }

    #[test]
    fn a_leg_ends_once_when_fail_fast_cancels_a_later_failed_leg() {
        let mut g = dynamic_with("true", FROM, true);
        after_setup(&mut g, r#"{"os":["windows","macos"]}"#);
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows = vec![(2, legs[0].clone()), (3, legs[1].clone())];
        let actions = g.expanded(2, rows, &ctx());
        for job in [2, 3] {
            let ended = actions
                .iter()
                .filter(|a| matches!(a, Action::Finished { job: j, .. } if *j == job))
                .count();
            assert_eq!(ended, 1, "{actions:?}");
        }
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Failure));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Cancelled));
    }

    #[test]
    fn a_leg_whose_if_is_false_is_skipped_not_failed_for_its_label() {
        let mut g = dynamic_with("matrix.os != 'windows'", FROM, false);
        after_setup(&mut g, r#"{"os":["windows","ubuntu-latest"]}"#);
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows = vec![(2, legs[0].clone()), (3, legs[1].clone())];
        g.expanded(2, rows, &ctx());
        assert_eq!(g.node(2).unwrap().job.id, "test (windows)");
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.node(3).unwrap().state, State::Queued);
    }

    #[test]
    fn only_a_queued_job_expands() {
        let mut g = dynamic_with("true", FROM, true);
        g.advance(&ctx());
        assert!(g.expansion(2, &ctx(), &BTreeMap::new()).is_none());
    }

    #[test]
    fn a_dynamic_job_waits_on_the_statuses_it_requires() {
        let yaml = "
jobs:
  setup: { runs-on: ubuntu-latest, steps: [] }
  test: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - setup
      - test:
          requires: [{ setup: failed }]
          matrix: { os: '${{ fromJSON(needs.setup.outputs.os) }}' }
";
        let config = parse(yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap();
        let build = || {
            Graph::new(
                plan.jobs
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(i, j)| (i as u64 + 1, j))
                    .collect(),
            )
        };
        // Setup succeeded, so `failed` is not met: no expansion.
        let mut g = build();
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.finished(1, ok(), &ctx());
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        // Setup failed: it expands, even though that is not a success.
        let mut g = build();
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.finished(1, done(Conclusion::Failure, &[]), &ctx());
        assert!(actions.contains(&Action::Expand(2)), "{actions:?}");
    }

    const CALLS: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  pkg: { runs-on: ubuntu-latest, outputs: { tag: '${{ steps.x.outputs.tag }}' }, steps: [] }
  push: { runs-on: ubuntu-latest, steps: [] }
workflows:
  publish:
    on:
      call:
        inputs: { name: { type: string, required: true } }
        outputs: { tag: '${{ jobs.pkg.outputs.tag }}' }
    jobs: [pkg, { push: { requires: [pkg] } }]
  ci:
    on: { push: {} }
    jobs:
      - build
      - p:
          call: publish
          requires: [build]
          if: github.ref == 'refs/heads/main'
          with: { name: 'app-${{ needs.build.outputs.v }}' }
      - { push: { name: after, requires: [p] } }
";

    fn calls(ref_: &str) -> Graph {
        let plan = compile(&parse(CALLS).unwrap(), "ci", ref_).unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        )
    }

    fn github(ref_: &str) -> Value {
        serde_json::json!({ "ref": ref_, "ref_name": "main" })
    }

    #[test]
    fn a_call_reads_as_one_entry_with_the_outputs_the_callee_declares() {
        let ctx = github("refs/heads/main");
        let mut g = calls("refs/heads/main");
        let names: Vec<&str> = g.jobs.iter().map(|n| n.job.id.as_str()).collect();
        assert_eq!(names, ["build", "p / pkg", "p / push", "after"]);
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("v", "7")]), &ctx);
        // The callee's root job reads the caller's `needs` through `with`.
        let inputs = g.call_inputs(2, &ctx, &BTreeMap::new()).unwrap().unwrap();
        assert_eq!(inputs["name"], "app-7");
        run(&mut g, 2);
        g.finished(2, done(Conclusion::Success, &[("tag", "v1")]), &ctx);
        run(&mut g, 3);
        g.finished(3, ok(), &ctx);
        let needs = g.needs(4);
        assert_eq!(needs.len(), 1, "{needs:?}");
        assert_eq!(needs["p"].result, Conclusion::Success);
        assert_eq!(needs["p"].outputs["tag"], "v1");
        // Inside the call, jobs still see each other by their own names.
        assert!(g.needs(3).contains_key("pkg"));
    }

    #[test]
    fn a_call_if_that_is_false_skips_every_job_of_the_call() {
        let ctx = github("refs/heads/dev");
        let mut g = calls("refs/heads/dev");
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, ok(), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.node(3).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn a_failed_callee_job_fails_the_call_for_those_that_need_it() {
        let ctx = github("refs/heads/main");
        let mut g = calls("refs/heads/main");
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, ok(), &ctx);
        run(&mut g, 2);
        g.finished(2, done(Conclusion::Failure, &[]), &ctx);
        assert_eq!(g.node(4).unwrap().state, State::Done(Conclusion::Skipped));
    }

    const CLEANUP: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  work: { runs-on: ubuntu-latest, steps: [] }
  tidy: { runs-on: ubuntu-latest, if: always(), steps: [] }
  report: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: {} }
    jobs: [work, { tidy: { requires: [work] } }]
  ci:
    on: { push: {} }
    jobs:
      - build
      - plain: { call: inner, requires: [build] }
      - always: { call: inner, requires: [build], if: always() }
      - failing: { call: inner, requires: [build], if: failure() }
";

    fn cleanup() -> Graph {
        let plan = compile(&parse(CLEANUP).unwrap(), "ci", "refs/heads/main")
            .unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        )
    }

    fn state_of_id(g: &Graph, id: &str) -> State {
        g.jobs.iter().find(|n| n.job.id == id).unwrap().state
    }

    #[test]
    fn a_call_if_reads_the_calls_own_needs() {
        let ctx = github("refs/heads/main");
        let mut g = cleanup();
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Failure, &[]), &ctx);
        // Without an `if` a call is skipped when what it needs failed.
        assert_eq!(
            state_of_id(&g, "plain / work"),
            State::Done(Conclusion::Skipped)
        );
        assert_eq!(
            state_of_id(&g, "plain / tidy"),
            State::Done(Conclusion::Skipped)
        );
        // `always()` runs the callee, and `failure()` runs it too.
        let id = |g: &Graph, name: &str| g.jobs.iter().find(|n| n.job.id == name).unwrap().id;
        assert_eq!(state_of_id(&g, "always / work"), State::Queued);
        assert_eq!(state_of_id(&g, "failing / work"), State::Queued);
        // A callee job with `if: always()` runs after a callee job fails,
        // and one without waits for success only inside the call.
        let work = id(&g, "always / work");
        run(&mut g, work);
        g.finished(work, done(Conclusion::Failure, &[]), &ctx);
        assert_eq!(state_of_id(&g, "always / tidy"), State::Queued);
    }

    #[test]
    fn a_callee_root_job_does_not_see_what_the_call_waits_for() {
        let ctx = github("refs/heads/main");
        let mut g = cleanup();
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, ok(), &ctx);
        assert_eq!(state_of_id(&g, "plain / work"), State::Queued);
        // `failure()` is false when the call's needs passed.
        assert_eq!(
            state_of_id(&g, "failing / work"),
            State::Done(Conclusion::Skipped)
        );
    }

    #[test]
    fn a_call_matrix_instance_reads_its_own_strategy() {
        let yaml = "
jobs:
  j: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: { inputs: { n: { type: number } } } }
    jobs: [j]
  ci:
    on: { push: {} }
    jobs:
      - c:
          call: inner
          matrix: { x: [a, b, c] }
          with: { n: '${{ strategy.job-index }}' }
";
        let plan = compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        let ctx = github("refs/heads/main");
        g.advance(&ctx);
        for (row, want) in [(1, 0), (2, 1), (3, 2)] {
            let inputs = g.call_inputs(row, &ctx, &BTreeMap::new()).unwrap().unwrap();
            assert_eq!(inputs["n"], want);
        }
    }

    #[test]
    fn inputs_that_cannot_be_read_fail_the_job_with_the_message() {
        let yaml = "
jobs:
  j: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: { inputs: { flag: { type: boolean } } } }
    jobs: [j]
  ci:
    on: { push: {} }
    jobs:
      - c:
          call: inner
          matrix: { x: [maybe] }
          with: { flag: '${{ matrix.x }}' }
";
        let plan = compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        );
        let actions = g.advance(&github("refs/heads/main"));
        assert!(actions.contains(&Action::Finished {
            job: 1,
            conclusion: Conclusion::Failure
        }));
        let message = g.node(1).unwrap().message.clone().unwrap();
        assert!(message.contains("`with.flag` of `c (maybe)`"), "{message}");
    }

    #[test]
    fn each_expanded_job_resolves_its_environment_from_its_matrix_entry() {
        let yaml = "
jobs:
  setup: { runs-on: ubuntu-latest, steps: [], outputs: { zones: x } }
  deploy:
    runs-on: ubuntu-latest
    environment: 'deploy-${{ matrix.zone }}'
    steps: []
workflows:
  ci:
    on: { push: {} }
    jobs:
      - setup
      - deploy:
          requires: [setup]
          matrix: { zone: '${{ fromJSON(needs.setup.outputs.zones) }}' }
";
        let config = parse(yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap();
        let mut g = Graph::new(
            plan.jobs
                .into_iter()
                .enumerate()
                .map(|(i, j)| (i as u64 + 1, j))
                .collect(),
        );
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.finished(
            1,
            done(Conclusion::Success, &[("zones", r#"["a","b"]"#)]),
            &ctx(),
        );
        assert_eq!(actions.last(), Some(&Action::Expand(2)));
        // The stand-in settles nothing: the matrix feeds the name.
        assert_eq!(g.node(2).unwrap().environment, None);
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows = vec![(2, legs[0].clone()), (3, legs[1].clone())];
        let actions = g.expanded(2, rows, &ctx());
        assert_eq!(actions, [Action::Launch(2), Action::Launch(3)]);
        assert_eq!(g.node(2).unwrap().environment.as_deref(), Some("deploy-a"));
        assert_eq!(g.node(3).unwrap().environment.as_deref(), Some("deploy-b"));
    }

    #[test]
    fn a_matrix_dependent_job_if_waits_for_the_legs() {
        let mut g = dynamic_with("matrix.os == 'ubuntu-latest'", FROM, true);
        let actions = after_setup(&mut g, r#"{"os":["ubuntu-latest"]}"#);
        assert!(
            actions.iter().any(|a| matches!(a, Action::Expand(_))),
            "{actions:?}"
        );
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let actions = g.expanded(2, vec![(2, legs[0].clone())], &ctx());
        assert_eq!(actions, [Action::Launch(2)]);
        // A leg that the `if` does not match is skipped, not the job.
        let mut g = dynamic_with("matrix.os == 'ubuntu-latest'", FROM, true);
        after_setup(&mut g, r#"{"os":["ubuntu-latest","ubuntu-26.04-arm"]}"#);
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows = vec![(2, legs[0].clone()), (10, legs[1].clone())];
        let actions = g.expanded(2, rows, &ctx());
        assert!(actions.contains(&Action::Launch(2)));
        assert!(actions.contains(&Action::Finished {
            job: 10,
            conclusion: Conclusion::Skipped
        }));
    }

    #[test]
    fn a_rerun_of_the_producer_reads_the_matrix_again() {
        let mut g = dynamic();
        let actions = after_setup(&mut g, r#"["ubuntu-latest","ubuntu-26.04-arm"]"#);
        assert_eq!(actions.last(), Some(&Action::Expand(2)));
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows = vec![(2, legs[0].clone()), (10, legs[1].clone())];
        g.expanded(2, rows, &ctx());
        for job in [2, 10] {
            run(&mut g, job);
            g.finished(job, ok(), &ctx());
        }
        run(&mut g, 3);
        g.finished(3, ok(), &ctx());
        assert!(g.conclusion().is_some());
        let actions = g.rerun(1, &ctx());
        assert_eq!(actions, [Action::Launch(1)]);
        // The jobs the matrix made are the one job again, which waits.
        let ids: Vec<&str> = g.jobs.iter().map(|n| n.job.id.as_str()).collect();
        assert_eq!(ids, ["setup", "test", "after"]);
        assert_eq!(g.node(2).unwrap().attempt, 2);
        assert_eq!(g.node(3).unwrap().job.requires, ["test"]);
        run(&mut g, 1);
        let actions = g.finished(
            1,
            done(
                Conclusion::Success,
                &[(
                    "os",
                    r#"["ubuntu-26.04-arm","ubuntu-latest","ubuntu-26.04"]"#,
                )],
            ),
            &ctx(),
        );
        assert_eq!(actions.last(), Some(&Action::Expand(2)));
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        assert_eq!(legs.len(), 3);
    }

    const VARS: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  work: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: { inputs: { flag: { type: boolean } } } }
    jobs: [work]
  ci:
    on: { push: {} }
    jobs:
      - build
      - c:
          call: inner
          requires: [build]
          if: vars.GO == 'yes'
          with: { flag: '${{ needs.build.outputs.v }}' }
";

    fn vars_graph(yaml: &str) -> Graph {
        let plan = compile(&parse(yaml).unwrap(), "ci", "refs/heads/main")
            .unwrap_or_else(|e| panic!("{e:?}"));
        Graph::new(
            plan.jobs
                .into_iter()
                .zip(1u64..)
                .map(|(j, i)| (i, j))
                .collect(),
        )
    }

    #[test]
    fn a_call_reads_the_variables_the_graph_was_given() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(VARS);
        assert!(g.reads_vars());
        g.set_vars(BTreeMap::from([("GO".to_owned(), "yes".to_owned())]));
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("v", "true")]), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Queued);
        let mut g = vars_graph(VARS);
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("v", "true")]), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn a_call_that_is_skipped_never_reads_its_inputs() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(VARS);
        g.advance(&ctx);
        run(&mut g, 1);
        // `maybe` is no boolean, but the call is off, so it is no failure.
        g.finished(1, done(Conclusion::Success, &[("v", "maybe")]), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn what_a_call_waits_for_is_not_in_the_needs_of_its_jobs() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(VARS);
        g.set_vars(BTreeMap::from([("GO".to_owned(), "yes".to_owned())]));
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("v", "true")]), &ctx);
        assert!(g.needs(2).is_empty(), "{:?}", g.needs(2));
        let inputs = g.call_inputs(2, &ctx, &g.vars.clone()).unwrap().unwrap();
        assert_eq!(inputs["flag"], true);
    }

    /// Runs the dynamic graph through its first expansion into `os`
    /// labels, with every job done, and reruns setup.
    fn rerun_with(first: &str, second: &str) -> (Graph, Vec<Action>) {
        let mut g = dynamic();
        after_setup(&mut g, first);
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows: Vec<(u64, _)> = legs
            .iter()
            .enumerate()
            .map(|(i, l)| (if i == 0 { 2 } else { 9 + i as u64 }, l.clone()))
            .collect();
        let launched = rows.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        g.expanded(2, rows, &ctx());
        for id in launched {
            run(&mut g, id);
            g.finished(id, ok(), &ctx());
        }
        run(&mut g, 3);
        g.finished(3, ok(), &ctx());
        assert!(g.conclusion().is_some());
        g.rerun(1, &ctx());
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("os", second)]), &ctx());
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        let rows: Vec<(u64, _)> = legs
            .iter()
            .enumerate()
            .map(|(i, l)| (if i == 0 { 2 } else { 9 + i as u64 }, l.clone()))
            .collect();
        let actions = g.expanded(2, rows, &ctx());
        (g, actions)
    }

    #[test]
    fn a_rerun_with_a_smaller_matrix_ends_the_dropped_jobs_as_skipped() {
        let (mut g, actions) = rerun_with(
            r#"["ubuntu-latest","ubuntu-26.04-arm","ubuntu-26.04"]"#,
            r#"["ubuntu-latest"]"#,
        );
        // Rows 10 and 11 are dropped for this attempt.
        for job in [10, 11] {
            let node = g.node(job).unwrap();
            assert!(node.stale);
            assert_eq!(node.state, State::Done(Conclusion::Skipped));
            assert_eq!(
                node.message.as_deref(),
                Some("Not in the matrix of this attempt.")
            );
            assert!(actions.contains(&Action::Finished {
                job,
                conclusion: Conclusion::Skipped
            }));
        }
        // The job after the matrix waits for the current leg only.
        let after = g.jobs.iter().find(|n| n.job.id == "after").unwrap();
        assert_eq!(after.job.requires, ["test (ubuntu-latest)"]);
        // The run ends on the current jobs, whatever the dropped ones say.
        run(&mut g, 2);
        let actions = g.finished(2, ok(), &ctx());
        assert!(actions.contains(&Action::Launch(3)), "{actions:?}");
        run(&mut g, 3);
        let actions = g.finished(3, ok(), &ctx());
        assert!(actions.contains(&Action::RunDone(Conclusion::Success)));
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
        // The strategy numbering counts the current leg alone.
        let strategy = g.node(2).unwrap().job.strategy.as_ref().unwrap();
        assert_eq!((strategy.job_index, strategy.job_total), (0, 1));
    }

    #[test]
    fn a_rerun_with_a_larger_matrix_adds_jobs_and_a_returning_job_is_current() {
        let (g, actions) = rerun_with(
            r#"["ubuntu-latest"]"#,
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
        );
        assert!(g.jobs.iter().all(|n| !n.stale));
        let ids: Vec<&str> = g.jobs.iter().map(|n| n.job.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "setup",
                "test (ubuntu-latest)",
                "test (ubuntu-26.04-arm)",
                "after"
            ]
        );
        assert!(!actions.iter().any(|a| matches!(
            a,
            Action::Finished {
                conclusion: Conclusion::Skipped,
                ..
            }
        )));
        // Dropped and then made again: the stale job is replaced.
        let (mut g, _) = rerun_with(
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
            r#"["ubuntu-latest"]"#,
        );
        for job in [2, 3] {
            if g.node(job).is_some_and(|n| n.state == State::Queued) {
                run(&mut g, job);
                g.finished(job, ok(), &ctx());
            }
        }
        assert!(g.conclusion().is_some());
        g.rerun(1, &ctx());
        run(&mut g, 1);
        g.finished(
            1,
            done(
                Conclusion::Success,
                &[("os", r#"["ubuntu-latest","ubuntu-26.04-arm"]"#)],
            ),
            &ctx(),
        );
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        assert_eq!(legs.len(), 2);
        let rows = vec![(2, legs[0].clone()), (10, legs[1].clone())];
        g.expanded(2, rows, &ctx());
        assert!(
            g.jobs.iter().all(|n| !n.stale),
            "the dropped job is current again"
        );
        assert_eq!(g.node(10).unwrap().state, State::Queued);
    }

    const NESTED: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  work: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: { inputs: { v: { type: string } } } }
    jobs: [work]
  outer:
    on: { call: {} }
    jobs:
      - i: { call: inner, with: { v: '${{ needs.build.outputs.v }}' } }
  ci:
    on: { push: {} }
    jobs:
      - build
      - c: { call: outer, requires: [build] }
";

    #[test]
    fn an_inner_call_does_not_read_what_the_outer_call_waits_for() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(NESTED);
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("v", "secret-value")]), &ctx);
        let inputs = g.call_inputs(2, &ctx, &BTreeMap::new()).unwrap().unwrap();
        assert_eq!(inputs["v"], "");
    }

    const ADMIT: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  work: { runs-on: ubuntu-latest, steps: [] }
  more: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: {} }
    jobs: [work, { more: { requires: [work] } }]
  ci:
    on: { push: {} }
    jobs:
      - build
      - c: { call: inner, requires: [build], if: vars.GO == 'yes' }
";

    #[test]
    fn a_call_that_was_let_in_is_not_stopped_by_a_variable_that_changes() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(ADMIT);
        g.set_vars(BTreeMap::from([("GO".to_owned(), "yes".to_owned())]));
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, ok(), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Queued);
        g.set_vars(BTreeMap::from([("GO".to_owned(), "no".to_owned())]));
        run(&mut g, 2);
        let actions = g.finished(2, ok(), &ctx);
        assert!(actions.contains(&Action::Launch(3)), "{actions:?}");
        // After a rerun the call is decided again.
        run(&mut g, 3);
        g.finished(3, ok(), &ctx);
        g.rerun(1, &ctx);
        run(&mut g, 1);
        g.finished(1, ok(), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn a_matrix_condition_is_read_from_the_parsed_expression() {
        // Any case reads the matrix, so the job expands.
        let mut g = dynamic_with("MATRIX.os == 'ubuntu-latest'", FROM, true);
        let actions = after_setup(&mut g, r#"{"os":["ubuntu-latest"]}"#);
        assert!(actions.iter().any(|a| matches!(a, Action::Expand(_))));
        // A string that spells it is not a read: the condition is false.
        let mut g = dynamic_with("'matrix' == 'other'", FROM, true);
        let actions = after_setup(&mut g, r#"{"os":["ubuntu-latest"]}"#);
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
    }

    #[test]
    fn a_status_guard_that_is_false_skips_the_job_before_the_matrix_is_read() {
        let mut g = dynamic_with("failure() && matrix.os == 'ubuntu-latest'", FROM, true);
        // Malformed producer output is no failure of a job that is off.
        let actions = after_setup(&mut g, "not json");
        assert!(!actions.iter().any(|a| matches!(a, Action::Expand(_))));
        assert_eq!(g.node(2).unwrap().state, State::Done(Conclusion::Skipped));
        // The same guard holds after a failure, and the legs read it.
        let mut g = dynamic_with("failure() && matrix.os == 'ubuntu-latest'", FROM, true);
        g.advance(&ctx());
        run(&mut g, 1);
        let actions = g.finished(
            1,
            done(
                Conclusion::Failure,
                &[("os", r#"{"os":["ubuntu-latest"]}"#)],
            ),
            &ctx(),
        );
        // The job after setup needs it to succeed unless it says otherwise.
        assert!(actions.iter().all(|a| !matches!(a, Action::Launch(_))));
    }

    const FAILED_BUILD: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [] }
  work: { runs-on: ubuntu-latest, steps: [] }
  after: { runs-on: ubuntu-latest, steps: [] }
workflows:
  inner:
    on: { call: {} }
    jobs: [work]
  ci:
    on: { push: {} }
    jobs:
      - build
      - c: { call: inner, requires: [{ build: failed }] }
      - { after: { requires: [c] } }
";

    #[test]
    fn a_job_that_needs_a_call_does_not_count_what_the_call_waited_for() {
        let ctx = github("refs/heads/main");
        let mut g = vars_graph(FAILED_BUILD);
        g.advance(&ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Failure, &[]), &ctx);
        assert_eq!(g.node(2).unwrap().state, State::Queued);
        run(&mut g, 2);
        let actions = g.finished(2, ok(), &ctx);
        // `after` needs the call, which succeeded, not the build that failed.
        assert!(actions.contains(&Action::Launch(3)), "{actions:?}");
    }

    fn shrunk() -> Graph {
        let (g, _) = rerun_with(
            r#"["ubuntu-latest","ubuntu-26.04-arm","ubuntu-26.04"]"#,
            r#"["ubuntu-latest"]"#,
        );
        g
    }

    #[test]
    fn a_job_a_rerun_left_out_cannot_be_rerun() {
        let mut g = shrunk();
        run(&mut g, 2);
        g.finished(2, ok(), &ctx());
        run(&mut g, 3);
        g.finished(3, ok(), &ctx());
        assert!(g.conclusion().is_some());
        assert_eq!(g.rerun(10, &ctx()), Vec::<Action>::new());
        assert_eq!(g.node(10).unwrap().state, State::Done(Conclusion::Skipped));
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn what_a_rerun_left_out_stays_in_the_graph_until_something_takes_its_place() {
        let mut g = shrunk();
        run(&mut g, 2);
        g.finished(2, ok(), &ctx());
        run(&mut g, 3);
        g.finished(3, ok(), &ctx());
        g.rerun(1, &ctx());
        // The skipped checks of the attempt before are still there to send.
        assert!(g.node(10).is_some_and(|n| n.stale));
        assert!(g.node(11).is_some_and(|n| n.stale));
        run(&mut g, 1);
        g.finished(
            1,
            done(Conclusion::Success, &[("os", r#"["ubuntu-latest"]"#)]),
            &ctx(),
        );
        let legs = g.expansion(2, &ctx(), &BTreeMap::new()).unwrap().unwrap();
        g.expanded(2, vec![(2, legs[0].clone())], &ctx());
        // One node for each row, dropped again for the new attempt.
        let ids: Vec<u64> = g.jobs.iter().map(|n| n.id).collect();
        assert_eq!(ids.iter().filter(|id| **id == 10).count(), 1);
        assert!(g.node(10).unwrap().stale);
    }

    #[test]
    fn a_rerun_whose_matrix_makes_no_job_ends_the_old_jobs_as_skipped() {
        let ctx = ctx();
        let (mut g, _) = rerun_with(
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
        );
        // Again, with a matrix that has nothing in it.
        for job in [2, 10] {
            if g.node(job).is_some_and(|n| n.state == State::Queued) {
                run(&mut g, job);
                g.finished(job, ok(), &ctx);
            }
        }
        for job in [2, 10, 3] {
            if g.node(job).is_some_and(|n| n.state == State::Queued) {
                run(&mut g, job);
                g.finished(job, ok(), &ctx);
            }
        }
        assert!(g.conclusion().is_some());
        g.rerun(1, &ctx);
        run(&mut g, 1);
        g.finished(
            1,
            done(Conclusion::Success, &[("os", r#"["ubuntu-latest"]"#)]),
            &ctx,
        );
        // The matrix reads as no entry, as an `exclude` can leave it.
        let actions = g.no_jobs(2, &ctx);
        assert!(actions.contains(&Action::Finished {
            job: 10,
            conclusion: Conclusion::Skipped
        }));
        let node = g.node(10).unwrap();
        assert!(node.stale);
        assert_eq!(node.state, State::Done(Conclusion::Skipped));
        assert_eq!(g.conclusion(), Some(Conclusion::Success));
    }

    #[test]
    fn a_matrix_that_cannot_be_read_ends_the_old_jobs_as_skipped_too() {
        let ctx = ctx();
        let (mut g, _) = rerun_with(
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
        );
        for job in [2, 10, 3] {
            if g.node(job).is_some_and(|n| n.state == State::Queued) {
                run(&mut g, job);
                g.finished(job, ok(), &ctx);
            }
        }
        assert!(g.conclusion().is_some());
        g.rerun(1, &ctx);
        run(&mut g, 1);
        g.finished(1, done(Conclusion::Success, &[("os", "not json")]), &ctx);
        let message = g.expansion(2, &ctx, &BTreeMap::new()).unwrap().unwrap_err();
        let actions = g.failed(2, message, &ctx);
        assert!(actions.contains(&Action::Finished {
            job: 10,
            conclusion: Conclusion::Skipped
        }));
        assert!(g.node(10).unwrap().stale);
        assert_eq!(g.conclusion(), Some(Conclusion::Failure));
    }
}
