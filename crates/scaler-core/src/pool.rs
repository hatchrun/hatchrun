// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The warm pool: stopped instances that already booted once, kept ready
//! for new runners.
//!
//! Every time is in the unit of the [`PoolConfig`]. [`crate::Scaler`] uses
//! seconds; another caller may use milliseconds as long as it does so
//! everywhere (config, `now` and the rows it restores).

use std::collections::{BTreeMap, BTreeSet};

use crate::{Action, Instance, PoolInstance, PoolState};

/// Size and timings of a pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Stopped instances kept ready. 0 turns the pool off.
    pub size: u32,
    /// How long an instance runs after launch before it is stopped, long
    /// enough to boot and read its files.
    pub warmup: u64,
    /// Age at which an instance is replaced, so the pool picks up new
    /// images and launch template versions.
    pub max_age: u64,
    /// How long a new instance may be missing from the inventory, which
    /// is eventually consistent, before it counts as gone.
    pub inventory_grace: u64,
}

/// Launch failures and the time before which nothing launches. A
/// [`crate::Scaler`] shares one between its runners and its pool.
#[derive(Clone, Debug)]
pub struct Backoff {
    /// Failures since the last success.
    pub failures: u32,
    /// Nothing launches before this time.
    pub after: u64,
    /// The wait unit: a failure waits `base * 2^failures`.
    pub base: u64,
    /// Longest wait.
    pub max: u64,
}

impl Backoff {
    /// No failures, ready to launch.
    #[must_use]
    pub fn new(base: u64, max: u64) -> Backoff {
        Backoff {
            failures: 0,
            after: 0,
            base,
            max,
        }
    }

    /// Records a failed launch at `now` and delays the next ones.
    pub fn failed(&mut self, now: u64) {
        self.failures = self.failures.saturating_add(1);
        let wait = self
            .base
            .saturating_mul(1 << self.failures.min(16))
            .min(self.max);
        self.after = now.saturating_add(wait);
    }

    /// Records a successful launch.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.after = 0;
    }

    /// Whether launching is allowed at `now`.
    #[must_use]
    pub fn ready(&self, now: u64) -> bool {
        now >= self.after
    }
}

/// Runner and pool names from one counter.
#[derive(Clone, Debug)]
pub struct Names {
    /// Prefix of every name.
    pub prefix: String,
    /// Names handed out so far.
    pub launched: u64,
}

impl Names {
    /// Names are unique across restarts as long as two owners of the same
    /// prefix never start within the same time unit.
    pub fn runner(&mut self, now: u64) -> String {
        self.launched += 1;
        format!("{}-{now:x}-{}", self.prefix, self.launched)
    }

    /// A name for a pool instance.
    pub fn pool(&mut self, now: u64) -> String {
        self.launched += 1;
        format!("{}-pool-{now:x}-{}", self.prefix, self.launched)
    }
}

/// The instances of a warm pool and the launches in flight.
#[derive(Debug)]
pub struct Pool {
    config: PoolConfig,
    /// Instances by instance id.
    instances: BTreeMap<String, PoolInstance>,
    /// Names of instances being launched.
    launching: BTreeSet<String>,
}

impl Pool {
    /// An empty pool.
    #[must_use]
    pub fn new(config: PoolConfig) -> Pool {
        Pool::restore(config, [])
    }

    /// A pool that already holds these instances, by id.
    #[must_use]
    pub fn restore(
        config: PoolConfig,
        instances: impl IntoIterator<Item = (String, PoolInstance)>,
    ) -> Pool {
        Pool {
            config,
            instances: instances.into_iter().collect(),
            launching: BTreeSet::new(),
        }
    }

    /// Changes the size. Excess instances go at the next [`Pool::upkeep`].
    pub fn set_size(&mut self, size: u32) {
        self.config.size = size;
    }

