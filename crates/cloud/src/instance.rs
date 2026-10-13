// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The instance type for a label.
//!
//! `type=` names it outright, and its architecture decides the image.
//! Otherwise the runner is the smallest current-generation general
//! purpose type with at least the vCPUs and memory asked for, from the
//! families connect.yaml allows by default: `c7g`, `m7g` and `r7g` on
//! arm64, `c7i`, `m7i` and `r7i` on amd64. Fewer vCPUs win over less
//! memory, so `ram=64` gets an `r7g.2xlarge` rather than an `m7g.4xlarge`.
//! A label with neither `cpu` nor `ram` gets 2 vCPUs and 8 GiB, close to
//! GitHub's standard Linux runner.

use crate::labels::{Arch, Spec};

/// Families by memory per vCPU, least first: (family, GiB per vCPU).
const ARM64: &[(&str, u16)] = &[("c7g", 2), ("m7g", 4), ("r7g", 8)];
const AMD64: &[(&str, u16)] = &[("c7i", 2), ("m7i", 4), ("r7i", 8)];
/// Sizes by vCPU count.
const SIZES: &[(&str, u16)] = &[
    ("large", 2),
    ("xlarge", 4),
    ("2xlarge", 8),
    ("4xlarge", 16),
    ("8xlarge", 32),
    ("12xlarge", 48),
    ("16xlarge", 64),
    ("24xlarge", 96),
    ("48xlarge", 192),
];
/// Graviton 3 stops at 64 vCPUs.
const ARM64_MAX_CPU: u16 = 64;

/// The runner to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// EC2 instance type.
    pub instance_type: String,
    /// Architecture, which picks the image.
    pub arch: Arch,
}

/// Picks the instance type for `spec`.
///
/// # Errors
///
/// When no type in the families has the vCPUs and memory asked for.
pub fn choose(spec: &Spec) -> Result<Choice, String> {
    if let Some(instance_type) = &spec.instance_type {
        return Ok(Choice {
            instance_type: instance_type.clone(),
            arch: arch_of(instance_type),
        });
    }
    let (cpu, ram) = match (spec.cpu, spec.ram_gib) {
        (None, None) => (2, 8),
        (cpu, ram) => (cpu.unwrap_or(1), ram.unwrap_or(0)),
    };
    let (families, max_cpu) = match spec.arch {
        Arch::Arm64 => (ARM64, ARM64_MAX_CPU),
        Arch::Amd64 => (AMD64, u16::MAX),
    };
    SIZES
        .iter()
        .filter(|(_, vcpus)| *vcpus >= cpu && *vcpus <= max_cpu)
        .find_map(|(size, vcpus)| {
            families
                .iter()
                .find(|(_, per_cpu)| u32::from(*vcpus) * u32::from(*per_cpu) >= u32::from(ram))
                .map(|(family, _)| format!("{family}.{size}"))
        })
        .map(|instance_type| Choice {
            instance_type,
            arch: spec.arch,
        })
        .ok_or_else(|| {
            format!(
                "no {} instance type has {cpu} vCPUs and {ram} GiB",
                match spec.arch {
                    Arch::Arm64 => "arm64",
                    Arch::Amd64 => "amd64",
                }
            )
        })
}

/// Graviton families carry a `g` after the generation, as in `c7g`,
/// `m6gd` or `c7gn`; `a1` is the first Graviton. Everything else is
/// x86-64.
fn arch_of(instance_type: &str) -> Arch {
    let family = instance_type.split('.').next().unwrap_or_default();
    let after_generation = family
        .find(|c: char| c.is_ascii_digit())
        .map_or("", |at| &family[at..]);
    if family == "a1" || after_generation.contains('g') {
        Arch::Arm64
    } else {
        Arch::Amd64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::parse;

    fn chosen(label: &str) -> Result<(String, Arch), String> {
        choose(&parse(label).unwrap()).map(|choice| (choice.instance_type, choice.arch))
    }

    #[test]
    fn default_runner() {
        assert_eq!(
            chosen("hatchrun"),
            Ok(("m7i.large".to_owned(), Arch::Amd64))
        );
        assert_eq!(
            chosen("hatchrun/arm64"),
            Ok(("m7g.large".to_owned(), Arch::Arm64))
        );
    }

    #[test]
    fn smallest_type_that_fits() {
        let types = |label: &str| chosen(label).unwrap().0;
        assert_eq!(types("hatchrun/cpu=1"), "c7i.large");
        assert_eq!(types("hatchrun/cpu=8"), "c7i.2xlarge");
        assert_eq!(types("hatchrun/cpu=6"), "c7i.2xlarge");
        assert_eq!(types("hatchrun/arm64/cpu=4/ram=16"), "m7g.xlarge");
        assert_eq!(types("hatchrun/arm64/cpu=4/ram=17"), "r7g.xlarge");
        assert_eq!(types("hatchrun/arm64/ram=64"), "r7g.2xlarge");
        assert_eq!(types("hatchrun/ram=3"), "c7i.large");
        assert_eq!(types("hatchrun/cpu=192"), "c7i.48xlarge");
        assert_eq!(types("hatchrun/ram=1536"), "r7i.48xlarge");
    }

    #[test]
    fn refuses_what_no_type_has() {
        assert_eq!(
            chosen("hatchrun/arm64/cpu=96"),
            Err("no arm64 instance type has 96 vCPUs and 0 GiB".to_owned())
        );
        assert!(chosen("hatchrun/arm64/ram=1024").is_err());
    }

    #[test]
    fn exact_type_decides_the_arch() {
        assert_eq!(
            chosen("hatchrun/type=c7gn.xlarge"),
            Ok(("c7gn.xlarge".to_owned(), Arch::Arm64))
        );
        assert_eq!(
            chosen("hatchrun/arm64/type=m7i-flex.large"),
            Ok(("m7i-flex.large".to_owned(), Arch::Amd64))
        );
        assert_eq!(arch_of("a1.large"), Arch::Arm64);
        assert_eq!(arch_of("g5g.xlarge"), Arch::Arm64);
        assert_eq!(arch_of("g6.xlarge"), Arch::Amd64);
        assert_eq!(arch_of("m6gd.large"), Arch::Arm64);
    }
}
