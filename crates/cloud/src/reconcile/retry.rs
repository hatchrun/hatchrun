// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Whether a failed launch is tried again, and when. Nothing here talks
//! to anything, so the rules test natively.
//!
//! A launch that failed for a reason that passes on its own (an EC2 or
//! Spot capacity error, throttling, an AWS, STS, GitHub or broker error, no
//! answer at all) or that the operator fixes (no image for the region,
//! a missing Worker setting) waits [`delay`] and the reconciler
//! launches it again, for as long as GitHub keeps the job queued.
//!
//! A launch that failed on the organization's own setup (the role
//! cannot be assumed or may not launch, the stack's launch template or
//! subnets are gone, no instance type the stack allows fits the
//! profile, the label names no profile, AWS is not connected, no
//! licence, the region is not allowed) would fail the same way every
//! time. It waits until an owner changes the setup: verifies the role
//! again, edits a profile, changes the settings or gains a licence.

/// What a launch failed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// AWS (STS or EC2) answered with this error code.
    Aws(String),
    /// GitHub answered with this HTTP status.
    GitHub(u16),
    /// The broker refused the runner's config.
    Broker,
    /// A request got no answer.
    Unreachable,
    /// EC2 answered with a capacity error, such as
    /// `InsufficientInstanceCapacity` or a Spot capacity error.
    NoCapacity,
    /// hatchrun has no image for the architecture in the region yet.
    NoImage,
    /// A Worker variable or secret is missing or does not parse.
    Settings,
    /// D1 failed, or an answer did not parse.
    Internal,
    /// The label names no runner profile of the organization.
    NoProfile,
    /// No instance type the connect stack allows fits the profile, or
    /// the profile asks for more disk than the stack allows.
    NoInstanceType,
    /// AWS is not connected, the organization has no licence, or its
    /// region is not allowed.
    Tenant,
}

/// What happens to a failed launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Launched again after [`delay`].
    Backoff,
    /// Launched again once an owner changes the setup.
    Setup,
}

impl Wait {
    /// The `jobs.launch_retry` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backoff => "backoff",
            Self::Setup => "setup",
        }
    }

    /// Reads the column value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "backoff" => Some(Self::Backoff),
            "setup" => Some(Self::Setup),
            _ => None,
        }
    }
}

/// AWS error codes that come from the organization's role or stack.
/// Any other code, capacity and throttling among them, passes or is
/// hatchrun's to fix.
const SETUP_CODES: &[&str] = &[
    // STS: the role does not exist, does not trust hatchrun's token,
    // or STS is off in the region.
    "AccessDenied",
    "InvalidIdentityToken",
    "IDPRejectedClaim",
    "ValidationError",
    "RegionDisabledException",
    // EC2: the role may not launch, or the account may not.
    "UnauthorizedOperation",
    "AuthFailure",
    "OptInRequired",
    "Blocked",
    // EC2: the stack's launch template or subnets are gone.
    "InvalidLaunchTemplateId.NotFound",
    "InvalidLaunchTemplateId.Malformed",
    "InvalidLaunchTemplateName.NotFoundException",
    "InvalidSubnetID.NotFound",
    "InvalidSubnetID.Malformed",
    "InvalidSubnet",
];

/// What happens after a launch failed on `cause`.
#[must_use]
pub fn classify(cause: &Cause) -> Wait {
    match cause {
        Cause::Aws(code) if SETUP_CODES.contains(&code.as_str()) => Wait::Setup,
        Cause::NoProfile | Cause::NoInstanceType | Cause::Tenant => Wait::Setup,
        Cause::Aws(_)
        | Cause::GitHub(_)
        | Cause::Broker
        | Cause::Unreachable
        | Cause::NoCapacity
        | Cause::NoImage
        | Cause::Settings
        | Cause::Internal => Wait::Backoff,
    }
}

/// The first wait, in milliseconds.
pub const FIRST_DELAY: u64 = 30 * 1000;
/// The longest wait, in milliseconds.
pub const MAX_DELAY: u64 = 5 * 60 * 1000;

/// How long to wait after the `attempts`th failed launch, in
/// milliseconds: 30 seconds, doubling up to 5 minutes. The reconciler
/// runs every minute, so the first waits round up to it.
#[must_use]
pub fn delay(attempts: u32) -> u64 {
    let doublings = attempts.saturating_sub(1).min(16);
    FIRST_DELAY.saturating_mul(1 << doublings).min(MAX_DELAY)
}

