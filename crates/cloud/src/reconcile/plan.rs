// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What one pass does about a tenant: which instances to terminate and
//! which queued jobs to launch again, from the tenant's jobs in D1 and
//! its live instances in EC2. Nothing here talks to anything, so the
//! rules test natively.
//!
//! A runner registers with its job's label and GitHub hands it any job
//! with that label (see [`crate::labels`]), so the runner launched for
//! job A may run job B. The `hatchrun:runner-name` tag ties an instance
//! to its runner and `jobs.ran_on`, from GitHub's `in_progress` and
//! `completed` events, ties a runner to the job it ran.
//!
//! An instance is
//!
//! - **busy** while the job its runner took is open: kept;
//! - **finished** once that job completed: the agent powers it off,
//!   and it is terminated if still up after [`Limits::done_grace`];
//! - **waiting** while its runner has taken nothing and the job it was
//!   launched for is queued and still names that runner: kept, unless
//!   it waited longer than [`Limits::idle_timeout`], when it is
//!   terminated and the job's launch marked failed so it is not
//!   launched again and again;
//! - **spare** when its runner has taken nothing and its job no longer
//!   needs it (completed, taken by another runner, or launched again):
//!   kept for a queued job with the same label that has no runner of
//!   its own, since GitHub will hand it that job, and otherwise
//!   terminated after [`Limits::done_grace`];
//! - an **orphan** when its job is unknown: terminated after
//!   [`Limits::launch_timeout`].
//!
//! Whatever its state, an instance older than [`Limits::max_age`] is
//! terminated.
//!
//! A job queued for longer than [`Limits::queued_grace`] is launched
//! again when it never launched (its launch was lost, or waited for
//! room under `max_runners`), when its launch stalled for longer than
//! [`Limits::launch_timeout`], when it launched but its runner is gone
//! or took another job, or when its launch failed on something that
//! passes and its wait is over (see [`super::retry`]), unless a spare
//! runner with its label will take it. Launches stop at `max_runners`;
//! `launch` checks that again when it claims the job.

use std::collections::{HashMap, HashSet};

use super::retry::Wait;
use crate::webhook::Status;

/// Timeouts, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How long a job may be queued before the pass looks at it; the
    /// webhook's own launch gets that long.
    pub queued_grace: u64,
    /// How long a launch may take, and an instance with no job may run.
    pub launch_timeout: u64,
    /// How long a runner may wait for its job.
    pub idle_timeout: u64,
    /// How long an instance may stay up once it is no longer needed.
    pub done_grace: u64,
    /// The longest any instance runs.
    pub max_age: u64,
    /// How long an open job goes without a webhook before the pass asks
    /// GitHub about it.
    pub sync_after: u64,
    /// How far back failed webhook deliveries are redelivered.
    pub redeliver_window: u64,
    /// How long finished jobs are kept.
    pub retention: u64,
}

/// The variables that set [`Limits`], with their defaults, in seconds
/// except the retention, in days.
pub const VARS: [(&str, u64); 8] = [
    ("RECONCILE_QUEUED_GRACE_SECS", 60),
    ("RECONCILE_LAUNCH_TIMEOUT_SECS", 10 * 60),
    ("RECONCILE_IDLE_TIMEOUT_SECS", 30 * 60),
    ("RECONCILE_DONE_GRACE_SECS", 3 * 60),
    ("RECONCILE_MAX_INSTANCE_AGE_SECS", 8 * 60 * 60),
    ("RECONCILE_SYNC_AFTER_SECS", 5 * 60),
    ("RECONCILE_REDELIVER_WINDOW_SECS", 6 * 60 * 60),
    ("RECONCILE_RETENTION_DAYS", 30),
];

impl Default for Limits {
    fn default() -> Self {
        Self::read(|_| None).expect("the defaults are numbers")
    }
}

