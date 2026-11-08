// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The runner templates a job can ask for with `runs-on`.
//!
//! This file holds every price and cap amount of the CI service. Nothing
//! else in the workspace may hard-code one.

/// The billing unit is one 2 vCPU second. The price of one 2 vCPU
/// minute (60 units), in millionths of a dollar.
pub const UNIT_PRICE_MICROS_PER_MINUTE: u64 = 2_000;
/// Jobs one organization may have launching or running at once.
pub const MAX_RUNNING_JOBS: u32 = 5;
/// The monthly spend cap until the owner changes it: $50.
pub const DEFAULT_SPEND_CAP_MICROS: u64 = 50_000_000;
/// Lowest cap the owner may set: $50.
pub const MIN_SPEND_CAP_MICROS: u64 = 50_000_000;
/// Highest cap the owner may set: $10,000.
pub const MAX_SPEND_CAP_MICROS: u64 = 10_000_000_000;

/// The CPU architecture of a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// x86-64.
    Amd64,
    /// 64-bit Arm.
    Arm64,
}

/// A runner size a job can ask for.
#[derive(Debug, PartialEq, Eq)]
pub struct Template {
    /// The `runs-on` labels that select this template.
    pub labels: &'static [&'static str],
    /// The CPU architecture.
    pub arch: Arch,
    /// Number of virtual CPUs.
    pub vcpu: u32,
    /// Memory in GiB.
    pub memory_gib: u32,
    /// Price in millionths of a dollar per minute ($0.002 is `2_000`).
    pub price_micros_per_minute: u64,
}

impl Template {
    /// Billing units of `seconds` on this template: seconds times vCPUs / 2.
    #[must_use]
    pub fn units(&self, seconds: u64) -> u64 {
        seconds * self.price_micros_per_minute / UNIT_PRICE_MICROS_PER_MINUTE
    }
}

/// Every runner template, in table order.
pub const TEMPLATES: &[Template] = &[
    Template {
        labels: &["ubuntu-latest", "ubuntu-26.04"],
        arch: Arch::Amd64,
        vcpu: 2,
        memory_gib: 8,
        price_micros_per_minute: 2000,
    },
    Template {
        labels: &["ubuntu-26.04-arm"],
        arch: Arch::Arm64,
        vcpu: 2,
        memory_gib: 8,
        price_micros_per_minute: 2000,
    },
    Template {
        labels: &["ubuntu-26.04-4core"],
        arch: Arch::Amd64,
        vcpu: 4,
        memory_gib: 16,
        price_micros_per_minute: 4000,
    },
    Template {
        labels: &["ubuntu-26.04-4core-arm"],
        arch: Arch::Arm64,
        vcpu: 4,
        memory_gib: 16,
        price_micros_per_minute: 4000,
    },
    Template {
        labels: &["ubuntu-26.04-8core"],
        arch: Arch::Amd64,
        vcpu: 8,
        memory_gib: 32,
        price_micros_per_minute: 8000,
    },
    Template {
        labels: &["ubuntu-26.04-8core-arm"],
        arch: Arch::Arm64,
        vcpu: 8,
        memory_gib: 32,
        price_micros_per_minute: 8000,
    },
    Template {
        labels: &["ubuntu-26.04-16core"],
        arch: Arch::Amd64,
        vcpu: 16,
        memory_gib: 64,
        price_micros_per_minute: 16000,
    },
    Template {
        labels: &["ubuntu-26.04-16core-arm"],
        arch: Arch::Arm64,
        vcpu: 16,
        memory_gib: 64,
        price_micros_per_minute: 16000,
    },
];

/// Finds the template a `runs-on` label selects.
#[must_use]
pub fn template(label: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.labels.contains(&label))
}

/// Every label, in table order. Used in error messages.
#[must_use]
pub fn template_labels() -> Vec<&'static str> {
    TEMPLATES
        .iter()
        .flat_map(|t| t.labels.iter().copied())
        .collect()
}

/// What a month of `units` costs, in micros, rounded down once.
#[must_use]
pub fn spend_micros(units: u64) -> u64 {
    units * UNIT_PRICE_MICROS_PER_MINUTE / 60
}

