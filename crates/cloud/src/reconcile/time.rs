// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! UTC timestamps as GitHub and EC2 write them, such as
//! `2026-10-14T10:00:00Z` and `2026-10-14T10:00:00.000Z`.

/// Unix milliseconds of an RFC 3339 timestamp in UTC (`Z` or `+00:00`),
/// with or without fractional seconds. `None` for anything else.
#[must_use]
pub fn parse(value: &str) -> Option<u64> {
    let value = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"))?;
    let (date, time) = value.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<u64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time = time.splitn(3, ':').map(str::parse::<u64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let millis = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(3)
        .fold(0, |sum, digit| sum * 10 + u64::from(digit - b'0'));
    let seconds = days(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds * 1000 + millis)
}

/// `ms` (Unix milliseconds) as `2026-10-14T10:00:00Z`, whole seconds.
#[must_use]
pub fn format(ms: u64) -> String {
    let seconds = ms / 1000;
    let (year, month, day) = civil(seconds / 86_400);
    let time = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// The date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Days from 1970-01-01 to a date, by Howard Hinnant's
/// `days_from_civil`, for years from 1970 on.
fn days(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month = (month + 9) % 12;
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_github_and_ec2_times() {
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("2026-10-14T10:00:00Z"), Some(1_791_972_000_000));
        assert_eq!(parse("2026-10-14T10:00:00.250Z"), Some(1_791_972_000_250));
        assert_eq!(
            parse("2026-10-14T10:00:00.5+00:00"),
            Some(1_791_972_000_500)
        );
        assert_eq!(parse("2024-02-29T23:59:59Z"), Some(1_709_251_199_000));
        assert_eq!(parse("2000-03-01T00:00:00Z"), Some(951_868_800_000));
    }

    #[test]
    fn writes_what_it_reads() {
        assert_eq!(format(0), "1970-01-01T00:00:00Z");
        assert_eq!(format(1_791_972_000_250), "2026-10-14T10:00:00Z");
        assert_eq!(format(1_709_251_199_000), "2024-02-29T23:59:59Z");
        assert_eq!(format(951_868_800_000), "2000-03-01T00:00:00Z");
        for ms in (0..20_000u64).map(|day| day * 86_400_000 + 3_723_000) {
            assert_eq!(parse(&format(ms)), Some(ms));
        }
    }

    #[test]
    fn refuses_other_shapes() {
        for value in [
            "",
            "2026-10-14",
            "2026-10-14T10:00:00",
            "2026-10-14T10:00:00+02:00",
            "2026-13-01T00:00:00Z",
            "2026-10-14T24:00:00Z",
            "2026-10-14T10:00:00.x1Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse(value), None, "{value}");
        }
    }
}
