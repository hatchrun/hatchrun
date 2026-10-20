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
}