    /// Takes in instances without a runner name, except those `handed`
    /// says a runner already owns; drops entries missing from the
    /// inventory past the grace.
    pub fn reconcile(&mut self, instances: &[Instance], handed: &dyn Fn(&str) -> bool, now: u64) {
        for instance in instances {
            // Handed to a runner but not yet tagged as one.
            if instance.runner_name.is_some() || handed(&instance.id) {
                continue;
            }
            let pooled = self
                .instances
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
        let grace = self.config.inventory_grace;
        self.instances.retain(|id, pooled| {
            instances.iter().any(|instance| &instance.id == id)
                || now.saturating_sub(pooled.since) < grace
        });
    }

    /// Stop actions for warmed-up instances, Terminate for old and excess
    /// ones.
    pub fn upkeep(&mut self, now: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut expired = Vec::new();
        for (instance_id, instance) in &mut self.instances {
            if now.saturating_sub(instance.launched_at) >= self.config.max_age {
                expired.push(instance_id.clone());
            } else if instance.state == PoolState::Warming
                && now.saturating_sub(instance.since) >= self.config.warmup
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
            .instances
            .len()
            .saturating_sub(self.config.size as usize)
            .saturating_sub(expired.len());
        let mut by_age: Vec<_> = self
            .instances
            .iter()
            .filter(|(id, _)| !expired.contains(id))
            .map(|(id, instance)| (instance.launched_at, id.clone()))
            .collect();
        by_age.sort();
        expired.extend(by_age.into_iter().take(excess).map(|(_, id)| id));
        for instance_id in expired {
            self.instances.remove(&instance_id);
            actions.push(Action::Terminate { instance_id });
        }
        actions
    }

    /// `LaunchPool` actions up to the size, none while `backoff` is not
    /// ready.
    pub fn refill(&mut self, now: u64, backoff: &Backoff, names: &mut Names) -> Vec<Action> {
        if !backoff.ready(now) {
            return Vec::new();
        }
        let held = self.instances.len() + self.launching.len();
        let held = u32::try_from(held).unwrap_or(u32::MAX);
        let mut actions = Vec::new();
        for _ in held..self.config.size {
            let name = names.pool(now);
            self.launching.insert(name.clone());
            actions.push(Action::LaunchPool { name });
        }
        actions
    }

    /// Takes the oldest ready instance, so the pool turns over.
    pub fn take_ready(&mut self) -> Option<String> {
        let instance_id = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.state == PoolState::Ready)
            .min_by_key(|(_, instance)| instance.launched_at)
            .map(|(id, _)| id.clone())?;
        self.instances.remove(&instance_id);
        Some(instance_id)
    }

    /// Records a launched instance.
    pub fn launched(&mut self, name: &str, instance_id: String, now: u64, backoff: &mut Backoff) {
        backoff.succeeded();
        self.launching.remove(name);
        self.instances.insert(
            instance_id,
            PoolInstance {
                state: PoolState::Warming,
                launched_at: now,
                since: now,
            },
        );
    }

    /// Records a failed launch and backs off further launches.
    pub fn launch_failed(&mut self, name: &str, now: u64, backoff: &mut Backoff) {
        self.launching.remove(name);
        backoff.failed(now);
    }

    /// The instances by instance id.
    #[must_use]
    pub fn instances(&self) -> &BTreeMap<String, PoolInstance> {
        &self.instances
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(size: u32) -> PoolConfig {
        PoolConfig {
            size,
            warmup: 180,
            max_age: 1000,
            inventory_grace: 120,
        }
    }

    fn names() -> Names {
        Names {
            prefix: "p".into(),
            launched: 0,
        }
    }

    fn launches(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|action| matches!(action, Action::LaunchPool { .. }))
            .count()
    }

    fn ready(launched_at: u64) -> PoolInstance {
        PoolInstance {
            state: PoolState::Ready,
            launched_at,
            since: launched_at,
        }
    }

    fn untagged(id: &str, launched_at: u64, stopped: bool) -> Instance {
        Instance {
            id: id.into(),
            runner_name: None,
            runner_id: None,
            launched_at,
            stopped,
        }
    }

    #[test]
    fn refill_launches_up_to_size() {
        let mut pool = Pool::new(config(3));
        let backoff = Backoff::new(5, 300);
        let mut names = names();
        assert_eq!(launches(&pool.refill(0, &backoff, &mut names)), 3);
        assert_eq!(pool.refill(0, &backoff, &mut names), []);
    }

    #[test]
    fn refill_waits_for_the_backoff() {
        let mut pool = Pool::new(config(1));
        let mut backoff = Backoff::new(5, 300);
        let mut names = names();
        let Action::LaunchPool { name } = pool.refill(0, &backoff, &mut names).remove(0) else {
            panic!("expected a pool launch");
        };
        pool.launch_failed(&name, 0, &mut backoff);
        // The first failure waits base * 2, as the scaler always has.
        assert_eq!(pool.refill(9, &backoff, &mut names), []);
        assert_eq!(launches(&pool.refill(10, &backoff, &mut names)), 1);
    }

    #[test]
    fn backoff_doubles_up_to_its_maximum() {
        let mut backoff = Backoff::new(5, 300);
        backoff.failed(0);
        assert_eq!(backoff.after, 10);
        backoff.failed(10);
        assert_eq!(backoff.after, 30);
        for _ in 0..40 {
            backoff.failed(0);
        }
        assert_eq!(backoff.after, 300);
        backoff.succeeded();
        assert!(backoff.ready(0));
    }

    #[test]
    fn names_share_one_counter() {
        let mut names = names();
        assert_eq!(names.runner(255), "p-ff-1");
        assert_eq!(names.pool(255), "p-pool-ff-2");
        assert_eq!(names.runner(255), "p-ff-3");
    }

    #[test]
    fn upkeep_stops_warmed_instances() {
        let mut pool = Pool::new(config(1));
        let mut backoff = Backoff::new(5, 300);
        pool.launched("n", "i-1".into(), 0, &mut backoff);
        assert_eq!(pool.upkeep(179), []);
        assert_eq!(
            pool.upkeep(180),
            [Action::Stop {
                instance_id: "i-1".into()
            }]
        );
        assert_eq!(pool.upkeep(181), []);
    }

    #[test]
    fn shrinking_terminates_the_oldest() {
        let mut pool = Pool::restore(
            config(3),
            [
                ("i-b".to_owned(), ready(20)),
                ("i-a".to_owned(), ready(10)),
                ("i-c".to_owned(), ready(30)),
            ],
        );
        pool.set_size(1);
        let mut terminated: Vec<_> = pool
            .upkeep(100)
            .into_iter()
            .map(|action| match action {
                Action::Terminate { instance_id } => instance_id,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        terminated.sort();
        assert_eq!(terminated, ["i-a", "i-b"]);
        assert_eq!(pool.instances().keys().collect::<Vec<_>>(), ["i-c"]);
    }

    #[test]
    fn old_instances_are_replaced() {
        let mut pool = Pool::restore(config(1), [("i-1".to_owned(), ready(0))]);
        assert_eq!(
            pool.upkeep(1000),
            [Action::Terminate {
                instance_id: "i-1".into()
            }]
        );
        let launched = pool.refill(1000, &Backoff::new(5, 300), &mut names());
        assert_eq!(launches(&launched), 1);
    }

    #[test]
    fn handed_instances_are_skipped() {
        let mut pool = Pool::new(config(2));
        let inventory = [untagged("i-1", 0, true), untagged("i-2", 0, true)];
        pool.reconcile(&inventory, &|id| id == "i-1", 10);
        assert_eq!(pool.instances().keys().collect::<Vec<_>>(), ["i-2"]);
    }

    #[test]
    fn restore_keeps_entries_within_grace() {
        let mut pool = Pool::restore(config(1), [("i-1".to_owned(), ready(100))]);
        pool.reconcile(&[], &|_| false, 100 + 119);
        assert_eq!(pool.instances().len(), 1);
        pool.reconcile(&[], &|_| false, 100 + 120);
        assert!(pool.instances().is_empty());
    }

    #[test]
    fn take_ready_takes_the_oldest() {
        let mut pool = Pool::restore(
            config(3),
            [
                ("i-b".to_owned(), ready(20)),
                ("i-a".to_owned(), ready(10)),
                (
                    "i-w".to_owned(),
                    PoolInstance {
                        state: PoolState::Warming,
                        launched_at: 1,
                        since: 1,
                    },
                ),
            ],
        );
        assert_eq!(pool.take_ready().as_deref(), Some("i-a"));
        assert_eq!(pool.take_ready().as_deref(), Some("i-b"));
        assert_eq!(pool.take_ready(), None);
    }

    #[test]
    fn milliseconds_work_the_same() {
        let mut pool = Pool::new(PoolConfig {
            size: 1,
            warmup: 180_000,
            max_age: 86_400_000,
            inventory_grace: 120_000,
        });
        let mut backoff = Backoff::new(5_000, 300_000);
        pool.launched("n", "i-1".into(), 1_700_000_000_000, &mut backoff);
        assert_eq!(pool.upkeep(1_700_000_179_999), []);
        assert_eq!(pool.upkeep(1_700_000_180_000).len(), 1);
    }
}
