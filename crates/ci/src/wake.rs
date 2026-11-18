// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What wakes a job held for each reason.
//!
//! A held job waits with no queue traffic until its event fires (see
//! [`crate::launches::hold_job`]); the reconciler's backstop
//! ([`crate::reconcile::wake_held`]) only catches a wake that was lost.
//! The table below names, for each word `ci_jobs.held` stores, the
//! event that wakes it:
//!
//! | held | woken by |
//! | --- | --- |
//! | `card` | a licensed subscription delivery from Polar |
//! | `spend_cap` | the owner raising the cap, or a renewal from Polar |
//! | `concurrency` | a job of the tenant ending |
//! | `approval` | the environment's decision, or its release or deletion |
//! | `max_parallel` | a job of its matrix ending |
//! | `group` | the concurrency group's release |
//! | `capacity` | a job on its template ending, or its own backoff timer |
//! | `fork_approval` | the run's approval decision |
//! | `fork_limit` | a fork job of its repository ending, or a raised fork cap |
//!
//! [`wakes`] gives the machine form of the table. A hold whose event is
//! a place freeing up can race that event: the place frees after the
//! consumer counted it and before the hold is written, and the end
//! wakes nothing. [`HoldReason::rechecks`] names those reasons; the
//! consumer looks for room again right after writing such a hold.

/// Why a queued job waits, as `ci_jobs.held` stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// The tenant has no card.
    Card,
    /// This window's spend reached the tenant's cap.
    SpendCap,
    /// The tenant already runs its most jobs.
    Concurrency,
    /// An environment the job names waits for an approval.
    Approval,
    /// Its matrix already runs `max_parallel` legs.
    MaxParallel,
    /// A concurrency group holds it.
    Group,
    /// EC2 has no capacity for its template.
    Capacity,
    /// Its run waits for a maintainer's approval.
    ForkApproval,
    /// Its repository already runs its most fork jobs.
    ForkLimit,
}

impl HoldReason {
    /// The reason the word `held` stores, `None` for anything else.
    #[must_use]
    pub fn parse(held: &str) -> Option<Self> {
        match held {
            "card" => Some(Self::Card),
            "spend_cap" => Some(Self::SpendCap),
            "concurrency" => Some(Self::Concurrency),
            "approval" => Some(Self::Approval),
            "max_parallel" => Some(Self::MaxParallel),
            "group" => Some(Self::Group),
            "capacity" => Some(Self::Capacity),
            "fork_approval" => Some(Self::ForkApproval),
            "fork_limit" => Some(Self::ForkLimit),
            _ => None,
        }
    }

    /// The word `ci_jobs.held` stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::SpendCap => "spend_cap",
            Self::Concurrency => "concurrency",
            Self::Approval => "approval",
            Self::MaxParallel => "max_parallel",
            Self::Group => "group",
            Self::Capacity => "capacity",
            Self::ForkApproval => "fork_approval",
            Self::ForkLimit => "fork_limit",
        }
    }
}

impl HoldReason {
    /// Whether a hold for this reason waits for a place that a job end
    /// frees, so the consumer checks for room again after writing it.
    /// Billing holds wait for a person or Polar, approvals for a
    /// decision, and capacity for EC2 or its timer.
    #[must_use]
    pub const fn rechecks(self) -> bool {
        matches!(
            self,
            Self::Concurrency | Self::MaxParallel | Self::ForkLimit | Self::Group
        )
    }
}

/// The event that wakes a held job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A job of the tenant ended.
    JobEndTenant,
    /// A job of its matrix ended.
    JobEndMatrix,
    /// A fork job of its repository ended.
    JobEndRepoFork,
    /// A job on its template ended.
    JobEndTemplate,
    /// Its own backoff timer ran out.
    Timer,
    /// Its concurrency group released it.
    GroupRelease,
    /// Polar sent a licensed subscription: a card, or a renewal that
    /// starts a new spend window.
    Subscription,
    /// The owner raised the spend cap.
    CapRaised,
    /// Its approval was decided.
    Decision,
}

