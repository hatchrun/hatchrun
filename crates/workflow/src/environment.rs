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

#[cfg(test)]
mod tests {
    use super::*;

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
