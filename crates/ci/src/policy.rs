// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a repository does with pull requests from forks: the policy
//! row, the approval levels and the limits that keep a flood of fork
//! pull requests from filling the queue.
//!
//! The functions here are pure. [`crate::store::repo_policy`] reads the
//! row, [`crate::webhook`] decides with [`admission`] and the reconciler
//! admits deferred runs with the same function.

use serde::{Deserialize, Serialize};

/// Runs a repository may have waiting for an approval at once. A run
/// over it is deferred.
pub const MAX_PENDING: u64 = 20;
/// Deferred runs a repository keeps. A delivery over it is dropped.
pub const MAX_DEFERRED: u64 = 100;
/// Runs of one author a repository has active at once. A delivery over
/// it is deferred, like one over the approval slots.
pub const MAX_ACTIVE_PER_AUTHOR: u64 = 3;
/// How long a run waits for a decision before it is cancelled, in
/// milliseconds: 7 days.
pub const APPROVAL_EXPIRES_MS: u64 = 7 * 86_400_000;
/// The most vCPUs a runner template of a fork run has.
pub const FORK_MAX_VCPU: u32 = 4;

/// The identifier of the approval check run's button.
pub const APPROVE_ACTION: &str = "approve-fork-run";
/// The name of the check run that asks for the approval.
pub const APPROVAL_CHECK: &str = "hatchrun / approval";

/// Whether pull requests from forks run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkRuns {
    /// They are ignored.
    Off,
    /// They run, behind the approval level.
    On,
}

/// Which authors of fork pull requests need a maintainer's approval
/// first. The levels use GitHub's `author_association`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkApproval {
    /// `FIRST_TIMER`: new to GitHub.
    FirstTimers,
    /// `FIRST_TIMER` or `FIRST_TIME_CONTRIBUTOR`: first contribution to
    /// this repository.
    FirstTimeContributors,
    /// Anyone but `OWNER`, `MEMBER` or `COLLABORATOR`.
    AllOutside,
}

/// What to do with a head checkout in `pull_request_target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadCheckout {
    /// Warn in the run summary.
    Warn,
    /// Fail the run at planning.
    Block,
}

/// A repository's policy for pull requests from forks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPolicy {
    /// Whether fork pull requests run.
    pub fork_runs: ForkRuns,
    /// Who needs approval.
    pub fork_approval: ForkApproval,
    /// The most fork jobs of the repository that run at once, 1 to 10.
    pub fork_max_running: u32,
    /// The longest a fork job may run, in minutes, 5 to 60.
    pub fork_timeout_minutes: u32,
    /// The rule for a head checkout in `pull_request_target`.
    pub pull_request_target_head_checkout: HeadCheckout,
}

impl RepoPolicy {
    /// The policy of a repository that never saved one: fork runs are
    /// on for a public repository and off for a private one.
    #[must_use]
    pub const fn default_for(private: bool) -> Self {
        Self {
            fork_runs: if private { ForkRuns::Off } else { ForkRuns::On },
            fork_approval: ForkApproval::FirstTimeContributors,
            fork_max_running: 2,
            fork_timeout_minutes: 30,
            pull_request_target_head_checkout: HeadCheckout::Warn,
        }
    }

    /// Checks the numbers a settings write carries.
    ///
    /// # Errors
    ///
    /// The message to show when one is out of range.
    pub fn valid(&self) -> Result<(), String> {
        if !(1..=10).contains(&self.fork_max_running) {
            return Err("fork_max_running is 1 to 10".to_owned());
        }
        if !(5..=60).contains(&self.fork_timeout_minutes) {
            return Err("fork_timeout_minutes is 5 to 60".to_owned());
        }
        Ok(())
    }
}

impl ForkRuns {
    /// The word the column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    /// The level a stored word names.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            _ => None,
        }
    }
}

impl ForkApproval {
    /// The word the column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstTimers => "first_timers",
            Self::FirstTimeContributors => "first_time_contributors",
            Self::AllOutside => "all_outside",
        }
    }

    /// The level a stored word names.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "first_timers" => Some(Self::FirstTimers),
            "first_time_contributors" => Some(Self::FirstTimeContributors),
            "all_outside" => Some(Self::AllOutside),
            _ => None,
        }
    }
}

impl HeadCheckout {
    /// The word the column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Block => "block",
        }
    }

    /// The rule a stored word names.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "warn" => Some(Self::Warn),
            "block" => Some(Self::Block),
            _ => None,
        }
    }
}

