// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Five-field cron expressions for `on: schedule`, in UTC.
//!
//! Minute `0-59`, hour `0-23`, day of month `1-31`, month `1-12` or
//! `JAN-DEC`, day of week `0-7` or `SUN-SAT` (0 and 7 are Sunday). Each
//! field is `*` or a comma list of `n`, `a-b`, `*/s`, `a-b/s` and `n/s`
//! (`n-max/s`). A day
//! field that starts with `*` is unrestricted; when both day fields are
//! restricted a time matches if either day field matches, as in Vixie
//! cron.

/// A minute of the UTC calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcMinute {
    /// The year.
    pub year: i32,
    /// `1` to `12`.
    pub month: u32,
    /// `1` to `31`.
    pub day: u32,
    /// `0` to `23`.
    pub hour: u32,
    /// `0` to `59`.
    pub minute: u32,
    /// `0` is Sunday.
    pub weekday: u32,
}

impl UtcMinute {
    /// The minute that Unix time `secs` falls in.
    #[must_use]
    pub fn from_unix(secs: u64) -> UtcMinute {
        // Howard Hinnant's civil-from-days, on the days since 1970-01-01.
        let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        let of_day = secs % 86_400;
        UtcMinute {
            year: i32::try_from(year).unwrap_or(i32::MAX),
            month: u32::try_from(month).unwrap_or(0),
            day: u32::try_from(day).unwrap_or(0),
            hour: u32::try_from(of_day / 3600).unwrap_or(0),
            minute: u32::try_from(of_day / 60 % 60).unwrap_or(0),
            // 1970-01-01 was a Thursday.
            weekday: u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
        }
    }
}

/// A parsed cron expression: one bit set per allowed value of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
}

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// What one field allows.
struct Spec {
    label: &'static str,
    lo: u32,
    hi: u32,
    /// Names for `names[i]` = `lo + i`.
    names: &'static [&'static str],
}

impl Cron {
    /// Reads `expr`.
    ///
    /// # Errors
    ///
    /// The reason it is not a cron expression.
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!("expected 5 fields, found {}", fields.len()));
        };
        let minutes = field(
            minute,
            &Spec {
                label: "minute",
                lo: 0,
                hi: 59,
                names: &[],
            },
        )?;
        let hours = field(
            hour,
            &Spec {
                label: "hour",
                lo: 0,
                hi: 23,
                names: &[],
            },
        )?;
        let days = field(
            day,
            &Spec {
                label: "day of month",
                lo: 1,
                hi: 31,
                names: &[],
            },
        )?;
        let months = field(
            month,
            &Spec {
                label: "month",
                lo: 1,
                hi: 12,
                names: &MONTH_NAMES,
            },
        )?;
        let mut weekdays = field(
            weekday,
            &Spec {
                label: "day of week",
                lo: 0,
                hi: 7,
                names: &WEEKDAY_NAMES,
            },
        )?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Cron {
            minutes,
            hours: bits32(hours),
            days: bits32(days),
            months: u16::try_from(months).unwrap_or(0),
            weekdays: u8::try_from(weekdays).unwrap_or(0),
            days_restricted: !day.starts_with('*'),
            weekdays_restricted: !weekday.starts_with('*'),
        })
    }

    /// Whether the cron fires in minute `t`.
    #[must_use]
    pub fn matches(&self, t: &UtcMinute) -> bool {
        let day = self.days & (1 << t.day) != 0;
        let weekday = self.weekdays & (1 << t.weekday) != 0;
        let days = if self.days_restricted && self.weekdays_restricted {
            day || weekday
        } else {
            day && weekday
        };
        self.minutes & (1 << t.minute) != 0
            && self.hours & (1 << t.hour) != 0
            && self.months & (1 << t.month) != 0
            && days
    }
}

fn bits32(bits: u64) -> u32 {
    u32::try_from(bits).unwrap_or(0)
}

/// The bits a comma list sets.
fn field(text: &str, spec: &Spec) -> Result<u64, String> {
    let mut bits = 0u64;
    for item in text.split(',') {
        bits |= item_bits(item, spec)?;
    }
    Ok(bits)
}

fn item_bits(item: &str, spec: &Spec) -> Result<u64, String> {
    if item.is_empty() {
        return Err(format!("{} has an empty item", spec.label));
    }
    let (range, step) = match item.split_once('/') {
        Some((range, step)) => (range, Some(step)),
        None => (item, None),
    };
    let (lo, hi) = if range == "*" {
        (spec.lo, spec.hi)
    } else if let Some((a, b)) = range.split_once('-') {
        let (a, b) = (value(a, spec)?, value(b, spec)?);
        if a > b {
            return Err(format!("{} range {range} runs backwards", spec.label));
        }
        (a, b)
    } else {
        let n = value(range, spec)?;
        // `n/s` runs from n to the top of the field.
        (n, if step.is_some() { spec.hi } else { n })
    };
    let step = match step {
        None => 1,
        Some(text) => match text.parse::<u32>() {
            Ok(0) => return Err(format!("{} step must be at least 1", spec.label)),
            Ok(n) => n,
            Err(_) => return Err(format!("{} step `{text}` is not a number", spec.label)),
        },
    };
    let mut bits = 0u64;
    let mut n = lo;
    while n <= hi {
        bits |= 1 << n;
        match n.checked_add(step) {
            Some(next) => n = next,
            None => break,
        }
    }
    Ok(bits)
}

fn value(text: &str, spec: &Spec) -> Result<u32, String> {
    let n = if let Ok(n) = text.parse::<u32>() {
        n
    } else if let Some(i) = spec
        .names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(text))
    {
        spec.lo + u32::try_from(i).unwrap_or(0)
    } else {
        return Err(format!("{} `{text}` is not a value or name", spec.label));
    };
    if (spec.lo..=spec.hi).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "{} {n} is out of range {}-{}",
            spec.label, spec.lo, spec.hi
        ))
    }
}
