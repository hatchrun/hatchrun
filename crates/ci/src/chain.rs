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
use crate::intake::Repo;
use crate::run::RunInfo;
use crate::runs;
use crate::store::{self, RepoRow};

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
/// `repo` is the repository as it is now, which may differ from the copy
/// the run holds after a rename.
#[must_use]
pub fn payload(info: &RunInfo, repo: &Repo, change: &Change<'_>, chain_depth: u32) -> Value {
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
            "id": repo.id,
            "full_name": repo.full_name,
            "default_branch": repo.default_branch,
            "private": repo.private,
        },
        "sender": { "login": info.actor },
    })
}

/// The repository as it is now and the installation that reaches it,
/// from its row. `None` when the repository is gone or belongs to
/// another tenant now, which leaves the run's changes nowhere to go.
#[must_use]
pub fn current_repo(info: &RunInfo, row: Option<RepoRow>) -> Option<(Repo, u64)> {
    let row = row.filter(|row| row.tenant_id == info.tenant_id)?;
    Some((
        Repo {
            id: row.id,
            full_name: row.full_name,
            default_branch: row.default_branch,
            private: row.private,
        },
        row.installation_id,
    ))
}

/// What one change asks [`runs::create`] for.
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    /// The delivery key, which makes the runs once.
    pub delivery: String,
    /// The `workflow_run` payload the workflows see.
    pub event: Value,
    /// What workflows match their triggers against.
    pub trigger: Event,
}

/// What each of `changes` of the run `info` asks for, one more hop down
/// the chain than `parent`.
#[must_use]
pub fn children(info: &RunInfo, repo: &Repo, parent: u32, changes: &[Change<'_>]) -> Vec<Child> {
    changes
        .iter()
        .map(|change| Child {
            delivery: delivery(info, change.action),
            event: payload(info, repo, change, parent + 1),
            trigger: Event::WorkflowRun {
                action: change.action.to_owned(),
                workflow: info.workflow.clone(),
                branch: branch(info),
            },
        })
        .collect()
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
    // The run's own copy of the repository is from when it started; a
    // rename or a new default branch since then is in the row.
    let row = store::repo(db, info.repo.id).await?;
    let Some((repo, installation_id)) = current_repo(info, row) else {
        return Ok(Vec::new());
    };
    let git_ref = format!("refs/heads/{}", repo.default_branch);
    // A repository with no commit on its default branch has nothing to
    // start; asking again later would not change that.
    let Some((sha, config)) = runs::default_head(tokens, settings, installation_id, &repo).await?
    else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for child in children(info, &repo, parent, changes) {
        ids.extend(
            runs::create(
                env,
                db,
                settings,
                tokens,
                &runs::Request {
                    tenant_id: &info.tenant_id,
                    installation_id,
                    repo: &repo,
                    delivery: &child.delivery,
                    event_name: "workflow_run",
                    event: &child.event,
                    trigger: &child.trigger,
                    compile_ref: &git_ref,
                    git_ref: &git_ref,
                    sha: &sha,
                    actor: &info.actor,
                    head_ref: "",
                    base_ref: "",
                    workflow: None,
                    origin: runs::Origin::own(&info.actor),
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
            title: None,
        }
    }

    #[test]
    fn a_renamed_repository_is_read_from_its_row() {
        let run = info("refs/heads/main", "");
        let row = |tenant: &str| RepoRow {
            id: 4242,
            tenant_id: tenant.into(),
            installation_id: 10,
            full_name: "octo/gadgets".into(),
            default_branch: "trunk".into(),
            private: true,
        };
        let (repo, installation) = current_repo(&run, Some(row("t1"))).unwrap();
        assert_eq!(repo.full_name, "octo/gadgets");
        assert_eq!(repo.default_branch, "trunk");
        assert!(repo.private);
        assert_eq!(installation, 10);
        assert!(current_repo(&run, None).is_none());
        assert!(current_repo(&run, Some(row("other"))).is_none());
    }

    #[test]
    fn a_child_sees_the_repository_as_it_is_now() {
        let run = info("refs/heads/feat/x", "");
        let renamed = Repo {
            id: 4242,
            full_name: "octo/gadgets".into(),
            default_branch: "trunk".into(),
            private: true,
        };
        let changes = [
            Change {
                action: "requested",
                conclusion: None,
            },
            Change {
                action: "completed",
                conclusion: Some("failure"),
            },
        ];
        let children = children(&run, &renamed, 1, &changes);
        assert_eq!(children.len(), 2);
        let done = &children[1];
        assert_eq!(done.delivery, "workflow_run:5:1:completed");
        assert_eq!(done.event["repository"]["full_name"], "octo/gadgets");
        assert_eq!(done.event["repository"]["default_branch"], "trunk");
        assert_eq!(depth("workflow_run", &done.event), 2);
        assert_eq!(
            done.trigger,
            Event::WorkflowRun {
                action: "completed".into(),
                workflow: "ci".into(),
                branch: "feat/x".into(),
            }
        );
    }

    #[test]
    fn a_child_starts_the_workflow_that_listens_on_the_default_branch() {
        let config = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs: [t]
  deploy:
    on: { workflow_run: { workflows: [ci], types: [completed], branches: [main] } }
    jobs: [t]
  early:
    on: { workflow_run: { workflows: [ci], types: [requested] } }
    jobs: [t]
";
        let run = info("refs/heads/main", "");
        let changes = [Change {
            action: "completed",
            conclusion: Some("success"),
        }];
        let children = children(&run, &run.repo, 0, &changes);
        let selected = crate::planning::select(config, &children[0].trigger, "refs/heads/main");
        let crate::planning::Selected::Plans { plans, failed } = selected else {
            panic!("config is valid");
        };
        assert_eq!(failed.len(), 0);
        let names: Vec<&str> = plans.iter().map(|p| p.workflow.as_str()).collect();
        assert_eq!(names, ["deploy"]);
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
        let run = info("refs/heads/main", "");
        let p = payload(&run, &run.repo, &change, 1);
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
            payload(&run, &run.repo, &change, 1)["workflow_run"]["status"].clone()
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
