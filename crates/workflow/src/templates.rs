// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The runner templates a job can ask for with `runs-on`.

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

/// Reads a third-party runner label as the template with its arch and at
/// least its vCPUs: the smallest that fits, or the largest when none
/// does. Returns the template and the vCPUs the label asks for.
///
/// Knows `blacksmith-<N>vcpu-ubuntu-<ver>[-arm]`,
/// `buildjet-<N>vcpu-ubuntu-<ver>[-arm]`, `depot-ubuntu-<ver>[-arm][-<N>]`,
/// `namespace-profile-<name>` (sized by a `<cpu>x<mem>` part, else two
/// vCPUs), `ubicloud-standard-<N>[-arm]`,
/// `warp-ubuntu-<ver>-<x64|arm64>-<N>x` and GitHub's one-vCPU
/// `ubuntu-slim`.
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

/// The arch and vCPUs a third-party label asks for.
fn vendor_size(label: &str) -> Option<(Arch, u32)> {
    let arch = |arm: bool| if arm { Arch::Arm64 } else { Arch::Amd64 };
    if label == "ubuntu-slim" {
        return Some((Arch::Amd64, 1));
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