/// What wakes a job held for `reason`, as the table above says.
#[must_use]
pub fn wakes(reason: HoldReason) -> &'static [Wake] {
    match reason {
        HoldReason::Card => &[Wake::Subscription],
        HoldReason::SpendCap => &[Wake::CapRaised, Wake::Subscription],
        HoldReason::Concurrency => &[Wake::JobEndTenant],
        HoldReason::Approval | HoldReason::ForkApproval => &[Wake::Decision],
        HoldReason::MaxParallel => &[Wake::JobEndMatrix],
        HoldReason::Group => &[Wake::GroupRelease],
        HoldReason::Capacity => &[Wake::JobEndTemplate, Wake::Timer],
        HoldReason::ForkLimit => &[Wake::JobEndRepoFork],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_word_is_no_reason() {
        assert_eq!(HoldReason::parse("later"), None);
    }

    #[test]
    fn a_missing_card_waits_for_the_card() {
        assert_eq!(HoldReason::parse("card"), Some(HoldReason::Card));
        assert_eq!(HoldReason::Card.as_str(), "card");
        assert_eq!(wakes(HoldReason::Card), [Wake::Subscription]);
        assert!(!HoldReason::Card.rechecks());
    }

    #[test]
    fn a_spend_cap_waits_for_a_raised_cap() {
        assert_eq!(HoldReason::parse("spend_cap"), Some(HoldReason::SpendCap));
        assert_eq!(HoldReason::SpendCap.as_str(), "spend_cap");
        assert_eq!(
            wakes(HoldReason::SpendCap),
            [Wake::CapRaised, Wake::Subscription]
        );
        assert!(!HoldReason::SpendCap.rechecks());
    }

    #[test]
    fn a_full_tenant_waits_for_a_job_end() {
        assert_eq!(
            HoldReason::parse("concurrency"),
            Some(HoldReason::Concurrency)
        );
        assert_eq!(HoldReason::Concurrency.as_str(), "concurrency");
        assert_eq!(wakes(HoldReason::Concurrency), [Wake::JobEndTenant]);
        assert!(HoldReason::Concurrency.rechecks());
    }

    #[test]
    fn an_environment_approval_waits_for_its_decision() {
        assert_eq!(HoldReason::parse("approval"), Some(HoldReason::Approval));
        assert_eq!(HoldReason::Approval.as_str(), "approval");
        assert_eq!(wakes(HoldReason::Approval), [Wake::Decision]);
        assert!(!HoldReason::Approval.rechecks());
    }

    #[test]
    fn a_full_matrix_waits_for_a_leg_to_end() {
        assert_eq!(
            HoldReason::parse("max_parallel"),
            Some(HoldReason::MaxParallel)
        );
        assert_eq!(HoldReason::MaxParallel.as_str(), "max_parallel");
        assert_eq!(wakes(HoldReason::MaxParallel), [Wake::JobEndMatrix]);
        assert!(HoldReason::MaxParallel.rechecks());
    }

    #[test]
    fn a_group_hold_waits_for_its_release() {
        assert_eq!(HoldReason::parse("group"), Some(HoldReason::Group));
        assert_eq!(HoldReason::Group.as_str(), "group");
        assert_eq!(wakes(HoldReason::Group), [Wake::GroupRelease]);
        assert!(HoldReason::Group.rechecks());
    }

    #[test]
    fn missing_capacity_waits_for_a_template_end_or_its_timer() {
        assert_eq!(HoldReason::parse("capacity"), Some(HoldReason::Capacity));
        assert_eq!(HoldReason::Capacity.as_str(), "capacity");
        assert_eq!(
            wakes(HoldReason::Capacity),
            [Wake::JobEndTemplate, Wake::Timer]
        );
        assert!(!HoldReason::Capacity.rechecks());
    }

    #[test]
    fn a_fork_run_waits_for_its_approval() {
        assert_eq!(
            HoldReason::parse("fork_approval"),
            Some(HoldReason::ForkApproval)
        );
        assert_eq!(HoldReason::ForkApproval.as_str(), "fork_approval");
        assert_eq!(wakes(HoldReason::ForkApproval), [Wake::Decision]);
        assert!(!HoldReason::ForkApproval.rechecks());
    }

    #[test]
    fn a_full_fork_repo_waits_for_a_fork_job_to_end() {
        assert_eq!(HoldReason::parse("fork_limit"), Some(HoldReason::ForkLimit));
        assert_eq!(HoldReason::ForkLimit.as_str(), "fork_limit");
        assert_eq!(wakes(HoldReason::ForkLimit), [Wake::JobEndRepoFork]);
        assert!(HoldReason::ForkLimit.rechecks());
    }

    #[test]
    fn nothing_else_is_a_hold_reason() {
        assert_eq!(HoldReason::parse("waiting"), None);
        assert_eq!(HoldReason::parse(""), None);
    }
}
