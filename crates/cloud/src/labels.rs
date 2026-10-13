// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The `runs-on` label a job asks for and the runner it describes.
//!
//! A job asks for one label: `hatchrun`, or `hatchrun/` followed by
//! options separated by `/`, in any order:
//!
//! - `amd64` or `arm64`: CPU architecture, `amd64` when not given.
//! - `cpu=N`: at least N vCPUs.
//! - `ram=N`: at least N GiB of memory.
//! - `disk=N`: a root volume of N GiB.
//! - `spot`: a Spot instance.
//! - `type=c7g.xlarge`: this instance type, instead of `cpu` and `ram`.
//!
//! For example `runs-on: hatchrun/arm64/cpu=8/spot`.
//!
//! The runner registers with exactly the job's label. GitHub gives a job
//! any runner whose labels include all of the job's labels, so with two
//! labels a job asking only for `hatchrun` could take the runner launched
//! for `[hatchrun, cpu=8]` and leave that job waiting for a runner nobody
//! launches. With one label, jobs with the same label get identical
//! runners and it does not matter which job takes which runner.

/// The label every hatchrun job asks for, alone or as the prefix.
pub const PREFIX: &str = "hatchrun";

/// CPU architecture of a runner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Arch {
    /// x86-64.
    #[default]
    Amd64,
    /// 64-bit Arm (Graviton).
    Arm64,
}

/// The runner a label asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// CPU architecture.
    pub arch: Arch,
    /// Fewest vCPUs.
    pub cpu: Option<u16>,
    /// Least memory in GiB.
    pub ram_gib: Option<u16>,
    /// Root volume size in GiB.
    pub disk_gib: Option<u16>,
    /// Run on Spot.
    pub spot: bool,
    /// Exact instance type.
    pub instance_type: Option<String>,
}

/// Why a job's labels do not ask for a hatchrun runner we can launch.
#[derive(Debug, PartialEq, Eq)]
pub enum LabelError {
    /// No label is `hatchrun` or starts with `hatchrun/`.
    NotOurs,
    /// A hatchrun label together with other labels.
    MoreThanOne,
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
    lower == PREFIX || lower.starts_with("hatchrun/")
}

/// Reads the runner a hatchrun label asks for.
///
/// # Errors
///
/// A [`LabelError`] naming the first option that is unknown, repeated or
/// out of range.
pub fn parse(label: &str) -> Result<Spec, LabelError> {
    let lower = label.to_ascii_lowercase();
    let mut options = lower.split('/');
    if options.next() != Some(PREFIX) {
        return Err(LabelError::NotOurs);
    }

    let mut spec = Spec::default();
    let mut seen: Vec<&str> = Vec::new();
    for option in options {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        let name = match key {
            "amd64" | "arm64" => "arch",
            other => other,
        };
        if seen.contains(&name) {
            return Err(LabelError::Duplicate(option.to_owned()));
        }
        seen.push(name);

        let invalid = || LabelError::Invalid(option.to_owned());
        match (key, value.is_empty()) {
            ("amd64", true) => spec.arch = Arch::Amd64,
            ("arm64", true) => spec.arch = Arch::Arm64,
            ("spot", true) => spec.spot = true,
            ("cpu", false) => spec.cpu = Some(number(value, 1..=192).ok_or_else(invalid)?),
            ("ram", false) => spec.ram_gib = Some(number(value, 1..=1536).ok_or_else(invalid)?),
            ("disk", false) => {
                spec.disk_gib = Some(number(value, 10..=16384).ok_or_else(invalid)?);
            }
            ("type", false) if valid_instance_type(value) => {
                spec.instance_type = Some(value.to_owned());
            }
            ("amd64" | "arm64" | "spot" | "cpu" | "ram" | "disk" | "type", _) => {
                return Err(invalid());
            }
            _ => return Err(LabelError::Unknown(option.to_owned())),
        }
    }
    if spec.instance_type.is_some() && (spec.cpu.is_some() || spec.ram_gib.is_some()) {
        return Err(LabelError::Invalid("type with cpu or ram".to_owned()));
    }
    Ok(spec)
}

