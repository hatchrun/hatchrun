// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cron expressions of `on: schedule`.

use hatchrun_workflow::{Cron, UtcMinute, parse, validate};

fn at(secs: u64) -> UtcMinute {
    UtcMinute::from_unix(secs)
}

fn matches(expr: &str, secs: u64) -> bool {
    Cron::parse(expr).unwrap().matches(&at(secs))
}

#[test]
fn from_unix_known_dates() {
    assert_eq!(
        UtcMinute::from_unix(1_792_676_700),
        UtcMinute {
            year: 2026,
            month: 10,
            day: 22,
            hour: 13,
            minute: 45,
            weekday: 4
        }
    );
    assert_eq!(
        UtcMinute::from_unix(1_835_481_540),
        UtcMinute {
            year: 2028,
            month: 2,
            day: 29,
            hour: 23,
            minute: 59,
            weekday: 2
        }
    );
    assert_eq!(
        UtcMinute::from_unix(0),
        UtcMinute {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            weekday: 4
        }
    );
}

#[test]
fn every_fifteen_minutes() {
    // 13:45 and 13:44 on 2026-10-22.
    assert!(matches("*/15 * * * *", 1_792_676_700));
    assert!(!matches("*/15 * * * *", 1_792_676_700 - 60));
}

#[test]
fn fields_take_lists_ranges_and_steps() {
    assert!(matches("5,45 13 * * *", 1_792_676_700));
    assert!(matches("40-50/5 13 * * *", 1_792_676_700));
    assert!(!matches("40-50/10 13 * * *", 1_792_676_700));
    assert!(!matches("45 14 * * *", 1_792_676_700));
    assert!(matches("45 13 22 10 *", 1_792_676_700));
    assert!(!matches("45 13 23 10 *", 1_792_676_700));
}

#[test]
fn day_fields_or_when_both_set() {
    let cron = Cron::parse("0 0 1 * MON").unwrap();
    // 2026-10-01 is a Thursday, 2026-10-05 a Monday, 2026-10-06 a Tuesday.
    let midnight = |day: u64| (20_727 + day) * 86_400;
    assert_eq!(at(midnight(0)).day, 1);
    assert!(cron.matches(&at(midnight(0))));
    assert!(cron.matches(&at(midnight(4))));
    assert!(!cron.matches(&at(midnight(5))));
    // One restricted day field alone still has to match.
    let monday = Cron::parse("0 0 * * MON").unwrap();
    assert!(!monday.matches(&at(midnight(0))));
    assert!(monday.matches(&at(midnight(4))));
}

#[test]
fn names_and_sunday_seven() {
    let named = Cron::parse("0 12 * JAN-MAR 7").unwrap();
    let numeric = Cron::parse("0 12 * 1-3 0").unwrap();
    assert_eq!(named, numeric);
    // 2027-02-07 is a Sunday.
    assert!(named.matches(&at(1_802_001_600)));
    assert_eq!(
        Cron::parse("0 0 * jan sun").unwrap(),
        Cron::parse("0 0 * 1 0").unwrap()
    );
}

#[test]
fn rejects_bad_fields() {
    for bad in [
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * 32 * *",
        "* * * 13 *",
        "* * * * 8",
        "* * *",
        "* * * * * *",
        "*/0 * * * *",
        "*/x * * * *",
        "5-1 * * * *",
        "1,,2 * * * *",
        "* * * FOO *",
        "* * * * 5/2",
        "",
    ] {
        assert!(Cron::parse(bad).is_err(), "{bad:?} parsed");
    }
}

#[test]
fn validate_reports_bad_crons() {
    let source = "
jobs: { build: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  nightly:
    on:
      schedule:
        - cron: \"0 25 * * *\"
        - cron: \"0 3 * * *\"
    jobs: [build]
";
    let config = parse(source).unwrap();
    let messages: Vec<String> = validate(&config).into_iter().map(|d| d.message).collect();
    assert_eq!(
        messages,
        [r#"invalid cron "0 25 * * *": hour 25 is out of range 0-23"#]
    );
}
