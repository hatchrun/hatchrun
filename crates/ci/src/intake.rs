// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads the GitHub deliveries `hatchrun-ci` acts on.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most commits a push delivery lists. A push this long may have
/// more, so its changed paths are not known.
pub const PUSH_COMMIT_LIMIT: usize = 20;

/// The repository a delivery is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    /// GitHub's repository id, which survives renames.
    pub id: u64,
    /// `owner/name`.
    pub full_name: String,
    /// The default branch, as a short name.
    pub default_branch: String,
    /// Whether the repository is private.
    pub private: bool,
}

/// A push to a branch or tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository pushed to.
    pub repo: Repo,
    /// The full ref, such as `refs/heads/main`.
    pub r#ref: String,
    /// The commit the ref points at after the push.
    pub after: String,
    /// The login of the user who pushed.
    pub actor: String,
    /// Sorted paths touched by the pushed commits. Empty when the
    /// delivery lists no commits or [`PUSH_COMMIT_LIMIT`] of them, whose
    /// paths are partial: `paths:` filters then count the push as
    /// matching.
    pub changed: Vec<String>,
    /// The delivery body.
    pub payload: Value,
}

/// Activity on a pull request from the same repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The base repository.
    pub repo: Repo,
    /// The activity type, such as `opened`.
    pub action: String,
    /// The pull request number.
    pub number: u64,
    /// The head commit.
    pub head_sha: String,
    /// The head branch, as a short name.
    pub head_ref: String,
    /// The base branch, as a short name.
    pub base_ref: String,
    /// The login of the user behind the activity.
    pub actor: String,
    /// The delivery body.
    pub payload: Value,
}

/// A request to run a job again, from the check run's button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rerequested {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository of the check run.
    pub repo: Repo,
    /// The job named by the check run's `external_id`.
    pub job_id: u64,
}

/// What a delivery asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A push.
    Push(Push),
    /// A pull request from the same repository.
    PullRequest(PullRequest),
    /// A re-run of one job.
    Rerequested(Rerequested),
    /// Nothing to do, and why.
    Ignored(String),
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Deserialize)]
struct Sender {
    login: String,
}

#[derive(Deserialize)]
struct PushBody {
    r#ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    commits: Vec<Commit>,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct Commit {
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    removed: Vec<String>,
    #[serde(default)]
    modified: Vec<String>,
}