/// A failed launch: why, in words for the owner, and on what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// What the launch failed on.
    pub cause: Cause,
    /// The error, shown on the job.
    pub message: String,
    /// When what it failed on may have passed, Unix milliseconds: the
    /// next try waits at least until then.
    pub not_before: Option<u64>,
}

impl Failure {
    /// A failure on `cause` that says `message`.
    pub fn new(cause: Cause, message: impl Into<String>) -> Self {
        Self {
            cause,
            message: message.into(),
            not_before: None,
        }
    }

    /// The same failure, tried again no sooner than `at`.
    #[must_use]
    pub fn not_before(mut self, at: Option<u64>) -> Self {
        self.not_before = at;
        self
    }

    /// How the job row records the failure of its `attempts`th launch
    /// at `now`: the wait, and when to launch again, if on a timer.
    #[must_use]
    pub fn outcome(&self, attempts: u32, now: u64) -> (Wait, Option<u64>) {
        match classify(&self.cause) {
            Wait::Backoff => {
                let next = now.saturating_add(delay(attempts));
                (
                    Wait::Backoff,
                    Some(self.not_before.map_or(next, |at| at.max(next))),
                )
            }
            Wait::Setup => (Wait::Setup, None),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aws(code: &str) -> Wait {
        classify(&Cause::Aws(code.to_owned()))
    }

    #[test]
    fn capacity_throttling_and_aws_faults_back_off() {
        for code in crate::aws::ec2::CAPACITY_ERRORS {
            assert_eq!(aws(code), Wait::Backoff, "{code}");
        }
        for code in [
            "RequestLimitExceeded",
            "Throttling",
            "ThrottlingException",
            "InternalError",
            "ServiceUnavailable",
            "Unavailable",
            "IDPCommunicationError",
            "VcpuLimitExceeded",
            "InvalidAMIID.NotFound",
            "Unreachable",
            "SigningFailed",
            "MalformedResponse",
        ] {
            assert_eq!(aws(code), Wait::Backoff, "{code}");
        }
        for cause in [
            Cause::GitHub(502),
            Cause::GitHub(429),
            Cause::GitHub(403),
            Cause::Broker,
            Cause::Unreachable,
            Cause::NoCapacity,
            Cause::NoImage,
            Cause::Settings,
            Cause::Internal,
        ] {
            assert_eq!(classify(&cause), Wait::Backoff, "{cause:?}");
        }
    }

    #[test]
    fn the_organizations_setup_waits_for_a_change() {
        for code in [
            "AccessDenied",
            "InvalidIdentityToken",
            "UnauthorizedOperation",
            "InvalidLaunchTemplateId.NotFound",
            "InvalidSubnetID.NotFound",
        ] {
            assert_eq!(aws(code), Wait::Setup, "{code}");
        }
        for cause in [Cause::NoProfile, Cause::NoInstanceType, Cause::Tenant] {
            assert_eq!(classify(&cause), Wait::Setup, "{cause:?}");
        }
    }

    #[test]
    fn waits_double_up_to_five_minutes() {
        let secs: Vec<u64> = (1..=7).map(|attempts| delay(attempts) / 1000).collect();
        assert_eq!(secs, [30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(delay(0), FIRST_DELAY);
        assert_eq!(delay(u32::MAX), MAX_DELAY);
    }

    #[test]
    fn outcomes() {
        let image = Failure::new(Cause::NoImage, "no arm64 image in ap-south-1");
        assert_eq!(image.outcome(1, 1_000), (Wait::Backoff, Some(31_000)));
        assert_eq!(image.outcome(3, 1_000), (Wait::Backoff, Some(121_000)));
        // Capacity that comes back later than the backoff waits for it.
        let full = Failure::new(Cause::NoCapacity, "none").not_before(Some(200_000));
        assert_eq!(full.outcome(1, 1_000), (Wait::Backoff, Some(200_000)));
        assert_eq!(full.outcome(5, 1_000), (Wait::Backoff, Some(301_000)));
        let denied = Failure::new(Cause::Aws("AccessDenied".to_owned()), "denied");
        assert_eq!(denied.outcome(1, 1_000), (Wait::Setup, None));
        assert_eq!(Wait::parse(Wait::Backoff.as_str()), Some(Wait::Backoff));
        assert_eq!(Wait::parse(Wait::Setup.as_str()), Some(Wait::Setup));
        assert_eq!(Wait::parse("other"), None);
    }
}
