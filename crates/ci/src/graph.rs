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
    Conclusion, ExprError, Functions, JobKind, JobResult, JobStatus, Leg, Need, Outcome, Plan,
    PlannedJob, Ready, Siblings, Statuses, checks_status, continue_on_error, expand,
    hatchrun_context, job_condition, ready,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The message of an approval job, which ends as skipped for now.
const APPROVAL_MESSAGE: &str = "Approval jobs come in a later release.";

/// Why a job that failed ends as a success.
const CONTINUED: &str = "The job failed, and continue-on-error counts it as a success.";

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
    /// 0 until the job is asked to stop, 1 once asked, 2 once told to
    /// stop hard.
    pub cancel: u32,
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
                    id,
                    job,
                    state: State::Waiting,
                    attempt: 1,
                    result: None,
                    message: None,
                    cancel: 0,
                })
                .collect(),
            cancelled: false,
            run_done: false,
            vars: BTreeMap::new(),
        }
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
        self.cancelled = false;
        self.run_done = false;
        self.advance(github_ctx)
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
        let ctx = self.job_context(job, github_ctx, vars)?;
        let group = self.jobs[at].job.strategy.as_ref().map(|s| &s.group);
        let in_group = |i: usize| {
            i != at
                && group.is_some()
                && self.jobs[i].job.strategy.as_ref().map(|s| &s.group) == group
        };
        let count = |range: std::ops::Range<usize>| {
            u32::try_from(range.filter(|i| in_group(*i)).count()).unwrap_or(u32::MAX)
        };
        let siblings = Siblings {
            jobs: self.jobs.len() - 1,
            group_before: count(0..at),
            group_others: count(0..self.jobs.len()),
        };
        Some(expand(&self.jobs[at].job, &ctx, siblings).and_then(|legs| {
            let clash = legs.iter().find(|leg| {
                self.jobs
                    .iter()
                    .enumerate()
                    .any(|(i, n)| i != at && n.job.id == leg.job.id)
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
                    id,
                    job: leg.job,
                    state: State::Waiting,
                    attempt: old.attempt,
                    result: None,
                    message: None,
                    cancel: 0,
                }
            })
            .collect();
        self.jobs.splice(at..at, nodes);
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
        let mut actions = Vec::new();
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

    /// Numbers the jobs of each strategy group in graph order and counts
    /// them, for `strategy.job-index` and `strategy.job-total`.
    fn number_groups(&mut self) {
        let mut totals: BTreeMap<String, u32> = BTreeMap::new();
        for strategy in self.jobs.iter().filter_map(|n| n.job.strategy.as_ref()) {
            *totals.entry(strategy.group.clone()).or_default() += 1;
        }
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        for strategy in self.jobs.iter_mut().filter_map(|n| n.job.strategy.as_mut()) {
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
        for node in &self.jobs {
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
        self.jobs.iter().position(|n| n.job.id == id)
    }

    fn requirements_done(&self, at: usize) -> bool {
        self.jobs[at].job.requires.iter().all(|r| {
            self.position_of(r)
                .is_some_and(|p| matches!(self.jobs[p].state, State::Done(_)))
        })
    }

    fn needs_at(&self, at: usize) -> BTreeMap<String, Need> {
        let mut groups: BTreeMap<String, Vec<&Node>> = BTreeMap::new();
        for id in &self.jobs[at].job.requires {
            if let Some(p) = self.position_of(id)
                && matches!(self.jobs[p].state, State::Done(_))
            {
                groups
                    .entry(self.jobs[p].job.name.clone())
                    .or_default()
                    .push(&self.jobs[p]);
            }
        }
        // Members merge in plan order, not in the order `requires` lists
        // them.
        let order: BTreeMap<&str, usize> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(i, n)| (n.job.id.as_str(), i))
            .collect();
        groups
            .into_iter()
            .map(|(name, mut members)| {
                members.sort_by_key(|n| order[n.job.id.as_str()]);
                let result = combine(members.iter().filter_map(|n| match n.state {
                    State::Done(c) => Some(c),
                    _ => None,
                }));
                let mut outputs: BTreeMap<String, String> = BTreeMap::new();
                for member in &members {
                    let Some(r) = &member.result else { continue };
                    for (k, v) in &r.outputs {
                        match outputs.get_mut(k) {
                            Some(slot) if !v.is_empty() => slot.clone_from(v),
                            Some(_) => {}
                            None => {
                                outputs.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                (name, Need { result, outputs })
            })
            .collect()
    }

    /// The status `success()` and `failure()` see: over every ancestor
    /// through `requires`, not only the direct ones. `Failure` when any
    /// ancestor failed (or was cancelled in a run that is not cancelled),
    /// `Success` when all succeeded, else `Skipped`.
    fn ancestor_status(&self, at: usize) -> JobStatus {
        let mut seen = vec![false; self.jobs.len()];
        let mut stack = vec![at];
        let mut skipped = false;
        while let Some(i) = stack.pop() {
            for r in &self.jobs[i].job.requires {
                let Some(p) = self.position_of(r) else {
                    continue;
                };
                if std::mem::replace(&mut seen[p], true) {
                    continue;
                }
                match self.jobs[p].state {
                    State::Done(Conclusion::Success) => {}
                    State::Done(Conclusion::Skipped) => skipped = true,
                    _ => return JobStatus::Failure,
                }
                stack.push(p);
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
        let node = &self.jobs[at];
        let status = if self.cancelled {
            JobStatus::Cancelled
        } else if statuses_ok && !node.job.if_.as_deref().map_or(Ok(false), checks_status)? {
            JobStatus::Success
        } else {
            self.ancestor_status(at)
        };
        let ctx = self.context(at, github_ctx);
        job_condition(node.job.if_.as_deref(), &ctx, &Status(status))
    }

    /// The context of job `job`'s own expressions, as the graph builds it
    /// (`needs`, `matrix`, `strategy`, `inputs`), with the repository
    /// and organization variables `vars` it was not given. `None` for a
    /// job the graph does not hold.
    #[must_use]
    pub fn job_context(
        &self,
        job: u64,
        github_ctx: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Option<Value> {
        let mut ctx = self.context(self.position(job)?, github_ctx);
        if let Some(map) = ctx.as_object_mut() {
            map.insert("vars".to_owned(), json!(vars));
        }
        Some(ctx)
    }

    /// The contexts the control plane gives an expression of the job at
    /// `at`: what it knows without a runner.
    fn context(&self, at: usize, github_ctx: &Value) -> Value {
        let node = &self.jobs[at];
        let inputs = github_ctx
            .get("event")
            .and_then(|e| e.get("inputs"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        json!({
            "hatchrun": hatchrun_context(github_ctx),
            "github": github_ctx,
            "needs": self.needs_at(at),
            "matrix": node.job.matrix,
            "strategy": node.job.strategy_context(),
            "vars": self.vars,
            "inputs": inputs,
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
        let ctx = self.context(at, github_ctx);
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
        match self.decide(at, github_ctx, statuses_ok) {
            Ok(true) if self.jobs[at].job.deferred.is_some() => {
                // The job's own `if:` held. Each job it makes meets its
                // `if:` again with its matrix entry.
                self.jobs[at].state = State::Queued;
                actions.push(Action::Expand(self.jobs[at].id));
            }
            Ok(true) => {
                self.jobs[at].state = State::Queued;
                actions.push(Action::Launch(self.jobs[at].id));
            }
            Ok(false) if self.cancelled => self.end(at, Conclusion::Cancelled, None, actions),
            Ok(false) => self.end(at, Conclusion::Skipped, None, actions),
            Err(e) => {
                let (conclusion, note) = self.forgive(at, Conclusion::Failure, github_ctx);
                let message = match note {
                    Some(note) => format!("{} {note}", if_error(&e)),
                    None => if_error(&e),
                };
                self.end(at, conclusion, Some(message), actions);
                self.fail_fast(at, actions);
            }
        }
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
}
