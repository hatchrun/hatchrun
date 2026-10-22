// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a tenant has used this month and what that costs, from the
//! [`Standing`] the store reads. The prices live in
//! `hatchrun_workflow::templates`.

use hatchrun_workflow::templates;

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

/// Billing units used this month: the sum of each template's units.
/// A label that names no template counts nothing.
#[must_use]
pub fn units(standing: &Standing) -> u64 {
    standing
        .minutes_by_template
        .iter()
        .filter_map(|(label, minutes)| templates::template(label).map(|t| t.units(*minutes)))
        .sum()
}

/// What this month's usage costs past the free units, in micros.
#[must_use]
pub fn spend_micros(standing: &Standing) -> u64 {
    templates::spend_micros(units(standing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(minutes: &[(&str, u64)]) -> Standing {
        Standing {
            tenant_id: "t1".into(),
            org_login: "acme".into(),
            card: true,
            cap_micros: templates::DEFAULT_SPEND_CAP_MICROS,
            minutes_by_template: minutes.iter().map(|(l, m)| ((*l).into(), *m)).collect(),
            running: 0,
        }
    }

    #[test]
    fn month_start_is_utc() {
        assert_eq!(month_start(1_792_676_700_000), 1_790_812_800_000);
        assert_eq!(month_start(1_790_812_800_000), 1_790_812_800_000);
        assert_eq!(month_start(1_790_812_799_999), 1_788_220_800_000);
        assert_eq!(month_start(0), 0);
    }

    #[test]
    fn spend_counts_after_free_units() {
        let s = standing(&[("ubuntu-latest", 400), ("ubuntu-26.04-4core", 100)]);
        assert_eq!(units(&s), 600);
        assert_eq!(spend_micros(&s), 200_000);
        assert_eq!(spend_micros(&standing(&[("ubuntu-latest", 500)])), 0);
        assert_eq!(units(&standing(&[("no-such", 9)])), 0);
    }
}
