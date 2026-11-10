// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The decisions of `ChatOps` that need no I/O: who may run a command,
//! what the platform says when a command cannot run, the `command` part
//! of the run's event payload, the cap on runs per issue and the
//! reactions. The webhook does the calls and acts on what these say.

use hatchrun_workflow::command::Args;
use hatchrun_workflow::{CommandOptions, CommandPermission};
use serde_json::{Value, json};

use crate::github::PullInfo;
use crate::policy::APPROVAL_EXPIRES_MS;
use crate::store::{GateStatus, RunRow};

/// The most runs of the `issue_comment` event that are not over for one
/// issue or pull request. A comment over the cap starts nothing.
pub const MAX_ACTIVE_PER_ISSUE: u64 = 5;

/// The reaction left when a command delivery is accepted.
pub const ACCEPTED: &str = "eyes";

/// The reply to a `--sha=` on a plain issue.
pub const NO_HEAD_COMMIT: &str = "Issues have no head commit";

/// The reply to `/approve-run` on a pull request that is not open.
pub const CLOSED_PULL_REQUEST: &str = "The pull request is closed";

/// The reply to `/approve-run` on a pull request of this repository,
/// which runs without an approval.
pub const NOTHING_TO_APPROVE: &str =
    "This pull request is from this repository, so there is nothing to approve";

/// The reply when approving started no run: no workflow listens to the
/// pull request.
pub const NOTHING_TO_RUN: &str = "No workflow runs on this pull request";

/// The reply when the repository has too many fork runs waiting to take
/// another.
pub const TOO_MANY_WAITING: &str = "Too many fork runs wait for approval";

/// How many more runs a comment may start on an issue that already has
/// `active` runs that are not over.
#[must_use]
pub fn room(active: u64) -> u64 {
    MAX_ACTIVE_PER_ISSUE.saturating_sub(active)
}

/// Whether a commenter who holds `role` (`None` for no access) may run a
/// command that needs `needed`. The roles run read, triage, write,
/// maintain, admin, lowest first.
#[must_use]
pub fn permits(role: Option<CommandPermission>, needed: CommandPermission) -> bool {
    role.is_some_and(|role| role >= needed)
}

/// The reply to a command that needs the head sha and has none: it
/// quotes the line as typed so the maintainer can send it again with the
/// pin.
#[must_use]
pub fn sha_prompt(head: &str, line: &str) -> String {
    format!("Add --sha={head} to: {line}")
}

/// The reply to a command whose pin is not the pull request's head any
/// more: it names the head and quotes the line again.
#[must_use]
pub fn stale_pin(head: &str, line: &str) -> String {
    format!("The pull request head is now {head}, so nothing ran. Use --sha={head} in: {line}")
}

/// What `/approve-run` does for the runs of a pull request at its head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Some run waits: approve what waits.
    Approve,
    /// A run is going or has gone well already: nothing to do.
    Nothing,
    /// There is no run, or only ones that ended without an approval or
    /// with a failure: make a new one.
    Create,
}

/// Where one run stands for `/approve-run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waits for a decision and can still be approved.
    Waiting,
    /// Runs, or ended well: it needs nothing.
    Settled,
    /// Rejected, superseded, expired, failed or cancelled.
    Ended,
}

fn stage(run: &RunRow, now: u64) -> Stage {
    let completed = run.status == "completed";
    match run.gate_status {
        GateStatus::Pending | GateStatus::Deferred if completed => Stage::Ended,
        // Past its deadline, the reconciler has not expired it yet but
        // an approval is refused: it is as good as expired.
        GateStatus::Pending | GateStatus::Deferred
            if run.created_at < now.saturating_sub(APPROVAL_EXPIRES_MS) =>
        {
            Stage::Ended
        }
        GateStatus::Pending | GateStatus::Deferred => Stage::Waiting,
        GateStatus::Expired | GateStatus::Cancelled => Stage::Ended,
        GateStatus::NotGated | GateStatus::Approved => match run.conclusion.as_deref() {
            _ if !completed => Stage::Settled,
            Some("success" | "skipped") => Stage::Settled,
            _ => Stage::Ended,
        },
    }
}