impl Limits {
    /// Reads the limits through `get`, which returns a variable's value;
    /// a variable that is unset or empty keeps its default.
    ///
    /// # Errors
    ///
    /// A variable that is not a whole number.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut values = [0; VARS.len()];
        for ((name, default), value) in VARS.iter().zip(&mut values) {
            *value = match get(name).filter(|value| !value.trim().is_empty()) {
                Some(value) => value
                    .trim()
                    .parse()
                    .map_err(|_| format!("{name} is not a whole number: {value:?}"))?,
                None => *default,
            };
        }
        let [
            queued_grace,
            launch_timeout,
            idle_timeout,
            done_grace,
            max_age,
            sync_after,
            redeliver_window,
            retention_days,
        ] = values.map(|value| value.saturating_mul(1000));
        Ok(Self {
            queued_grace,
            launch_timeout,
            idle_timeout,
            done_grace,
            max_age,
            sync_after,
            redeliver_window,
            retention: retention_days.saturating_mul(24 * 60 * 60),
        })
    }
}

/// Where a job's launch stands (`jobs.launch_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchState {
    /// The Worker took it and is starting the instance.
    Launching,
    /// The instance exists.
    Launched,
    /// The launch gave up.
    Failed,
}

impl LaunchState {
    /// The column value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Launched => "launched",
            Self::Failed => "failed",
        }
    }

    /// Reads the column value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "launching" => Some(Self::Launching),
            "launched" => Some(Self::Launched),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A job row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Workflow job id.
    pub id: u64,
    /// Its hatchrun label.
    pub label: String,
    /// Its status.
    pub status: Status,
    /// Where its launch stands; `None` before any.
    pub launch_state: Option<LaunchState>,
    /// The runner launched for it.
    pub runner_name: Option<String>,
    /// The runner that took it.
    pub ran_on: Option<String>,
    /// When it was queued, Unix milliseconds.
    pub queued_at: u64,
    /// When its row last changed, Unix milliseconds.
    pub updated_at: u64,
    /// After a failed launch, what it waits for.
    pub launch_retry: Option<Wait>,
    /// After a failed launch that backs off, when to launch it again,
    /// Unix milliseconds.
    pub next_launch_at: Option<u64>,
}

/// A pending or running instance with the tenant's tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `i-...`
    pub id: String,
    /// When it started, Unix milliseconds.
    pub launched_at: u64,
    /// Its `hatchrun:job-id` tag.
    pub job_id: Option<u64>,
    /// Its `hatchrun:runner-name` tag.
    pub runner_name: Option<String>,
}

/// Why an instance is terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Older than [`Limits::max_age`].
    MaxAge,
    /// Its runner ran a job that completed.
    Finished,
    /// Its runner waited too long for its job.
    Idle,
    /// Its job no longer needs it, nor does any other.
    Spare,
    /// Its job is unknown.
    Orphan,
}

impl Reason {
    /// A word for the log.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxAge => "max-age",
            Self::Finished => "finished",
            Self::Idle => "idle",
            Self::Spare => "spare",
            Self::Orphan => "orphan",
        }
    }
}

/// A job whose launch is undone so it can launch again. The pass
/// applies it only while the row still says what the plan saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reset {
    /// Workflow job id.
    pub job_id: u64,
    /// The launch state the plan saw.
    pub launch_state: LaunchState,
    /// The runner the plan saw.
    pub runner_name: Option<String>,
}

/// What to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Instances to terminate.
    pub terminate: Vec<(String, Reason)>,
    /// Stalled launches whose instance exists after all: job id and
    /// instance id, to mark launched.
    pub adopt: Vec<(u64, String)>,
    /// Launches to undo before launching again.
    pub reset: Vec<Reset>,
    /// Jobs whose runner waited too long, to mark failed: job id and
    /// the runner the plan saw.
    pub give_up: Vec<(u64, String)>,
    /// Jobs to launch, oldest first.
    pub launch: Vec<u64>,
}

