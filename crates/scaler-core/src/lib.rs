// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Decides when a scale set needs runners launched or removed.
//!
//! [`Scaler`] does no I/O and never reads the clock. The caller feeds it
//! scale set messages, the instances it finds in the cloud and the time,
//! carries out the [`Action`]s it returns and reports back how each one
//! went. The same logic then runs in a tokio process and in a Cloudflare
//! Worker.
//!
//! Every runner gets its own instance and runs one job. A runner moves
//! through these phases:
//!
//! - [`Phase::Launching`]: the caller is registering it and starting its
//!   instance.
//! - [`Phase::Idle`]: the instance is up and the runner waits for a job.
//! - [`Phase::Busy`]: the runner started a job.
//! - [`Phase::Finished`]: the job completed. The instance powers itself
//!   off; [`Scaler::plan`] terminates it if it is still there after
//!   [`Config::finish_grace`].
//! - [`Phase::Removing`]: the caller is deleting the runner registration
//!   before terminating the instance.
//!
//! The scale set wants `min_runners + assigned jobs` runners, capped at
//! `max_runners`, the same target Actions Runner Controller uses. A
//! runner idle for longer than [`Config::idle_timeout`] is removed: either
//! its instance failed to start the runner, or the job it was launched for
//! went elsewhere. Warm runners kept by `min_runners` are recycled the
//! same way.
//!
//! A warm pool keeps [`Config::pool_size`] instances that booted once
//! without a runner and were then stopped. Their first boot read the
//! image's files off the snapshot, so a runner started from the pool
//! skips both the launch and the slow first read. A pool instance moves
//! through [`PoolState::Warming`], [`PoolState::Stopping`] and
//! [`PoolState::Ready`]; [`Scaler::plan`] hands ready ones to new runners
//! and launches replacements.

use std::collections::{BTreeMap, BTreeSet};

use hatchrun_scaleset::{JobMessageKind, Message};

/// Scaling limits and timeouts. Times are in seconds.
#[derive(Clone, Debug)]
pub struct Config {
    /// Prefix of runner names, usually the scale set name.
    pub name_prefix: String,
    /// Runners kept even with no jobs.
    pub min_runners: u32,
    /// Most runners at once.
    pub max_runners: u32,
    /// How long a runner may wait for a job before it is removed.
    pub idle_timeout: u64,
    /// How long an instance may stay up after its job completed.
    pub finish_grace: u64,
    /// How long a new instance may be missing from the inventory, which
    /// is eventually consistent, before it counts as gone.
    pub inventory_grace: u64,
    /// Stopped instances kept ready for new runners. 0 turns the pool
    /// off.
    pub pool_size: u32,
    /// How long a pool instance runs after launch before it is stopped,
    /// long enough to boot and read its files.
    pub pool_warmup: u64,
    /// Age at which a pool instance is replaced, so the pool picks up new
    /// images and launch template versions.
    pub pool_max_age: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name_prefix: "hatchrun".into(),
            min_runners: 0,
            max_runners: 10,
            idle_timeout: 15 * 60,
            finish_grace: 5 * 60,
            inventory_grace: 2 * 60,
            pool_size: 0,
            pool_warmup: 3 * 60,
            pool_max_age: 24 * 60 * 60,
        }
    }
}

/// Something the caller has to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Generate a JIT config for a runner with this name and start an
    /// instance with it. Report [`Scaler::launched`] or
    /// [`Scaler::launch_failed`].
    Launch {
        /// The runner name.
        name: String,
        /// A stopped pool instance to start for the runner instead of
        /// launching a new one. The runner owns it from here: when the
        /// launch fails after registering the runner, removing the runner
        /// terminates it.
        pooled: Option<String>,
    },
    /// Launch a pool instance with this name and no runner. Report
    /// [`Scaler::pool_launched`] or [`Scaler::pool_launch_failed`].
    LaunchPool {
        /// The instance name.
        name: String,
    },
    /// Stop the instance. Nothing to report: the inventory shows it
    /// stopped.
    Stop {
        /// The instance id.
        instance_id: String,
    },
    /// Delete the runner registration. Report [`Scaler::runner_removed`]
    /// (also when the runner is already gone) or
    /// [`Scaler::remove_failed`].
    RemoveRunner {
        /// The runner name.
        name: String,
        /// The runner id.
        runner_id: u64,
    },
    /// Terminate the instance. Nothing to report: an instance that
    /// survives shows up in the next inventory.
    Terminate {
        /// The instance id.
        instance_id: String,
    },
}