/// What `/approve-run` does, given the runs of the pull request at the
/// head the comment names, at time `now`. An earlier approval does not
/// carry over to a run that ended, and an ended run is never revived.
#[must_use]
pub fn approval(runs: &[RunRow], now: u64) -> Approval {
    let stages: Vec<Stage> = runs.iter().map(|run| stage(run, now)).collect();
    if stages.contains(&Stage::Waiting) {
        Approval::Approve
    } else if stages.contains(&Stage::Settled) {
        Approval::Nothing
    } else {
        Approval::Create
    }
}

/// What the head sha rule says about a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// The command runs. The commit is the pull request's head, or
    /// `None` on a plain issue.
    Run(Option<String>),
    /// The command starts nothing and the platform posts this reply.
    Reply(String),
}

/// Applies the sha rule of `options` to `args`, the words of `line`.
/// `pull` is the pull request the comment is on, `None` on a plain issue.
/// A sha is compared with the pull request's current head as full
/// strings.
#[must_use]
pub fn pin(options: &CommandOptions, args: &Args, line: &str, pull: Option<&PullInfo>) -> Pin {
    let Some(pull) = pull else {
        return match args.sha {
            Some(_) => Pin::Reply(NO_HEAD_COMMIT.to_owned()),
            None => Pin::Run(None),
        };
    };
    match args.sha.as_deref() {
        None if options.sha_required(pull.fork) => Pin::Reply(sha_prompt(&pull.head_sha, line)),
        Some(sha) if sha != pull.head_sha => Pin::Reply(stale_pin(&pull.head_sha, line)),
        _ => Pin::Run(Some(pull.head_sha.clone())),
    }
}

/// The sha rule of the built-in `/approve-run`: the pin is always
/// required.
#[must_use]
pub fn approve_pin(args: &Args, line: &str, pull: Option<&PullInfo>) -> Pin {
    let options = CommandOptions {
        sha: hatchrun_workflow::ShaPolicy::Required,
        ..CommandOptions::default()
    };
    pin(&options, args, line, pull)
}

/// What a command run knows of its command: `github.event.command`.
#[derive(Debug, Clone, Copy)]
pub struct CommandContext<'a> {
    /// The command's name, without the slash.
    pub name: &'a str,
    /// The line as typed.
    pub line: &'a str,
    /// The argument words.
    pub words: &'a [String],
    /// The names the workflow gave the words.
    pub names: &'a [String],
    /// The commenter's login.
    pub actor: &'a str,
    /// The commenter's role on the repository.
    pub role: CommandPermission,
    /// The pull request, for a comment on one.
    pub pull: Option<(u64, &'a PullInfo)>,
}

/// The event payload of a command run: the comment's own delivery with
/// `command` added, as the spec lays it out. `named` holds the words the
/// workflow named; the others are only in `args`.
#[must_use]
pub fn event_with_command(payload: &Value, command: &CommandContext<'_>) -> Value {
    let named: serde_json::Map<String, Value> = command
        .names
        .iter()
        .zip(command.words)
        .map(|(name, word)| (name.clone(), Value::String(word.clone())))
        .collect();
    let mut body = json!({
        "name": command.name,
        "line": command.line,
        "args": command.words,
        "named": named,
        "actor": command.actor,
        "permission": command.role.as_str(),
    });
    if let Some((number, pull)) = command.pull {
        body["pull_request"] = json!({
            "number": number,
            "head_sha": pull.head_sha,
            "head_repo": pull.head_repo_name,
            "base_ref": pull.base_ref,
            "fork": pull.fork,
        });
    }
    let mut event = payload.clone();
    if let Some(fields) = event.as_object_mut() {
        fields.insert("command".to_owned(), body);
    }
    event
}