/// Plans a pass over one tenant at `now`. `instances` is `None` when
/// EC2 could not be asked; then only jobs that never launched are
/// launched, since the rest depend on what is running.
#[must_use]
pub fn plan(
    now: u64,
    limits: &Limits,
    max_runners: u32,
    instances: Option<&[Instance]>,
    jobs: &[Job],
) -> Plan {
    let mut plan = Plan::default();
    let mut fleet = sort_instances(now, limits, instances.unwrap_or_default(), jobs, &mut plan);
    let unserved = unserved(now, limits, instances.is_some(), jobs, &fleet, &mut plan);

    // A spare runner with the job's label will be handed the job.
    for list in fleet.spare.values_mut() {
        list.sort_by_key(|(instance, _)| (instance.launched_at, instance.id.clone()));
    }
    let mut launch = Vec::new();
    for job in unserved {
        let taken = fleet
            .spare
            .get_mut(job.label.as_str())
            .and_then(|list| (!list.is_empty()).then(|| list.remove(0)));
        if taken.is_none() {
            launch.push(job.id);
        }
    }
    let mut leftover: Vec<(&Instance, u64)> = fleet.spare.into_values().flatten().collect();
    leftover.sort_by_key(|(instance, _)| (instance.launched_at, instance.id.clone()));
    for (instance, idle_since) in leftover {
        if now.saturating_sub(idle_since) > limits.done_grace {
            plan.terminate.push((instance.id.clone(), Reason::Spare));
        }
    }

    let undone: HashSet<u64> = plan
        .reset
        .iter()
        .map(|reset| reset.job_id)
        .chain(fleet.given_up.iter().copied())
        .collect();
    let active = jobs
        .iter()
        .filter(|job| {
            job.status != Status::Completed
                && matches!(
                    job.launch_state,
                    Some(LaunchState::Launching | LaunchState::Launched)
                )
                && !undone.contains(&job.id)
        })
        .count();
    let room = usize::try_from(max_runners)
        .unwrap_or(usize::MAX)
        .saturating_sub(active);
    launch.truncate(room);
    plan.launch = launch;
    plan
}

/// The instances that are kept, by what they wait for.
struct Fleet<'a> {
    /// Instances that wait for their own job, by job id.
    waiting: HashMap<u64, &'a Instance>,
    /// Instances nothing needs, by label, with since when.
    spare: HashMap<&'a str, Vec<(&'a Instance, u64)>>,
    /// Jobs whose runner waited too long.
    given_up: HashSet<u64>,
}

/// Terminates what is old, finished or orphaned, and sorts the rest.
fn sort_instances<'a>(
    now: u64,
    limits: &Limits,
    instances: &'a [Instance],
    jobs: &'a [Job],
    plan: &mut Plan,
) -> Fleet<'a> {
    let by_id: HashMap<u64, &Job> = jobs.iter().map(|job| (job.id, job)).collect();
    let mut ran_on: HashMap<&str, &Job> = HashMap::new();
    for job in jobs {
        if let Some(runner) = job.ran_on.as_deref() {
            // A runner runs one job; prefer the open one if rows disagree.
            let keep = ran_on
                .get(runner)
                .is_some_and(|other| other.status != Status::Completed);
            if !keep {
                ran_on.insert(runner, job);
            }
        }
    }
    let since = |at: u64| now.saturating_sub(at);
    let mut fleet = Fleet {
        waiting: HashMap::new(),
        spare: HashMap::new(),
        given_up: HashSet::new(),
    };
    for instance in instances {
        let age = since(instance.launched_at);
        if age > limits.max_age {
            plan.terminate.push((instance.id.clone(), Reason::MaxAge));
            continue;
        }
        let runner = instance.runner_name.as_deref();
        if let Some(took) = runner.and_then(|runner| ran_on.get(runner)) {
            if took.status == Status::Completed && since(took.updated_at) > limits.done_grace {
                plan.terminate.push((instance.id.clone(), Reason::Finished));
            }
            continue;
        }
        let Some(job) = instance.job_id.and_then(|id| by_id.get(&id)) else {
            if age > limits.launch_timeout {
                plan.terminate.push((instance.id.clone(), Reason::Orphan));
            }
            continue;
        };
        let its_runner = runner.is_some() && job.runner_name.as_deref() == runner;
        if job.status == Status::Queued
            && its_runner
            && job.launch_state != Some(LaunchState::Failed)
        {
            if age > limits.idle_timeout {
                plan.terminate.push((instance.id.clone(), Reason::Idle));
                if job.launch_state == Some(LaunchState::Launched) {
                    plan.give_up
                        .push((job.id, runner.unwrap_or_default().to_owned()));
                    fleet.given_up.insert(job.id);
                }
            } else {
                fleet.waiting.insert(job.id, instance);
            }
            continue;
        }
        if job.status == Status::InProgress && job.ran_on.is_none() {
            // Taken by a runner GitHub did not name; this one may be it.
            continue;
        }
        fleet
            .spare
            .entry(job.label.as_str())
            .or_default()
            .push((instance, job.updated_at.max(instance.launched_at)));
    }
    fleet
}

