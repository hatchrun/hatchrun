// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Environments: the rules of a repository's deployment targets and how
//! the API shows them.

use serde_json::{Value, json};

use hatchrun_workflow::Filter;

use crate::store::{EnvironmentRow, JobEnvironment, JobRow};

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

/// A recorded decision on the job's current attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision<'a> {
    /// Approved by this GitHub login.
    Approved(&'a str),
    /// Rejected by this GitHub login.
    Rejected(&'a str),
}

/// The decision of `job.attempt`: `None` when there is none, or it was
/// for another attempt.
#[must_use]
pub fn decision(job: &JobRow) -> Option<Decision<'_>> {
    if job.approval_attempt != Some(job.attempt) {
        return None;
    }
    let by = job.approval_by.as_deref()?;
    match job.approval_decision.as_deref()? {
        "approved" => Some(Decision::Approved(by)),
        "rejected" => Some(Decision::Rejected(by)),
        _ => None,
    }
}

/// What the job's check run says when `login` rejected the job.
#[must_use]
pub fn rejected_message(login: &str) -> String {
    format!("Rejected by {login}")
}

/// Whether a run may use an environment with `patterns`: any run when
/// there are none; otherwise only a run whose event is not
/// `pull_request` and whose ref is `refs/heads/<branch>` with `branch`
/// passing the patterns.
#[must_use]
pub fn branch_allowed(patterns: &[String], event: &str, git_ref: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    if event == "pull_request" {
        return false;
    }
    git_ref
        .strip_prefix("refs/heads/")
        .is_some_and(|branch| Filter::new(patterns.to_vec()).matches(branch))
}

/// What the check at launch decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Go on to billing and the launch.
    Pass,
    /// End the job with this message.
    Fail(String),
    /// Hold the job for approval; the check run says this.
    Approval(String),
}

/// The check, in order: the environment exists, the run's ref is
/// allowed, the attempt was not rejected, an approval environment has an
/// approval for this attempt. `asked` is the name the job gave.
#[must_use]
pub fn gate(
    asked: &str,
    found: &JobEnvironment,
    decision: Option<Decision<'_>>,
    app_url: &str,
) -> Gate {
    let full_name = &found.full_name;
    let Some(environment) = &found.environment else {
        return Gate::Fail(format!(
            "Environment `{asked}` does not exist in {full_name}. \
             Create it at {app_url}/ci/{full_name}/settings."
        ));
    };
    if !branch_allowed(&environment.branches, &found.event, &found.git_ref) {
        return Gate::Fail(format!(
            "Environment `{}` allows only branches {}; this run is on {}.",
            environment.name,
            environment.branches.join(", "),
            found.git_ref
        ));
    }
    if let Some(Decision::Rejected(login)) = decision {
        return Gate::Fail(rejected_message(login));
    }
    if environment.approval && !matches!(decision, Some(Decision::Approved(_))) {
        let owner = full_name.split('/').next().unwrap_or(full_name);
        return Gate::Approval(format!(
            "Waiting for approval to use environment `{}`. An admin of {owner} can approve \
             or reject it at {app_url}/ci/{full_name}/runs/{}.",
            environment.name, found.run_id
        ));
    }
    Gate::Pass
}

