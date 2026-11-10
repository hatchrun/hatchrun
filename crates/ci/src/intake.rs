// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads the GitHub deliveries `hatchrun-ci` acts on.

use std::collections::BTreeSet;

use hatchrun_workflow::{MERGE_GROUP_TYPES, RELEASE_TYPES};
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

/// Where the head of a pull request from another repository is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkInfo {
    /// The head repository's id; `None` for a deleted fork.
    pub head_repo_id: Option<u64>,
    /// The head repository's `owner/name`; `None` for a deleted fork.
    pub head_repo_name: Option<String>,
}

/// Activity on a pull request, from the same repository or a fork.
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
    /// The pull request's title.
    pub title: String,
    /// The login of the user who opened it.
    pub author: String,
    /// GitHub's `author_association` of the user who opened it, such as
    /// `FIRST_TIME_CONTRIBUTOR`; empty when the delivery has none.
    pub author_association: String,
    /// Where the head is when it is not in the base repository; `None`
    /// for a pull request from the same repository.
    pub fork: Option<ForkInfo>,
    /// `open`, `closed` or `merged`, as the delivery says it stands.
    pub state: &'static str,
    /// When GitHub last changed the pull request, in Unix milliseconds:
    /// the order of its deliveries.
    pub updated_at: u64,
    /// The delivery body.
    pub payload: Value,
}

/// Activity on a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository of the release.
    pub repo: Repo,
    /// The activity type, such as `published`.
    pub action: String,
    /// The release's tag, as a short name.
    pub tag: String,
    /// The login of the user behind the activity.
    pub actor: String,
    /// The delivery body.
    pub payload: Value,
}

/// A branch or tag that was created or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository.
    pub repo: Repo,
    /// The full ref, `refs/heads/<name>` or `refs/tags/<name>`.
    pub r#ref: String,
    /// The login of the user who made the change.
    pub actor: String,
    /// The delivery body.
    pub payload: Value,
}

/// A pull request entering the merge queue asks for its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeGroup {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository.
    pub repo: Repo,
    /// The activity type, `checks_requested`.
    pub action: String,
    /// The merge group's commit, which the queue reads checks on.
    pub head_sha: String,
    /// The full ref of the merge group, `refs/heads/gh-readonly-queue/...`.
    pub head_ref: String,
    /// The login of the user behind the entry.
    pub actor: String,
    /// The delivery body.
    pub payload: Value,
}

/// A `repository_dispatch` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryDispatch {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository.
    pub repo: Repo,
    /// The request's `event_type`.
    pub action: String,
    /// The login of the user behind the request.
    pub actor: String,
    /// The delivery body, with the request's `client_payload`.
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

/// A click on the approval check run's button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveFork {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository of the check run.
    pub repo: Repo,
    /// The check run that carried the button.
    pub check_run_id: u64,
    /// The GitHub login of whoever clicked.
    pub sender: String,
}

/// What a delivery asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A push.
    Push(Push),
    /// A pull request from the same repository.
    PullRequest(PullRequest),
    /// Activity on a release.
    Release(Release),
    /// A branch or tag was created.
    Create(RefChange),
    /// A branch or tag was deleted.
    Delete(RefChange),
    /// A merge queue entry asks for its checks.
    MergeGroup(MergeGroup),
    /// A repository dispatch request.
    RepositoryDispatch(RepositoryDispatch),
    /// A re-run of one job.
    Rerequested(Rerequested),
    /// A maintainer's click on "Approve and run".
    ApproveFork(ApproveFork),
    /// A repository changed visibility, name, owner or default branch.
    Repository(RepositoryChange),
    /// Repositories were added to or removed from an installation. The
    /// payload lists no default branches, so the delivery only says to
    /// read the installation again.
    InstallationRepositories {
        /// The installation that changed.
        installation_id: u64,
    },
    /// Nothing to do, and why.
    Ignored(String),
}

/// A `repository` event that changes what CI records of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryChange {
    /// The App installation the delivery came through.
    pub installation_id: u64,
    /// The repository as it is now.
    pub repo: Repo,
}

/// The `repository` actions that change a field of [`Repo`].
const REPOSITORY_ACTIONS: [&str; 5] = [
    "publicized",
    "privatized",
    "renamed",
    "transferred",
    "edited",
];

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
    #[serde(default)]
    title: String,
    user: Option<Sender>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    author_association: String,
    #[serde(deserialize_with = "rfc3339_ms")]
    updated_at: u64,
}

