// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Shards of the CI reconciler.
//!
//! The reconciler fans its per-repository work out over [`SHARDS`]
//! shards, each a contiguous range of [`BUCKETS`] buckets. A
//! repository's bucket is a hash of its id, so tenants spread over the
//! shards and a shard holds about its fair share of every tenant. The
//! cron pass sends one [`ShardMessage`] per shard to the
//! `hatchrun-ci-reconcile` queue, and each message's shard runs alone.
//!
//! A shard's range only ever splits in two: raising the shard count to
//! a higher power of two halves every shard, so [`range_of`] can prove
//! that property for any count.

use serde::{Deserialize, Serialize};

/// How many buckets the repositories hash into.
pub const BUCKETS: u16 = hatchrun_cloud::ci_wake::BUCKETS;
/// How many shards the buckets split into.
pub const SHARDS: u16 = 16;
const _: () = assert!(SHARDS > 0 && SHARDS.is_power_of_two() && BUCKETS.is_multiple_of(SHARDS));

/// The jitter between the schedule messages of one fan-out, in
/// seconds. Each shard's message waits its own share of it, so the
/// schedule passes spread over the minute.
pub const JITTER_SECS: u32 = 40;

/// The bucket of repository `repo_id`: the top 12 bits of the low 32
/// of a Knuth multiplicative hash, so a number in `0..BUCKETS`.
#[must_use]
pub fn bucket(repo_id: u64) -> u16 {
    ((repo_id.wrapping_mul(2_654_435_761) & 0xFFFF_FFFF) >> 20) as u16
}

/// One of the [`SHARDS`] shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    index: u16,
}

impl Shard {
    /// The shard of `index`, which must be below [`SHARDS`].
    #[must_use]
    pub fn new(index: u16) -> Shard {
        debug_assert!(index < SHARDS);
        Shard { index }
    }

    /// The shard's number, in `0..SHARDS`.
    #[must_use]
    pub fn index(self) -> u16 {
        self.index
    }

    /// The shard's bucket range, half open: `lo` to `hi`.
    #[must_use]
    pub fn range(self) -> (u16, u16) {
        range_of(self.index, SHARDS)
    }

    /// The shard's first bucket.
    #[must_use]
    pub fn first_bucket(self) -> u16 {
        self.range().0
    }

    /// The shard holding `bucket`.
    #[must_use]
    pub fn of_bucket(bucket: u16) -> Shard {
        Shard {
            index: bucket / (BUCKETS / SHARDS),
        }
    }

    /// The shard holding repository `repo_id`.
    #[must_use]
    pub fn of_repo(repo_id: u64) -> Shard {
        Shard::of_bucket(bucket(repo_id))
    }

    /// Every shard, in order.
    pub fn all() -> impl Iterator<Item = Shard> {
        (0..SHARDS).map(Shard::new)
    }
}

/// The bucket range of shard `index` of `count` shards, half open. The
/// count must divide [`BUCKETS`] and the index must be below it.
#[must_use]
pub fn range_of(index: u16, count: u16) -> (u16, u16) {
    debug_assert!(count > 0 && BUCKETS.is_multiple_of(count) && index < count);
    let width = BUCKETS / count;
    (index * width, (index + 1) * width)
}

/// How long after the fan-out the schedule message of `shard` is
/// delivered, in seconds: its share of [`JITTER_SECS`].
#[must_use]
pub fn schedule_delay(shard: Shard) -> u32 {
    u32::from(shard.index) * JITTER_SECS / u32::from(SHARDS)
}

/// What one shard of one reconciler pass does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Work {
    /// Fixes stalled jobs and wakes held ones of the shard's buckets.
    Pass,
    /// Fires the schedules of the shard's buckets.
    Schedule,
}

/// One shard's work of one reconciler pass. The orphan instance sweep
/// (#196) and cache entry eviction (#198) still run in the cron pass;
/// each gets its own kind here once it is sharded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardMessage {
    /// What the shard does.
    pub kind: Work,
    /// The shard's number.
    pub shard: u16,
    /// How many shards the fan-out sent: a stale deploy's messages
    /// carry another count and are refused.
    pub shards: u16,
    /// When the cron pass fanned out, in Unix milliseconds.
    pub minute_ms: u64,
}

