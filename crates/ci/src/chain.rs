// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `on: workflow_run`: starts the workflows that listen for another
//! workflow's run to be requested, to start or to finish. GitHub sends
//! no webhook for this, so the run's object calls [`fire`] when the run
//! changes state.

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::Event;
use serde_json::{Value, json};
use worker::Result;

use crate::env::Settings;
use crate::run::RunInfo;
use crate::runs;

/// The deepest a chain of `workflow_run` runs goes, as on GitHub: a
/// workflow started by a workflow started by a workflow started by a
/// first workflow is the last; its own finish starts nothing.
pub const MAX_DEPTH: u32 = 3;

/// One state change of a run and what it carries: the `action` and, for
/// `completed`, the run's conclusion.
pub struct Change<'a> {
    /// `requested`, `in_progress` or `completed`.
    pub action: &'a str,
    /// `success`, `failure`, `cancelled` or `skipped` once completed.
    pub conclusion: Option<&'a str>,
}

/// What [`fire`] reaches the world through.
pub struct Services<'a> {
    /// The Worker's bindings.
    pub env: &'a worker::Env,
    /// Postgres.
    pub db: &'a Db,
    /// The Worker's settings.
    pub settings: &'a Settings,
    /// Installation tokens.
    pub tokens: &'a Tokens<'a, Kv, App, Counted>,
    /// The time of the change, in milliseconds.
    pub now: u64,
}

/// How many `workflow_run` hops led to a run: 0 for a run started by
/// anything else, one more than the run that started it otherwise. The
/// depth rides in the run's event payload.
#[must_use]
pub fn depth(event_name: &str, event: &Value) -> u32 {
    if event_name != "workflow_run" {
        return 0;
    }
    event
        .get("chain_depth")
        .and_then(Value::as_u64)
        .and_then(|d| u32::try_from(d).ok())
        // A payload that lost its depth must not start a chain again.
        .unwrap_or(MAX_DEPTH)
}

/// The branch a run is on, as `workflow_run.branches` filters see it:
/// the pull request's head branch, the branch or the tag name. Empty for
/// any other ref.
#[must_use]
pub fn branch(info: &RunInfo) -> String {
    if !info.head_ref.is_empty() {
        return info.head_ref.clone();
    }
    info.git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| info.git_ref.strip_prefix("refs/tags/"))
        .unwrap_or_default()
        .to_owned()
}

/// The delivery key of the runs one change starts, so a change handled
/// twice starts them once.
#[must_use]
pub fn delivery(info: &RunInfo, action: &str) -> String {
    format!("workflow_run:{}:{}:{action}", info.run_id, info.run_attempt)
}

/// The `workflow_run` event payload for a change of the run `info`.
#[must_use]
pub fn payload(info: &RunInfo, change: &Change<'_>, chain_depth: u32) -> Value {
    let status = if change.action == "completed" {
        "completed"
    } else if change.action == "requested" {
        "queued"
    } else {
        "in_progress"
    };
    json!({
        "action": change.action,
        "chain_depth": chain_depth,
        "workflow": info.workflow,
        "workflow_run": {
            "id": info.run_id,
            "name": info.workflow,
            "path": format!(".hatchrun/{}", info.workflow),
            "head_branch": branch(info),
            "head_sha": info.sha,
            "event": info.event_name,
            "status": status,
            "conclusion": change.conclusion,
            "run_number": info.run_number,
            "run_attempt": info.run_attempt,
            "actor": { "login": info.actor },
        },
        "repository": {
            "id": info.repo.id,
            "full_name": info.repo.full_name,
            "default_branch": info.repo.default_branch,
            "private": info.repo.private,
        },
        "sender": { "login": info.actor },
    })
}

