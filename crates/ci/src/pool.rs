// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The warm pool of CI runner instances, one per runner template.
//!
//! An instance in the pool booted once with no user data and was
//! stopped. It holds no job, no ticket and no tenant data. A job takes
//! it ([`crate::launches`]): the launcher sets the job's user data on the
//! stopped instance, tags it and starts it, and the job's agent then
//! claims it like any fresh instance, with the metadata endpoint turned
//! off before the job document is handed over. An instance is never
//! returned to the pool; it is terminated when the job ends.
//!
//! The pool logic is [`hatchrun_scaler_core::pool::Pool`], the same type
//! hatchrun.io's own warm pool uses. This module gives it the CI
//! numbers, in milliseconds throughout, and maps between its instances,
//! the `ci_pool` rows and what EC2 describes. The reconciler runs it
//! ([`crate::reconcile`]).

use std::collections::BTreeMap;

use hatchrun_cloud::aws::instances::Described;
use hatchrun_cloud::parse_rfc3339;
use hatchrun_scaler_core::pool::{Backoff, Names, PoolConfig};
use hatchrun_scaler_core::{Instance, PoolInstance, PoolState};
use hatchrun_workflow::{Template, template};

use crate::store::PoolRow;

/// The most instances one template's pool holds.
pub const POOL_MAX: u32 = 10;
/// The length of a demand bucket, in milliseconds.
pub const BUCKET_MS: u64 = 300_000;
/// How far back the demand looks, in milliseconds.
pub const WINDOW_MS: u64 = 3_600_000;

/// How long an instance runs after launch before it is stopped.
const WARMUP_MS: u64 = 180_000;
/// The age at which an instance is replaced, so the pool picks up new
/// images.
const MAX_AGE_MS: u64 = 86_400_000;
/// How long a new instance may be missing from the inventory.
const GRACE_MS: u64 = 120_000;
/// How long past [`MAX_AGE_MS`] an instance no row records may stay.
const STALE_AFTER_MS: u64 = 3_600_000;

/// The pool size for the claims of each five-minute bucket of the last
/// hour: the busiest bucket, at most [`POOL_MAX`]. 0 when idle.
#[must_use]
pub fn pool_target(claims_per_bucket: &[u64]) -> u32 {
    let busiest = claims_per_bucket.iter().copied().max().unwrap_or(0);
    u32::try_from(busiest).map_or(POOL_MAX, |busiest| busiest.min(POOL_MAX))
}

/// The claims of each bucket by template, from [`crate::store::claims_by_bucket`]:
/// the template's first label to the claims of each bucket that had any.
/// A job names any label of its template; labels of no template are left
/// out.
#[must_use]
pub fn demand(claims: &[(String, u64, u64)]) -> BTreeMap<&'static str, Vec<u64>> {
    let mut buckets: BTreeMap<(&'static str, u64), u64> = BTreeMap::new();
    for (label, bucket, claimed) in claims {
        if let Some(found) = template(label) {
            *buckets.entry((found.labels[0], *bucket)).or_default() += claimed;
        }
    }
    let mut by_template: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    for ((label, _), claimed) in buckets {
        by_template.entry(label).or_default().push(claimed);
    }
    by_template
}

/// The timings of a pool of `size`, in milliseconds.
#[must_use]
pub fn config(_template: &Template, size: u32) -> PoolConfig {
    PoolConfig {
        size,
        warmup: WARMUP_MS,
        max_age: MAX_AGE_MS,
        inventory_grace: GRACE_MS,
    }
}

/// Names for a template's pool instances, `ci-pool-<label>-pool-<now
/// hex>-<n>`. The counter starts at 0 each pass and the time in the name
/// keeps names of different passes apart.
#[must_use]
pub fn names(template: &Template) -> Names {
    Names {
        prefix: format!("ci-pool-{}", template.labels[0]),
        launched: 0,
    }
}