fn number(value: &str, range: std::ops::RangeInclusive<u16>) -> Option<u16> {
    // Leading zeros and signs would let two spellings mean one runner.
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|number| range.contains(number))
}

/// `family.size`, as in `c7g.xlarge` or `m7i-flex.large`.
fn valid_instance_type(value: &str) -> bool {
    let Some((family, size)) = value.split_once('.') else {
        return false;
    };
    let word = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    value.len() <= 32 && word(family) && word(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn picks_the_only_label() {
        assert_eq!(job_label(&labels(&["hatchrun"])), Ok("hatchrun"));
        assert_eq!(
            job_label(&labels(&["HatchRun/arm64"])),
            Ok("HatchRun/arm64")
        );
    }

    #[test]
    fn ignores_other_runners() {
        assert_eq!(
            job_label(&labels(&["ubuntu-latest"])),
            Err(LabelError::NotOurs)
        );
        assert_eq!(
            job_label(&labels(&["hatchrunner"])),
            Err(LabelError::NotOurs)
        );
        assert_eq!(job_label(&[]), Err(LabelError::NotOurs));
    }

    #[test]
    fn refuses_a_label_set() {
        assert_eq!(
            job_label(&labels(&["self-hosted", "hatchrun"])),
            Err(LabelError::MoreThanOne)
        );
        assert_eq!(
            job_label(&labels(&["hatchrun", "hatchrun/arm64"])),
            Err(LabelError::MoreThanOne)
        );
    }

    #[test]
    fn plain_label_is_the_default_runner() {
        assert_eq!(parse("hatchrun"), Ok(Spec::default()));
    }

    #[test]
    fn reads_every_option() {
        assert_eq!(
            parse("hatchrun/arm64/cpu=8/ram=16/disk=100/spot"),
            Ok(Spec {
                arch: Arch::Arm64,
                cpu: Some(8),
                ram_gib: Some(16),
                disk_gib: Some(100),
                spot: true,
                instance_type: None,
            })
        );
        assert_eq!(
            parse("HATCHRUN/Type=M7i-Flex.Large").map(|spec| spec.instance_type),
            Ok(Some("m7i-flex.large".to_owned()))
        );
    }

    #[test]
    fn refuses_bad_options() {
        let invalid = |option: &str| Err(LabelError::Invalid(option.to_owned()));
        assert_eq!(parse("hatchrun/cpu=0"), invalid("cpu=0"));
        assert_eq!(parse("hatchrun/cpu=08"), invalid("cpu=08"));
        assert_eq!(parse("hatchrun/cpu=+8"), invalid("cpu=+8"));
        assert_eq!(parse("hatchrun/cpu=193"), invalid("cpu=193"));
        assert_eq!(parse("hatchrun/disk=9"), invalid("disk=9"));
        assert_eq!(parse("hatchrun/cpu"), invalid("cpu"));
        assert_eq!(parse("hatchrun/spot=yes"), invalid("spot=yes"));
        assert_eq!(parse("hatchrun/type=c7g"), invalid("type=c7g"));
        assert_eq!(
            parse("hatchrun/type=c7g.large/cpu=4"),
            invalid("type with cpu or ram")
        );
        assert_eq!(parse("hatchrun/"), Err(LabelError::Unknown(String::new())));
        assert_eq!(
            parse("hatchrun/gpu=1"),
            Err(LabelError::Unknown("gpu=1".to_owned()))
        );
        assert_eq!(
            parse("hatchrun/arm64/amd64"),
            Err(LabelError::Duplicate("amd64".to_owned()))
        );
        assert_eq!(
            parse("hatchrun/cpu=2/cpu=4"),
            Err(LabelError::Duplicate("cpu=4".to_owned()))
        );
    }
}
