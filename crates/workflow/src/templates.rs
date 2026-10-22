// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The runner templates a job can ask for with `runs-on`.
//!
//! This file holds every price and cap amount of the CI service. Nothing
//! else in the workspace may hard-code one.

/// The price of one 2 vCPU minute, the billing unit, in millionths of a dollar.
pub const UNIT_PRICE_MICROS: u64 = 2_000;
/// Free units (2 vCPU minutes) per organization and UTC month.
pub const FREE_UNITS: u64 = 500;
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
    /// Billing units of `minutes` on this template.
    #[must_use]
    pub fn units(&self, minutes: u64) -> u64 {
        minutes * self.price_micros_per_minute / UNIT_PRICE_MICROS
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

/// What a month of `units` costs past the free units, in micros.
#[must_use]
pub fn spend_micros(units: u64) -> u64 {
    units.saturating_sub(FREE_UNITS) * UNIT_PRICE_MICROS
}

/// Formats micros as dollars and cents, rounded to the nearest cent.
#[must_use]
pub fn dollars(micros: u64) -> String {
    let cents = (micros + 5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
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
                u64::from(t.vcpu) * UNIT_PRICE_MICROS / 2
            );
            assert_eq!(t.memory_gib, 4 * t.vcpu);
        }
    }

    #[test]
    fn units_scale_with_price() {
        assert_eq!(template("ubuntu-latest").unwrap().units(10), 10);
        assert_eq!(template("ubuntu-26.04-4core-arm").unwrap().units(10), 20);
        assert_eq!(template("ubuntu-26.04-16core").unwrap().units(3), 24);
    }

    #[test]
    fn spend_starts_after_the_free_units() {
        assert_eq!(spend_micros(0), 0);
        assert_eq!(spend_micros(500), 0);
        assert_eq!(spend_micros(600), 200_000);
    }

    #[test]
    fn dollars_has_cents() {
        assert_eq!(dollars(50_000_000), "$50.00");
        assert_eq!(dollars(200_000), "$0.20");
    }
}
