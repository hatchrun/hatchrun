// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a tenant has used this month and what that costs, from the
//! [`Standing`] the store reads. The prices live in
//! `hatchrun_workflow::templates`.

use hatchrun_workflow::templates;
use serde::Serialize;

use crate::store::Standing;

const DAY_MS: u64 = 86_400_000;

/// The start of the UTC calendar month of `now_ms`, in Unix
/// milliseconds.
#[must_use]
pub fn month_start(now_ms: u64) -> u64 {
    let days = i64::try_from(now_ms / DAY_MS).unwrap_or(i64::MAX / 2);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let first = days - (day - 1);
    u64::try_from(first).unwrap_or(0) * DAY_MS
}

/// Billing units used this month, as priced when each job completed.
/// A later price change or a removed template does not alter them.
#[must_use]
pub fn units(standing: &Standing) -> u64 {
    standing.units
}

/// What this month's usage costs, in micros.
#[must_use]
pub fn spend_micros(standing: &Standing) -> u64 {
    templates::spend_micros(units(standing))
}

/// What the billing page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingView {
    /// Whether a card is on file.
    pub card: bool,
    /// Billed runner seconds this month, of every template; every
    /// attempt counts at least 60.
    pub seconds: u64,
    /// Billing units used this month; one unit is one 2 vCPU second.
    pub units: u64,
    /// What this month's usage costs, in cents.
    pub spend_cents: u64,
    /// The monthly spend cap, in whole dollars.
    pub cap_dollars: u64,
    /// The least the cap can be set to, in whole dollars.
    pub min_cap_dollars: u64,
    /// The most the cap can be set to, in whole dollars.
    pub max_cap_dollars: u64,
    /// Jobs running now.
    pub running: u32,
    /// The most jobs that run at once.
    pub max_running: u32,
}

/// The billing page of `standing`.
#[must_use]
pub fn view(standing: &Standing) -> BillingView {
    BillingView {
        card: standing.card,
        seconds: standing.seconds_by_template.iter().map(|(_, s)| s).sum(),
        units: units(standing),
        spend_cents: spend_micros(standing) / MICROS_PER_CENT,
        cap_dollars: standing.cap_micros / MICROS_PER_DOLLAR,
        min_cap_dollars: templates::MIN_SPEND_CAP_MICROS / MICROS_PER_DOLLAR,
        max_cap_dollars: templates::MAX_SPEND_CAP_MICROS / MICROS_PER_DOLLAR,
        running: standing.running,
        max_running: templates::MAX_RUNNING_JOBS,
    }
}

const MICROS_PER_CENT: u64 = 10_000;
const MICROS_PER_DOLLAR: u64 = 1_000_000;

/// The spend cap in micros for whole `dollars`.
///
/// # Errors
///
/// The amount is outside the cap range; the message says the range.
pub fn cap_micros(dollars: u64) -> Result<u64, String> {
    dollars
        .checked_mul(MICROS_PER_DOLLAR)
        .filter(|micros| {
            (templates::MIN_SPEND_CAP_MICROS..=templates::MAX_SPEND_CAP_MICROS).contains(micros)
        })
        .ok_or_else(|| {
            format!(
                "Set a cap from {} to {}",
                templates::dollars(templates::MIN_SPEND_CAP_MICROS),
                templates::dollars(templates::MAX_SPEND_CAP_MICROS)
            )
        })
}

/// How long a held job waits before its caps are checked again, in
/// milliseconds.
pub const HOLD_RECHECK_MS: u64 = 30_000;

/// Why a queued job does not launch yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// The tenant has no card.
    Card,
    /// This month's spend reached the tenant's cap.
    SpendCap,
    /// The tenant already runs [`templates::MAX_RUNNING_JOBS`] jobs.
    Concurrency,
}

impl Hold {
    /// The word the `ci_jobs.held` column stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::SpendCap => "spend_cap",
            Self::Concurrency => "concurrency",
        }
    }

    /// What the job's check run tells the user.
    #[must_use]
    pub fn message(self, org: &str, app_url: &str, cap_micros: u64) -> String {
        match self {
            Self::Card => {
                format!("Waiting for a card. Add one for {org} at {app_url}/o/{org}/ci.")
            }
            Self::SpendCap => format!(
                "Waiting: {org} reached its {} monthly spend cap. Raise it at {app_url}/o/{org}/ci.",
                templates::dollars(cap_micros)
            ),
            Self::Concurrency => format!(
                "Waiting: {org} has {} jobs running, the most at once. \
                 This job starts when one of them ends.",
                templates::MAX_RUNNING_JOBS
            ),
        }
    }
}