/// Unix milliseconds of the RFC 3339 time GitHub wrote.
fn rfc3339_ms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let text = String::deserialize(d)?;
    hatchrun_cloud::parse_rfc3339(&text)
        .ok_or_else(|| serde::de::Error::custom(format!("bad time {text:?}")))
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
    #[serde(default)]
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct ReleaseBody {
    action: String,
    release: ReleaseInfo,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

#[derive(Deserialize)]
struct MergeGroupBody {
    action: String,
    merge_group: MergeGroupInfo,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct MergeGroupInfo {
    head_sha: String,
    head_ref: String,
}

#[derive(Deserialize)]
struct DispatchBody {
    /// The request's `event_type`.
    action: String,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct RefBody {
    r#ref: String,
    ref_type: String,
    sender: Sender,
    repository: Repo,
}

#[derive(Deserialize)]
struct RepositoryBody {
    action: String,
    repository: Repo,
}

#[derive(Deserialize)]
struct CheckRunBody {
    action: String,
    check_run: CheckRun,
    repository: Repo,
    #[serde(default)]
    requested_action: Option<RequestedAction>,
    #[serde(default)]
    sender: Option<Sender>,
}

#[derive(Deserialize)]
struct CheckRun {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    external_id: Option<String>,
}

#[derive(Deserialize)]
struct RequestedAction {
    identifier: String,
}

fn ignored(reason: &str) -> Trigger {
    Trigger::Ignored(reason.to_owned())
}

fn release(installation_id: u64, payload: Value) -> Result<Trigger, serde_json::Error> {
    let r = ReleaseBody::deserialize(&payload)?;
    if !RELEASE_TYPES.contains(&r.action.as_str()) {
        return Ok(ignored("release action is not used"));
    }
    Ok(Trigger::Release(Release {
        installation_id,
        repo: r.repository,
        action: r.action,
        tag: r.release.tag_name,
        actor: r.sender.login,
        payload,
    }))
}

fn merge_group(installation_id: u64, payload: Value) -> Result<Trigger, serde_json::Error> {
    let g = MergeGroupBody::deserialize(&payload)?;
    if !MERGE_GROUP_TYPES.contains(&g.action.as_str()) {
        return Ok(ignored("merge_group action is not used"));
    }
    Ok(Trigger::MergeGroup(MergeGroup {
        installation_id,
        repo: g.repository,
        action: g.action,
        head_sha: g.merge_group.head_sha,
        head_ref: g.merge_group.head_ref,
        actor: g.sender.login,
        payload,
    }))
}

fn repository_dispatch(installation_id: u64, payload: Value) -> Result<Trigger, serde_json::Error> {
    let d = DispatchBody::deserialize(&payload)?;
    Ok(Trigger::RepositoryDispatch(RepositoryDispatch {
        installation_id,
        repo: d.repository,
        action: d.action,
        actor: d.sender.login,
        payload,
    }))
}

fn ref_change(
    created: bool,
    installation_id: u64,
    payload: Value,
) -> Result<Trigger, serde_json::Error> {
    let r = RefBody::deserialize(&payload)?;
    let prefix = match r.ref_type.as_str() {
        "branch" => "refs/heads/",
        "tag" => "refs/tags/",
        _ => return Ok(ignored("not a branch or a tag")),
    };
    let change = RefChange {
        installation_id,
        repo: r.repository,
        r#ref: format!("{prefix}{}", r.r#ref),
        actor: r.sender.login,
        payload,
    };
    Ok(if created {
        Trigger::Create(change)
    } else {
        Trigger::Delete(change)
    })
}

/// Reads one delivery. `event` is the `X-GitHub-Event` header and
/// `body` the raw payload.
///
/// # Errors
///
/// Returns the JSON error when the body is not the payload GitHub sends
/// for an event this crate uses.
#[allow(clippy::too_many_lines)] // One arm per event.
pub fn parse(event: &str, body: &[u8]) -> Result<Trigger, serde_json::Error> {
    if !matches!(
        event,
        "push"
            | "pull_request"
            | "check_run"
            | "repository"
            | "release"
            | "create"
            | "delete"
            | "merge_group"
            | "repository_dispatch"
            | "installation_repositories"
    ) {
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
        "installation_repositories" => Ok(Trigger::InstallationRepositories { installation_id }),
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
            // A head repository that is not the base, or is gone (a
            // deleted fork), makes a fork pull request.
            let fork = match &p.pull_request.head.repo {
                Some(head) if head.id == p.repository.id => None,
                Some(head) => Some(ForkInfo {
                    head_repo_id: Some(head.id),
                    head_repo_name: head.full_name.clone(),
                }),
                None => Some(ForkInfo {
                    head_repo_id: None,
                    head_repo_name: None,
                }),
            };
            Ok(Trigger::PullRequest(PullRequest {
                installation_id,
                repo: p.repository,
                action: p.action,
                number: p.number,
                head_sha: p.pull_request.head.sha,
                head_ref: p.pull_request.head.r#ref,
                base_ref: p.pull_request.base.r#ref,
                author: p
                    .pull_request
                    .user
                    .map_or_else(|| p.sender.login.clone(), |user| user.login),
                actor: p.sender.login,
                title: p.pull_request.title,
                author_association: p.pull_request.author_association,
                fork,
                updated_at: p.pull_request.updated_at,
                state: if p.pull_request.merged {
                    "merged"
                } else if p.pull_request.state == "closed" {
                    "closed"
                } else {
                    "open"
                },
                payload,
            }))
        }
        "release" => release(installation_id, payload),
        "merge_group" => merge_group(installation_id, payload),
        "repository_dispatch" => repository_dispatch(installation_id, payload),
        "create" | "delete" => ref_change(event == "create", installation_id, payload),
        "repository" => {
            let r = RepositoryBody::deserialize(&payload)?;
            if !REPOSITORY_ACTIONS.contains(&r.action.as_str()) {
                return Ok(ignored("repository action changes nothing recorded"));
            }
            Ok(Trigger::Repository(RepositoryChange {
                installation_id,
                repo: r.repository,
            }))
        }
        _ => {
            let c = CheckRunBody::deserialize(&payload)?;
            if c.action == "requested_action" {
                let clicked = c
                    .requested_action
                    .as_ref()
                    .is_some_and(|a| a.identifier == crate::policy::APPROVE_ACTION);
                return Ok(match c.sender {
                    Some(sender) if clicked && c.check_run.id != 0 => {
                        Trigger::ApproveFork(ApproveFork {
                            installation_id,
                            repo: c.repository,
                            check_run_id: c.check_run.id,
                            sender: sender.login,
                        })
                    }
                    _ => ignored("check_run action is not one hatchrun-ci offers"),
                });
            }
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
    use crate::trust::Trust;

    const PUSH: &str = include_str!("../testdata/push.json");
    const PULL_REQUEST: &str = include_str!("../testdata/pull_request.json");
    const FORK: &str = include_str!("../testdata/pull_request_fork.json");
    const RERUN: &str = include_str!("../testdata/check_run_rerequested.json");
    const RELEASE: &str = include_str!("../testdata/release.json");
    const CREATE: &str = include_str!("../testdata/create.json");
    const MERGE_GROUP: &str = include_str!("../testdata/merge_group.json");
    const DISPATCH: &str = include_str!("../testdata/repository_dispatch.json");

    fn edited(body: &str, f: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut v: Value = serde_json::from_str(body).unwrap();
        f(&mut v);
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn installation_repository_changes_ask_for_a_reread() {
        let body = br#"{"action":"added","installation":{"id":55},"repositories_added":[]}"#;
        assert_eq!(
            parse("installation_repositories", body).unwrap(),
            Trigger::InstallationRepositories {
                installation_id: 55
            }
        );
        assert!(matches!(
            parse("installation_repositories", br#"{"action":"added"}"#).unwrap(),
            Trigger::Ignored(_)
        ));
    }

    #[test]
    fn repository_events_that_change_the_record_are_read() {
        let body = |action: &str, private: bool| {
            edited(PUSH, |v| {
                v.as_object_mut().unwrap().remove("commits");
                v["action"] = action.into();
                v["repository"]["private"] = private.into();
            })
        };
        let Trigger::Repository(c) = parse("repository", &body("publicized", false)).unwrap()
        else {
            panic!("not a repository change");
        };
        assert_eq!(
            (c.installation_id, c.repo.id, c.repo.private),
            (777, 4242, false)
        );
        for action in ["privatized", "renamed", "transferred", "edited"] {
            assert!(matches!(
                parse("repository", &body(action, true)).unwrap(),
                Trigger::Repository(_)
            ));
        }
        for action in ["created", "deleted", "archived"] {
            assert!(matches!(
                parse("repository", &body(action, true)).unwrap(),
                Trigger::Ignored(_)
            ));
        }
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
        assert_eq!(p.updated_at, 1_793_956_919_000);
    }

    #[test]
    fn pull_request_state_and_text_are_read() {
        let read = |f: fn(&mut Value)| {
            let Trigger::PullRequest(p) = parse("pull_request", &edited(PULL_REQUEST, f)).unwrap()
            else {
                panic!("not a pull request");
            };
            p
        };
        let open = read(|v| {
            v["pull_request"]["title"] = "Fix it".into();
            v["pull_request"]["user"] = serde_json::json!({ "login": "author" });
            v["pull_request"]["state"] = "open".into();
        });
        assert_eq!(open.title, "Fix it");
        assert_eq!(open.author, "author");
        assert_eq!(open.state, "open");
        let closed = read(|v| v["pull_request"]["state"] = "closed".into());
        assert_eq!(closed.state, "closed");
        // Without a user the author is the sender.
        assert_eq!(closed.author, "octocat");
        let merged = read(|v| {
            v["pull_request"]["state"] = "closed".into();
            v["pull_request"]["merged"] = true.into();
        });
        assert_eq!(merged.state, "merged");
    }

    #[test]
    fn a_pull_request_from_the_same_repo_is_not_a_fork() {
        let Trigger::PullRequest(p) = parse("pull_request", PULL_REQUEST.as_bytes()).unwrap()
        else {
            panic!("not a pull request");
        };
        assert_eq!(p.fork, None);
        assert_eq!(p.author_association, "");
        assert_eq!(Trust::of(&p), Trust::Trusted);
    }

    #[test]
    fn a_pull_request_from_a_fork_is_read_with_its_head_and_association() {
        let Trigger::PullRequest(p) = parse("pull_request", FORK.as_bytes()).unwrap() else {
            panic!("not a pull request");
        };
        assert_eq!(
            p.fork,
            Some(ForkInfo {
                head_repo_id: Some(9999),
                head_repo_name: Some("mallory/widgets".into()),
            })
        );
        assert_eq!(p.author, "mallory");
        assert_eq!(p.author_association, "FIRST_TIME_CONTRIBUTOR");
        assert_eq!(p.head_sha, "3333333333333333333333333333333333333333");
        assert_eq!(p.repo.id, 4242, "the base repository is the one that runs");
        assert_eq!(Trust::of(&p), Trust::Fork);
    }

    #[test]
    fn a_deleted_fork_is_a_fork_with_no_name() {
        let body = edited(PULL_REQUEST, |v| {
            v["pull_request"]["head"]["repo"] = Value::Null;
        });
        let Trigger::PullRequest(p) = parse("pull_request", &body).unwrap() else {
            panic!("not a pull request");
        };
        assert_eq!(
            p.fork,
            Some(ForkInfo {
                head_repo_id: None,
                head_repo_name: None,
            })
        );
        assert_eq!(Trust::of(&p), Trust::Fork);
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
    fn the_approve_button_is_read_with_who_clicked() {
        let body = edited(RERUN, |v| {
            v["action"] = "requested_action".into();
            v["check_run"]["id"] = 5150.into();
            v["requested_action"] = serde_json::json!({ "identifier": "approve-fork-run" });
            v["sender"] = serde_json::json!({ "login": "maintainer" });
        });
        let Trigger::ApproveFork(a) = parse("check_run", &body).unwrap() else {
            panic!("not an approval");
        };
        assert_eq!(a.check_run_id, 5150);
        assert_eq!(a.sender, "maintainer");
        assert_eq!(a.installation_id, 777);
        assert_eq!(a.repo.full_name, "octo/widgets");
    }

    #[test]
    fn another_button_or_no_sender_approves_nothing() {
        let click = |identifier: &str, sender: bool| {
            edited(RERUN, |v| {
                v["action"] = "requested_action".into();
                v["check_run"]["id"] = 5150.into();
                v["requested_action"] = serde_json::json!({ "identifier": identifier });
                if sender {
                    v["sender"] = serde_json::json!({ "login": "maintainer" });
                } else {
                    v.as_object_mut().unwrap().remove("sender");
                }
            })
        };
        for body in [click("other", true), click("approve-fork-run", false)] {
            assert!(matches!(
                parse("check_run", &body).unwrap(),
                Trigger::Ignored(_)
            ));
        }
        let no_action = edited(RERUN, |v| {
            v["action"] = "requested_action".into();
            v["sender"] = serde_json::json!({ "login": "maintainer" });
        });
        assert!(matches!(
            parse("check_run", &no_action).unwrap(),
            Trigger::Ignored(_)
        ));
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
        for event in [
            "push",
            "pull_request",
            "check_run",
            "release",
            "create",
            "delete",
            "merge_group",
            "repository_dispatch",
        ] {
            let body = match event {
                "push" => PUSH,
                "pull_request" => PULL_REQUEST,
                "merge_group" => MERGE_GROUP,
                "repository_dispatch" => DISPATCH,
                "release" => RELEASE,
                "create" | "delete" => CREATE,
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
    fn release_is_read() {
        let Trigger::Release(r) = parse("release", RELEASE.as_bytes()).unwrap() else {
            panic!("not a release");
        };
        assert_eq!(r.action, "published");
        assert_eq!(r.tag, "v1.2.0");
        assert_eq!(r.actor, "octocat");
        assert_eq!(r.installation_id, 777);
        assert_eq!(r.repo.full_name, "octo/widgets");
    }

    #[test]
    fn every_release_type_is_read_and_others_are_ignored() {
        for action in RELEASE_TYPES {
            let body = edited(RELEASE, |v| v["action"] = (*action).into());
            assert!(matches!(
                parse("release", &body).unwrap(),
                Trigger::Release(_)
            ));
        }
        let body = edited(RELEASE, |v| v["action"] = "changed".into());
        assert_eq!(
            parse("release", &body).unwrap(),
            Trigger::Ignored("release action is not used".into())
        );
    }

    #[test]
    fn created_and_deleted_refs_are_read() {
        let Trigger::Create(c) = parse("create", CREATE.as_bytes()).unwrap() else {
            panic!("not a create");
        };
        assert_eq!(c.r#ref, "refs/tags/v1.2.0");
        assert_eq!(c.actor, "octocat");
        let body = edited(CREATE, |v| {
            v["ref"] = "topic/x".into();
            v["ref_type"] = "branch".into();
        });
        let Trigger::Delete(d) = parse("delete", &body).unwrap() else {
            panic!("not a delete");
        };
        assert_eq!(d.r#ref, "refs/heads/topic/x");
    }

    #[test]
    fn a_merge_group_is_read_by_its_head() {
        let Trigger::MergeGroup(g) = parse("merge_group", MERGE_GROUP.as_bytes()).unwrap() else {
            panic!("not a merge group");
        };
        assert_eq!(g.action, "checks_requested");
        assert_eq!(g.head_sha, "6d2f1c0e9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d");
        assert!(
            g.head_ref
                .starts_with("refs/heads/gh-readonly-queue/main/pr-5-")
        );
        assert_eq!(g.actor, "octocat");
        assert_eq!(g.installation_id, 777);
        assert_eq!(g.repo.full_name, "octo/widgets");
    }

    #[test]
    fn a_destroyed_merge_group_is_ignored() {
        let body = edited(MERGE_GROUP, |v| v["action"] = "destroyed".into());
        assert_eq!(
            parse("merge_group", &body).unwrap(),
            Trigger::Ignored("merge_group action is not used".into())
        );
    }

    #[test]
    fn a_repository_dispatch_keeps_its_type_and_client_payload() {
        let Trigger::RepositoryDispatch(d) =
            parse("repository_dispatch", DISPATCH.as_bytes()).unwrap()
        else {
            panic!("not a dispatch");
        };
        assert_eq!(d.action, "deploy");
        assert_eq!(d.actor, "octocat");
        assert_eq!(d.repo.default_branch, "main");
        assert_eq!(d.payload["client_payload"]["target"], "staging");
    }

    #[test]
    fn create_of_something_else_is_ignored() {
        let body = edited(CREATE, |v| v["ref_type"] = "repository".into());
        assert_eq!(
            parse("create", &body).unwrap(),
            Trigger::Ignored("not a branch or a tag".into())
        );
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