/// The reaction a command's comment gets when its run ends with
/// `conclusion`. A run that succeeded or was skipped (its `if:` had
/// nothing to do) gets the good one; a failed or cancelled run the bad
/// one. `rocket` picks `rocket` and `confused` over `+1` and `-1`.
#[must_use]
pub fn finished(rocket: bool, conclusion: &str) -> &'static str {
    match (matches!(conclusion, "success" | "skipped"), rocket) {
        (true, false) => "+1",
        (false, false) => "-1",
        (true, true) => "rocket",
        (false, true) => "confused",
    }
}

#[cfg(test)]
mod tests {
    use hatchrun_workflow::ShaPolicy;
    use hatchrun_workflow::command::parse_comment;

    use super::*;

    const HEAD: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    const OTHER: &str = "ffffffffffffffffffffffffffffffffffffffff";

    fn pull(fork: bool) -> PullInfo {
        PullInfo {
            head_sha: HEAD.to_owned(),
            head_repo_id: Some(9),
            head_repo_name: Some("alice/repo".to_owned()),
            base_ref: "main".to_owned(),
            fork,
            state: "open",
            head_ref: "feature".to_owned(),
            title: "change".to_owned(),
            author: "alice".to_owned(),
            author_association: "NONE".to_owned(),
            updated_at: 0,
            raw: serde_json::Value::Null,
        }
    }

    fn options(sha: ShaPolicy) -> CommandOptions {
        CommandOptions {
            sha,
            ..CommandOptions::default()
        }
    }

    fn args(line: &str) -> Args {
        parse_comment(line, |_| true)[0].args.clone().unwrap()
    }

    fn run(gate: &str, status: &str, conclusion: Option<&str>, created_at: u64) -> RunRow {
        serde_json::from_value(json!({
            "id": 9, "run_number": 1, "run_attempt": 1, "tenant_id": "t1",
            "repo_id": 7, "workflow": "ci", "delivery": "d", "event": "pull_request",
            "git_ref": "refs/pull/4/merge", "sha": HEAD, "actor": "a",
            "status": status, "conclusion": conclusion, "created_at": created_at,
            "retention_days": 30, "pull_number": 4, "trust": "fork",
            "author_login": "a", "author_association": "NONE", "gate_status": gate
        }))
        .unwrap()
    }

    const NOW: u64 = 10 * APPROVAL_EXPIRES_MS;

    #[test]
    fn a_waiting_run_is_approved_and_a_running_one_is_left_alone() {
        let waiting = |gate| run(gate, "queued", None, NOW - 1);
        assert_eq!(approval(&[waiting("pending")], NOW), Approval::Approve);
        assert_eq!(approval(&[waiting("deferred")], NOW), Approval::Approve);
        for gate in ["approved", "none"] {
            for status in ["queued", "in_progress"] {
                assert_eq!(
                    approval(&[run(gate, status, None, 0)], NOW),
                    Approval::Nothing,
                    "{gate} {status}"
                );
            }
        }
        // A run that went well has nothing to approve either.
        let done = run("approved", "completed", Some("success"), 0);
        assert_eq!(approval(&[done], NOW), Approval::Nothing);
    }

    #[test]
    fn a_waiting_run_wins_over_the_others_at_the_head() {
        let runs = [
            run("cancelled", "completed", Some("cancelled"), 1),
            run("approved", "in_progress", None, 1),
            run("pending", "queued", None, NOW),
        ];
        assert_eq!(approval(&runs, NOW), Approval::Approve);
        assert_eq!(approval(&runs[..2], NOW), Approval::Nothing);
    }

    #[test]
    fn only_ended_runs_or_none_make_a_new_run() {
        assert_eq!(approval(&[], NOW), Approval::Create);
        for (gate, conclusion) in [
            ("cancelled", "cancelled"),
            ("expired", "cancelled"),
            ("approved", "failure"),
            ("approved", "cancelled"),
            ("none", "failure"),
        ] {
            let ended = run(gate, "completed", Some(conclusion), 1);
            assert_eq!(approval(&[ended], NOW), Approval::Create, "{gate}");
        }
        // A rejected run whose cancellation is still going is not alive.
        let rejected = run("cancelled", "queued", None, 1);
        assert_eq!(approval(&[rejected], NOW), Approval::Create);
    }

