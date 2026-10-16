// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Which offerings, an instance type in a subnet on Spot or on-demand,
//! a launch hands to EC2 Fleet.
//!
//! A launch prefers the newest generation among the types that fit: it
//! hands the fleet every offering of that generation and lets EC2 pick
//! the cheapest with capacity. Offerings EC2 refused for lack of
//! capacity are skipped for [`UNAVAILABLE_FOR`], like Karpenter's
//! unavailable offerings cache; when a generation has none left the
//! launch moves to the next one, and a Spot launch to on-demand. When
//! nothing is left the launch fails on capacity and waits at least
//! until the first offering comes back (see
//! [`crate::reconcile::retry`]).
//!
//! Everything here but [`store`], which reads and writes the
//! `unavailable_offerings` table, runs and tests without the Workers
//! runtime.

pub(crate) mod store;

use crate::aws::ec2::CAPACITY_ERRORS;
use crate::aws::fleet::Override;

/// How long an offering EC2 refused for capacity is skipped, in
/// milliseconds.
pub const UNAVAILABLE_FOR: u64 = 3 * 60 * 1000;
/// Most overrides one fleet takes.
pub const MAX_OVERRIDES: usize = 300;

/// An instance type that fits the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// As in `m8g.large`.
    pub instance_type: String,
    /// As in 8; 0 when unknown.
    pub generation: u16,
}

/// An offering EC2 refused recently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    /// As in `m8g.large`.
    pub instance_type: String,
    /// The subnet, or `None` for the template's network.
    pub subnet_id: Option<String>,
    /// Spot rather than on-demand.
    pub spot: bool,
    /// Until when it is skipped, Unix milliseconds.
    pub until: u64,
}

/// Whether EC2 refused an offering for lack of capacity, which passes,
/// rather than for something about the request.
#[must_use]
pub fn is_capacity(code: &str) -> bool {
    CAPACITY_ERRORS.contains(&code) || code == "InsufficientCapacity"
}

/// The overrides for the next fleet: every offering of the newest
/// generation that still has one at `now`, in the order the candidates
/// and subnets are given. Empty when every offering is unavailable.
#[must_use]
pub fn overrides(
    candidates: &[Candidate],
    subnets: &[Option<&str>],
    spot: bool,
    unavailable: &[Unavailable],
    now: u64,
) -> Vec<Override> {
    let skipped = |instance_type: &str, subnet: Option<&str>| {
        unavailable.iter().any(|gone| {
            gone.until > now
                && gone.spot == spot
                && gone.instance_type == instance_type
                && gone.subnet_id.as_deref() == subnet
        })
    };
    let mut generations: Vec<u16> = candidates.iter().map(|c| c.generation).collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    generations.dedup();
    for generation in generations {
        let found: Vec<Override> = candidates
            .iter()
            .filter(|candidate| candidate.generation == generation)
            .flat_map(|candidate| {
                subnets
                    .iter()
                    .filter(|subnet| !skipped(&candidate.instance_type, **subnet))
                    .map(|subnet| Override {
                        instance_type: candidate.instance_type.clone(),
                        subnet_id: subnet.map(str::to_owned),
                    })
            })
            .take(MAX_OVERRIDES)
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// When the first of the candidates' offerings in `subnets` on `spot`
/// comes back, if all are unavailable at `now`.
#[must_use]
pub fn back_at(
    candidates: &[Candidate],
    subnets: &[Option<&str>],
    markets: &[bool],
    unavailable: &[Unavailable],
    now: u64,
) -> Option<u64> {
    let mut earliest: Option<u64> = None;
    for spot in markets {
        for candidate in candidates {
            for subnet in subnets {
                let until = unavailable
                    .iter()
                    .filter(|gone| {
                        gone.spot == *spot
                            && gone.instance_type == candidate.instance_type
                            && gone.subnet_id.as_deref() == *subnet
                    })
                    .map(|gone| gone.until)
                    .max()
                    .filter(|until| *until > now)?;
                earliest = Some(earliest.map_or(until, |at: u64| at.min(until)));
            }
        }
    }
    earliest
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn candidate(name: &str, generation: u16) -> Candidate {
        Candidate {
            instance_type: name.to_owned(),
            generation,
        }
    }

    fn gone(name: &str, subnet: &str, spot: bool, until: u64) -> Unavailable {
        Unavailable {
            instance_type: name.to_owned(),
            subnet_id: Some(subnet.to_owned()),
            spot,
            until,
        }
    }

    fn names(overrides: &[Override]) -> Vec<String> {
        overrides
            .iter()
            .map(|o| {
                format!(
                    "{} {}",
                    o.instance_type,
                    o.subnet_id.as_deref().unwrap_or("-")
                )
            })
            .collect()
    }

    #[test]
    fn hands_the_newest_generation_to_the_fleet() {
        let candidates = [
            candidate("m7g.large", 7),
            candidate("m8g.large", 8),
            candidate("c8g.xlarge", 8),
        ];
        let subnets = [Some("subnet-a"), Some("subnet-b")];
        assert_eq!(
            names(&overrides(&candidates, &subnets, false, &[], NOW)),
            [
                "m8g.large subnet-a",
                "m8g.large subnet-b",
                "c8g.xlarge subnet-a",
                "c8g.xlarge subnet-b"
            ]
        );
        assert_eq!(
            names(&overrides(&candidates, &[None], false, &[], NOW)),
            ["m8g.large -", "c8g.xlarge -"]
        );
    }

    #[test]
    fn skips_offerings_out_of_capacity_for_a_while() {
        let candidates = [candidate("m7g.large", 7), candidate("m8g.large", 8)];
        let subnets = [Some("subnet-a"), Some("subnet-b")];
        let refused = [
            gone("m8g.large", "subnet-a", true, NOW + 60_000),
            gone("m8g.large", "subnet-b", true, NOW + 120_000),
        ];
        // Spot moves to the older generation; on-demand is unaffected.
        assert_eq!(
            names(&overrides(&candidates, &subnets, true, &refused, NOW)),
            ["m7g.large subnet-a", "m7g.large subnet-b"]
        );
        assert_eq!(
            names(&overrides(&candidates, &subnets, false, &refused, NOW))[0],
            "m8g.large subnet-a"
        );
        // Back once the entry lapses.
        assert_eq!(
            names(&overrides(
                &candidates,
                &subnets,
                true,
                &refused,
                NOW + 60_001
            ))[0],
            "m8g.large subnet-a"
        );
    }

    #[test]
    fn says_when_capacity_may_be_back() {
        let candidates = [candidate("m8g.large", 8)];
        let subnets = [Some("subnet-a"), Some("subnet-b")];
        let mut refused = vec![
            gone("m8g.large", "subnet-a", false, NOW + 60_000),
            gone("m8g.large", "subnet-b", false, NOW + 120_000),
        ];
        assert_eq!(overrides(&candidates, &subnets, false, &refused, NOW), []);
        assert_eq!(
            back_at(&candidates, &subnets, &[false], &refused, NOW),
            Some(NOW + 60_000)
        );
        // One offering left: nothing to wait for.
        refused.pop();
        assert_eq!(
            back_at(&candidates, &subnets, &[false], &refused, NOW),
            None
        );
    }

    #[test]
    fn capacity_codes() {
        assert!(is_capacity("InsufficientInstanceCapacity"));
        assert!(is_capacity("Unsupported"));
        assert!(!is_capacity("UnauthorizedOperation"));
        assert!(!is_capacity("InvalidParameterValue"));
    }
}
