// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Environments: the rules of a repository's deployment targets and how
//! the API shows them.

use serde_json::{Value, json};

use crate::store::EnvironmentRow;

/// The most environments one repository holds.
pub const MAX_PER_REPO: u32 = 100;
/// The most branch patterns of one environment.
pub const MAX_BRANCH_PATTERNS: usize = 50;
/// The longest branch pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 255;

/// Checks the branch patterns of an environment.
///
/// # Errors
///
/// The message to show the user when a rule is broken.
pub fn valid_branches(patterns: &[String]) -> Result<(), String> {
    if patterns.len() > MAX_BRANCH_PATTERNS {
        return Err(format!("Set at most {MAX_BRANCH_PATTERNS} branch patterns"));
    }
    for pattern in patterns {
        if pattern.is_empty() {
            return Err("Branch patterns cannot be empty".into());
        }
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(format!(
                "Branch patterns are at most {MAX_PATTERN_BYTES} bytes"
            ));
        }
        if pattern.chars().any(char::is_control) {
            return Err("Branch patterns cannot hold control characters".into());
        }
    }
    Ok(())
}

/// The JSON the API answers for an environment.
#[must_use]
pub fn environment_json(row: &EnvironmentRow) -> Value {
    json!({
        "name": row.name,
        "branches": row.branches,
        "approval": row.approval,
        "updatedAt": row.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_pattern_rules() {
        assert!(valid_branches(&[]).is_ok());
        assert!(
            valid_branches(&["main".into(), "release/*".into(), "!release/old".into()]).is_ok()
        );
        assert!(valid_branches(&vec!["a".to_owned(); 50]).is_ok());
        assert_eq!(
            valid_branches(&vec!["a".to_owned(); 51]),
            Err("Set at most 50 branch patterns".into())
        );
        assert_eq!(
            valid_branches(&[String::new()]),
            Err("Branch patterns cannot be empty".into())
        );
        assert!(valid_branches(&["a".repeat(255)]).is_ok());
        assert_eq!(
            valid_branches(&["a".repeat(256)]),
            Err("Branch patterns are at most 255 bytes".into())
        );
        assert_eq!(
            valid_branches(&["a\nb".into()]),
            Err("Branch patterns cannot hold control characters".into())
        );
    }

    #[test]
    fn environment_json_shape() {
        let row = EnvironmentRow {
            id: 3,
            repo_id: 7,
            name: "Production".into(),
            branches: vec!["main".into()],
            approval: true,
            updated_at: 42,
        };
        let value = environment_json(&row);
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["approval", "branches", "name", "updatedAt"]);
        assert_eq!(value["name"], "Production");
        assert_eq!(value["branches"], json!(["main"]));
        assert_eq!(value["approval"], true);
        assert_eq!(value["updatedAt"], 42);
    }
}