/// Whether an author with `association` (GitHub's `author_association`)
/// needs a maintainer's approval at `level`. An association the
/// delivery did not carry counts as an outsider at every level, so a
/// payload that leaves it out is never waved through.
#[must_use]
pub fn needs_approval(level: ForkApproval, association: &str) -> bool {
    match level {
        ForkApproval::FirstTimers => {
            matches!(association, "FIRST_TIMER" | "") || !known(association)
        }
        ForkApproval::FirstTimeContributors => {
            matches!(association, "FIRST_TIMER" | "FIRST_TIME_CONTRIBUTOR" | "")
                || !known(association)
        }
        ForkApproval::AllOutside => !matches!(association, "OWNER" | "MEMBER" | "COLLABORATOR"),
    }
}

/// Whether `association` is one of the values GitHub documents.
fn known(association: &str) -> bool {
    matches!(
        association,
        "COLLABORATOR"
            | "CONTRIBUTOR"
            | "FIRST_TIMER"
            | "FIRST_TIME_CONTRIBUTOR"
            | "MANNEQUIN"
            | "MEMBER"
            | "NONE"
            | "OWNER"
    )
}

/// What a fork pull request delivery becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Nothing is started: fork runs are off.
    Off,
    /// Runs start at once with no approval.
    Run,
    /// Runs wait for an approval, and the card is shown.
    Pending,
    /// Runs wait for a free approval slot, or for the author's active
    /// runs to drop under the cap; the card says so and has no button.
    Deferred,
    /// Nothing is started: the repository has too many deferred runs.
    Drop,
}

/// How many runs of a repository wait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Waiting {
    /// Runs with a pending approval.
    pub pending: u64,
    /// Deferred runs.
    pub deferred: u64,
    /// Fork runs of the delivery's author that are not over.
    pub author_active: u64,
}

/// Decides what a fork delivery from an author with `association`
/// becomes under `policy`, given what already waits.
#[must_use]
pub fn admission(policy: &RepoPolicy, association: &str, waiting: Waiting) -> Admission {
    if policy.fork_runs == ForkRuns::Off {
        return Admission::Off;
    }
    // An author over the cap waits like a run over the approval slots,
    // whether or not the run would need an approval.
    let later = if waiting.deferred < MAX_DEFERRED {
        Admission::Deferred
    } else {
        Admission::Drop
    };
    if waiting.author_active >= MAX_ACTIVE_PER_AUTHOR {
        return later;
    }
    if !needs_approval(policy.fork_approval, association) {
        return Admission::Run;
    }
    if waiting.pending < MAX_PENDING {
        Admission::Pending
    } else {
        later
    }
}

/// Why a delivery that [`admission`] refused starts nothing.
#[must_use]
pub const fn refusal(admission: Admission) -> Option<&'static str> {
    match admission {
        Admission::Off => Some("fork runs are off for this repository"),
        Admission::Drop => Some("too many fork runs wait for approval"),
        Admission::Run | Admission::Pending | Admission::Deferred => None,
    }
}

