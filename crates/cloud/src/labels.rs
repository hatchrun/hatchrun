// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The `runs-on` label a job asks for and the runner it describes.
//!
//! A job asks for one label: the name of a runner profile (see
//! [`crate::profiles`]), optionally followed by overrides separated by
//! `/`, in any order:
//!
//! - `cpu=N`: at least N vCPUs.
//! - `ram=N`: at least N GiB of memory.
//! - `disk=N`: a root volume of N GiB.
//! - `spot=true` or `spot=false`: Spot, or on-demand.
//!
//! For example `runs-on: hatchrun-4cpu-arm64/disk=200/spot=true`.
//! Profile names are `hatchrun` or start with `hatchrun-`, so a label
//! for another kind of runner is never mistaken for one of ours.
//!
//! The runner registers with exactly the job's label. GitHub gives a job
//! any runner whose labels include all of the job's labels, so with two
//! labels a job asking only for `hatchrun` could take the runner launched
//! for `[hatchrun, cpu=8]` and leave that job waiting for a runner nobody
//! launches. With one label, jobs with the same label get identical
//! runners and it does not matter which job takes which runner.

use serde::{Deserialize, Serialize};

/// The label every hatchrun job asks for, alone or as the prefix.
pub const PREFIX: &str = "hatchrun";
/// Longest profile name. GitHub allows 256 characters per label; this
/// leaves room for overrides.
pub const NAME_MAX: usize = 64;

/// CPU architecture of a runner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    /// x86-64.
    #[default]
    Amd64,
    /// 64-bit Arm (Graviton).
    Arm64,
}

impl Arch {
    /// `amd64` or `arm64`, as the `images` table and the app name it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

/// Changes a label makes to its profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Fewest vCPUs.
    pub cpu: Option<u16>,
    /// Least memory in GiB.
    pub ram_gib: Option<u16>,
    /// Root volume size in GiB.
    pub disk_gib: Option<u16>,
    /// Spot or on-demand.
    pub spot: Option<bool>,
}

/// The runner a label asks for: a profile and what to change about it.
/// Which profiles exist depends on the organization, so the label alone
/// does not say whether it can be launched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// The profile name, lowercase.
    pub profile: String,
    /// Changes to the profile.
    pub overrides: Overrides,
}

/// Why a job's labels do not ask for a hatchrun runner we can launch.
#[derive(Debug, PartialEq, Eq)]
pub enum LabelError {
    /// No label is a hatchrun profile.
    NotOurs,
    /// A hatchrun label together with other labels.
    MoreThanOne,
    /// A profile name that cannot exist.
    BadName(String),
    /// An option this version does not know.
    Unknown(String),
    /// The same option twice.
    Duplicate(String),
    /// A known option with a value out of range.
    Invalid(String),
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOurs => write!(f, "no {PREFIX} label"),
            Self::MoreThanOne => write!(f, "a {PREFIX} label must be the job's only label"),
            Self::BadName(name) => write!(f, "{name:?} is not a runner profile name"),
            Self::Unknown(option) => write!(f, "unknown option {option:?}"),
            Self::Duplicate(option) => write!(f, "option {option:?} given twice"),
            Self::Invalid(option) => write!(f, "invalid option {option:?}"),
        }
    }
}

/// Picks the hatchrun label out of a job's labels.
///
/// # Errors
///
/// [`LabelError::NotOurs`] when no label is ours, which is the usual case
/// for jobs on GitHub-hosted or other runners, and
/// [`LabelError::MoreThanOne`] when ours comes with others.
pub fn job_label(labels: &[String]) -> Result<&str, LabelError> {
    let ours = labels.iter().filter(|label| is_ours(label)).count();
    match (ours, labels.len()) {
        (0, _) => Err(LabelError::NotOurs),
        (1, 1) => Ok(&labels[0]),
        _ => Err(LabelError::MoreThanOne),
    }
}

fn is_ours(label: &str) -> bool {
    // GitHub compares labels without regard to case.
    let lower = label.to_ascii_lowercase();
    let name = lower.split('/').next().unwrap_or_default();
    name == PREFIX || name.starts_with("hatchrun-")
}