/// Queued jobs with no runner coming, oldest first. Adopts stalled
/// launches whose instance turned up and undoes launches whose runner
/// is gone. `described` is whether EC2 was asked.
fn unserved<'a>(
    now: u64,
    limits: &Limits,
    described: bool,
    jobs: &'a [Job],
    fleet: &Fleet<'_>,
    plan: &mut Plan,
) -> Vec<&'a Job> {
    let since = |at: u64| now.saturating_sub(at);
    let mut queued: Vec<&Job> = jobs
        .iter()
        .filter(|job| job.status == Status::Queued && since(job.queued_at) > limits.queued_grace)
        .filter(|job| !fleet.given_up.contains(&job.id))
        .collect();
    queued.sort_by_key(|job| (job.queued_at, job.id));
    let mut unserved = Vec::new();
    for job in queued {
        match job.launch_state {
            None => unserved.push(job),
            Some(LaunchState::Failed) => {
                let due = job.launch_retry == Some(Wait::Backoff)
                    && job.next_launch_at.is_some_and(|at| at <= now);
                if due {
                    plan.reset.push(reset(job, LaunchState::Failed));
                    unserved.push(job);
                }
            }
            Some(_) if !described => {}
            Some(LaunchState::Launching) => {
                if since(job.updated_at) <= limits.launch_timeout {
                    continue;
                }
                if let Some(instance) = fleet.waiting.get(&job.id) {
                    plan.adopt.push((job.id, instance.id.clone()));
                    continue;
                }
                plan.reset.push(reset(job, LaunchState::Launching));
                unserved.push(job);
            }
            Some(LaunchState::Launched) => {
                if fleet.waiting.contains_key(&job.id)
                    || since(job.updated_at) <= limits.queued_grace
                {
                    continue;
                }
                plan.reset.push(reset(job, LaunchState::Launched));
                unserved.push(job);
            }
        }
    }
    unserved
}