    #[test]
    fn a_waiting_run_past_its_deadline_is_not_revived() {
        let late = run("pending", "queued", None, NOW - APPROVAL_EXPIRES_MS - 1);
        assert_eq!(approval(&[late], NOW), Approval::Create);
        let on_time = run("deferred", "queued", None, NOW - APPROVAL_EXPIRES_MS);
        assert_eq!(approval(&[on_time], NOW), Approval::Approve);
    }

    #[test]
    fn approve_run_always_asks_for_the_pin() {
        let line = "/approve-run";
        for fork in [true, false] {
            assert_eq!(
                approve_pin(&args(line), line, Some(&pull(fork))),
                Pin::Reply(format!("Add --sha={HEAD} to: /approve-run"))
            );
        }
        let line = format!("/approve-run --sha={HEAD}");
        assert_eq!(
            approve_pin(&args(&line), &line, Some(&pull(true))),
            Pin::Run(Some(HEAD.to_owned()))
        );
        let line = format!("/approve-run --sha={OTHER}");
        assert!(matches!(
            approve_pin(&args(&line), &line, Some(&pull(true))),
            Pin::Reply(text) if text.contains(HEAD) && text.contains(&line)
        ));
    }

    #[test]
    fn roles_compare_from_read_up_to_admin() {
        use CommandPermission::{Admin, Maintain, Read, Triage, Write};
        let order = [Read, Triage, Write, Maintain, Admin];
        for (i, role) in order.into_iter().enumerate() {
            for (j, needed) in order.into_iter().enumerate() {
                assert_eq!(
                    permits(Some(role), needed),
                    i >= j,
                    "{role:?} for {needed:?}"
                );
            }
        }
        // No access at all passes nothing, not even `read`.
        for needed in order {
            assert!(!permits(None, needed));
        }
    }

    #[test]
    fn the_cap_leaves_room_until_five_runs_are_active() {
        assert_eq!(MAX_ACTIVE_PER_ISSUE, 5);
        assert_eq!(room(0), 5);
        assert_eq!(room(4), 1);
        assert_eq!(room(5), 0);
        assert_eq!(room(9), 0);
    }

    #[test]
    fn a_fork_command_without_a_pin_is_told_to_add_the_head() {
        let line = "/deploy staging";
        let got = pin(
            &options(ShaPolicy::Auto),
            &args(line),
            line,
            Some(&pull(true)),
        );
        assert_eq!(
            got,
            Pin::Reply(format!("Add --sha={HEAD} to: /deploy staging"))
        );
        // `required` asks on a pull request of the repository too.
        let got = pin(
            &options(ShaPolicy::Required),
            &args(line),
            line,
            Some(&pull(false)),
        );
        assert!(matches!(got, Pin::Reply(_)));
    }

    #[test]
    fn an_optional_pin_is_not_asked_for() {
        let line = "/retest";
        for (policy, fork) in [
            (ShaPolicy::Auto, false),
            (ShaPolicy::Optional, true),
            (ShaPolicy::Optional, false),
        ] {
            assert_eq!(
                pin(&options(policy), &args(line), line, Some(&pull(fork))),
                Pin::Run(Some(HEAD.to_owned())),
                "{policy:?} fork={fork}"
            );
        }
    }

    #[test]
    fn a_pin_must_be_the_current_head() {
        let line = format!("/deploy --sha={OTHER}");
        let got = pin(
            &options(ShaPolicy::Auto),
            &args(&line),
            &line,
            Some(&pull(true)),
        );
        assert_eq!(
            got,
            Pin::Reply(format!(
                "The pull request head is now {HEAD}, so nothing ran. \
                 Use --sha={HEAD} in: {line}"
            ))
        );
        // Optional or not, a pin that is given is compared.
        let got = pin(
            &options(ShaPolicy::Optional),
            &args(&line),
            &line,
            Some(&pull(false)),
        );
        assert!(matches!(got, Pin::Reply(_)));
        let line = format!("/deploy --sha={HEAD}");
        let got = pin(
            &options(ShaPolicy::Auto),
            &args(&line),
            &line,
            Some(&pull(true)),
        );
        assert_eq!(got, Pin::Run(Some(HEAD.to_owned())));
    }