/// Whether `name` can name a profile: `hatchrun`, or `hatchrun-`
/// followed by lowercase words of letters and digits joined by single
/// hyphens, at most [`NAME_MAX`] characters in all.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    if name == PREFIX {
        return true;
    }
    let Some(rest) = name.strip_prefix("hatchrun-") else {
        return false;
    };
    name.len() <= NAME_MAX
        && rest.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// Reads the runner a hatchrun label asks for.
///
/// # Errors
///
/// A [`LabelError`] naming a bad profile name, or the first option that
/// is unknown, repeated or out of range.
pub fn parse(label: &str) -> Result<Spec, LabelError> {
    let lower = label.to_ascii_lowercase();
    let mut parts = lower.split('/');
    let name = parts.next().unwrap_or_default();
    if !is_ours(name) {
        return Err(LabelError::NotOurs);
    }
    if !valid_name(name) {
        return Err(LabelError::BadName(name.to_owned()));
    }

    let mut overrides = Overrides::default();
    let mut seen: Vec<&str> = Vec::new();
    for option in parts {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        if seen.contains(&key) {
            return Err(LabelError::Duplicate(option.to_owned()));
        }
        seen.push(key);

        let invalid = || LabelError::Invalid(option.to_owned());
        match key {
            "cpu" => overrides.cpu = Some(number(value, 1..=CPU_MAX).ok_or_else(invalid)?),
            "ram" => overrides.ram_gib = Some(number(value, 1..=RAM_MAX).ok_or_else(invalid)?),
            "disk" => {
                overrides.disk_gib = Some(number(value, DISK_MIN..=DISK_MAX).ok_or_else(invalid)?);
            }
            "spot" => {
                overrides.spot = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(invalid()),
                });
            }
            _ => return Err(LabelError::Unknown(option.to_owned())),
        }
    }
    Ok(Spec {
        profile: name.to_owned(),
        overrides,
    })
}

/// Most vCPUs anything may ask for, the largest size in the catalog.
pub const CPU_MAX: u16 = 384;
/// Most memory anything may ask for, in GiB.
pub const RAM_MAX: u16 = 3072;
/// Smallest root volume, in GiB; the image needs room.
pub const DISK_MIN: u16 = 10;
/// Largest root volume, in GiB, the most gp3 allows.
pub const DISK_MAX: u16 = 16384;