/// Where a runner is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Being registered and started.
    Launching,
    /// Up and waiting for a job.
    Idle,
    /// Running a job.
    Busy,
    /// Done with its job.
    Finished,
    /// Being removed.
    Removing,
}

/// A runner the scaler tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runner {
    /// Where the runner is.
    pub phase: Phase,
    /// The runner id, once registered.
    pub runner_id: Option<u64>,
    /// The instance id, once started.
    pub instance_id: Option<String>,
    /// When the runner entered its phase.
    pub since: u64,
}

/// An instance found in the cloud.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    /// The instance id.
    pub id: String,
    /// The runner name from the instance tags, `None` for a pool instance.
    pub runner_name: Option<String>,
    /// The runner id from the instance tags.
    pub runner_id: Option<u64>,
    /// When the instance launched.
    pub launched_at: u64,
    /// Whether the instance is stopped.
    pub stopped: bool,
}

/// Where a pool instance is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolState {
    /// Booting for the first time, reading its files.
    Warming,
    /// Asked to stop.
    Stopping,
    /// Stopped and ready for a runner.
    Ready,
}

/// A pool instance the scaler tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolInstance {
    /// Where the instance is.
    pub state: PoolState,
    /// When the instance launched.
    pub launched_at: u64,
    /// When the instance entered its state.
    pub since: u64,
}

/// A job event from a scale set message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEvent<'a> {
    /// A job can be acquired.
    Available {
        /// The id to acquire it by.
        runner_request_id: i64,
    },
    /// A runner started a job.
    Started {
        /// The runner name.
        runner_name: &'a str,
    },
    /// A runner finished a job.
    Completed {
        /// The runner name.
        runner_name: &'a str,
    },
}

/// Longest wait between launch attempts after failures.
const MAX_BACKOFF: u64 = 5 * 60;

/// Tracks the runners of one scale set and decides what to do next.
#[derive(Debug)]
pub struct Scaler {
    config: Config,
    runners: BTreeMap<String, Runner>,
    /// Pool instances by instance id.
    pool: BTreeMap<String, PoolInstance>,
    /// Names of pool instances being launched.
    pool_launching: BTreeSet<String>,
    assigned_jobs: u32,
    launched: u64,
    launch_failures: u32,
    launch_after: u64,
}