/// Starts the workflows that listen for `changes` of the run `info`, on
/// the head of the default branch, and returns the new runs' ids. Starts
/// nothing for a run that is [`MAX_DEPTH`] hops down a chain. `event` is
/// the run's own event payload.
///
/// # Errors
///
/// GitHub, Postgres or a run's object failed. Handling the same change
/// again starts the runs that did not start and no others.
pub async fn fire(
    sv: &Services<'_>,
    info: &RunInfo,
    event: &Value,
    changes: &[Change<'_>],
) -> Result<Vec<u64>> {
    let Services {
        env,
        db,
        settings,
        tokens,
        now,
    } = *sv;
    let parent = depth(&info.event_name, event);
    if parent >= MAX_DEPTH || changes.is_empty() {
        return Ok(Vec::new());
    }
    let git_ref = format!("refs/heads/{}", info.repo.default_branch);
    // A repository with no commit on its default branch has nothing to
    // start; asking again later would not change that.
    let Some((sha, config)) =
        runs::default_head(tokens, settings, info.installation_id, &info.repo).await?
    else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for change in changes {
        let trigger = Event::WorkflowRun {
            action: change.action.to_owned(),
            workflow: info.workflow.clone(),
            branch: branch(info),
        };
        let event = payload(info, change, parent + 1);
        let delivery = delivery(info, change.action);
        ids.extend(
            runs::create(
                env,
                db,
                settings,
                tokens,
                &runs::Request {
                    tenant_id: &info.tenant_id,
                    installation_id: info.installation_id,
                    repo: &info.repo,
                    delivery: &delivery,
                    event_name: "workflow_run",
                    event: &event,
                    trigger: &trigger,
                    compile_ref: &git_ref,
                    git_ref: &git_ref,
                    sha: &sha,
                    actor: &info.actor,
                    head_ref: "",
                    base_ref: "",
                    workflow: None,
                },
                &config,
                now,
            )
            .await?,
        );
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::Repo;

    fn info(git_ref: &str, head_ref: &str) -> RunInfo {
        RunInfo {
            run_id: 5,
            run_number: 2,
            run_attempt: 1,
            tenant_id: "t1".into(),
            retention_days: 30,
            installation_id: 9,
            repo: Repo {
                id: 4242,
                full_name: "octo/widgets".into(),
                default_branch: "main".into(),
                private: false,
            },
            workflow: "ci".into(),
            event_name: "push".into(),
            git_ref: git_ref.into(),
            sha: "abc".into(),
            actor: "octocat".into(),
            head_ref: head_ref.into(),
            base_ref: String::new(),
            concurrency: None,
        }
    }

    #[test]
    fn depth_counts_workflow_run_hops() {
        assert_eq!(depth("push", &json!({})), 0);
        assert_eq!(depth("push", &json!({ "chain_depth": 2 })), 0);
        assert_eq!(depth("workflow_run", &json!({ "chain_depth": 1 })), 1);
        assert_eq!(depth("workflow_run", &json!({ "chain_depth": 3 })), 3);
    }

    #[test]
    fn a_workflow_run_payload_without_depth_ends_the_chain() {
        assert_eq!(depth("workflow_run", &json!({})), MAX_DEPTH);
        assert_eq!(
            depth("workflow_run", &json!({ "chain_depth": "x" })),
            MAX_DEPTH
        );
    }

    #[test]
    fn the_payload_carries_the_next_depth() {
        let change = Change {
            action: "completed",
            conclusion: Some("success"),
        };
        let p = payload(&info("refs/heads/main", ""), &change, 1);
        assert_eq!(depth("workflow_run", &p), 1);
        assert_eq!(p["action"], "completed");
        assert_eq!(p["workflow"], "ci");
        assert_eq!(p["workflow_run"]["conclusion"], "success");
        assert_eq!(p["workflow_run"]["status"], "completed");
        assert_eq!(p["workflow_run"]["head_branch"], "main");
        assert_eq!(p["repository"]["full_name"], "octo/widgets");
    }

    #[test]
    fn requested_runs_are_queued_and_started_ones_in_progress() {
        let run = info("refs/heads/main", "");
        let status = |action| {
            let change = Change {
                action,
                conclusion: None,
            };
            payload(&run, &change, 1)["workflow_run"]["status"].clone()
        };
        assert_eq!(status("requested"), "queued");
        assert_eq!(status("in_progress"), "in_progress");
    }

    #[test]
    fn the_branch_is_the_head_branch_or_the_ref_name() {
        assert_eq!(branch(&info("refs/heads/feat/x", "")), "feat/x");
        assert_eq!(branch(&info("refs/tags/v1", "")), "v1");
        assert_eq!(branch(&info("refs/pull/7/merge", "topic")), "topic");
        assert_eq!(branch(&info("refs/pull/7/merge", "")), "");
    }

    #[test]
    fn each_change_of_each_attempt_has_its_own_delivery() {
        let mut run = info("refs/heads/main", "");
        assert_eq!(delivery(&run, "completed"), "workflow_run:5:1:completed");
        assert_ne!(delivery(&run, "requested"), delivery(&run, "completed"));
        run.run_attempt = 2;
        assert_eq!(delivery(&run, "completed"), "workflow_run:5:2:completed");
    }
}