/// Formats micros as dollars and cents, rounded to the nearest cent.
#[must_use]
pub fn dollars(micros: u64) -> String {
    let cents = (micros + 5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// The template a runner label of a workflow that ran on GitHub or
/// another service selects: the label itself, the template a retired
/// Ubuntu label moved to, or the template of a third-party label's arch
/// and size. Read when a job's `runs-on` or matrix is read at run time.
pub(crate) fn runner_label(label: &str) -> Option<&'static str> {
    /// Labels GitHub retires and the template each becomes.
    const OLD_UBUNTU: &[(&str, &str)] = &[
        ("ubuntu-24.04", "ubuntu-latest"),
        ("ubuntu-22.04", "ubuntu-latest"),
        ("ubuntu-24.04-arm", "ubuntu-26.04-arm"),
        ("ubuntu-22.04-arm", "ubuntu-26.04-arm"),
    ];
    if let Some((_, new)) = OLD_UBUNTU.iter().find(|(old, _)| *old == label) {
        return Some(new);
    }
    if let Some(found) = template_labels().into_iter().find(|l| *l == label) {
        return Some(found);
    }
    vendor_template(label).map(|(t, _)| t.labels[0])
}

/// Reads a third-party runner label as the template with its arch and at
/// least its vCPUs: the smallest that fits, or the largest when none
/// does. Returns the template and the vCPUs the label asks for.
///
/// Knows `blacksmith-<N>vcpu-ubuntu-<ver>[-arm]`,
/// `buildjet-<N>vcpu-ubuntu-<ver>[-arm]`, `depot-ubuntu-<ver>[-arm][-<N>]`,
/// `namespace-profile-<name>` (sized by a `<cpu>x<mem>` part, else two
/// vCPUs), `ubicloud-standard-<N>[-arm]`,
/// `warp-ubuntu-<ver>-<x64|arm64>-<N>x`, GitHub's one-vCPU
/// `ubuntu-slim` and GitHub's larger runners (see [`github_larger`]).
pub(crate) fn vendor_template(label: &str) -> Option<(&'static Template, u32)> {
    let (arch, vcpu) = vendor_size(label)?;
    if vcpu == 0 {
        return None;
    }
    let fits = TEMPLATES
        .iter()
        .filter(|t| t.arch == arch && t.vcpu >= vcpu)
        .min_by_key(|t| t.vcpu);
    let template = fits.or_else(|| {
        TEMPLATES
            .iter()
            .filter(|t| t.arch == arch)
            .max_by_key(|t| t.vcpu)
    })?;
    Some((template, vcpu))
}

/// The arch and vCPUs of a GitHub larger runner label the way
/// organizations name them: `ubuntu-<latest|NN.NN>[-arm|-arm64]` then a
/// size, `<N>-cores`, `<N>core` or `xl`. The `xl` size has no fixed
/// meaning on GitHub, where the owner names the runner; it is read as 8
/// vCPUs, the size such runners most often have.
fn github_larger(label: &str) -> Option<(Arch, u32)> {
    let mut parts = label.strip_prefix("ubuntu-")?.split('-');
    let version = parts.next()?;
    let release = version.split_once('.').is_some_and(|(year, month)| {
        [year, month]
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
    });
    if version != "latest" && !release {
        return None;
    }
    let (mut arm, mut vcpu) = (false, None);
    for part in parts {
        match part {
            "arm" | "arm64" => arm = true,
            "cores" | "core" => {}
            "xl" => vcpu = Some(8),
            _ => {
                let n = part
                    .strip_suffix("cores")
                    .or_else(|| part.strip_suffix("core"));
                vcpu = Some(n.unwrap_or(part).parse().ok()?);
            }
        }
    }
    Some((if arm { Arch::Arm64 } else { Arch::Amd64 }, vcpu?))
}

/// The arch and vCPUs a third-party label asks for.
fn vendor_size(label: &str) -> Option<(Arch, u32)> {
    let arch = |arm: bool| if arm { Arch::Arm64 } else { Arch::Amd64 };
    if label == "ubuntu-slim" {
        return Some((Arch::Amd64, 1));
    }
    if let Some(size) = github_larger(label) {
        return Some(size);
    }
    for vendor in ["blacksmith-", "buildjet-"] {
        if let Some(rest) = label.strip_prefix(vendor) {
            let (vcpu, os) = rest.split_once("vcpu-ubuntu-")?;
            let arm = os.strip_suffix("-arm").is_some();
            return Some((arch(arm), vcpu.parse().ok()?));
        }
    }
    if let Some(rest) = label.strip_prefix("depot-ubuntu-") {
        let mut parts = rest.split('-').skip(1).peekable();
        let arm = parts.next_if_eq(&"arm").is_some();
        let vcpu = match parts.next() {
            Some(n) => n.parse().ok()?,
            None => 2,
        };
        return parts.next().is_none().then_some((arch(arm), vcpu));
    }
    if let Some(rest) = label.strip_prefix("namespace-profile-") {
        let arm = rest.split('-').any(|p| p.starts_with("arm"));
        let vcpu = rest
            .split('-')
            .find_map(|p| {
                let (cpu, mem) = p.split_once('x')?;
                mem.parse::<u32>().ok()?;
                cpu.parse().ok()
            })
            .unwrap_or(2);
        return Some((arch(arm), vcpu));
    }
    if let Some(rest) = label.strip_prefix("ubicloud-standard-") {
        let (vcpu, arm) = match rest.split_once('-') {
            Some((vcpu, "arm")) => (vcpu, true),
            Some(_) => return None,
            None => (rest, false),
        };
        return Some((arch(arm), vcpu.parse().ok()?));
    }
    if let Some(rest) = label.strip_prefix("warp-ubuntu-") {
        let mut parts = rest.split('-').skip(1);
        let arm = match parts.next()? {
            "x64" => false,
            "arm64" => true,
            _ => return None,
        };
        let vcpu = parts.next()?.strip_suffix('x')?.parse().ok()?;
        return parts.next().is_none().then_some((arch(arm), vcpu));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_is_two_vcpu_amd64() {
        let t = template("ubuntu-latest").unwrap();
        assert_eq!(
            (t.arch, t.vcpu, t.memory_gib, t.price_micros_per_minute),
            (Arch::Amd64, 2, 8, 2_000)
        );
        assert!(std::ptr::eq(t, template("ubuntu-26.04").unwrap()));
    }

    #[test]
    fn sixteen_core_arm() {
        let t = template("ubuntu-26.04-16core-arm").unwrap();
        assert_eq!(
            (t.arch, t.vcpu, t.memory_gib, t.price_micros_per_minute),
            (Arch::Arm64, 16, 64, 16_000)
        );
    }

    #[test]
    fn unknown_label() {
        assert!(template("ubuntu-24.04").is_none());
        assert!(template_labels().contains(&"ubuntu-26.04-8core"));
        assert_eq!(template_labels().len(), 9);
    }

    #[test]
    fn github_larger_runner_labels_take_the_template_of_their_size() {
        let cases = [
            ("ubuntu-latest-4-cores", "ubuntu-26.04-4core", 4),
            ("ubuntu-latest-8-cores", "ubuntu-26.04-8core", 8),
            ("ubuntu-22.04-8core", "ubuntu-26.04-8core", 8),
            ("ubuntu-24.04-16core", "ubuntu-26.04-16core", 16),
            ("ubuntu-latest-xl", "ubuntu-26.04-8core", 8),
            ("ubuntu-24.04-xl", "ubuntu-26.04-8core", 8),
            ("ubuntu-24.04-arm-4-cores", "ubuntu-26.04-4core-arm", 4),
            ("ubuntu-latest-32-cores", "ubuntu-26.04-16core", 32),
            ("ubuntu-22.04-64core", "ubuntu-26.04-16core", 64),
        ];
        for (label, want, asked) in cases {
            let (t, vcpu) = vendor_template(label).unwrap_or_else(|| panic!("{label}"));
            assert_eq!((t.labels[0], vcpu), (want, asked), "{label}");
        }
        for label in [
            "ubuntu-latest",
            "ubuntu-22.04",
            "ubuntu-latest-big",
            "ubuntu-latest-cores",
            "ubuntu-2x.04-4core",
            "macos-latest-xl",
        ] {
            assert!(vendor_template(label).is_none(), "{label}");
        }
    }

    #[test]
    fn the_table_is_the_spec() {
        let rows: Vec<_> = TEMPLATES
            .iter()
            .map(|t| {
                (
                    t.labels.to_vec(),
                    t.arch,
                    t.vcpu,
                    t.memory_gib,
                    t.price_micros_per_minute,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    vec!["ubuntu-latest", "ubuntu-26.04"],
                    Arch::Amd64,
                    2,
                    8,
                    2_000
                ),
                (vec!["ubuntu-26.04-arm"], Arch::Arm64, 2, 8, 2_000),
                (vec!["ubuntu-26.04-4core"], Arch::Amd64, 4, 16, 4_000),
                (vec!["ubuntu-26.04-4core-arm"], Arch::Arm64, 4, 16, 4_000),
                (vec!["ubuntu-26.04-8core"], Arch::Amd64, 8, 32, 8_000),
                (vec!["ubuntu-26.04-8core-arm"], Arch::Arm64, 8, 32, 8_000),
                (vec!["ubuntu-26.04-16core"], Arch::Amd64, 16, 64, 16_000),
                (vec!["ubuntu-26.04-16core-arm"], Arch::Arm64, 16, 64, 16_000),
            ]
        );
    }

    #[test]
    fn prices_follow_vcpus() {
        for t in TEMPLATES {
            assert_eq!(
                t.price_micros_per_minute,
                u64::from(t.vcpu) * UNIT_PRICE_MICROS_PER_MINUTE / 2
            );
            assert_eq!(t.memory_gib, 4 * t.vcpu);
        }
    }

    /// The first label of the template a vendor label maps to, and the
    /// vCPUs it asks for.
    fn vendor(label: &str) -> Option<(&'static str, u32)> {
        vendor_template(label).map(|(t, vcpu)| (t.labels[0], vcpu))
    }

    #[test]
    fn vendor_labels_round_up_to_a_template() {
        for (label, want) in [
            ("ubuntu-slim", ("ubuntu-latest", 1)),
            ("blacksmith-4vcpu-ubuntu-2404", ("ubuntu-26.04-4core", 4)),
            (
                "blacksmith-8vcpu-ubuntu-2204-arm",
                ("ubuntu-26.04-8core-arm", 8),
            ),
            ("blacksmith-6vcpu-ubuntu-2404", ("ubuntu-26.04-8core", 6)),
            ("buildjet-2vcpu-ubuntu-2204", ("ubuntu-latest", 2)),
            (
                "buildjet-16vcpu-ubuntu-2204-arm",
                ("ubuntu-26.04-16core-arm", 16),
            ),
            ("depot-ubuntu-24.04", ("ubuntu-latest", 2)),
            ("depot-ubuntu-latest-arm", ("ubuntu-26.04-arm", 2)),
            ("depot-ubuntu-22.04-4", ("ubuntu-26.04-4core", 4)),
            ("depot-ubuntu-24.04-arm-16", ("ubuntu-26.04-16core-arm", 16)),
            ("namespace-profile-linux-8x16", ("ubuntu-26.04-8core", 8)),
            (
                "namespace-profile-ci-4x8-ubuntu-24.04-arm64",
                ("ubuntu-26.04-4core-arm", 4),
            ),
            ("namespace-profile-default", ("ubuntu-latest", 2)),
            ("ubicloud-standard-2", ("ubuntu-latest", 2)),
            ("ubicloud-standard-30-arm", ("ubuntu-26.04-16core-arm", 30)),
            ("warp-ubuntu-latest-x64-4x", ("ubuntu-26.04-4core", 4)),
            (
                "warp-ubuntu-2404-arm64-32x",
                ("ubuntu-26.04-16core-arm", 32),
            ),
        ] {
            assert_eq!(vendor(label), Some(want), "{label}");
        }
    }

    #[test]
    fn units_are_two_vcpu_seconds() {
        assert_eq!(template("ubuntu-latest").unwrap().units(61), 61);
        assert_eq!(template("ubuntu-26.04-4core-arm").unwrap().units(60), 120);
        assert_eq!(template("ubuntu-26.04-8core").unwrap().units(90), 360);
        assert_eq!(template("ubuntu-26.04-16core").unwrap().units(90), 720);
    }

    #[test]
    fn spend_is_exact_with_no_free_units() {
        assert_eq!(spend_micros(0), 0);
        assert_eq!(spend_micros(1), 33);
        assert_eq!(spend_micros(3), 100);
        assert_eq!(spend_micros(60), 2_000);
        assert_eq!(spend_micros(1_500_000), 50_000_000);
    }

    #[test]
    fn dollars_has_cents() {
        assert_eq!(dollars(50_000_000), "$50.00");
        assert_eq!(dollars(200_000), "$0.20");
    }

    #[test]
    fn unknown_vendor_labels() {
        for label in [
            "ubuntu-latest",
            "macos-latest",
            "blacksmith-ubuntu-2404",
            "blacksmith-0vcpu-ubuntu-2404",
            "depot-macos-latest",
            "depot-ubuntu-24.04-x-4",
            "ubicloud-standard-2-x",
            "warp-ubuntu-latest-mips-4x",
            "warp-macos-latest-arm64-6x",
        ] {
            assert_eq!(vendor(label), None, "{label}");
        }
    }
}