fn number(value: &str, range: std::ops::RangeInclusive<u16>) -> Option<u16> {
    // Leading zeros and signs would let two spellings mean one runner.
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|number| range.contains(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn spec(profile: &str, overrides: Overrides) -> Spec {
        Spec {
            profile: profile.to_owned(),
            overrides,
        }
    }

    #[test]
    fn picks_the_only_label() {
        assert_eq!(job_label(&labels(&["hatchrun"])), Ok("hatchrun"));
        assert_eq!(
            job_label(&labels(&["HatchRun-4cpu-arm64/disk=100"])),
            Ok("HatchRun-4cpu-arm64/disk=100")
        );
        assert_eq!(
            job_label(&labels(&["hatchrun/spot=true"])),
            Ok("hatchrun/spot=true")
        );
    }

    #[test]
    fn ignores_other_runners() {
        for other in [
            "ubuntu-latest",
            "hatchrunner",
            "self-hosted",
            "linux/hatchrun-4cpu",
        ] {
            assert_eq!(
                job_label(&labels(&[other])),
                Err(LabelError::NotOurs),
                "{other}"
            );
        }
        assert_eq!(job_label(&[]), Err(LabelError::NotOurs));
    }

    #[test]
    fn refuses_a_label_set() {
        assert_eq!(
            job_label(&labels(&["self-hosted", "hatchrun-4cpu"])),
            Err(LabelError::MoreThanOne)
        );
        assert_eq!(
            job_label(&labels(&["hatchrun", "hatchrun-4cpu"])),
            Err(LabelError::MoreThanOne)
        );
    }

    /// GitHub hands a queued job to any idle runner whose labels include
    /// all of the job's labels. Were a runner registered with a label set,
    /// a job asking for fewer labels could take it, and the job it was
    /// launched for would wait for a runner nobody launches. With exactly
    /// one label per job and per runner, a runner only matches jobs that
    /// asked for the same label, which get the same runner.
    #[test]
    fn nested_label_sets_strand_jobs_one_label_does_not() {
        let takes = |runner: &[&str], job: &[&str]| job.iter().all(|label| runner.contains(label));

        // RunsOn style: the runner launched for the bigger job also
        // matches the smaller one.
        let big_job = ["hatchrun", "cpu=8"];
        let small_job = ["hatchrun"];
        assert!(takes(&big_job, &small_job));
        // So the smaller job can run on it and strand the bigger one,
        // which is why a label set is refused outright.
        assert_eq!(job_label(&labels(&big_job)), Err(LabelError::MoreThanOne));

        // One label each: the runner for one never matches the other.
        let (big_labels, small_labels) = (labels(&["hatchrun/cpu=8"]), labels(&["hatchrun"]));
        let big = job_label(&big_labels).unwrap();
        let small = job_label(&small_labels).unwrap();
        assert!(!takes(&[big], &[small]));
        assert!(!takes(&[small], &[big]));
        assert!(takes(&[big], &[big]));
    }

    #[test]
    fn names_a_profile() {
        assert_eq!(
            parse("hatchrun"),
            Ok(spec("hatchrun", Overrides::default()))
        );
        assert_eq!(
            parse("HatchRun-4CPU-arm64"),
            Ok(spec("hatchrun-4cpu-arm64", Overrides::default()))
        );
        assert_eq!(
            parse("hatchrun-big"),
            Ok(spec("hatchrun-big", Overrides::default()))
        );
    }

    #[test]
    fn reads_every_override() {
        assert_eq!(
            parse("hatchrun-4cpu/cpu=8/ram=64/disk=200/spot=false"),
            Ok(spec(
                "hatchrun-4cpu",
                Overrides {
                    cpu: Some(8),
                    ram_gib: Some(64),
                    disk_gib: Some(200),
                    spot: Some(false),
                }
            ))
        );
        assert_eq!(
            parse("hatchrun/SPOT=TRUE").map(|spec| spec.overrides.spot),
            Ok(Some(true))
        );
    }

    #[test]
    fn checks_profile_names() {
        for good in ["hatchrun", "hatchrun-2cpu", "hatchrun-a-b-c", "hatchrun-x1"] {
            assert!(valid_name(good), "{good}");
        }
        let long = format!("hatchrun-{}", "a".repeat(NAME_MAX));
        for bad in [
            "hatchrun-",
            "hatchrun--x",
            "hatchrun-x-",
            "hatchrun-a_b",
            "hatchrun-A",
            "runner",
            "hatchrunx",
            long.as_str(),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(
            parse("hatchrun--x"),
            Err(LabelError::BadName("hatchrun--x".to_owned()))
        );
        assert_eq!(parse("ubuntu-latest"), Err(LabelError::NotOurs));
    }

    #[test]
    fn refuses_bad_options() {
        let invalid = |option: &str| Err(LabelError::Invalid(option.to_owned()));
        assert_eq!(parse("hatchrun/cpu=0"), invalid("cpu=0"));
        assert_eq!(parse("hatchrun/cpu=08"), invalid("cpu=08"));
        assert_eq!(parse("hatchrun/cpu=+8"), invalid("cpu=+8"));
        assert_eq!(parse("hatchrun/cpu=385"), invalid("cpu=385"));
        assert_eq!(parse("hatchrun/disk=9"), invalid("disk=9"));
        assert_eq!(parse("hatchrun/cpu"), invalid("cpu"));
        assert_eq!(parse("hatchrun/spot"), invalid("spot"));
        assert_eq!(parse("hatchrun/spot=yes"), invalid("spot=yes"));
        assert_eq!(parse("hatchrun/"), Err(LabelError::Unknown(String::new())));
        // The old syntax is gone.
        assert_eq!(
            parse("hatchrun/arm64"),
            Err(LabelError::Unknown("arm64".to_owned()))
        );
        assert_eq!(
            parse("hatchrun/type=c7g.large"),
            Err(LabelError::Unknown("type=c7g.large".to_owned()))
        );
        assert_eq!(
            parse("hatchrun/cpu=2/cpu=4"),
            Err(LabelError::Duplicate("cpu=4".to_owned()))
        );
    }
}