    #[test]
    fn a_plain_issue_has_no_head_to_pin() {
        let options = CommandOptions {
            places: vec![hatchrun_workflow::CommandPlace::Issue],
            ..CommandOptions::default()
        };
        let line = format!("/triage --sha={HEAD}");
        assert_eq!(
            pin(&options, &args(&line), &line, None),
            Pin::Reply("Issues have no head commit".to_owned())
        );
        assert_eq!(
            pin(&options, &args("/triage"), "/triage", None),
            Pin::Run(None)
        );
    }

    #[test]
    fn the_command_is_added_to_the_event_as_the_spec_lays_it_out() {
        let pr = pull(true);
        let words = ["staging".to_owned()];
        let names = ["environment".to_owned()];
        let line = format!("/deploy staging --sha={HEAD}");
        let payload = json!({ "action": "created", "comment": { "id": 5 } });
        let event = event_with_command(
            &payload,
            &CommandContext {
                name: "deploy",
                line: &line,
                words: &words,
                names: &names,
                actor: "alice",
                role: CommandPermission::Maintain,
                pull: Some((12, &pr)),
            },
        );
        assert_eq!(event["comment"]["id"], 5);
        assert_eq!(
            event["command"],
            json!({
                "name": "deploy",
                "line": line,
                "args": ["staging"],
                "named": { "environment": "staging" },
                "actor": "alice",
                "permission": "maintain",
                "pull_request": {
                    "number": 12,
                    "head_sha": HEAD,
                    "head_repo": "alice/repo",
                    "base_ref": "main",
                    "fork": true
                }
            })
        );
        // The delivery itself is left as it was.
        assert!(payload.get("command").is_none());
    }

    #[test]
    fn a_command_on_an_issue_has_no_pull_request_and_extra_words_stay_unnamed() {
        let words = ["a".to_owned(), "b".to_owned()];
        let names = ["first".to_owned()];
        let event = event_with_command(
            &json!({}),
            &CommandContext {
                name: "triage",
                line: "/triage a b",
                words: &words,
                names: &names,
                actor: "bob",
                role: CommandPermission::Write,
                pull: None,
            },
        );
        assert_eq!(event["command"]["args"], json!(["a", "b"]));
        assert_eq!(event["command"]["named"], json!({ "first": "a" }));
        assert!(event["command"].get("pull_request").is_none());
    }

    #[test]
    fn a_deleted_fork_has_a_null_head_repo() {
        let mut pr = pull(true);
        pr.head_repo_name = None;
        let event = event_with_command(
            &json!({}),
            &CommandContext {
                name: "retest",
                line: "/retest",
                words: &[],
                names: &[],
                actor: "bob",
                role: CommandPermission::Write,
                pull: Some((3, &pr)),
            },
        );
        assert_eq!(event["command"]["pull_request"]["head_repo"], Value::Null);
    }

    #[test]
    fn a_run_end_picks_the_reaction_by_conclusion_and_set() {
        assert_eq!(finished(false, "success"), "+1");
        assert_eq!(finished(false, "skipped"), "+1");
        assert_eq!(finished(false, "failure"), "-1");
        assert_eq!(finished(false, "cancelled"), "-1");
        assert_eq!(finished(true, "success"), "rocket");
        assert_eq!(finished(true, "skipped"), "rocket");
        assert_eq!(finished(true, "failure"), "confused");
        assert_eq!(finished(true, "cancelled"), "confused");
        assert_eq!(ACCEPTED, "eyes");
    }
}