/// The first binding cap, in this order: card, spend cap (spend at or
/// past the cap), concurrency.
#[must_use]
pub fn admission(standing: &Standing) -> Option<Hold> {
    if !standing.card {
        Some(Hold::Card)
    } else if spend_micros(standing) >= standing.cap_micros {
        Some(Hold::SpendCap)
    } else if standing.running >= templates::MAX_RUNNING_JOBS {
        Some(Hold::Concurrency)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(seconds: &[(&str, u64)]) -> Standing {
        let units = seconds
            .iter()
            .filter_map(|(l, s)| templates::template(l).map(|t| t.units(*s)))
            .sum();
        Standing {
            tenant_id: "t1".into(),
            org_login: "acme".into(),
            card: true,
            cap_micros: templates::DEFAULT_SPEND_CAP_MICROS,
            seconds_by_template: seconds.iter().map(|(l, s)| ((*l).into(), *s)).collect(),
            units,
            running: 0,
        }
    }

    #[test]
    fn cap_bounds() {
        assert_eq!(
            cap_micros(49),
            Err("Set a cap from $50.00 to $10000.00".to_owned())
        );
        assert_eq!(cap_micros(50), Ok(50_000_000));
        assert_eq!(cap_micros(10_000), Ok(10_000_000_000));
        assert!(cap_micros(10_001).is_err());
        assert!(cap_micros(u64::MAX).is_err());
    }

    #[test]
    fn view_from_standing() {
        let v = view(&standing(&[
            ("ubuntu-latest", 24_000),
            ("ubuntu-26.04-4core", 6_000),
        ]));
        assert_eq!(
            v,
            BillingView {
                card: true,
                seconds: 30_000,
                units: 36_000,
                spend_cents: 120,
                cap_dollars: 50,
                min_cap_dollars: 50,
                max_cap_dollars: 10_000,
                running: 0,
                max_running: 5,
            }
        );
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["seconds"], 30_000);
        assert_eq!(json["spendCents"], 120);
        assert!(json.get("freeUnits").is_none());
        assert!(json.get("minutes").is_none());
        assert_eq!(json["maxRunning"], 5);
        assert_eq!(json["minCapDollars"], 50);
        assert_eq!(json["maxCapDollars"], 10_000);
    }

    #[test]
    fn month_start_is_utc() {
        assert_eq!(month_start(1_792_676_700_000), 1_790_812_800_000);
        assert_eq!(month_start(1_790_812_800_000), 1_790_812_800_000);
        assert_eq!(month_start(1_790_812_799_999), 1_788_220_800_000);
        assert_eq!(month_start(0), 0);
    }

    #[test]
    fn spend_counts_every_unit() {
        let s = standing(&[("ubuntu-latest", 24_000), ("ubuntu-26.04-4core", 6_000)]);
        assert_eq!(units(&s), 36_000);
        assert_eq!(spend_micros(&s), 1_200_000);
        assert_eq!(spend_micros(&standing(&[("ubuntu-latest", 60)])), 2_000);
        // Recorded units win over what the catalog would compute.
        let mut s = standing(&[("no-such", 9)]);
        assert_eq!(units(&s), 0);
        s.units = 36_000;
        assert_eq!(units(&s), 36_000);
        assert_eq!(spend_micros(&s), 1_200_000);
    }

    #[test]
    fn admission_order() {
        let mut s = standing(&[("ubuntu-latest", 6_000_000)]);
        s.card = false;
        s.running = 5;
        assert_eq!(admission(&s), Some(Hold::Card));
        s.card = true;
        s.units = 1_500_000;
        assert_eq!(spend_micros(&s), templates::DEFAULT_SPEND_CAP_MICROS);
        assert_eq!(admission(&s), Some(Hold::SpendCap));
        s.units = 1_499_999;
        assert_eq!(spend_micros(&s), 49_999_966);
        assert_eq!(admission(&s), Some(Hold::Concurrency));
        s.running = 4;
        assert_eq!(admission(&s), None);
    }

    #[test]
    fn hold_messages() {
        let app = "https://app.hatchrun.io";
        assert_eq!(
            Hold::Card.message("acme", app, 50_000_000),
            "Waiting for a card. Add one for acme at https://app.hatchrun.io/o/acme/ci."
        );
        assert_eq!(
            Hold::SpendCap.message("acme", app, 50_000_000),
            "Waiting: acme reached its $50.00 monthly spend cap. Raise it at \
             https://app.hatchrun.io/o/acme/ci."
        );
        assert_eq!(
            Hold::Concurrency.message("acme", app, 50_000_000),
            "Waiting: acme has 5 jobs running, the most at once. \
             This job starts when one of them ends."
        );
        assert_eq!(Hold::Card.as_str(), "card");
        assert_eq!(Hold::SpendCap.as_str(), "spend_cap");
        assert_eq!(Hold::Concurrency.as_str(), "concurrency");
    }
}