/// The job timeout of a fork run: the workflow's, cut to the policy's.
#[must_use]
pub fn clamp_timeout(minutes: u32, policy: &RepoPolicy) -> u32 {
    minutes.min(policy.fork_timeout_minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC: RepoPolicy = RepoPolicy::default_for(false);

    #[test]
    fn a_public_repository_runs_forks_and_a_private_one_does_not() {
        assert_eq!(RepoPolicy::default_for(false).fork_runs, ForkRuns::On);
        assert_eq!(RepoPolicy::default_for(true).fork_runs, ForkRuns::Off);
        let policy = RepoPolicy::default_for(false);
        assert_eq!(policy.fork_approval, ForkApproval::FirstTimeContributors);
        assert_eq!(policy.fork_max_running, 2);
        assert_eq!(policy.fork_timeout_minutes, 30);
        assert_eq!(policy.pull_request_target_head_checkout, HeadCheckout::Warn);
    }

    #[test]
    fn the_three_levels_ask_for_the_associations_the_spec_lists() {
        let asks = |level, association| needs_approval(level, association);
        for (association, first, contrib, outside) in [
            ("FIRST_TIMER", true, true, true),
            ("FIRST_TIME_CONTRIBUTOR", false, true, true),
            ("CONTRIBUTOR", false, false, true),
            ("NONE", false, false, true),
            ("MANNEQUIN", false, false, true),
            ("COLLABORATOR", false, false, false),
            ("MEMBER", false, false, false),
            ("OWNER", false, false, false),
        ] {
            assert_eq!(
                asks(ForkApproval::FirstTimers, association),
                first,
                "{association}"
            );
            assert_eq!(
                asks(ForkApproval::FirstTimeContributors, association),
                contrib,
                "{association}"
            );
            assert_eq!(
                asks(ForkApproval::AllOutside, association),
                outside,
                "{association}"
            );
        }
    }

    #[test]
    fn an_unknown_association_always_asks() {
        for level in [
            ForkApproval::FirstTimers,
            ForkApproval::FirstTimeContributors,
            ForkApproval::AllOutside,
        ] {
            assert!(needs_approval(level, ""));
            assert!(needs_approval(level, "SOMETHING_NEW"));
        }
    }

    #[test]
    fn fork_runs_off_starts_nothing() {
        let off = RepoPolicy::default_for(true);
        assert_eq!(admission(&off, "OWNER", Waiting::default()), Admission::Off);
        assert_eq!(
            refusal(Admission::Off),
            Some("fork runs are off for this repository")
        );
    }

    #[test]
    fn an_author_who_needs_no_approval_runs_at_once() {
        assert_eq!(
            admission(&PUBLIC, "CONTRIBUTOR", Waiting::default()),
            Admission::Run
        );
        assert_eq!(
            admission(&PUBLIC, "FIRST_TIME_CONTRIBUTOR", Waiting::default()),
            Admission::Pending
        );
    }

    #[test]
    fn pending_runs_fill_then_defer_then_drop() {
        let waiting = |pending, deferred| Waiting {
            pending,
            deferred,
            author_active: 0,
        };
        let ask = |w| admission(&PUBLIC, "FIRST_TIMER", w);
        assert_eq!(ask(waiting(19, 0)), Admission::Pending);
        assert_eq!(ask(waiting(20, 0)), Admission::Deferred);
        assert_eq!(ask(waiting(20, 99)), Admission::Deferred);
        assert_eq!(ask(waiting(20, 100)), Admission::Drop);
        assert_eq!(
            refusal(Admission::Drop),
            Some("too many fork runs wait for approval")
        );
    }

    #[test]
    fn an_author_over_three_active_runs_is_deferred_not_dropped() {
        let busy = Waiting {
            author_active: 3,
            ..Waiting::default()
        };
        assert_eq!(admission(&PUBLIC, "FIRST_TIMER", busy), Admission::Deferred);
        assert_eq!(admission(&PUBLIC, "OWNER", busy), Admission::Deferred);
        // The deferred runs are capped too.
        let full = Waiting {
            deferred: 100,
            ..busy
        };
        assert_eq!(admission(&PUBLIC, "OWNER", full), Admission::Drop);
        let free = Waiting {
            author_active: 2,
            ..Waiting::default()
        };
        assert_eq!(admission(&PUBLIC, "OWNER", free), Admission::Run);
    }

    #[test]
    fn the_timeout_is_cut_to_the_policy() {
        let policy = RepoPolicy {
            fork_timeout_minutes: 10,
            ..PUBLIC
        };
        assert_eq!(clamp_timeout(360, &policy), 10);
        assert_eq!(clamp_timeout(5, &policy), 5);
    }

    #[test]
    fn a_policy_write_is_range_checked() {
        let mut policy = PUBLIC;
        assert!(policy.valid().is_ok());
        for bad in [0, 11] {
            policy.fork_max_running = bad;
            assert!(policy.valid().is_err());
        }
        policy.fork_max_running = 10;
        for bad in [4, 61] {
            policy.fork_timeout_minutes = bad;
            assert!(policy.valid().is_err());
        }
        policy.fork_timeout_minutes = 60;
        assert!(policy.valid().is_ok());
    }

    #[test]
    fn words_round_trip() {
        for level in [
            ForkApproval::FirstTimers,
            ForkApproval::FirstTimeContributors,
            ForkApproval::AllOutside,
        ] {
            assert_eq!(ForkApproval::parse(level.as_str()), Some(level));
        }
        for runs in [ForkRuns::Off, ForkRuns::On] {
            assert_eq!(ForkRuns::parse(runs.as_str()), Some(runs));
        }
        for rule in [HeadCheckout::Warn, HeadCheckout::Block] {
            assert_eq!(HeadCheckout::parse(rule.as_str()), Some(rule));
        }
        assert_eq!(ForkApproval::parse("everyone"), None);
    }
}