#[derive(Deserialize)]
struct PullRequestBody {
    action: String,
    number: u64,
    pull_request: PullRequestInfo,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct PullRequestInfo {
    head: Side,
    base: Side,
}

#[derive(Deserialize)]
struct Side {
    r#ref: String,
    #[serde(default)]
    sha: String,
    repo: Option<SideRepo>,
}

#[derive(Deserialize)]
struct SideRepo {
    id: u64,
}

#[derive(Deserialize)]
struct CheckRunBody {
    action: String,
    check_run: CheckRun,
    repository: Repo,
}

#[derive(Deserialize)]
struct CheckRun {
    #[serde(default)]
    external_id: Option<String>,
}

fn ignored(reason: &str) -> Trigger {
    Trigger::Ignored(reason.to_owned())
}

/// Reads one delivery. `event` is the `X-GitHub-Event` header and
/// `body` the raw payload.
///
/// # Errors
///
/// Returns the JSON error when the body is not the payload GitHub sends
/// for an event this crate uses.
pub fn parse(event: &str, body: &[u8]) -> Result<Trigger, serde_json::Error> {
    if !matches!(event, "push" | "pull_request" | "check_run") {
        return Ok(Trigger::Ignored(format!("{event} events are not used")));
    }
    let payload: Value = serde_json::from_slice(body)?;
    let Some(installation_id) = payload
        .get("installation")
        .filter(|v| !v.is_null())
        .map(Installation::deserialize)
        .transpose()?
        .map(|i| i.id)
    else {
        return Ok(ignored("no App installation"));
    };
    match event {
        "push" => {
            let p = PushBody::deserialize(&payload)?;
            if p.deleted {
                return Ok(ignored("ref deleted"));
            }
            // A delivery lists at most 20 commits, so with that many the
            // paths are partial. An empty list makes path filters pass.
            let changed: BTreeSet<String> = if p.commits.len() >= PUSH_COMMIT_LIMIT {
                BTreeSet::new()
            } else {
                p.commits
                    .into_iter()
                    .flat_map(|c| c.added.into_iter().chain(c.removed).chain(c.modified))
                    .collect()
            };
            Ok(Trigger::Push(Push {
                installation_id,
                repo: p.repository,
                r#ref: p.r#ref,
                after: p.after,
                actor: p.sender.login,
                changed: changed.into_iter().collect(),
                payload,
            }))
        }
        "pull_request" => {
            let p = PullRequestBody::deserialize(&payload)?;
            let same_repo = p
                .pull_request
                .head
                .repo
                .as_ref()
                .is_some_and(|r| r.id == p.repository.id);
            if !same_repo {
                return Ok(ignored("pull request from a fork"));
            }
            Ok(Trigger::PullRequest(PullRequest {
                installation_id,
                repo: p.repository,
                action: p.action,
                number: p.number,
                head_sha: p.pull_request.head.sha,
                head_ref: p.pull_request.head.r#ref,
                base_ref: p.pull_request.base.r#ref,
                actor: p.sender.login,
                payload,
            }))
        }
        _ => {
            let c = CheckRunBody::deserialize(&payload)?;
            if c.action != "rerequested" {
                return Ok(ignored("check_run action is not rerequested"));
            }
            let job_id = c
                .check_run
                .external_id
                .as_deref()
                .and_then(crate::checks::parse_external_id);
            match job_id {
                Some(job_id) => Ok(Trigger::Rerequested(Rerequested {
                    installation_id,
                    repo: c.repository,
                    job_id,
                })),
                None => Ok(ignored("check run does not belong to hatchrun-ci")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUSH: &str = include_str!("../testdata/push.json");
    const PULL_REQUEST: &str = include_str!("../testdata/pull_request.json");
    const FORK: &str = include_str!("../testdata/pull_request_fork.json");
    const RERUN: &str = include_str!("../testdata/check_run_rerequested.json");

    fn edited(body: &str, f: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut v: Value = serde_json::from_str(body).unwrap();
        f(&mut v);
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn push_lists_changed_paths() {
        let Trigger::Push(p) = parse("push", PUSH.as_bytes()).unwrap() else {
            panic!("not a push");
        };
        assert_eq!(p.changed, ["README.md", "src/a.rs"]);
        assert_eq!(p.installation_id, 777);
        assert_eq!(p.repo.id, 4242);
        assert_eq!(p.repo.full_name, "octo/widgets");
        assert!(p.repo.private);
        assert_eq!(p.r#ref, "refs/heads/main");
        assert_eq!(p.after, "2222222222222222222222222222222222222222");
        assert_eq!(p.actor, "octocat");
        assert_eq!(p.payload["ref"], "refs/heads/main");
    }

    #[test]
    fn push_without_commits_has_no_changed_paths() {
        let body = edited(PUSH, |v| v["commits"] = serde_json::json!([]));
        let Trigger::Push(p) = parse("push", &body).unwrap() else {
            panic!("not a push");
        };
        assert_eq!(p.changed, Vec::<String>::new());
    }

    #[test]
    fn push_with_twenty_commits_has_unknown_changed_paths() {
        let commit = |n: usize| serde_json::json!({ "modified": [format!("f{n}.rs")] });
        for (count, known) in [(19, true), (20, false), (25, false)] {
            let body = edited(PUSH, |v| {
                v["commits"] = (0..count).map(commit).collect();
            });
            let Trigger::Push(p) = parse("push", &body).unwrap() else {
                panic!("not a push");
            };
            assert_eq!(!p.changed.is_empty(), known, "{count} commits");
        }
    }

    #[test]
    fn deleted_ref_is_ignored() {
        let body = edited(PUSH, |v| v["deleted"] = true.into());
        assert_eq!(
            parse("push", &body).unwrap(),
            Trigger::Ignored("ref deleted".into())
        );
    }

    #[test]
    fn pull_request_from_same_repo_is_read() {
        let Trigger::PullRequest(p) = parse("pull_request", PULL_REQUEST.as_bytes()).unwrap()
        else {
            panic!("not a pull request");
        };
        assert_eq!(p.action, "opened");
        assert_eq!(p.number, 7);
        assert_eq!(p.head_sha, "3333333333333333333333333333333333333333");
        assert_eq!(p.head_ref, "feature");
        assert_eq!(p.base_ref, "main");
        assert_eq!(p.actor, "octocat");
        assert_eq!(p.repo.id, 4242);
        assert!(!p.repo.private);
    }

    #[test]
    fn fork_pull_request_is_ignored() {
        assert_eq!(
            parse("pull_request", FORK.as_bytes()).unwrap(),
            Trigger::Ignored("pull request from a fork".into())
        );
    }

    #[test]
    fn fork_with_deleted_head_repo_is_ignored() {
        let body = edited(PULL_REQUEST, |v| {
            v["pull_request"]["head"]["repo"] = Value::Null;
        });
        assert_eq!(
            parse("pull_request", &body).unwrap(),
            Trigger::Ignored("pull request from a fork".into())
        );
    }

    #[test]
    fn rerequested_names_the_job() {
        let Trigger::Rerequested(r) = parse("check_run", RERUN.as_bytes()).unwrap() else {
            panic!("not a rerequest");
        };
        assert_eq!(r.job_id, 42);
        assert_eq!(r.installation_id, 777);
        assert_eq!(r.repo.full_name, "octo/widgets");
    }

    #[test]
    fn foreign_check_run_is_ignored() {
        for external_id in [
            Value::from("other"),
            Value::from("hatchrun-ci:x"),
            Value::Null,
        ] {
            let body = edited(RERUN, |v| v["check_run"]["external_id"] = external_id);
            assert!(matches!(
                parse("check_run", &body).unwrap(),
                Trigger::Ignored(_)
            ));
        }
    }

    #[test]
    fn other_check_run_actions_are_ignored() {
        let body = edited(RERUN, |v| v["action"] = "completed".into());
        assert!(matches!(
            parse("check_run", &body).unwrap(),
            Trigger::Ignored(_)
        ));
    }

    #[test]
    fn payload_without_installation_is_ignored() {
        for event in ["push", "pull_request", "check_run"] {
            let body = match event {
                "push" => PUSH,
                "pull_request" => PULL_REQUEST,
                _ => RERUN,
            };
            let body = edited(body, |v| {
                v.as_object_mut().unwrap().remove("installation");
            });
            assert_eq!(
                parse(event, &body).unwrap(),
                Trigger::Ignored("no App installation".into())
            );
        }
    }

    #[test]
    fn other_events_are_ignored() {
        assert_eq!(
            parse("issues", b"{}").unwrap(),
            Trigger::Ignored("issues events are not used".into())
        );
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(parse("push", b"not json").is_err());
        assert!(parse("push", b"{\"installation\":{\"id\":1}}").is_err());
    }
}