/// What the agent's open says when the job's environment is gone.
#[must_use]
pub fn gone_message(name: &str) -> String {
    format!("Environment `{name}` was deleted before the job started.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(environment: Option<EnvironmentRow>) -> JobEnvironment {
        JobEnvironment {
            run_id: 12,
            repo_id: 7,
            full_name: "acme/app".into(),
            event: "push".into(),
            git_ref: "refs/heads/main".into(),
            environment,
        }
    }

    fn production(branches: &[&str], approval: bool) -> EnvironmentRow {
        EnvironmentRow {
            id: 3,
            repo_id: 7,
            name: "production".into(),
            branches: branches.iter().map(|b| (*b).to_owned()).collect(),
            approval,
            updated_at: 1,
        }
    }

    const APP: &str = "https://app.hatchrun.io";

    #[test]
    fn empty_patterns_allow_any_ref() {
        assert!(branch_allowed(&[], "push", "refs/tags/v1"));
        assert!(branch_allowed(&[], "pull_request", "refs/pull/3/merge"));
    }

    #[test]
    fn patterns_match_branch_names() {
        let p = vec!["main".to_owned(), "release/*".to_owned()];
        assert!(branch_allowed(&p, "push", "refs/heads/main"));
        assert!(branch_allowed(
            &p,
            "workflow_dispatch",
            "refs/heads/release/1.2"
        ));
        assert!(!branch_allowed(&p, "push", "refs/heads/feature"));
        assert!(!branch_allowed(&p, "push", "refs/tags/main"));
        assert!(!branch_allowed(&p, "pull_request", "refs/heads/main"));
    }

    #[test]
    fn gate_order_and_messages() {
        assert_eq!(
            gate("prod", &found(None), None, APP),
            Gate::Fail(
                "Environment `prod` does not exist in acme/app. \
                 Create it at https://app.hatchrun.io/ci/acme/app/settings."
                    .into()
            )
        );
        let mut elsewhere = found(Some(production(&["main"], false)));
        elsewhere.git_ref = "refs/heads/x".into();
        assert_eq!(
            gate("production", &elsewhere, None, APP),
            Gate::Fail(
                "Environment `production` allows only branches main; \
                 this run is on refs/heads/x."
                    .into()
            )
        );
        let approval = found(Some(production(&[], true)));
        assert_eq!(
            gate("production", &approval, None, APP),
            Gate::Approval(
                "Waiting for approval to use environment `production`. An admin of acme \
                 can approve or reject it at https://app.hatchrun.io/ci/acme/app/runs/12."
                    .into()
            )
        );
        assert_eq!(
            gate(
                "production",
                &approval,
                Some(Decision::Approved("octo")),
                APP
            ),
            Gate::Pass
        );
        assert_eq!(
            gate(
                "production",
                &approval,
                Some(Decision::Rejected("octo")),
                APP
            ),
            Gate::Fail("Rejected by octo".into())
        );
        assert_eq!(rejected_message("octo"), "Rejected by octo");
        // A refused ref wins over an approval.
        let mut refused = found(Some(production(&["main"], true)));
        refused.git_ref = "refs/heads/x".into();
        assert!(matches!(
            gate(
                "production",
                &refused,
                Some(Decision::Approved("octo")),
                APP
            ),
            Gate::Fail(_)
        ));
        // No approval rule: no decision is needed.
        assert_eq!(
            gate(
                "production",
                &found(Some(production(&[], false))),
                None,
                APP
            ),
            Gate::Pass
        );
    }

    #[test]
    fn a_decision_is_for_its_attempt() {
        let mut job = JobRow {
            id: 1,
            run_id: 12,
            plan_id: "deploy".into(),
            name: "deploy".into(),
            status: "queued".into(),
            conclusion: None,
            instance_id: None,
            attempt: 2,
            launch_attempts: 0,
            next_launch_at: None,
            timeout_minutes: 10,
            updated_at: 1,
            heartbeat_at: None,
            started_at: None,
            completed_at: None,
            steps: json!([]),
            log_key: None,
            template: None,
            token_permissions: json!({}),
            secret_refs: json!({}),
            held: None,
            environment: Some("production".into()),
            approval_attempt: Some(1),
            approval_decision: Some("approved".into()),
            approval_by: Some("octo".into()),
            approval_at: Some(5),
        };
        assert_eq!(decision(&job), None);
        job.approval_attempt = Some(2);
        assert_eq!(decision(&job), Some(Decision::Approved("octo")));
        job.approval_decision = Some("rejected".into());
        assert_eq!(decision(&job), Some(Decision::Rejected("octo")));
        job.approval_decision = None;
        assert_eq!(decision(&job), None);
    }

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