fn reset(job: &Job, launch_state: LaunchState) -> Reset {
    Reset {
        job_id: job.id,
        launch_state,
        runner_name: job.runner_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::retry::{Cause, Failure};
    use super::*;

    const MIN: u64 = 60 * 1000;
    /// Far enough from zero that subtracting hours stays positive.
    const NOW: u64 = 1_000 * 60 * MIN;

    fn limits() -> Limits {
        Limits::default()
    }

    fn job(id: u64, status: Status, launch_state: Option<LaunchState>) -> Job {
        Job {
            id,
            label: "hatchrun".to_owned(),
            status,
            launch_state,
            runner_name: launch_state.map(|_| format!("r{id}")),
            ran_on: None,
            queued_at: NOW - 5 * MIN,
            updated_at: NOW - 5 * MIN,
            launch_retry: None,
            next_launch_at: None,
        }
    }

    /// A job whose launch failed at `failed_at` on `failure`.
    fn failed(id: u64, failure: &Failure, attempts: u32, failed_at: u64) -> Job {
        let (wait, next) = failure.outcome(attempts, failed_at);
        Job {
            updated_at: failed_at,
            launch_retry: Some(wait),
            next_launch_at: next,
            ..job(id, Status::Queued, Some(LaunchState::Failed))
        }
    }

    fn instance(id: &str, job_id: u64, age: u64) -> Instance {
        Instance {
            id: id.to_owned(),
            launched_at: NOW - age,
            job_id: Some(job_id),
            runner_name: Some(format!("r{job_id}")),
        }
    }

    fn terminated(plan: &Plan) -> Vec<(&str, Reason)> {
        plan.terminate
            .iter()
            .map(|(id, reason)| (id.as_str(), *reason))
            .collect()
    }

    #[test]
    fn reads_limits_from_variables() {
        let limits = Limits::default();
        assert_eq!(limits.queued_grace, 60_000);
        assert_eq!(limits.max_age, 8 * 60 * MIN);
        assert_eq!(limits.retention, 30 * 24 * 60 * MIN);
        let set = Limits::read(|name| match name {
            "RECONCILE_IDLE_TIMEOUT_SECS" => Some(" 90 ".to_owned()),
            "RECONCILE_QUEUED_GRACE_SECS" => Some(String::new()),
            _ => None,
        })
        .unwrap();
        assert_eq!(set.idle_timeout, 90_000);
        assert_eq!(set.queued_grace, 60_000);
        assert_eq!(
            Limits::read(|name| (name == "RECONCILE_DONE_GRACE_SECS").then(|| "2m".to_owned())),
            Err("RECONCILE_DONE_GRACE_SECS is not a whole number: \"2m\"".to_owned())
        );
    }

    #[test]
    fn keeps_a_healthy_fleet() {
        let mut running = job(1, Status::InProgress, Some(LaunchState::Launched));
        running.ran_on = Some("r1".to_owned());
        let waiting = job(2, Status::Queued, Some(LaunchState::Launched));
        let instances = [instance("i-1", 1, 4 * MIN), instance("i-2", 2, 4 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[running, waiting]);
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn launches_jobs_that_never_launched_up_to_max_runners() {
        let mut fresh = job(4, Status::Queued, None);
        fresh.queued_at = NOW - 30 * 1000;
        let jobs = [
            job(1, Status::Queued, Some(LaunchState::Launched)),
            job(2, Status::Queued, None),
            job(3, Status::Queued, None),
            fresh,
            job(5, Status::Queued, Some(LaunchState::Failed)),
            job(6, Status::Completed, None),
        ];
        let instances = [instance("i-1", 1, 4 * MIN)];
        let plan_with = |max| plan(NOW, &limits(), max, Some(&instances), &jobs).launch;
        assert_eq!(plan_with(10), [2, 3]);
        assert_eq!(plan_with(2), [2]);
        assert_eq!(plan_with(1), Vec::<u64>::new());
        // Without EC2, jobs that never launched still launch.
        assert_eq!(plan(NOW, &limits(), 10, None, &jobs).launch, [2, 3]);
    }

    #[test]
    fn launches_again_when_the_runner_is_gone() {
        let jobs = [job(1, Status::Queued, Some(LaunchState::Launched))];
        let plan = plan(NOW, &limits(), 10, Some(&[]), &jobs);
        assert_eq!(
            plan.reset,
            [Reset {
                job_id: 1,
                launch_state: LaunchState::Launched,
                runner_name: Some("r1".to_owned()),
            }]
        );
        assert_eq!(plan.launch, [1]);
        // Without EC2 nothing is known about the runner.
        assert_eq!(
            super::plan(NOW, &limits(), 10, None, &jobs),
            Plan::default()
        );
    }

    #[test]
    fn a_fresh_launch_may_not_show_in_ec2_yet() {
        let mut launched = job(1, Status::Queued, Some(LaunchState::Launched));
        launched.updated_at = NOW - 20 * 1000;
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), &[launched]),
            Plan::default()
        );
    }

    #[test]
    fn launches_again_when_another_job_took_the_runner() {
        // Job 2's runner took job 1, so job 2 waits on a runner that
        // will never come back to it.
        let mut first = job(1, Status::InProgress, Some(LaunchState::Launched));
        first.ran_on = Some("r2".to_owned());
        let second = job(2, Status::Queued, Some(LaunchState::Launched));
        let instances = [instance("i-1", 1, 4 * MIN), instance("i-2", 2, 4 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[first, second]);
        // r1 is spare with job 1 taken elsewhere; GitHub hands it job 2.
        assert_eq!(plan.reset.len(), 1);
        assert_eq!(plan.reset[0].job_id, 2);
        assert_eq!(plan.launch, Vec::<u64>::new());
        assert_eq!(plan.terminate, []);
    }

    #[test]
    fn a_stalled_launch_is_adopted_or_undone() {
        let stalled = |id| {
            let mut job = job(id, Status::Queued, Some(LaunchState::Launching));
            job.updated_at = NOW - 11 * MIN;
            job.queued_at = NOW - 11 * MIN;
            job
        };
        let instances = [instance("i-1", 1, 10 * MIN)];
        let plan = plan(
            NOW,
            &limits(),
            10,
            Some(&instances),
            &[stalled(1), stalled(2)],
        );
        assert_eq!(plan.adopt, [(1, "i-1".to_owned())]);
        assert_eq!(plan.reset.len(), 1);
        assert_eq!(plan.reset[0].job_id, 2);
        assert_eq!(plan.reset[0].launch_state, LaunchState::Launching);
        assert_eq!(plan.launch, [2]);
        // A launch in progress is left alone.
        let busy = job(3, Status::Queued, Some(LaunchState::Launching));
        assert_eq!(
            super::plan(NOW, &limits(), 10, Some(&[]), &[busy]),
            Plan::default()
        );
    }

    #[test]
    fn terminates_finished_runners_after_a_grace() {
        let mut done = job(1, Status::Completed, Some(LaunchState::Launched));
        done.ran_on = Some("r1".to_owned());
        done.updated_at = NOW - 4 * MIN;
        let mut just_done = job(2, Status::Completed, Some(LaunchState::Launched));
        just_done.ran_on = Some("r2".to_owned());
        just_done.updated_at = NOW - MIN;
        let instances = [instance("i-1", 1, 9 * MIN), instance("i-2", 2, 9 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[done, just_done]);
        assert_eq!(terminated(&plan), [("i-1", Reason::Finished)]);
    }

    #[test]
    fn keeps_a_runner_busy_with_another_job() {
        // Job 1 completed on another runner while r1 runs job 2.
        let mut first = job(1, Status::Completed, Some(LaunchState::Launched));
        first.ran_on = Some("r2".to_owned());
        first.updated_at = NOW - 20 * MIN;
        let mut second = job(2, Status::InProgress, Some(LaunchState::Launched));
        second.ran_on = Some("r1".to_owned());
        let instances = [instance("i-1", 1, 30 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[first, second]);
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn terminates_spare_runners_nothing_needs() {
        // Job 1 was cancelled before its runner took it.
        let mut cancelled = job(1, Status::Completed, Some(LaunchState::Launched));
        cancelled.updated_at = NOW - 4 * MIN;
        let mut recent = job(2, Status::Completed, Some(LaunchState::Launched));
        recent.updated_at = NOW - MIN;
        let instances = [instance("i-1", 1, 6 * MIN), instance("i-2", 2, 6 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[cancelled, recent]);
        assert_eq!(terminated(&plan), [("i-1", Reason::Spare)]);
    }

    #[test]
    fn a_spare_runner_serves_a_queued_job_with_its_label() {
        let mut cancelled = job(1, Status::Completed, Some(LaunchState::Launched));
        cancelled.updated_at = NOW - 4 * MIN;
        let waiting = job(2, Status::Queued, None);
        let mut other = job(3, Status::Queued, None);
        other.label = "hatchrun-4cpu-arm64".to_owned();
        let instances = [instance("i-1", 1, 6 * MIN)];
        let plan = plan(
            NOW,
            &limits(),
            10,
            Some(&instances),
            &[cancelled, waiting, other],
        );
        assert_eq!(plan.terminate, []);
        assert_eq!(plan.launch, [3]);
    }

    #[test]
    fn gives_up_on_a_runner_that_waits_too_long() {
        let mut stuck = job(1, Status::Queued, Some(LaunchState::Launched));
        stuck.queued_at = NOW - 40 * MIN;
        stuck.updated_at = NOW - 39 * MIN;
        let instances = [instance("i-1", 1, 39 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[stuck]);
        assert_eq!(terminated(&plan), [("i-1", Reason::Idle)]);
        assert_eq!(plan.give_up, [(1, "r1".to_owned())]);
        assert_eq!(plan.launch, Vec::<u64>::new());
        assert_eq!(plan.reset, []);
    }

    #[test]
    fn terminates_orphans_and_old_instances() {
        let unknown = Instance {
            job_id: None,
            ..instance("i-1", 0, 11 * MIN)
        };
        let young_unknown = Instance {
            job_id: Some(99),
            ..instance("i-2", 99, 2 * MIN)
        };
        let mut long = job(3, Status::InProgress, Some(LaunchState::Launched));
        long.ran_on = Some("r3".to_owned());
        let instances = [unknown, young_unknown, instance("i-3", 3, 9 * 60 * MIN)];
        let plan = plan(NOW, &limits(), 10, Some(&instances), &[long]);
        assert_eq!(
            terminated(&plan),
            [("i-1", Reason::Orphan), ("i-3", Reason::MaxAge)]
        );
    }

    #[test]
    fn keeps_a_runner_github_did_not_name() {
        // Rows from before ran_on was recorded.
        let running = job(1, Status::InProgress, Some(LaunchState::Launched));
        let instances = [instance("i-1", 1, 60 * MIN)];
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&instances), &[running]),
            Plan::default()
        );
    }

    #[test]
    fn undone_launches_free_their_place() {
        // At max_runners with one launch whose runner is gone: undoing
        // it makes room to launch it again.
        let jobs = [
            job(1, Status::Queued, Some(LaunchState::Launched)),
            job(2, Status::InProgress, Some(LaunchState::Launched)),
        ];
        let instances = [instance("i-2", 2, 4 * MIN)];
        let plan = plan(NOW, &limits(), 2, Some(&instances), &jobs);
        assert_eq!(plan.launch, [1]);
    }

    #[test]
    fn launches_a_missing_image_again_once_it_is_there() {
        // 2026-10-14: the first hosted job failed with no arm64 image in
        // ap-south-1; the image was published a minute later while the
        // job stayed queued.
        let image = Failure::new(Cause::NoImage, "no arm64 image in ap-south-1");
        let mut first = failed(1, &image, 1, NOW - 20 * 1000);
        first.queued_at = NOW - 2 * MIN;
        // Not before its wait is over.
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), &[first.clone()]),
            Plan::default()
        );
        let later = NOW + 15 * 1000;
        let relaunch = plan(later, &limits(), 10, Some(&[]), &[first.clone()]);
        assert_eq!(
            relaunch.reset,
            [Reset {
                job_id: 1,
                launch_state: LaunchState::Failed,
                runner_name: Some("r1".to_owned()),
            }]
        );
        assert_eq!(relaunch.launch, [1]);
        // Without EC2 too: a failed launch has no instance to look for.
        assert_eq!(plan(later, &limits(), 10, None, &[first]).launch, [1]);
    }

    #[test]
    fn backs_off_between_failed_launches() {
        let capacity = Failure::new(Cause::NoCapacity, "no capacity for c7g.xlarge");
        // The fourth failure waits four minutes.
        let fourth = failed(1, &capacity, 4, NOW - 3 * MIN);
        assert_eq!(fourth.next_launch_at, Some(NOW + MIN));
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), std::slice::from_ref(&fourth)).launch,
            Vec::<u64>::new()
        );
        assert_eq!(
            plan(NOW + MIN, &limits(), 10, Some(&[]), &[fourth]).launch,
            [1]
        );
    }

    #[test]
    fn stops_once_github_finishes_the_job() {
        let throttled = Failure::new(Cause::Aws("RequestLimitExceeded".to_owned()), "slow down");
        let mut cancelled = failed(1, &throttled, 2, NOW - 10 * MIN);
        cancelled.status = Status::Completed;
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), &[cancelled]),
            Plan::default()
        );
    }

    #[test]
    fn a_setup_failure_waits_for_a_change() {
        let denied = Failure::new(Cause::Aws("AccessDenied".to_owned()), "denied");
        let waiting = failed(1, &denied, 1, NOW - 60 * MIN);
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), &[waiting]),
            Plan::default()
        );
        // A runner that waited too long is not launched again either.
        let mut given_up = job(2, Status::Queued, Some(LaunchState::Failed));
        given_up.updated_at = NOW - 60 * MIN;
        assert_eq!(
            plan(NOW, &limits(), 10, Some(&[]), &[given_up]),
            Plan::default()
        );
    }

    #[test]
    fn failed_launches_wait_for_room() {
        let broker = Failure::new(Cause::Broker, "broker answered 503");
        let jobs = [
            job(1, Status::InProgress, Some(LaunchState::Launched)),
            failed(2, &broker, 1, NOW - MIN),
        ];
        let instances = [instance("i-1", 1, 4 * MIN)];
        let full = plan(NOW, &limits(), 1, Some(&instances), &jobs);
        assert_eq!(full.launch, Vec::<u64>::new());
        assert_eq!(plan(NOW, &limits(), 2, Some(&instances), &jobs).launch, [2]);
    }
}
