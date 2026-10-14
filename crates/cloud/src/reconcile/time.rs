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