/// Whether `msg` is worth running at `now`: it names this many shards
/// and one of them, and its fan-out is at most two minutes old.
#[must_use]
pub fn accept(msg: &ShardMessage, now: u64) -> bool {
    msg.shards == SHARDS && msg.shard < SHARDS && msg.minute_ms.saturating_add(120_000) >= now
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn the_bucket_is_deterministic_and_in_range() {
        for repo_id in [
            0,
            1,
            7,
            8,
            123_456_789,
            1_000_000_000,
            4_000_000_000,
            9_000_000_000_000,
        ] {
            let first = bucket(repo_id);
            assert_eq!(bucket(repo_id), first);
            assert!(first < BUCKETS, "{repo_id}");
        }
        assert_eq!(
            [
                bucket(0),
                bucket(1),
                bucket(7),
                bucket(8),
                bucket(123_456_789),
                bucket(1_000_000_000),
                bucket(4_000_000_000),
                bucket(9_000_000_000_000),
            ],
            [0, 2531, 1336, 3867, 2046, 3159, 351, 1283]
        );
    }

    #[test]
    fn every_power_of_two_count_covers_every_bucket_once() {
        let mut count = 1;
        while count <= BUCKETS {
            let mut seen = vec![false; usize::from(BUCKETS)];
            for index in 0..count {
                let (lo, hi) = range_of(index, count);
                assert!(lo < hi && hi <= BUCKETS, "{index} of {count}");
                for bucket in lo..hi {
                    assert!(!seen[usize::from(bucket)], "{bucket} twice for {count}");
                    seen[usize::from(bucket)] = true;
                }
            }
            assert!(seen.into_iter().all(|seen| seen), "{count}");
            count *= 2;
        }
    }

    #[test]
    fn doubling_the_shards_halves_each_shard() {
        let mut count = 1;
        while count < BUCKETS {
            for bucket in 0..BUCKETS {
                let wide = bucket / (BUCKETS / count);
                let narrow = bucket / (BUCKETS / (2 * count));
                assert_eq!(narrow / 2, wide, "{bucket} of {count}");
            }
            count *= 2;
        }
    }

    #[test]
    fn sequential_repositories_spread_over_every_shard() {
        let mut held = [0u32; SHARDS as usize];
        for repo_id in 0..100_000 {
            held[usize::from(Shard::of_repo(repo_id).index)] += 1;
        }
        assert!(held.iter().all(|&held| held > 0), "{held:?}");
        let fair = 100_000u32 / u32::from(SHARDS);
        assert!(held.iter().all(|&held| held <= fair + fair / 2), "{held:?}");
    }

    #[test]
    fn shards_round_trip_through_buckets() {
        for shard in Shard::all() {
            let (lo, hi) = shard.range();
            assert_eq!(lo, shard.first_bucket());
            assert_eq!(hi - lo, BUCKETS / SHARDS);
            for bucket in lo..hi {
                assert_eq!(Shard::of_bucket(bucket), shard);
            }
        }
        assert_eq!(Shard::all().count(), usize::from(SHARDS));
    }

    #[test]
    fn the_schedule_jitter_spreads_inside_itself() {
        let mut delays: Vec<u32> = Shard::all().map(schedule_delay).collect();
        assert!(delays.iter().all(|&delay| delay < JITTER_SECS));
        let mut sorted = delays.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            delays.len(),
            sorted.len(),
            "every shard waits its own delay"
        );
        delays.sort_unstable();
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(schedule_delay(Shard::new(0)), 0);
    }

    #[test]
    fn only_fresh_messages_of_this_many_shards_run() {
        let now = 1_792_676_700_000;
        let message = ShardMessage {
            kind: Work::Pass,
            shard: 3,
            shards: SHARDS,
            minute_ms: now,
        };
        assert!(accept(&message, now));
        assert!(accept(&message, now + 120_000));
        assert!(!accept(&message, now + 120_001), "a stale pass is refused");
        assert!(
            !accept(
                &ShardMessage {
                    shards: SHARDS + 1,
                    ..message
                },
                now
            ),
            "another fan-out's messages are refused"
        );
        assert!(
            !accept(
                &ShardMessage {
                    shard: SHARDS,
                    ..message
                },
                now
            ),
            "a shard past the last is refused"
        );
        assert!(
            serde_json::from_value::<ShardMessage>(serde_json::to_value(message).unwrap())
                .is_ok_and(|read| read == message)
        );
        let kinds: HashSet<&str> = ["pass", "schedule"].into_iter().collect();
        assert!(kinds.contains(serde_json::to_value(Work::Pass).unwrap().as_str().unwrap()));
    }
}