/// The wait between failed pool launches, new for each pass.
#[must_use]
pub fn backoff() -> Backoff {
    Backoff::new(5_000, 300_000)
}

/// What the pool tracks of a stored row. `None` for a state that is not
/// one of the three.
#[must_use]
pub fn instance(row: &PoolRow) -> Option<(String, PoolInstance)> {
    let state = match row.state.as_str() {
        "warming" => PoolState::Warming,
        "stopping" => PoolState::Stopping,
        "ready" => PoolState::Ready,
        _ => return None,
    };
    Some((
        row.instance_id.clone(),
        PoolInstance {
            state,
            launched_at: row.launched_at,
            since: row.since,
        },
    ))
}

/// The row of a pool instance of `template`.
#[must_use]
pub fn row(template: &Template, instance_id: &str, pooled: &PoolInstance) -> PoolRow {
    let state = match pooled.state {
        PoolState::Warming => "warming",
        PoolState::Stopping => "stopping",
        PoolState::Ready => "ready",
    };
    PoolRow {
        instance_id: instance_id.to_owned(),
        template: template.labels[0].to_owned(),
        state: state.to_owned(),
        launched_at: pooled.launched_at,
        since: pooled.since,
    }
}

/// The pool instances of `template` among `described`, as the pool's
/// inventory. A stopped instance is ready; a launch time that does not
/// parse counts as 0, which is old.
#[must_use]
pub fn inventory(described: &[Described], template: &Template) -> Vec<Instance> {
    described
        .iter()
        .filter(|instance| instance.pool.as_deref() == Some(template.labels[0]))
        .map(|instance| Instance {
            id: instance.id.clone(),
            runner_name: None,
            runner_id: None,
            launched_at: parse_rfc3339(&instance.launch_time).unwrap_or(0),
            stopped: instance.state == "stopped",
        })
        .collect()
}

/// Pool instances in the inventory that no row records.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unrecorded {
    /// Running ones, with their launch time: a launch whose row was
    /// lost. A job never starts an instance that still has its pool tag,
    /// so these belong to the pool and are recorded as warming.
    pub running: Vec<(String, u64)>,
    /// Stopped ones older than the pool keeps an instance, with an hour
    /// to spare: no job is taking them any more, so they are terminated.
    /// A younger stopped one may be a job's, between its row being taken
    /// and its pool tag deleted.
    pub stale: Vec<String>,
}

