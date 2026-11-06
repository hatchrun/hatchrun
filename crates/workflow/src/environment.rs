// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The rule for environment names.

/// The longest environment name.
pub const ENVIRONMENT_NAME_MAX: usize = 255;

/// Ok when `name` is 1 to 255 ASCII letters, digits, `-`, `_` or `.`, and
/// is not made only of dots (`.` and `..` collapse in URL paths).
///
/// # Errors
/// The message to show the user when the name breaks the rule.
pub fn valid_environment_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Environment names cannot be empty".into());
    }
    if name.len() > ENVIRONMENT_NAME_MAX {
        return Err("Environment names are at most 255 characters".into());
    }
    let allowed = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !allowed || name.bytes().all(|b| b == b'.') {
        return Err("Use letters, digits, -, _ and . in environment names".into());
    }
    Ok(())
}

/// The longest environment URL.
pub const ENVIRONMENT_URL_MAX: usize = 2048;

/// Whether `url` can be shown as a link: an `http` or `https` address of
/// at most 2048 bytes with no whitespace or control characters. Anything
/// else, such as a `javascript:` address, is not shown.
#[must_use]
pub fn valid_environment_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    url.len() <= ENVIRONMENT_URL_MAX
        && rest.is_some_and(|rest| !rest.is_empty())
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_urls() {
        assert!(valid_environment_url(
            "https://app.example.com/pr-7?x=1#top"
        ));
        assert!(valid_environment_url("http://10.0.0.1:8080"));
        assert!(!valid_environment_url(""));
        assert!(!valid_environment_url("https://"));
        assert!(!valid_environment_url("javascript:alert(1)"));
        assert!(!valid_environment_url("ftp://example.com"));
        assert!(!valid_environment_url("https://a b"));
        assert!(!valid_environment_url("https://a\nb"));
        assert!(!valid_environment_url(&format!(
            "https://{}",
            "a".repeat(2048)
        )));
    }

    #[test]
    fn environment_names() {
        assert!(valid_environment_name("production").is_ok());
        assert!(valid_environment_name("eu-west.1_b").is_ok());
        assert!(valid_environment_name(&"a".repeat(255)).is_ok());
        assert_eq!(
            valid_environment_name(""),
            Err("Environment names cannot be empty".into())
        );
        assert_eq!(
            valid_environment_name(&"a".repeat(256)),
            Err("Environment names are at most 255 characters".into())
        );
        assert_eq!(
            valid_environment_name("prod eu"),
            Err("Use letters, digits, -, _ and . in environment names".into())
        );
        assert!(valid_environment_name("a/b").is_err());
    }

    #[test]
    fn a_dot_only_environment_name_is_refused() {
        assert!(valid_environment_name(".").is_err());
        assert!(valid_environment_name("..").is_err());
        assert!(valid_environment_name("...").is_err());
        assert!(valid_environment_name(".a").is_ok());
    }
}