impl Scaler {
    /// Starts with no runners and no assigned jobs.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            runners: BTreeMap::new(),
            pool: BTreeMap::new(),
            pool_launching: BTreeSet::new(),
            assigned_jobs: 0,
            launched: 0,
            launch_failures: 0,
            launch_after: 0,
        }
    }

    /// Returns the runners by name.
    #[must_use]
    pub fn runners(&self) -> &BTreeMap<String, Runner> {
        &self.runners
    }

    /// Returns the pool instances by instance id.
    #[must_use]
    pub fn pool(&self) -> &BTreeMap<String, PoolInstance> {
        &self.pool
    }

    /// Returns the most runners the scale set can run, for the message
    /// poll.
    #[must_use]
    pub fn max_runners(&self) -> u32 {
        self.config.max_runners
    }

    /// Takes the job events and statistics of a message and returns the
    /// request ids to acquire. Call [`Scaler::plan`] afterwards.
    pub fn handle_message(&mut self, message: &Message, now: u64) -> Vec<i64> {
        let events = message.jobs.iter().filter_map(|job| match job.kind {
            JobMessageKind::JobAvailable => Some(JobEvent::Available {
                runner_request_id: job.runner_request_id,
            }),
            JobMessageKind::JobStarted => job
                .runner_name
                .as_deref()
                .map(|runner_name| JobEvent::Started { runner_name }),
            JobMessageKind::JobCompleted => job
                .runner_name
                .as_deref()
                .map(|runner_name| JobEvent::Completed { runner_name }),
            _ => None,
        });
        let assigned = message
            .statistics
            .map(|statistics| statistics.total_assigned_jobs);
        self.handle_jobs(events, assigned, now)
    }

    /// Like [`Scaler::handle_message`], for events from elsewhere.
    /// `assigned_jobs` is the scale set's assigned job count when known.
    pub fn handle_jobs<'a>(
        &mut self,
        events: impl IntoIterator<Item = JobEvent<'a>>,
        assigned_jobs: Option<u32>,
        now: u64,
    ) -> Vec<i64> {
        let mut acquire = Vec::new();
        for event in events {
            match event {
                JobEvent::Available { runner_request_id } => acquire.push(runner_request_id),
                JobEvent::Started { runner_name } => self.enter(runner_name, Phase::Busy, now),
                JobEvent::Completed { runner_name } => {
                    self.enter(runner_name, Phase::Finished, now);
                }
            }
        }
        if let Some(assigned) = assigned_jobs {
            self.assigned_jobs = assigned;
        }
        acquire
    }

    /// Returns the actions due at `now`: launches up to the target,
    /// removal of runners idle too long, termination of instances that
    /// outlived their job, and the upkeep of the warm pool.
    pub fn plan(&mut self, now: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        self.plan_pool_upkeep(now, &mut actions);

        let idle_timeout = self.config.idle_timeout;
        let finish_grace = self.config.finish_grace;
        let mut done = Vec::new();
        for (name, runner) in &mut self.runners {
            let age = now.saturating_sub(runner.since);
            match runner.phase {
                Phase::Idle if age >= idle_timeout => {
                    if let Some(runner_id) = runner.runner_id {
                        runner.phase = Phase::Removing;
                        runner.since = now;
                        actions.push(Action::RemoveRunner {
                            name: name.clone(),
                            runner_id,
                        });
                    } else if let Some(instance_id) = runner.instance_id.take() {
                        actions.push(Action::Terminate { instance_id });
                        done.push(name.clone());
                    }
                }
                Phase::Finished if age >= finish_grace => {
                    if let Some(instance_id) = runner.instance_id.take() {
                        actions.push(Action::Terminate { instance_id });
                    }
                    done.push(name.clone());
                }
                _ => {}
            }
        }
        for name in done {
            self.runners.remove(&name);
        }

        if now >= self.launch_after {
            let active = self
                .runners
                .values()
                .filter(|runner| {
                    matches!(runner.phase, Phase::Launching | Phase::Idle | Phase::Busy)
                })
                .count();
            let active = u32::try_from(active).unwrap_or(u32::MAX);
            for _ in active..self.target() {
                let name = self.next_name(now);
                let pooled = self.take_ready();
                self.runners.insert(
                    name.clone(),
                    Runner {
                        phase: Phase::Launching,
                        runner_id: None,
                        // A pooled instance belongs to the runner from now
                        // on, so the inventory never puts it back in the
                        // pool.
                        instance_id: pooled.clone(),
                        since: now,
                    },
                );
                actions.push(Action::Launch { name, pooled });
            }

            let pooled = self.pool.len() + self.pool_launching.len();
            let pooled = u32::try_from(pooled).unwrap_or(u32::MAX);
            for _ in pooled..self.config.pool_size {
                let name = self.next_pool_name(now);
                self.pool_launching.insert(name.clone());
                actions.push(Action::LaunchPool { name });
            }
        }
        actions
    }

    /// Stops pool instances that have warmed up and replaces old ones.
    fn plan_pool_upkeep(&mut self, now: u64, actions: &mut Vec<Action>) {
        let mut expired = Vec::new();
        for (instance_id, instance) in &mut self.pool {
            if now.saturating_sub(instance.launched_at) >= self.config.pool_max_age {
                expired.push(instance_id.clone());
            } else if instance.state == PoolState::Warming
                && now.saturating_sub(instance.since) >= self.config.pool_warmup
            {
                instance.state = PoolState::Stopping;
                instance.since = now;
                actions.push(Action::Stop {
                    instance_id: instance_id.clone(),
                });
            }
        }
        // A pool larger than configured, after the size was lowered,
        // gives up its oldest instances.
        let excess = self
            .pool
            .len()
            .saturating_sub(self.config.pool_size as usize)
            .saturating_sub(expired.len());
        let mut by_age: Vec<_> = self
            .pool
            .iter()
            .filter(|(id, _)| !expired.contains(id))
            .map(|(id, instance)| (instance.launched_at, id.clone()))
            .collect();
        by_age.sort();
        expired.extend(by_age.into_iter().take(excess).map(|(_, id)| id));
        for instance_id in expired {
            self.pool.remove(&instance_id);
            actions.push(Action::Terminate { instance_id });
        }
    }

    /// Takes the oldest ready pool instance, so the pool turns over.
    fn take_ready(&mut self) -> Option<String> {
        let instance_id = self
            .pool
            .iter()
            .filter(|(_, instance)| instance.state == PoolState::Ready)
            .min_by_key(|(_, instance)| instance.launched_at)
            .map(|(id, _)| id.clone())?;
        self.pool.remove(&instance_id);
        Some(instance_id)
    }

    /// Records a launched pool instance.
    pub fn pool_launched(&mut self, name: &str, instance_id: String, now: u64) {
        self.launch_failures = 0;
        self.launch_after = 0;
        self.pool_launching.remove(name);
        self.pool.insert(
            instance_id,
            PoolInstance {
                state: PoolState::Warming,
                launched_at: now,
                since: now,
            },
        );
    }

    /// Records a failed pool launch and backs off further launches.
    pub fn pool_launch_failed(&mut self, name: &str, now: u64) {
        self.pool_launching.remove(name);
        self.back_off(now);
    }

    /// Records a started instance.
    pub fn launched(&mut self, name: &str, runner_id: u64, instance_id: String, now: u64) {
        self.launch_failures = 0;
        self.launch_after = 0;
        if let Some(runner) = self.runners.get_mut(name) {
            runner.runner_id = Some(runner_id);
            runner.instance_id = Some(instance_id);
            if runner.phase == Phase::Launching {
                runner.phase = Phase::Idle;
                runner.since = now;
            }
        }
    }

    /// Records a failed launch and backs off further launches. Pass the
    /// runner id when the runner got registered before the launch failed;
    /// the returned action removes it again.
    pub fn launch_failed(
        &mut self,
        name: &str,
        runner_id: Option<u64>,
        now: u64,
    ) -> Option<Action> {
        self.back_off(now);
        let Some(runner_id) = runner_id else {
            self.runners.remove(name);
            return None;
        };
        let runner = self.runners.get_mut(name)?;
        runner.phase = Phase::Removing;
        runner.runner_id = Some(runner_id);
        runner.since = now;
        Some(Action::RemoveRunner {
            name: name.to_owned(),
            runner_id,
        })
    }

    /// Records a deleted runner registration and returns the termination
    /// of its instance.
    pub fn runner_removed(&mut self, name: &str) -> Option<Action> {
        let runner = self.runners.remove(name)?;
        runner
            .instance_id
            .map(|instance_id| Action::Terminate { instance_id })
    }

    /// Records a failed removal. `still_running` is set when the service
    /// refused because the runner picked up a job meanwhile.
    pub fn remove_failed(&mut self, name: &str, still_running: bool, now: u64) {
        if let Some(runner) = self.runners.get_mut(name) {
            if still_running {
                runner.phase = Phase::Busy;
                runner.since = now;
            } else {
                // Back to idle and already past the timeout, so the next
                // plan retries.
                runner.phase = Phase::Idle;
                runner.since = now.saturating_sub(self.config.idle_timeout);
            }
        }
    }

    /// Compares the tracked runners and pool with the instances that
    /// exist.
    ///
    /// Instances the scaler does not know, for example after a restart,
    /// are adopted: runner instances as idle runners, pool instances by
    /// whether they are stopped. A runner whose instance vanished, such as
    /// a reclaimed spot instance, has its registration removed.
    pub fn reconcile(&mut self, instances: &[Instance], now: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        for instance in instances {
            match &instance.runner_name {
                Some(runner_name) => {
                    self.runners
                        .entry(runner_name.clone())
                        .or_insert_with(|| Runner {
                            phase: Phase::Idle,
                            runner_id: instance.runner_id,
                            instance_id: Some(instance.id.clone()),
                            since: instance.launched_at,
                        });
                }
                None => self.reconcile_pool_instance(instance, now),
            }
        }
        let grace = self.config.inventory_grace;
        self.pool.retain(|id, pooled| {
            instances.iter().any(|instance| &instance.id == id)
                || now.saturating_sub(pooled.since) < grace
        });

        let grace = self.config.inventory_grace;
        let mut gone = Vec::new();
        for (name, runner) in &mut self.runners {
            let Some(instance_id) = &runner.instance_id else {
                continue;
            };
            if instances.iter().any(|instance| &instance.id == instance_id)
                || now.saturating_sub(runner.since) < grace
            {
                continue;
            }
            runner.instance_id = None;
            match (runner.phase, runner.runner_id) {
                (Phase::Finished, _) | (_, None) => gone.push(name.clone()),
                (Phase::Removing, Some(_)) => {}
                (_, Some(runner_id)) => {
                    runner.phase = Phase::Removing;
                    runner.since = now;
                    actions.push(Action::RemoveRunner {
                        name: name.clone(),
                        runner_id,
                    });
                }
            }
        }
        for name in gone {
            self.runners.remove(&name);
        }
        actions
    }

    fn reconcile_pool_instance(&mut self, instance: &Instance, now: u64) {
        // Handed to a runner but not yet tagged as one.
        if self
            .runners
            .values()
            .any(|runner| runner.instance_id.as_deref() == Some(instance.id.as_str()))
        {
            return;
        }
        let pooled = self
            .pool
            .entry(instance.id.clone())
            .or_insert_with(|| PoolInstance {
                state: if instance.stopped {
                    PoolState::Ready
                } else {
                    PoolState::Warming
                },
                launched_at: instance.launched_at,
                since: instance.launched_at,
            });
        if instance.stopped && pooled.state != PoolState::Ready {
            pooled.state = PoolState::Ready;
            pooled.since = now;
        }
    }

    fn back_off(&mut self, now: u64) {
        self.launch_failures = self.launch_failures.saturating_add(1);
        let backoff = 5u64
            .saturating_mul(1 << self.launch_failures.min(16))
            .min(MAX_BACKOFF);
        self.launch_after = now.saturating_add(backoff);
    }

    fn target(&self) -> u32 {
        self.config
            .min_runners
            .saturating_add(self.assigned_jobs)
            .min(self.config.max_runners)
    }

    fn enter(&mut self, name: &str, phase: Phase, now: u64) {
        if let Some(runner) = self.runners.get_mut(name) {
            runner.phase = phase;
            runner.since = now;
        }
    }

    /// Names are unique across restarts as long as two scalers for the
    /// same scale set never start within the same second, which the single
    /// message session already rules out.
    fn next_name(&mut self, now: u64) -> String {
        self.launched += 1;
        format!("{}-{now:x}-{}", self.config.name_prefix, self.launched)
    }

    fn next_pool_name(&mut self, now: u64) -> String {
        self.launched += 1;
        format!("{}-pool-{now:x}-{}", self.config.name_prefix, self.launched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;
    const NOTHING: [Action; 0] = [];

    fn scaler(min_runners: u32, max_runners: u32) -> Scaler {
        Scaler::new(Config {
            name_prefix: "linux".into(),
            min_runners,
            max_runners,
            ..Config::default()
        })
    }

    fn launches(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Launch { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// Launches `count` runners and reports them started.
    fn start(scaler: &mut Scaler, count: u32, now: u64) -> Vec<String> {
        scaler.handle_jobs([], Some(count), now);
        let names = launches(&scaler.plan(now));
        for (id, name) in (1..).zip(&names) {
            scaler.launched(name, id, format!("i-{id}"), now);
        }
        names
    }

    #[test]
    fn launches_one_runner_per_assigned_job() {
        let mut scaler = scaler(0, 10);
        assert_eq!(scaler.plan(T0), NOTHING);

        let acquire = scaler.handle_jobs(
            [
                JobEvent::Available {
                    runner_request_id: 7,
                },
                JobEvent::Available {
                    runner_request_id: 8,
                },
            ],
            Some(2),
            T0,
        );
        assert_eq!(acquire, [7, 8]);

        let names = launches(&scaler.plan(T0));
        assert_eq!(names, ["linux-f4240-1", "linux-f4240-2"]);
        // Runners being launched count, so a second plan adds nothing.
        assert_eq!(scaler.plan(T0), NOTHING);
    }

    #[test]
    fn target_respects_min_and_max() {
        let mut scaler = scaler(2, 3);
        assert_eq!(launches(&scaler.plan(T0)).len(), 2);
        scaler.handle_jobs([], Some(5), T0);
        assert_eq!(launches(&scaler.plan(T0)).len(), 1);
    }

    #[test]
    fn busy_runners_count_and_finished_ones_free_a_slot() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs(
            [JobEvent::Started {
                runner_name: &names[0],
            }],
            Some(1),
            T0 + 10,
        );
        assert_eq!(scaler.plan(T0 + 10), NOTHING);
        assert_eq!(scaler.runners()[&names[0]].phase, Phase::Busy);

        // The next job is assigned as the first one completes.
        scaler.handle_jobs(
            [JobEvent::Completed {
                runner_name: &names[0],
            }],
            Some(1),
            T0 + 20,
        );
        assert_eq!(launches(&scaler.plan(T0 + 20)).len(), 1);
    }

    #[test]
    fn terminates_instances_that_outlive_their_job() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs(
            [JobEvent::Completed {
                runner_name: &names[0],
            }],
            Some(0),
            T0,
        );
        assert_eq!(scaler.plan(T0 + 60), NOTHING);
        assert_eq!(
            scaler.plan(T0 + 5 * 60),
            [Action::Terminate {
                instance_id: "i-1".into()
            }]
        );
        assert_eq!(scaler.runners().len(), 0);
    }

    #[test]
    fn removes_runners_idle_too_long() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs([], Some(0), T0 + 60);
        assert_eq!(scaler.plan(T0 + 60), NOTHING);

        let later = T0 + 15 * 60;
        assert_eq!(
            scaler.plan(later),
            [Action::RemoveRunner {
                name: names[0].clone(),
                runner_id: 1
            }]
        );
        assert_eq!(scaler.runners()[&names[0]].phase, Phase::Removing);
        assert_eq!(
            scaler.runner_removed(&names[0]),
            Some(Action::Terminate {
                instance_id: "i-1".into()
            })
        );
        assert_eq!(scaler.runners().len(), 0);
    }

    #[test]
    fn stuck_runner_is_replaced() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        // The job is still assigned, but the runner never picked it up.
        let later = T0 + 15 * 60;
        let actions = scaler.plan(later);
        assert!(matches!(&actions[0], Action::RemoveRunner { name, .. } if *name == names[0]));
        assert_eq!(launches(&actions).len(), 1);
    }

    #[test]
    fn removal_refused_for_running_job_marks_busy() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs([], Some(0), T0);
        scaler.plan(T0 + 15 * 60);
        scaler.remove_failed(&names[0], true, T0 + 15 * 60);
        assert_eq!(scaler.runners()[&names[0]].phase, Phase::Busy);
    }

    #[test]
    fn failed_removal_is_retried() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs([], Some(0), T0);
        let later = T0 + 15 * 60;
        scaler.plan(later);
        scaler.remove_failed(&names[0], false, later);
        assert!(matches!(
            scaler.plan(later + 1)[..],
            [Action::RemoveRunner { .. }]
        ));
    }

    #[test]
    fn launch_failures_back_off() {
        let mut scaler = scaler(0, 10);
        scaler.handle_jobs([], Some(1), T0);
        let name = launches(&scaler.plan(T0)).remove(0);
        assert_eq!(scaler.launch_failed(&name, None, T0), None);

        assert_eq!(scaler.plan(T0 + 9), NOTHING);
        let name = launches(&scaler.plan(T0 + 10)).remove(0);
        scaler.launch_failed(&name, None, T0 + 10);
        assert_eq!(scaler.plan(T0 + 29), NOTHING);
        let name = launches(&scaler.plan(T0 + 30)).remove(0);

        // Success resets the backoff.
        scaler.launched(&name, 1, "i-1".into(), T0 + 30);
        scaler.handle_jobs([], Some(2), T0 + 31);
        assert_eq!(launches(&scaler.plan(T0 + 31)).len(), 1);
    }

    #[test]
    fn backoff_is_capped() {
        let mut scaler = scaler(0, 10);
        scaler.handle_jobs([], Some(1), T0);
        let mut now = T0;
        for _ in 0..40 {
            let name = launches(&scaler.plan(now)).remove(0);
            scaler.launch_failed(&name, None, now);
            now += MAX_BACKOFF;
        }
    }

    #[test]
    fn launch_failure_after_registration_removes_runner() {
        let mut scaler = scaler(0, 10);
        scaler.handle_jobs([], Some(1), T0);
        let name = launches(&scaler.plan(T0)).remove(0);
        assert_eq!(
            scaler.launch_failed(&name, Some(9), T0),
            Some(Action::RemoveRunner {
                name: name.clone(),
                runner_id: 9
            })
        );
        assert_eq!(scaler.runner_removed(&name), None);
    }

    #[test]
    fn adopts_unknown_instances() {
        let mut scaler = scaler(0, 10);
        scaler.handle_jobs([], Some(1), T0);
        let instance = Instance {
            id: "i-9".into(),
            runner_name: Some("linux-1".into()),
            runner_id: Some(9),
            launched_at: T0 - 30,
            stopped: false,
        };
        assert_eq!(scaler.reconcile(&[instance], T0), NOTHING);
        assert_eq!(scaler.runners()["linux-1"].phase, Phase::Idle);
        // The adopted runner covers the assigned job.
        assert_eq!(scaler.plan(T0), NOTHING);
    }

    #[test]
    fn vanished_instance_removes_runner() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        // New instances may be missing from the inventory for a while.
        assert_eq!(scaler.reconcile(&[], T0 + 60), NOTHING);
        assert_eq!(
            scaler.reconcile(&[], T0 + 2 * 60),
            [Action::RemoveRunner {
                name: names[0].clone(),
                runner_id: 1
            }]
        );
        // The instance is gone, so there is nothing to terminate.
        assert_eq!(scaler.runner_removed(&names[0]), None);
    }

    #[test]
    fn vanished_finished_instance_is_forgotten() {
        let mut scaler = scaler(0, 10);
        let names = start(&mut scaler, 1, T0);
        scaler.handle_jobs(
            [JobEvent::Completed {
                runner_name: &names[0],
            }],
            Some(0),
            T0 + 60,
        );
        assert_eq!(scaler.reconcile(&[], T0 + 4 * 60), NOTHING);
        assert_eq!(scaler.runners().len(), 0);
    }

    #[test]
    fn events_for_unknown_runners_are_ignored() {
        let mut scaler = scaler(0, 10);
        scaler.handle_jobs(
            [
                JobEvent::Started {
                    runner_name: "other",
                },
                JobEvent::Completed {
                    runner_name: "other",
                },
            ],
            None,
            T0,
        );
        assert_eq!(scaler.runners().len(), 0);
    }

    #[test]
    fn reads_scale_set_messages() {
        let session: hatchrun_scaleset::Session = serde_json::from_str(
            r#"{"sessionId":"s","messageQueueUrl":"https://q.example/m","messageQueueAccessToken":"t"}"#,
        )
        .unwrap();
        let jobs = serde_json::json!([
            {"messageType": "JobAvailable", "runnerRequestId": 11},
            {"messageType": "JobAssigned", "runnerRequestId": 11},
            {"messageType": "JobStarted", "runnerRequestId": 10, "runnerName": "linux-a"},
        ])
        .to_string();
        let body = serde_json::json!({
            "messageId": 1,
            "messageType": "RunnerScaleSetJobMessages",
            "body": jobs,
            "statistics": {"totalAssignedJobs": 3},
        })
        .to_string();
        let response = http::Response::builder()
            .status(200)
            .body(body.into_bytes())
            .unwrap();
        let message = session
            .get_message(0, 10)
            .unwrap()
            .parser
            .parse(&response)
            .unwrap()
            .unwrap();

        let mut scaler = scaler(0, 10);
        assert_eq!(scaler.handle_message(&message, T0), [11]);
        assert_eq!(launches(&scaler.plan(T0)).len(), 3);
    }

    fn pool_scaler(pool_size: u32) -> Scaler {
        Scaler::new(Config {
            name_prefix: "linux".into(),
            pool_size,
            ..Config::default()
        })
    }

    fn pool_instance(id: &str, launched_at: u64, stopped: bool) -> Instance {
        Instance {
            id: id.into(),
            runner_name: None,
            runner_id: None,
            launched_at,
            stopped,
        }
    }

    fn pool_launches(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::LaunchPool { name } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// Fills a pool and reports every instance stopped.
    fn ready_pool(scaler: &mut Scaler, now: u64) -> Vec<String> {
        let names = pool_launches(&scaler.plan(now));
        let ids: Vec<String> = (1..=names.len()).map(|n| format!("i-p{n}")).collect();
        for (name, id) in names.iter().zip(&ids) {
            scaler.pool_launched(name, id.clone(), now);
        }
        let stopped: Vec<_> = ids.iter().map(|id| pool_instance(id, now, true)).collect();
        scaler.reconcile(&stopped, now + 1);
        ids
    }

    #[test]
    fn pool_fills_warms_up_and_stops() {
        let mut scaler = pool_scaler(2);
        let names = pool_launches(&scaler.plan(T0));
        assert_eq!(names, ["linux-pool-f4240-1", "linux-pool-f4240-2"]);
        // Launches in flight count towards the pool.
        assert_eq!(scaler.plan(T0), NOTHING);

        scaler.pool_launched(&names[0], "i-a".into(), T0);
        scaler.pool_launched(&names[1], "i-b".into(), T0);
        assert_eq!(scaler.plan(T0 + 60), NOTHING);
        assert_eq!(
            scaler.plan(T0 + 3 * 60),
            [
                Action::Stop {
                    instance_id: "i-a".into()
                },
                Action::Stop {
                    instance_id: "i-b".into()
                },
            ]
        );
        assert_eq!(scaler.pool()["i-a"].state, PoolState::Stopping);

        let inventory = [
            pool_instance("i-a", T0, true),
            pool_instance("i-b", T0, false),
        ];
        assert_eq!(scaler.reconcile(&inventory, T0 + 4 * 60), NOTHING);
        assert_eq!(scaler.pool()["i-a"].state, PoolState::Ready);
        assert_eq!(scaler.pool()["i-b"].state, PoolState::Stopping);
    }

    #[test]
    fn runner_starts_from_the_pool_and_the_pool_refills() {
        let mut scaler = pool_scaler(1);
        let ids = ready_pool(&mut scaler, T0);
        scaler.handle_jobs([], Some(1), T0 + 10);
        let actions = scaler.plan(T0 + 10);
        assert_eq!(
            actions,
            [
                Action::Launch {
                    name: "linux-f424a-2".into(),
                    pooled: Some(ids[0].clone()),
                },
                Action::LaunchPool {
                    name: "linux-pool-f424a-3".into()
                },
            ]
        );
        assert!(scaler.pool().is_empty());
        assert_eq!(
            scaler.runners()["linux-f424a-2"].instance_id.as_deref(),
            Some("i-p1")
        );
    }

    #[test]
    fn runner_launches_new_while_the_pool_warms() {
        let mut scaler = pool_scaler(1);
        let names = pool_launches(&scaler.plan(T0));
        scaler.pool_launched(&names[0], "i-a".into(), T0);
        scaler.handle_jobs([], Some(1), T0 + 10);
        assert_eq!(
            scaler.plan(T0 + 10),
            [Action::Launch {
                name: "linux-f424a-2".into(),
                pooled: None,
            }]
        );
    }

    #[test]
    fn handed_out_instance_does_not_return_to_the_pool() {
        let mut scaler = pool_scaler(1);
        let ids = ready_pool(&mut scaler, T0);
        scaler.handle_jobs([], Some(1), T0 + 10);
        scaler.plan(T0 + 10);
        // The inventory still lists it untagged, as a pool instance.
        scaler.reconcile(&[pool_instance(&ids[0], T0, true)], T0 + 20);
        assert!(!scaler.pool().contains_key(&ids[0]));
    }

    #[test]
    fn old_pool_instances_are_replaced() {
        let mut scaler = pool_scaler(1);
        let ids = ready_pool(&mut scaler, T0);
        let later = T0 + 24 * 60 * 60;
        assert_eq!(
            scaler.plan(later),
            [
                Action::Terminate {
                    instance_id: ids[0].clone()
                },
                Action::LaunchPool {
                    name: format!("linux-pool-{later:x}-2")
                },
            ]
        );
    }

    #[test]
    fn adopted_pool_shrinks_to_its_size() {
        let mut scaler = pool_scaler(1);
        let inventory = [
            pool_instance("i-old", T0 - 600, true),
            pool_instance("i-new", T0 - 60, true),
            pool_instance("i-warm", T0 - 10, false),
        ];
        scaler.reconcile(&inventory, T0);
        assert_eq!(scaler.pool()["i-warm"].state, PoolState::Warming);
        let terminated: Vec<_> = scaler
            .plan(T0)
            .into_iter()
            .filter_map(|action| match action {
                Action::Terminate { instance_id } => Some(instance_id),
                _ => None,
            })
            .collect();
        assert_eq!(terminated, ["i-old", "i-new"]);
        assert_eq!(scaler.pool().len(), 1);
    }

    #[test]
    fn vanished_pool_instance_is_forgotten() {
        let mut scaler = pool_scaler(1);
        ready_pool(&mut scaler, T0);
        scaler.reconcile(&[], T0 + 10 * 60);
        assert!(scaler.pool().is_empty());
        assert_eq!(pool_launches(&scaler.plan(T0 + 10 * 60)).len(), 1);
    }

    #[test]
    fn failed_pool_launch_backs_off() {
        let mut scaler = pool_scaler(1);
        let names = pool_launches(&scaler.plan(T0));
        scaler.pool_launch_failed(&names[0], T0);
        assert_eq!(scaler.plan(T0 + 1), NOTHING);
        assert_eq!(pool_launches(&scaler.plan(T0 + 10)).len(), 1);
    }
}