/// The instances of `inventory` that `recorded` does not know.
#[must_use]
pub fn unrecorded(inventory: &[Instance], recorded: &dyn Fn(&str) -> bool, now: u64) -> Unrecorded {
    let mut found = Unrecorded::default();
    for instance in inventory.iter().filter(|instance| !recorded(&instance.id)) {
        if !instance.stopped {
            found
                .running
                .push((instance.id.clone(), instance.launched_at));
        } else if now.saturating_sub(instance.launched_at) >= MAX_AGE_MS + STALE_AFTER_MS {
            found.stale.push(instance.id.clone());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use hatchrun_workflow::templates::template;

    use super::*;

    #[test]
    fn pool_target_follows_the_busiest_bucket() {
        assert_eq!(pool_target(&[0, 0, 1, 3, 0, 0, 0, 0, 0, 0, 0, 1]), 3);
    }

    #[test]
    fn pool_target_is_zero_when_idle() {
        assert_eq!(pool_target(&[0; 12]), 0);
        assert_eq!(pool_target(&[]), 0);
    }

    #[test]
    fn pool_target_caps_at_ten() {
        assert_eq!(pool_target(&[25]), 10);
        assert_eq!(pool_target(&[u64::MAX]), 10);
    }

    #[test]
    fn rows_and_pool_agree_on_milliseconds() {
        let t = template("ubuntu-latest").unwrap();
        let stored = PoolRow {
            instance_id: "i-1".into(),
            template: "ubuntu-latest".into(),
            state: "ready".into(),
            launched_at: 1_700_000_000_123,
            since: 1_700_000_180_456,
        };
        let (id, pooled) = instance(&stored).unwrap();
        assert_eq!(pooled.state, PoolState::Ready);
        assert_eq!(row(t, &id, &pooled), stored);
        let mut odd = stored;
        odd.state = "gone".into();
        assert_eq!(instance(&odd), None);
    }

    #[test]
    fn timings_are_milliseconds() {
        let t = template("ubuntu-latest").unwrap();
        let config = config(t, 3);
        assert_eq!(
            (
                config.size,
                config.warmup,
                config.max_age,
                config.inventory_grace
            ),
            (3, 180_000, 86_400_000, 120_000)
        );
    }

    #[test]
    fn names_carry_the_template_and_the_time() {
        let t = template("ubuntu-26.04-4core-arm").unwrap();
        let mut names = names(t);
        assert_eq!(names.pool(255), "ci-pool-ubuntu-26.04-4core-arm-pool-ff-1");
        assert_eq!(names.pool(255), "ci-pool-ubuntu-26.04-4core-arm-pool-ff-2");
    }

    #[test]
    fn backoff_starts_ready() {
        assert!(backoff().ready(0));
    }

    #[test]
    fn the_inventory_holds_one_templates_instances() {
        let described = |id: &str, state: &str, pool: Option<&str>| Described {
            id: id.into(),
            state: state.into(),
            launch_time: "2026-10-22T13:40:00.000Z".into(),
            job_id: None,
            runner_name: None,
            pool: pool.map(str::to_owned),
        };
        let t = template("ubuntu-latest").unwrap();
        let found = inventory(
            &[
                described("i-1", "stopped", Some("ubuntu-latest")),
                described("i-2", "running", Some("ubuntu-latest")),
                described("i-3", "stopped", Some("ubuntu-26.04-8core")),
                described("i-4", "stopped", None),
            ],
            t,
        );
        assert_eq!(
            found
                .iter()
                .map(|i| (i.id.as_str(), i.stopped))
                .collect::<Vec<_>>(),
            [("i-1", true), ("i-2", false)]
        );
        assert!(found[0].launched_at > 1_700_000_000_000);
    }

    #[test]
    fn demand_merges_the_labels_of_one_template() {
        let claims = [
            ("ubuntu-latest".to_owned(), 0, 2),
            ("ubuntu-26.04".to_owned(), 0, 3),
            ("ubuntu-latest".to_owned(), BUCKET_MS, 1),
            ("ubuntu-26.04-8core".to_owned(), 0, 4),
            ("self-hosted".to_owned(), 0, 9),
        ];
        let found = demand(&claims);
        assert_eq!(found["ubuntu-latest"], [5, 1]);
        assert_eq!(found["ubuntu-26.04-8core"], [4]);
        assert_eq!(found.len(), 2);
        assert_eq!(pool_target(&found["ubuntu-latest"]), 5);
    }

    #[test]
    fn unrecorded_running_instances_join_and_old_stopped_ones_go() {
        let at = |id: &str, launched_at: u64, stopped: bool| Instance {
            id: id.into(),
            runner_name: None,
            runner_id: None,
            launched_at,
            stopped,
        };
        let now = 10 * MAX_AGE_MS;
        let old = now - MAX_AGE_MS - STALE_AFTER_MS;
        let inventory = [
            at("i-known", old, true),
            at("i-running", now - 1_000, false),
            at("i-old-running", old, false),
            at("i-young-stopped", now - MAX_AGE_MS, true),
            at("i-old-stopped", old, true),
        ];
        let found = unrecorded(&inventory, &|id| id == "i-known", now);
        assert_eq!(
            found,
            Unrecorded {
                running: vec![
                    ("i-running".into(), now - 1_000),
                    ("i-old-running".into(), old)
                ],
                stale: vec!["i-old-stopped".into()],
            }
        );
    }
}
