// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /github/webhook`: the `push`, `pull_request`, `check_run`,
//! `release`, `create`, `delete`, `repository`, `issues`, `issue_comment`
//! and `installation_repositories` deliveries that `hatchrun-cloud`
//! forwards.
//!
//! A repository that is not enabled for its tenant (see
//! [`crate::installation`]) starts no runs: its deliveries are
//! answered as ignored.

use std::borrow::Cow;

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_cloud::webhook::verify;
use hatchrun_workflow::command::{APPROVE_RUN, Found};
use hatchrun_workflow::{CommandPermission, CommandReaction, Event, MAX_CHANGED};
use serde_json::{Value, json};
use worker::{Env, Method, Request, RequestInit, Response, Result};

use crate::alerts;
use crate::chatops::{self, CommandContext, Pin, Unapprovable};
use crate::env::{Settings, run_stub};
use crate::fork;
use crate::github;
use crate::github::PullInfo;
use crate::installation;
use crate::intake::{
    self, ApproveFork, ForkInfo, IssueComment, Issues, MergeGroup, PullRequest, Push, RefChange,
    Release, Repo, RepositoryDispatch, Rerequested, Trigger,
};
use crate::planning;
use crate::policy::{self, Admission, ForkRuns, RepoPolicy, Waiting};
use crate::runs::{self, Config};
use crate::store::{self, GateStatus, RepoRow, TenantRow};
use crate::trust::Trust;

/// Secret holding the App's webhook secret.
const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn fail(message: impl Into<String>) -> worker::Error {
    worker::Error::RustError(message.into())
}

/// How a delivery ends.
enum Outcome {
    /// Answered 200 `{"ignored": reason}`.
    Ignored(String),
    /// Answered 202 `{"runs": ids}`.
    Done(Vec<u64>),
    /// Answered 503: try again shortly. The delivery is not recorded.
    Retry,
}

/// The commit of a release's tag, or the outcome when the tag is gone: a
/// deleted or unpublished release has nothing to run on, and the head of
/// the default branch would run it under a tag ref that is not there.
fn release_commit(found: Option<String>) -> std::result::Result<String, Outcome> {
    found.ok_or_else(|| Outcome::Ignored("release tag is gone".to_owned()))
}

/// Whether the `pull_request_target` runs of a pull request wait for an
/// approval under `policy`: it is from a fork and the repository
/// approves. A pull request of the repository itself never waits.
fn target_waits(policy: &RepoPolicy, fork: bool) -> bool {
    fork && policy.pull_request_target_head_checkout == policy::HeadCheckout::Approve
}

/// What the `pull_request_target` runs of a pull request become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetGate {
    /// They start: with no gate, or waiting for an approval (`Pending`)
    /// or a free slot (`Deferred`).
    Start(GateStatus),
    /// They need an approval that cannot be asked for, so they do not
    /// run, and the pull request's head says why.
    Refused(&'static str),
    /// The delivery is not the newest, and says nothing about the pull
    /// request as it stands.
    Stale,
}

/// What the `pull_request_target` runs of a pull request become under
/// `policy`, given what already waits. A run that waits for a closed pull
/// request (`open` false) could never be approved, and a repository over
/// its caps has no room to ask, so those are refused and never run
/// without an approval.
fn target_gate(
    policy: &RepoPolicy,
    fork: bool,
    open: bool,
    applied: bool,
    waiting: Waiting,
) -> TargetGate {
    if !target_waits(policy, fork) {
        return TargetGate::Start(GateStatus::NotGated);
    }
    if !applied {
        return TargetGate::Stale;
    }
    if !open {
        return TargetGate::Refused("a closed pull request cannot be approved");
    }
    let admission = policy::target_admission(waiting);
    if let Some(reason) = policy::refusal(admission) {
        return TargetGate::Refused(reason);
    }
    TargetGate::Start(match admission {
        Admission::Pending => GateStatus::Pending,
        Admission::Deferred => GateStatus::Deferred,
        _ => GateStatus::NotGated,
    })
}

/// Which workflows a pull request function makes runs of.
#[derive(Clone, Copy)]
enum Wanted<'a> {
    /// Those the delivery's own action starts.
    Delivery,
    /// `/approve-run` making workflows again. The comment has no pull
    /// request action of its own.
    Remake {
        /// The workflows to make, each as the first of `actions` that it
        /// listens to.
        among: &'a [String],
        /// The actions to try (see [`planning::remake_actions`]).
        actions: &'static [&'static str],
    },
}

/// What a pull request function makes its runs from. The caller reads
/// it, for a delivery and for `/approve-run` alike.
#[derive(Clone, Copy)]
struct Source<'a> {
    /// The commit the runs are on.
    sha: &'a str,
    /// The config at that commit, and the same parsed (`None` when it
    /// is missing or unusable, which makes no run).
    config: &'a Config,
    parsed: Option<&'a hatchrun_workflow::Config>,
    /// The pull request's changed paths, empty when no workflow looks at
    /// them.
    changed: &'a [String],
}

/// The runs to make for one action: the delivery to see as the event,
/// the workflows to limit it to (`None` for all that it triggers) and
/// its trigger.
struct Batch<'p> {
    pr: Cow<'p, PullRequest>,
    workflows: Option<Vec<String>>,
    trigger: Event,
}

impl Wanted<'_> {
    /// The pull request deliveries to make runs for. `event` builds the
    /// trigger of an action. A remake has none when the config did not
    /// parse.
    fn batches<'p>(
        &self,
        pr: &'p PullRequest,
        parsed: Option<&hatchrun_workflow::Config>,
        event: impl Fn(&str) -> Event,
    ) -> Vec<Batch<'p>> {
        let Self::Remake { among, actions } = self else {
            return vec![Batch {
                pr: Cow::Borrowed(pr),
                workflows: None,
                trigger: event(&pr.action),
            }];
        };
        let Some(parsed) = parsed else {
            return Vec::new();
        };
        planning::remake_batches(parsed, event, Some(among), actions)
            .into_iter()
            .map(|(action, names, trigger)| Batch {
                pr: Cow::Owned(with_action(pr, action)),
                workflows: Some(names),
                trigger,
            })
            .collect()
    }
}

/// The config as the planner parses it: `Ok(None)` when it is missing,
/// `Err` when it cannot be read or does not parse.
fn parsed(config: &Config) -> std::result::Result<Option<hatchrun_workflow::Config>, ()> {
    match config {
        Config::Missing => Ok(None),
        Config::Text(Ok(source)) => hatchrun_workflow::parse(source).map(Some).map_err(|_| ()),
        Config::Text(Err(_)) => Err(()),
    }
}

/// The workflows of `parsed` that listen to the event `event` builds the
/// trigger of, under `actions` (see [`planning::remake_batches`]).
fn listed(
    parsed: Option<&hatchrun_workflow::Config>,
    event: impl Fn(&str) -> Event,
    actions: &[&'static str],
) -> Vec<String> {
    parsed
        .map(|parsed| {
            planning::remake_batches(parsed, event, None, actions)
                .into_iter()
                .flat_map(|(_, names, _)| names)
                .collect()
        })
        .unwrap_or_default()
}

/// The trigger of a `pull_request` delivery of an action, for a pull
/// request against `base_ref` that changed `changed`.
fn head_event<'a>(base_ref: &'a str, changed: &'a [String]) -> impl Fn(&str) -> Event + 'a {
    move |action| Event::PullRequest {
        action: action.to_owned(),
        base_ref: base_ref.to_owned(),
        changed: changed.to_vec(),
    }
}

/// As [`head_event`], for `pull_request_target`.
fn target_event<'a>(base_ref: &'a str, changed: &'a [String]) -> impl Fn(&str) -> Event + 'a {
    move |action| Event::PullRequestTarget {
        action: action.to_owned(),
        base_ref: base_ref.to_owned(),
        changed: changed.to_vec(),
    }
}

/// `pr` as the delivery of `action`: its payload says so too.
fn with_action(pr: &PullRequest, action: &str) -> PullRequest {
    let mut made = pr.clone();
    action.clone_into(&mut made.action);
    made.payload["action"] = action.into();
    made
}

/// The KV namespace that keeps the App's bot login: the one the
/// installation tokens are in.
const BOT_LOGIN_BINDING: &str = "TOKENS";

/// The KV key of the App's bot login.
const BOT_LOGIN_KEY: &str = "ci:app-bot-login";

/// How long the bot login is kept, in seconds.
const BOT_LOGIN_TTL_SECS: u64 = 24 * 60 * 60;

/// Whether `sender` is `bot`, the App's own login. GitHub logins do not
/// differ by case.
fn is_bot_login(sender: &str, bot: &str) -> bool {
    sender.eq_ignore_ascii_case(bot)
}

/// Whether the delivery of `actor`, a bot, is the App's own, given the
/// bot login the lookup found, `None` when it failed. A failed lookup
/// counts as the App's own: a loop is worse than a bot-triggered run that
/// is missed.
fn own_sender(actor: &str, login: Option<&str>) -> bool {
    login.is_none_or(|login| is_bot_login(actor, login))
}

/// The delivery key of the `pull_request_target` runs of a delivery. A
/// run is unique per delivery and workflow, and a workflow that listens
/// to `pull_request` and `pull_request_target` has a run of each for one
/// pull request delivery.
pub(crate) fn target_delivery(delivery: &str) -> String {
    format!("{delivery}:target")
}

/// The tenant of a delivery, or why the delivery is ignored. A tenant
/// without a card passes, and [`runs::create`] answers it with the card
/// check run.
fn gate(tenant: Option<TenantRow>) -> std::result::Result<TenantRow, &'static str> {
    match tenant {
        None => Err("no tenant for this installation"),
        Some(tenant) => Ok(tenant),
    }
}

fn reply(outcome: &Outcome) -> Result<Response> {
    match outcome {
        Outcome::Ignored(reason) => Response::from_json(&json!({ "ignored": reason })),
        Outcome::Done(ids) => Ok(Response::from_json(&json!({ "runs": ids }))?.with_status(202)),
        Outcome::Retry => {
            let mut response =
                Response::from_json(&json!({ "error": "try again shortly" }))?.with_status(503);
            response.headers_mut().set("Retry-After", "5")?;
            Ok(response)
        }
    }
}

/// Handles one delivery. The signature is checked on the raw body
/// before anything reads it. Any failure answers 500, so GitHub records
/// the delivery as failed.
///
/// # Errors
///
/// The body cannot be read at all.
pub async fn handle(mut req: Request, env: &Env) -> Result<Response> {
    let secret = match env.secret(WEBHOOK_SECRET) {
        Ok(secret) => secret.to_string(),
        Err(err) => {
            worker::console_error!("webhook: {WEBHOOK_SECRET}: {err}");
            String::new()
        }
    };
    if secret.is_empty() {
        return Response::error("no GITHUB_WEBHOOK_SECRET", 500);
    }
    let signature = req.headers().get("X-Hub-Signature-256")?;
    let kind = req.headers().get("X-GitHub-Event")?.unwrap_or_default();
    let guid = req.headers().get("X-GitHub-Delivery")?.unwrap_or_default();
    let body = req.bytes().await?;
    if !verify(secret.as_bytes(), &body, signature.as_deref()) {
        return Response::error("bad signature", 401);
    }
    if guid.is_empty() {
        return Response::error("no X-GitHub-Delivery", 400);
    }
    match process(env, &kind, &guid, &body).await {
        Ok(response) => Ok(response),
        Err(err) => {
            worker::console_error!("webhook {kind} {guid}: {err}");
            Response::error("webhook failed", 500)
        }
    }
}

/// Handles one webhook delivery: drops duplicates, parses the event and starts the matching runs.
async fn process(env: &Env, kind: &str, guid: &str, body: &[u8]) -> Result<Response> {
    let db = Db::connect(env).await?;
    if store::delivery_seen(&db, guid).await? {
        return Response::from_json(&json!({ "duplicate": guid }));
    }
    let trigger = match intake::parse(kind, body) {
        Ok(trigger) => trigger,
        Err(err) => {
            // The payload is signed but unreadable, and a redelivery
            // would be the same bytes: record it and answer 400.
            worker::console_error!("webhook {kind} {guid}: bad payload: {err}");
            store::record_delivery(&db, guid, kind, now_ms()).await?;
            return Response::error("bad payload", 400);
        }
    };
    if let Trigger::InstallationRepositories { installation_id } = &trigger {
        return installation_changed(env, &db, *installation_id, guid, kind).await;
    }
    let (installation_id, repo) = match &trigger {
        Trigger::Ignored(reason) => return reply(&Outcome::Ignored(reason.clone())),
        Trigger::InstallationRepositories { .. } => unreachable!("answered above"),
        Trigger::Push(p) => (p.installation_id, &p.repo),
        Trigger::PullRequest(p) => (p.installation_id, &p.repo),
        Trigger::Release(r) => (r.installation_id, &r.repo),
        Trigger::Create(c) | Trigger::Delete(c) => (c.installation_id, &c.repo),
        Trigger::MergeGroup(g) => (g.installation_id, &g.repo),
        Trigger::RepositoryDispatch(d) => (d.installation_id, &d.repo),
        Trigger::Issues(i) => (i.installation_id, &i.repo),
        Trigger::IssueComment(c) => (c.installation_id, &c.repo),
        Trigger::Rerequested(r) => (r.installation_id, &r.repo),
        Trigger::ApproveFork(a) => (a.installation_id, &a.repo),
        Trigger::Repository(r) => (r.installation_id, &r.repo),
    };
    let found = store::tenant_for_installation(&db, installation_id).await?;
    // A visibility change is recorded even for a tenant without CI: the
    // read API shows runs left from before, by the recorded visibility.
    let tenant = match (&trigger, found) {
        (Trigger::Repository(_), Some(tenant)) => tenant,
        (_, found) => match gate(found) {
            Ok(tenant) => tenant,
            Err(reason) => return reply(&Outcome::Ignored(reason.to_owned())),
        },
    };
    let now = now_ms();
    store::upsert_repo(
        &db,
        &RepoRow {
            id: repo.id,
            tenant_id: tenant.id.clone(),
            installation_id,
            full_name: repo.full_name.clone(),
            default_branch: repo.default_branch.clone(),
            private: repo.private,
        },
        now,
    )
    .await?;
    if let Trigger::Repository(_) = &trigger {
        store::touch_installation_repo(
            &db,
            &tenant.id,
            repo.id,
            &repo.full_name,
            &repo.default_branch,
            repo.private,
            now,
        )
        .await?;
    } else if !store::repo_enabled(&db, &tenant.id, repo.id).await? {
        store::record_delivery(&db, guid, kind, now).await?;
        return reply(&Outcome::Ignored("repository is not enabled".to_owned()));
    }
    let settings = Settings::read(env).map_err(fail)?;
    let github = Counted::default();
    let tokens = Tokens::new(
        Kv::new(env),
        App::new(
            settings.github_api.clone(),
            settings.app_id.clone(),
            settings.app_key.clone(),
        ),
        &github,
        now,
    );
    let cx = Cx {
        env,
        db: &db,
        settings: &settings,
        tokens: &tokens,
        tenant: &tenant.id,
        delivery: guid,
        now,
    };
    let result = cx.run(&trigger).await;
    // The calls made are noted even when the delivery failed.
    alerts::note_rate_limits(&db, &github, now).await;
    let outcome = result?;
    if !matches!(outcome, Outcome::Retry) {
        store::record_delivery(&db, guid, kind, now).await?;
    }
    reply(&outcome)
}

/// Reads the installation's repositories again after
/// `installation_repositories` said they changed. A tenant with no
/// installation of that id has nothing to update.
async fn installation_changed(
    env: &Env,
    db: &Db,
    installation_id: u64,
    guid: &str,
    kind: &str,
) -> Result<Response> {
    let Some(tenant) = store::tenant_for_installation(db, installation_id).await? else {
        return reply(&Outcome::Ignored(
            "no tenant for this installation".to_owned(),
        ));
    };
    let settings = Settings::read(env).map_err(fail)?;
    let now = now_ms();
    installation::sync(env, db, &settings, &tenant.id, installation_id, now).await?;
    store::record_delivery(db, guid, kind, now).await?;
    reply(&Outcome::Done(Vec::new()))
}

/// A delivery that starts workflows on a commit it does not name itself:
/// what [`runs::create`] is asked for, before the config is read.
struct Fired<'a> {
    installation_id: u64,
    repo: &'a Repo,
    event_name: &'static str,
    payload: &'a Value,
    trigger: Event,
    git_ref: String,
    sha: String,
    actor: &'a str,
}

/// A release runs on its tag, at the tag's commit.
fn release_fired(release: &Release, sha: String) -> Fired<'_> {
    Fired {
        installation_id: release.installation_id,
        repo: &release.repo,
        event_name: "release",
        payload: &release.payload,
        trigger: Event::Release {
            action: release.action.clone(),
            tag: release.tag.clone(),
        },
        git_ref: format!("refs/tags/{}", release.tag),
        sha,
        actor: &release.actor,
    }
}

/// A created branch or tag runs on its own ref, at its commit.
fn create_fired(change: &RefChange, sha: String) -> Fired<'_> {
    Fired {
        installation_id: change.installation_id,
        repo: &change.repo,
        event_name: "create",
        payload: &change.payload,
        trigger: Event::Create,
        git_ref: change.r#ref.clone(),
        sha,
        actor: &change.actor,
    }
}

/// A deleted ref has no commit, so as on GitHub the run is on the
/// default branch, at `sha`, its head.
fn delete_fired(change: &RefChange, sha: String) -> Fired<'_> {
    Fired {
        installation_id: change.installation_id,
        repo: &change.repo,
        event_name: "delete",
        payload: &change.payload,
        trigger: Event::Delete,
        git_ref: format!("refs/heads/{}", change.repo.default_branch),
        sha,
        actor: &change.actor,
    }
}

/// A merge group runs on its own ref, at its head commit, which is where
/// the merge queue looks for the checks.
fn merge_group_fired(group: &MergeGroup) -> Fired<'_> {
    Fired {
        installation_id: group.installation_id,
        repo: &group.repo,
        event_name: "merge_group",
        payload: &group.payload,
        trigger: Event::MergeGroup {
            action: group.action.clone(),
        },
        git_ref: group.head_ref.clone(),
        sha: group.head_sha.clone(),
        actor: &group.actor,
    }
}

/// A repository dispatch runs on the default branch, at its head `sha`.
fn dispatch_fired(dispatch: &RepositoryDispatch, sha: String) -> Fired<'_> {
    Fired {
        installation_id: dispatch.installation_id,
        repo: &dispatch.repo,
        event_name: "repository_dispatch",
        payload: &dispatch.payload,
        trigger: Event::RepositoryDispatch {
            action: dispatch.action.clone(),
        },
        git_ref: format!("refs/heads/{}", dispatch.repo.default_branch),
        sha,
        actor: &dispatch.actor,
    }
}

/// An issue activity runs on the default branch, at its head `sha`, as
/// on GitHub.
fn issues_fired(issue: &Issues, sha: String) -> Fired<'_> {
    Fired {
        installation_id: issue.installation_id,
        repo: &issue.repo,
        event_name: "issues",
        payload: &issue.payload,
        trigger: Event::Issues {
            action: issue.action.clone(),
        },
        git_ref: default_ref(&issue.repo),
        sha,
        actor: &issue.actor,
    }
}

/// The full ref of the repository's default branch.
fn default_ref(repo: &Repo) -> String {
    format!("refs/heads/{}", repo.default_branch)
}

/// What one comment's commands run against.
struct CommentRun<'a> {
    comment: &'a IssueComment,
    /// The default branch's config, parsed once for the comment; `None`
    /// when it is missing or does not parse.
    workflows: Option<&'a hatchrun_workflow::Config>,
    /// The same as read, for the planner.
    config: &'a Config,
    /// The head of the default branch, which the runs are on.
    sha: &'a str,
    /// The full ref of the default branch.
    default_ref: &'a str,
    /// The pull request the comment is on, `None` on a plain issue.
    pull: Option<&'a PullInfo>,
}

/// Where a comment's runs report: trusted runs of the default branch,
/// with their check runs on the head of the pull request the comment is
/// on (see [`runs::Target`]).
fn comment_origin<'a>(
    comment: &'a IssueComment,
    pull: Option<&'a PullInfo>,
    chat: &'a store::Chat,
) -> runs::Origin<'a> {
    runs::Origin {
        target: pull.map(|pull| runs::Target {
            head_sha: &pull.head_sha,
            refused: None,
        }),
        chat: Some(chat),
        ..runs::Origin::own(&comment.actor)
    }
}

/// The row of `pr` in the table of pull requests.
fn pull_record(pr: &PullRequest) -> store::NewPull {
    store::NewPull {
        repo_id: pr.repo.id,
        number: pr.number,
        title: pr.title.clone(),
        head_ref: pr.head_ref.clone(),
        base_ref: pr.base_ref.clone(),
        author: pr.author.clone(),
        author_association: pr.author_association.clone(),
        head_repo_id: pr
            .fork
            .as_ref()
            .map_or(Some(pr.repo.id), |f| f.head_repo_id),
        head_sha: pr.head_sha.clone(),
        state: pr.state.to_owned(),
        updated_at: pr.updated_at,
    }
}

/// The pull request a comment is on, as GitHub has it now, in the shape
/// of a `pull_request` delivery: `/approve-run` starts the runs that
/// delivery would have, and they see this payload as their event. Its
/// action is `synchronize`; a remade workflow gets the action it listens
/// to (see [`Wanted::Remake`]).
fn pull_of_comment(comment: &IssueComment, pull: &PullInfo) -> PullRequest {
    let fork = pull.fork.then(|| ForkInfo {
        head_repo_id: pull.head_repo_id,
        head_repo_name: pull.head_repo_name.clone(),
    });
    let author = if pull.author.is_empty() {
        "ghost".to_owned()
    } else {
        pull.author.clone()
    };
    let payload = json!({
        "action": "synchronize",
        "number": comment.number,
        "pull_request": pull.raw,
        "repository": comment.payload["repository"],
        "sender": comment.payload["sender"],
    });
    PullRequest {
        installation_id: comment.installation_id,
        repo: comment.repo.clone(),
        action: "synchronize".to_owned(),
        number: comment.number,
        head_sha: pull.head_sha.clone(),
        head_ref: pull.head_ref.clone(),
        base_ref: pull.base_ref.clone(),
        actor: comment.actor.clone(),
        title: pull.title.clone(),
        author,
        author_association: pull.author_association.clone(),
        fork,
        state: pull.state,
        updated_at: pull.updated_at,
        payload,
    }
}

/// Adds `text` to the replies of a comment unless the same reply is
/// there already: two workflows that define a command can ask for the
/// same thing, and the maintainer reads it once.
fn push_reply(replies: &mut Vec<String>, text: String) {
    if !replies.contains(&text) {
        replies.push(text);
    }
}

struct Cx<'a> {
    env: &'a Env,
    db: &'a Db,
    settings: &'a Settings,
    tokens: &'a Tokens<'a, Kv, App, Counted>,
    tenant: &'a str,
    delivery: &'a str,
    now: u64,
}

impl Cx<'_> {
    /// Starts what `trigger` asks for.
    async fn run(&self, trigger: &Trigger) -> Result<Outcome> {
        Ok(match trigger {
            Trigger::Push(push) => self.push(push).await?,
            Trigger::PullRequest(pr) => self.pull_request(pr).await?,
            Trigger::Release(release) => self.release(release).await?,
            Trigger::Create(change) => self.create(change).await?,
            Trigger::Delete(change) => self.delete(change).await?,
            Trigger::MergeGroup(group) => self.merge_group(group).await?,
            Trigger::RepositoryDispatch(dispatch) => self.repository_dispatch(dispatch).await?,
            Trigger::Issues(issue) => self.issues(issue).await?,
            Trigger::IssueComment(comment) => self.issue_comment(comment).await?,
            Trigger::Rerequested(rerun) => self.rerun(rerun).await?,
            Trigger::ApproveFork(approval) => self.approve_fork(approval).await?,
            // The row was updated before, with the tenant's other
            // repository writes; nothing runs.
            Trigger::Repository(_) => Outcome::Done(Vec::new()),
            Trigger::Ignored(reason) => Outcome::Ignored(reason.clone()),
            Trigger::InstallationRepositories { .. } => unreachable!("answered above"),
        })
    }

    /// Plans and starts the workflows triggered by a push.
    async fn push(&self, push: &Push) -> Result<Outcome> {
        let trigger = Event::Push {
            r#ref: push.r#ref.clone(),
            changed: push.changed.clone(),
        };
        let config = runs::read_config(
            self.tokens,
            self.settings,
            push.installation_id,
            &push.repo.full_name,
            &push.after,
        )
        .await?;
        let ids = runs::create(
            self.env,
            self.db,
            self.settings,
            self.tokens,
            &runs::Request {
                tenant_id: self.tenant,
                installation_id: push.installation_id,
                repo: &push.repo,
                delivery: self.delivery,
                event_name: "push",
                event: &push.payload,
                trigger: &trigger,
                compile_ref: &push.r#ref,
                git_ref: &push.r#ref,
                sha: &push.after,
                actor: &push.actor,
                head_ref: "",
                base_ref: "",
                workflows: None,
                origin: runs::Origin::own(&push.actor),
            },
            &config,
            self.now,
        )
        .await?;
        if push.r#ref == format!("refs/heads/{}", push.repo.default_branch) {
            let schedules = match &config {
                Config::Text(Ok(source)) => planning::schedules(source),
                Config::Missing => Vec::new(),
                Config::Text(Err(err)) => {
                    worker::console_warn!(
                        "{}: schedules cleared, the config is unusable: {}",
                        push.repo.full_name,
                        err.message
                    );
                    Vec::new()
                }
            };
            store::replace_schedules(self.db, push.repo.id, &schedules).await?;
        }
        Ok(Outcome::Done(ids))
    }

    /// Settles a tie: two deliveries of the same second with different
    /// head commits. GitHub's time has one second, so which is newer is
    /// not in the deliveries; the pull request's head as GitHub has it
    /// now is. The delivery counts only when it names that head, and then
    /// the row is made to say so.
    async fn tie_is_current(
        &self,
        pr: &PullRequest,
        record: &store::NewPull,
        recorded: &str,
    ) -> Result<bool> {
        let response = self
            .tokens
            .send(pr.installation_id, |token| {
                github::pull_request(
                    &self.settings.github_api,
                    &pr.repo.full_name,
                    pr.number,
                    token,
                )
            })
            .await
            .map_err(|err| fail(err.to_string()))?;
        let head = github::pull_head_sha(&response).map_err(fail)?;
        if head != pr.head_sha {
            return Ok(false);
        }
        // Only while the row is still what it was at the tie: a delivery
        // that moved it since wins, and this one is stale.
        store::upsert_pull_if_tied(self.db, record, recorded).await
    }

    /// What a fork pull request becomes under its repository's policy:
    /// the gate its runs get and the policy to plan them under, or the
    /// outcome that starts nothing. A newer commit ends the waiting runs
    /// of older ones, and a closed pull request ends them all. `None`
    /// for a pull request from the same repository.
    async fn fork_admission(
        &self,
        pr: &PullRequest,
        policy: Option<RepoPolicy>,
    ) -> Result<std::result::Result<Option<(GateStatus, RepoPolicy)>, Outcome>> {
        let Some(policy) = policy.filter(|_| pr.fork.is_some()) else {
            return Ok(Ok(None));
        };
        let open = pr.state == "open";
        let keep = open.then_some(pr.head_sha.as_str());
        self.end_waiting(pr, keep, if open { fork::SUPERSEDED } else { fork::CLOSED })
            .await?;
        if !open {
            return Ok(Err(Outcome::Ignored("pull request is closed".to_owned())));
        }
        let waiting = store::fork_waiting(self.db, pr.repo.id, &pr.author, Some(pr.number)).await?;
        let admission = policy::admission(&policy, &pr.author_association, waiting);
        if let Some(reason) = policy::refusal(admission) {
            return Ok(Err(Outcome::Ignored(reason.to_owned())));
        }
        let gate = match admission {
            Admission::Pending => GateStatus::Pending,
            Admission::Deferred => GateStatus::Deferred,
            _ => GateStatus::NotGated,
        };
        Ok(Ok(Some((gate, policy))))
    }

    /// Reads the recorded row of the pull request `comment` is on and
    /// decides `/approve-run` pinned at `head` from it. Replies with why
    /// and returns `true` when the row says to stop (closed, or another
    /// head); `false` when it is open at `head`.
    async fn stops_on_row(&self, comment: &IssueComment, head: &str, line: &str) -> Result<bool> {
        let row = store::pull_head(self.db, comment.repo.id, comment.number).await?;
        let Pin::Reply(text) = chatops::pin_against_row(row.as_ref(), head, line) else {
            return Ok(false);
        };
        worker::console_warn!(
            "{}: /approve-run on #{} found a newer pull request row",
            comment.repo.full_name,
            comment.number
        );
        self.reply(comment, &text).await;
        Ok(true)
    }

    /// Cancels the runs of `pr` that wait for an approval, except those
    /// at `keep`, and marks their card with `message`.
    async fn end_waiting(&self, pr: &PullRequest, keep: Option<&str>, message: &str) -> Result<()> {
        let ended = store::end_pull_gates(self.db, pr.repo.id, pr.number, keep, self.now).await?;
        self.after_ended(pr, &ended, message).await
    }

    /// As [`Self::end_waiting`] keeping `head`, and only while the
    /// recorded pull request is open at `head`: a newer head recorded
    /// since is not superseded by it. Returns whether anything ended.
    async fn end_waiting_at_head(
        &self,
        pr: &PullRequest,
        head: &str,
        message: &str,
    ) -> Result<bool> {
        let ended =
            store::end_pull_gates_at_head(self.db, pr.repo.id, pr.number, head, self.now).await?;
        self.after_ended(pr, &ended, message).await?;
        Ok(!ended.is_empty())
    }

    /// Marks the cards of the `ended` runs of `pr` with `message`.
    async fn after_ended(
        &self,
        pr: &PullRequest,
        ended: &[store::RunRow],
        message: &str,
    ) -> Result<()> {
        if ended.is_empty() {
            return Ok(());
        }
        if let Some(repo) = store::repo(self.db, pr.repo.id).await? {
            fork::after_end(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &repo,
                ended,
                message,
            )
            .await;
        }
        Ok(())
    }

    /// Puts an approval check run on the head of `pr` for each of the
    /// runs the delivery made that waits: its own card, with its own
    /// button, named for its workflow. A delivery repeated after a
    /// failure adopts the card an earlier attempt made and records its
    /// id, so the button always finds its run (see
    /// [`fork::ensure_card`]).
    async fn ask_approval(
        &self,
        pr: &PullRequest,
        run_ids: &[u64],
        policy: &RepoPolicy,
    ) -> Result<()> {
        let Some(repo) = store::repo(self.db, pr.repo.id).await? else {
            return Ok(());
        };
        // The runs exist: a card that cannot be made now is logged, and
        // the reconciler makes it. The delivery does not fail for it.
        for &run_id in run_ids {
            let Some(run) = store::run(self.db, run_id).await? else {
                continue;
            };
            if let Err(err) = fork::ensure_card(
                self.tokens,
                self.db,
                self.settings,
                &repo,
                &run,
                policy,
                None,
            )
            .await
            {
                worker::console_error!("run {run_id}: the approval check run: {err}");
            }
        }
        Ok(())
    }

    /// A click on "Approve and run". GitHub shows the button to people
    /// who can write, and this does not rely on it: the sender's
    /// permission on the base repository is asked of GitHub, and only
    /// `write`, `maintain` and `admin` approve.
    async fn approve_fork(&self, approval: &ApproveFork) -> Result<Outcome> {
        let sent = self
            .tokens
            .send(approval.installation_id, |token| {
                github::collaborator_permission(
                    &self.settings.github_api,
                    &approval.repo.full_name,
                    &approval.sender,
                    token,
                )
            })
            .await
            .map_err(|err| fail(err.to_string()))?;
        if !github::can_write(&sent).map_err(fail)? {
            worker::console_warn!(
                "{}: {} cannot write and clicked approve on check run {}",
                approval.repo.full_name,
                approval.sender,
                approval.check_run_id
            );
            return Ok(Outcome::Ignored(
                "the sender cannot write to the repository".to_owned(),
            ));
        }
        let approved = store::approve_by_check(
            self.db,
            self.tenant,
            approval.repo.id,
            approval.check_run_id,
            &approval.sender,
            self.now,
        )
        .await?;
        if approved.is_empty() {
            return Ok(Outcome::Ignored(
                "no run waits on this check run".to_owned(),
            ));
        }
        if let Some(repo) = store::repo(self.db, approval.repo.id).await? {
            fork::after_approval(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &repo,
                &approved,
                &approval.sender,
            )
            .await;
        }
        Ok(Outcome::Done(approved.iter().map(|run| run.id).collect()))
    }

    /// A pull request delivery starts up to two groups of runs: the
    /// `pull_request_target` workflows, read from the base branch, and
    /// the `pull_request` workflows, read at the head.
    async fn pull_request(&self, pr: &PullRequest) -> Result<Outcome> {
        // Every action carries the pull request as it stands, and a run
        // of any of them (`labeled` included) needs its row, so all are
        // recorded. The time is GitHub's, so a late delivery cannot
        // replace newer data.
        let record = pull_record(pr);
        let applied = match store::upsert_pull(self.db, &record).await? {
            store::PullWrite::Applied => true,
            store::PullWrite::Stale => false,
            store::PullWrite::Tied(recorded) => self.tie_is_current(pr, &record, &recorded).await?,
        };
        // The base branch's workflows need nothing of the head: they run
        // whatever happens to the fork's, and a late delivery is as good
        // as an early one.
        // The repository's policy is read once, by whichever group needs
        // it first.
        let mut policy = None;
        // The base branch's config and the head's are read together. The
        // branch is probed by its name first: most repositories have no
        // `pull_request_target` workflow, so that read is all they cost.
        let base = async {
            let probe = runs::read_config(
                self.tokens,
                self.settings,
                pr.installation_id,
                &pr.repo.full_name,
                &runs::compile_ref_for(pr),
            )
            .await?;
            // A config that is missing or cannot be read has nothing to
            // run: the push that made it was told, on its own commit.
            if !matches!(&probe, Config::Text(Ok(source))
                if planning::has_pull_request_target(source))
            {
                return Ok(None);
            }
            self.base_config(pr).await
        };
        let head = self.config_at(pr.installation_id, &pr.repo, &pr.head_sha);
        let (base, head) = futures_util::future::join(base, head).await;
        let (base, head) = (base?, head?);
        let head_parsed = parsed(&head).ok().flatten();
        let base_parsed = base
            .as_ref()
            .and_then(|(_, config)| parsed(config).ok().flatten());
        // Files cost up to three calls, so they are read only for a
        // workflow that listens to this action and filters on paths.
        let wants_paths = head_parsed.as_ref().is_some_and(|parsed| {
            planning::needs_pull_request_paths(parsed, &pr.action, &pr.base_ref, None)
        }) || base_parsed.as_ref().is_some_and(|parsed| {
            planning::needs_pull_request_target_paths(parsed, &pr.action, &pr.base_ref, None)
        });
        let changed = if wants_paths {
            self.pull_paths(pr).await?
        } else {
            Vec::new()
        };
        let mut ids = Vec::new();
        if let Some((sha, config)) = &base {
            let source = Source {
                sha,
                config,
                parsed: base_parsed.as_ref(),
                changed: &changed,
            };
            ids = self
                .pull_request_target(
                    pr,
                    applied,
                    &mut policy,
                    self.delivery,
                    Wanted::Delivery,
                    source,
                )
                .await?
                .0;
        }
        let source = Source {
            sha: &pr.head_sha,
            config: &head,
            parsed: head_parsed.as_ref(),
            changed: &changed,
        };
        match self
            .pull_request_head(
                pr,
                applied,
                &mut policy,
                self.delivery,
                Wanted::Delivery,
                source,
            )
            .await?
        {
            Outcome::Done(more) => ids.extend(more),
            Outcome::Ignored(reason) if ids.is_empty() => return Ok(Outcome::Ignored(reason)),
            Outcome::Ignored(_) => {}
            Outcome::Retry => return Ok(Outcome::Retry),
        }
        Ok(Outcome::Done(ids))
    }

    /// The base branch's head commit as it is now, and the config at it.
    /// The commit comes first and the config is read at it, so the plan
    /// and the `GITHUB_SHA` of the steps are of one commit. `None` when
    /// the branch is gone.
    async fn base_config(&self, pr: &PullRequest) -> Result<Option<(String, Config)>> {
        let Some(sha) = runs::commit(
            self.tokens,
            self.settings,
            pr.installation_id,
            &pr.repo.full_name,
            &runs::compile_ref_for(pr),
        )
        .await?
        else {
            return Ok(None);
        };
        let config = self.config_at(pr.installation_id, &pr.repo, &sha).await?;
        Ok(Some((sha, config)))
    }

    /// The repository's policy, read once for a delivery and kept in
    /// `cache`.
    async fn policy_of(
        &self,
        pr: &PullRequest,
        cache: &mut Option<RepoPolicy>,
    ) -> Result<RepoPolicy> {
        if let Some(policy) = cache {
            return Ok(*policy);
        }
        let policy = store::repo_policy(self.db, pr.repo.id, pr.repo.private).await?;
        *cache = Some(policy);
        Ok(policy)
    }

    /// The changed paths of a pull request, as GitHub lists them. Files
    /// cost up to three calls, so callers ask only for a workflow that
    /// filters on paths.
    async fn pull_paths(&self, pr: &PullRequest) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        for page in 1..=github::FILES_MAX_PAGES {
            let response = self
                .tokens
                .send(pr.installation_id, |token| {
                    github::pull_files(
                        &self.settings.github_api,
                        &pr.repo.full_name,
                        pr.number,
                        page,
                        token,
                    )
                })
                .await
                .map_err(|err| fail(err.to_string()))?;
            let names = github::pull_file_names(&response).map_err(fail)?;
            let full = names.len() >= github::FILES_PAGE_SIZE;
            changed.extend(names);
            if !full || changed.len() >= MAX_CHANGED {
                break;
            }
        }
        changed.truncate(MAX_CHANGED);
        Ok(changed)
    }

    /// The `pull_request_target` workflows of a pull request. As on
    /// GitHub they run from the base branch: the config is read at the
    /// branch's head (never at the pull request's head, which the author
    /// controls), `GITHUB_REF` is the base branch and `GITHUB_SHA` its
    /// head commit as it is now (the payload's `base.sha` is as old as
    /// the pull request's last event and can be behind). The runs are
    /// trusted, so they get the repository's secrets and a token that can
    /// write.
    ///
    /// A fork's pull request under the repository's `approve` setting
    /// waits for a maintainer, whoever the author is, through the same
    /// card as a fork run of the `pull_request` event. A pull request of
    /// the same repository does not wait. A closed pull request, and a
    /// delivery older than what is recorded, start no run that would wait
    /// for an approval that cannot be given.
    ///
    /// The caller reads the commit and the config at it (see
    /// [`Self::base_config`]), and the policy is read here. A branch that
    /// moved since runs the config of the moment before on the commit of
    /// the moment after.
    ///
    /// `delivery` is the key the runs are recorded under (see
    /// [`target_delivery`]) and `wanted` says which workflows and as what
    /// action. Returns the runs made and why the group was refused, if it
    /// was.
    #[allow(clippy::too_many_lines)] // One delivery, read top to bottom.
    async fn pull_request_target(
        &self,
        pr: &PullRequest,
        applied: bool,
        cache: &mut Option<RepoPolicy>,
        delivery: &str,
        wanted: Wanted<'_>,
        source: Source<'_>,
    ) -> Result<(Vec<u64>, Option<&'static str>)> {
        let git_ref = runs::compile_ref_for(pr);
        let Source {
            sha,
            config,
            parsed,
            changed,
        } = source;
        let policy = self.policy_of(pr, cache).await?;
        let waits = target_waits(&policy, pr.fork.is_some());
        if waits && applied {
            // A newer commit ends the waiting runs of older ones, and a
            // closed pull request all of them, before anything is
            // counted, as for a fork run.
            let open = pr.state == "open";
            let keep = open.then_some(pr.head_sha.as_str());
            self.end_waiting(pr, keep, if open { fork::SUPERSEDED } else { fork::CLOSED })
                .await?;
        }
        let waiting = if waits {
            // The fork run of this delivery, asked for next, shares the
            // pull request's group with this run and counts once.
            store::fork_waiting_beside(
                self.db,
                pr.repo.id,
                &pr.author,
                Some(pr.number),
                Some(&pr.head_sha),
            )
            .await?
        } else {
            Waiting::default()
        };
        let (gate, refused) = match target_gate(
            &policy,
            pr.fork.is_some(),
            pr.state == "open",
            applied,
            waiting,
        ) {
            TargetGate::Start(gate) => (gate, None),
            TargetGate::Refused(reason) => (GateStatus::NotGated, Some(reason)),
            TargetGate::Stale => return Ok((Vec::new(), None)),
        };
        // A pull_request run of the same delivery and workflow is a
        // different run, so its key differs.
        let delivery = target_delivery(delivery);
        let batches = wanted.batches(pr, parsed, |action| Event::PullRequestTarget {
            action: action.to_owned(),
            base_ref: pr.base_ref.clone(),
            changed: changed.to_vec(),
        });
        // The gate and the waiting count above hold for all the batches:
        // the group is admitted once.
        let mut ids = Vec::new();
        for batch in &batches {
            let started = runs::create(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &runs::Request {
                    tenant_id: self.tenant,
                    installation_id: pr.installation_id,
                    repo: &pr.repo,
                    delivery: &delivery,
                    event_name: "pull_request_target",
                    event: &batch.pr.payload,
                    trigger: &batch.trigger,
                    compile_ref: &git_ref,
                    git_ref: &git_ref,
                    sha,
                    actor: &pr.actor,
                    head_ref: &pr.head_ref,
                    base_ref: &pr.base_ref,
                    workflows: batch.workflows.as_deref(),
                    origin: runs::Origin {
                        // The fork the pull request is from, for the card
                        // and the run's page; the run's trust stays the
                        // repository's own.
                        head_repo_id: pr.fork.as_ref().and_then(|fork| fork.head_repo_id),
                        // A deleted fork has no name; an empty one still
                        // marks the run as a fork's.
                        head_repo_name: pr
                            .fork
                            .as_ref()
                            .map(|fork| fork.head_repo_name.as_deref().unwrap_or("")),
                        author: &pr.author,
                        association: &pr.author_association,
                        gate,
                        target: Some(runs::Target {
                            head_sha: &pr.head_sha,
                            refused,
                        }),
                        ..runs::Origin::own(&pr.actor)
                    },
                },
                config,
                self.now,
            )
            .await?;
            ids.extend(started);
        }
        if gate != GateStatus::NotGated {
            self.ask_approval(pr, &ids, &policy).await?;
        }
        Ok((ids, refused))
    }

    /// The `pull_request` workflows of a pull request, read at the head,
    /// recorded under `delivery`; `wanted` says which workflows and as what
    /// action.
    #[allow(clippy::too_many_lines)] // One delivery, read top to bottom.
    async fn pull_request_head(
        &self,
        pr: &PullRequest,
        applied: bool,
        cache: &mut Option<RepoPolicy>,
        delivery: &str,
        wanted: Wanted<'_>,
        source: Source<'_>,
    ) -> Result<Outcome> {
        let Source {
            sha,
            config,
            parsed,
            changed,
        } = source;
        // A delivery older than what is recorded says nothing about the
        // pull request as it stands: it must not cancel the runs of the
        // newer commit as superseded, nor start runs of the older one.
        if pr.fork.is_some() && !applied {
            return Ok(Outcome::Ignored(
                "the delivery is older than the pull request's recorded state".to_owned(),
            ));
        }
        // Read after the stale check, which ends a delivery that has no
        // use for it.
        let fork_policy = if pr.fork.is_some() {
            Some(self.policy_of(pr, cache).await?)
        } else {
            None
        };
        let admitted = match self.fork_admission(pr, fork_policy).await? {
            Ok(admitted) => admitted,
            Err(outcome) => return Ok(outcome),
        };
        let compile_ref = runs::compile_ref_for(pr);
        let batches = wanted.batches(pr, parsed, |action| Event::PullRequest {
            action: action.to_owned(),
            base_ref: pr.base_ref.clone(),
            changed: changed.to_vec(),
        });
        let git_ref = format!("refs/pull/{}/merge", pr.number);
        let mut ids = Vec::new();
        for batch in &batches {
            let started = runs::create(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &runs::Request {
                    tenant_id: self.tenant,
                    installation_id: pr.installation_id,
                    repo: &pr.repo,
                    delivery,
                    event_name: "pull_request",
                    event: &batch.pr.payload,
                    trigger: &batch.trigger,
                    compile_ref: &compile_ref,
                    git_ref: &git_ref,
                    sha,
                    actor: &pr.actor,
                    head_ref: &pr.head_ref,
                    base_ref: &pr.base_ref,
                    workflows: batch.workflows.as_deref(),
                    origin: match (&admitted, &pr.fork) {
                        (Some((gate, policy)), Some(fork)) => runs::Origin {
                            trust: Trust::Fork,
                            head_repo_id: fork.head_repo_id,
                            head_repo_name: fork.head_repo_name.as_deref(),
                            author: &pr.author,
                            association: &pr.author_association,
                            gate: *gate,
                            policy: Some(*policy),
                            target: None,
                            chat: None,
                        },
                        _ => runs::Origin {
                            author: &pr.author,
                            association: &pr.author_association,
                            ..runs::Origin::own(&pr.actor)
                        },
                    },
                },
                config,
                self.now,
            )
            .await?;
            ids.extend(started);
        }
        if let Some((gate, policy)) = &admitted
            && *gate != GateStatus::NotGated
        {
            self.ask_approval(pr, &ids, policy).await?;
        }
        Ok(Outcome::Done(ids))
    }

    /// A release runs on its tag's commit. GitHub's payload names only
    /// the tag, so the commit is read. A tag that is gone (an
    /// unpublished or deleted release) starts nothing.
    async fn release(&self, release: &Release) -> Result<Outcome> {
        let (id, name) = (release.installation_id, &release.repo.full_name);
        let tag = format!("refs/tags/{}", release.tag);
        let found = runs::commit(self.tokens, self.settings, id, name, &tag).await?;
        let sha = match release_commit(found) {
            Ok(sha) => sha,
            Err(outcome) => return Ok(outcome),
        };
        let config = self.config_at(id, &release.repo, &sha).await?;
        self.fire(&release_fired(release, sha), &config).await
    }

    /// A created branch or tag runs on its own commit.
    async fn create(&self, change: &RefChange) -> Result<Outcome> {
        let id = change.installation_id;
        let Some(sha) = runs::commit(
            self.tokens,
            self.settings,
            id,
            &change.repo.full_name,
            &change.r#ref,
        )
        .await?
        else {
            return Ok(Outcome::Ignored("the created ref is gone".to_owned()));
        };
        let config = self.config_at(id, &change.repo, &sha).await?;
        self.fire(&create_fired(change, sha), &config).await
    }

    /// A deleted branch or tag has no commit, so as on GitHub the run is
    /// on the head of the default branch.
    async fn delete(&self, change: &RefChange) -> Result<Outcome> {
        let id = change.installation_id;
        let Some((sha, config)) =
            runs::default_head(self.tokens, self.settings, id, &change.repo).await?
        else {
            return Ok(Outcome::Ignored("the repository has no commits".to_owned()));
        };
        self.fire(&delete_fired(change, sha), &config).await
    }

    /// A merge group runs on its head commit, which the delivery names.
    async fn merge_group(&self, group: &MergeGroup) -> Result<Outcome> {
        let config = self
            .config_at(group.installation_id, &group.repo, &group.head_sha)
            .await?;
        self.fire(&merge_group_fired(group), &config).await
    }

    /// A repository dispatch runs on the head of the default branch, as
    /// on GitHub.
    async fn repository_dispatch(&self, dispatch: &RepositoryDispatch) -> Result<Outcome> {
        let id = dispatch.installation_id;
        let Some((sha, config)) =
            runs::default_head(self.tokens, self.settings, id, &dispatch.repo).await?
        else {
            return Ok(Outcome::Ignored("the repository has no commits".to_owned()));
        };
        self.fire(&dispatch_fired(dispatch, sha), &config).await
    }

    /// The login of this App's bot user, `<slug>[bot]`: the slug is
    /// read from `GET /app` with the App's JWT and kept in KV, where it
    /// never changes. Without it the loop guard could not tell, so a
    /// failure fails the delivery to be tried again.
    async fn bot_login(&self) -> Result<String> {
        let kv = match self.env.kv(BOT_LOGIN_BINDING) {
            Ok(kv) => Some(kv),
            Err(err) => {
                worker::console_warn!("app bot login: {BOT_LOGIN_BINDING} binding: {err}");
                None
            }
        };
        if let Some(kv) = &kv {
            match kv.get(BOT_LOGIN_KEY).text().await {
                Ok(Some(login)) if !login.is_empty() => return Ok(login),
                Ok(_) => {}
                Err(err) => worker::console_warn!("app bot login: cache: {err}"),
            }
        }
        let jwt = hatchrun_cloud::start::app_jwt(&self.settings.app_id, &self.settings.app_key)
            .await
            .map_err(fail)?;
        let response = hatchrun_cloud::fetch::send(&github::app(&self.settings.github_api, &jwt))
            .await
            .map_err(fail)?;
        let login = github::bot_login(&response).map_err(fail)?;
        if let Some(kv) = &kv {
            let stored = match kv.put(BOT_LOGIN_KEY, login.as_str()) {
                Ok(put) => put
                    .expiration_ttl(BOT_LOGIN_TTL_SECS)
                    .execute()
                    .await
                    .map_err(|err| err.to_string()),
                Err(err) => Err(err.to_string()),
            };
            if let Err(err) = stored {
                worker::console_warn!("app bot login: cache write: {err}");
            }
        }
        Ok(login)
    }

    /// Whether the delivery's sender `actor`, a `Bot`, is this App's bot
    /// user: what the App does starts no workflow, as an event of
    /// `GITHUB_TOKEN` does not on GitHub, so a step's comment cannot start
    /// another run. The caller asks only for a bot's delivery that some
    /// workflow would act on. A lookup that fails counts as the App's own
    /// and the delivery is skipped, logged and not failed: a loop is worse
    /// than a bot-triggered run that is missed.
    async fn is_own_app_sender(&self, actor: &str) -> bool {
        let login = match self.bot_login().await {
            Ok(login) => Some(login),
            Err(err) => {
                worker::console_error!(
                    "{actor}: the App's bot login is unknown, so its delivery is skipped: {err}"
                );
                None
            }
        };
        own_sender(actor, login.as_deref())
    }

    /// An issue activity runs on the head of the default branch, as on
    /// GitHub.
    async fn issues(&self, issue: &Issues) -> Result<Outcome> {
        let id = issue.installation_id;
        let Some((sha, config)) =
            runs::default_head(self.tokens, self.settings, id, &issue.repo).await?
        else {
            return Ok(Outcome::Ignored("the repository has no commits".to_owned()));
        };
        if issue.actor_is_bot {
            let event = Event::Issues {
                action: issue.action.clone(),
            };
            let listens = matches!(&config, Config::Text(Ok(source))
                if hatchrun_workflow::parse(source)
                    .is_ok_and(|workflows| planning::listens(&workflows, &event)));
            if !listens {
                return Ok(Outcome::Ignored(
                    "no workflow listens to this activity".to_owned(),
                ));
            }
            if self.is_own_app_sender(&issue.actor).await {
                return Ok(Outcome::Ignored(
                    "the activity is the hatchrun App's own".to_owned(),
                ));
            }
        }
        let chat = store::Chat {
            issue_number: issue.number,
            command: None,
        };
        self.fire_with(&issues_fired(issue, sha), &config, Some(&chat))
            .await
    }

    /// The pull request a comment is on, as GitHub has it now: the head
    /// a command is pinned to, and where the checks go.
    async fn comment_pull(&self, comment: &IssueComment) -> Result<Option<PullInfo>> {
        let response = self
            .tokens
            .send(comment.installation_id, |token| {
                github::pull_request(
                    &self.settings.github_api,
                    &comment.repo.full_name,
                    comment.number,
                    token,
                )
            })
            .await
            .map_err(|err| fail(err.to_string()))?;
        github::pull_info(&response).map_err(fail)
    }

    /// What the commenter holds on the repository, asked of GitHub once
    /// for the comment. `None` for no access at all.
    async fn commenter_role(&self, comment: &IssueComment) -> Result<Option<CommandPermission>> {
        let response = self
            .tokens
            .send(comment.installation_id, |token| {
                github::collaborator_permission(
                    &self.settings.github_api,
                    &comment.repo.full_name,
                    &comment.actor,
                    token,
                )
            })
            .await
            .map_err(|err| fail(err.to_string()))?;
        github::collaborator_role(&response).map_err(fail)
    }

    /// Posts `text` on the comment's issue or pull request as the App. A
    /// failure is logged: the runs are made, and the delivery does not
    /// fail for a reply.
    async fn reply(&self, comment: &IssueComment, text: &str) {
        let sent = self
            .tokens
            .send(comment.installation_id, |token| {
                github::create_comment(
                    &self.settings.github_api,
                    &comment.repo.full_name,
                    comment.number,
                    token,
                    text,
                )
            })
            .await;
        match sent {
            Ok(response) if response.ok() => {}
            Ok(response) => worker::console_warn!(
                "{}: reply on #{}: GitHub answered {}",
                comment.repo.full_name,
                comment.number,
                response.status
            ),
            Err(err) => worker::console_warn!(
                "{}: reply on #{}: {err}",
                comment.repo.full_name,
                comment.number
            ),
        }
    }

    /// Reacts to the comment as the App. A failure is logged, as for
    /// [`Self::reply`].
    async fn react(&self, comment: &IssueComment, content: &str) {
        let sent = self
            .tokens
            .send(comment.installation_id, |token| {
                github::add_reaction(
                    &self.settings.github_api,
                    &comment.repo.full_name,
                    comment.comment_id,
                    token,
                    content,
                )
            })
            .await;
        match sent {
            Ok(response) if response.ok() => {}
            Ok(response) => worker::console_warn!(
                "{}: reaction on comment {}: GitHub answered {}",
                comment.repo.full_name,
                comment.comment_id,
                response.status
            ),
            Err(err) => worker::console_warn!(
                "{}: reaction on comment {}: {err}",
                comment.repo.full_name,
                comment.comment_id
            ),
        }
    }

    /// A comment runs the `issue_comment` workflows on the head of the
    /// default branch, as on GitHub, and the chat commands it holds.
    ///
    /// The loop filter drops what the App itself wrote. A `created`
    /// comment from a person is read for commands (see
    /// [`Self::comment_commands`]); a bot's is not. The runs of one
    /// issue or pull request are capped, and a comment over the cap
    /// starts nothing.
    #[allow(clippy::too_many_lines)] // One comment, read top to bottom.
    async fn issue_comment(&self, comment: &IssueComment) -> Result<Outcome> {
        let id = comment.installation_id;
        let Some((sha, config)) =
            runs::default_head(self.tokens, self.settings, id, &comment.repo).await?
        else {
            return Ok(Outcome::Ignored("the repository has no commits".to_owned()));
        };
        // The config is the default branch's. One that is missing or
        // unusable has no workflow to run, and a comment is not the place
        // to report it: the push that made it was told. The built-in
        // `/approve-run` needs no config, so it is still read.
        let workflows = match &config {
            Config::Text(Ok(source)) => hatchrun_workflow::parse(source).ok(),
            _ => None,
        };
        let event = Event::IssueComment {
            action: comment.action.clone(),
        };
        let listens = workflows
            .as_ref()
            .is_some_and(|workflows| planning::listens(workflows, &event));
        let commands = if comment.action == "created" && !comment.actor_is_bot {
            let known = workflows
                .as_ref()
                .map(planning::command_names)
                .unwrap_or_default();
            hatchrun_workflow::command::parse_comment(&comment.body, |name| known.contains(name))
        } else {
            Vec::new()
        };
        if !listens && commands.is_empty() {
            return Ok(Outcome::Ignored(
                "no workflow listens to this comment".to_owned(),
            ));
        }
        // A bot's comment starts only `issue_comment` workflows, and
        // only when it is not the App's own.
        if listens && comment.actor_is_bot && self.is_own_app_sender(&comment.actor).await {
            return Ok(Outcome::Ignored(
                "the comment is the hatchrun App's own".to_owned(),
            ));
        }
        // The runs this delivery made already (a retry) are not counted,
        // so a retry is not refused for its own runs. Two comments that
        // arrive at once both read the count before either has made its
        // runs and can together pass the cap: that overshoot is accepted,
        // and the cap stays a soft one.
        let active =
            store::active_comment_runs(self.db, comment.repo.id, comment.number, self.delivery)
                .await?;
        let mut room = chatops::room(active);
        if room == 0 {
            worker::console_warn!(
                "{}: #{} has {active} active runs from comments, so comment {} starts none",
                comment.repo.full_name,
                comment.number,
                comment.comment_id
            );
            return Ok(Outcome::Ignored(
                "too many active runs for this issue".to_owned(),
            ));
        }
        // One call for the pull request's head, for the checks and for
        // the sha of a command.
        let pull = if comment.is_pull_request {
            match self.comment_pull(comment).await? {
                Some(pull) => Some(pull),
                None => return Ok(Outcome::Ignored("the pull request is gone".to_owned())),
            }
        } else {
            None
        };
        let default_ref = default_ref(&comment.repo);
        let chat = store::Chat {
            issue_number: comment.number,
            command: None,
        };
        let mut ids = Vec::new();
        if listens {
            let started = runs::create(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &runs::Request {
                    tenant_id: self.tenant,
                    installation_id: id,
                    repo: &comment.repo,
                    delivery: self.delivery,
                    event_name: "issue_comment",
                    event: &comment.payload,
                    trigger: &event,
                    compile_ref: &default_ref,
                    git_ref: &default_ref,
                    sha: &sha,
                    actor: &comment.actor,
                    head_ref: "",
                    base_ref: "",
                    workflows: None,
                    origin: comment_origin(comment, pull.as_ref(), &chat),
                },
                &config,
                self.now,
            )
            .await?;
            room = room.saturating_sub(started.len() as u64);
            ids.extend(started);
        }
        if !commands.is_empty() {
            let run = CommentRun {
                comment,
                workflows: workflows.as_ref(),
                config: &config,
                sha: &sha,
                default_ref: &default_ref,
                pull: pull.as_ref(),
            };
            ids.extend(self.comment_commands(&run, &commands, room).await?);
        }
        Ok(Outcome::Done(ids))
    }

    /// The chat commands of a created comment. One call asks GitHub for
    /// the commenter's role; a command whose workflow needs more starts
    /// nothing and is only logged, so a stranger learns nothing of which
    /// commands exist. A command that cannot run for want of a head sha
    /// (or has a bad line) gets one reply from the App; a command that
    /// started a run gets the `eyes` reaction.
    #[allow(clippy::too_many_lines)] // One comment, read top to bottom.
    async fn comment_commands(
        &self,
        run: &CommentRun<'_>,
        commands: &[Found],
        mut room: u64,
    ) -> Result<Vec<u64>> {
        let comment = run.comment;
        let role = self.commenter_role(comment).await?;
        let mut ids = Vec::new();
        let mut replies: Vec<String> = Vec::new();
        for found in commands {
            if found.name == APPROVE_RUN {
                ids.extend(self.approve_run(comment, found, role, run.pull).await?);
                continue;
            }
            // A command of the config is known only when the config
            // parsed.
            let Some(workflows) = run.workflows else {
                continue;
            };
            let defined =
                planning::command_workflows(workflows, &found.name, comment.is_pull_request);
            let allowed: Vec<_> = defined
                .into_iter()
                .filter(|wf| chatops::permits(role, wf.options.permission))
                .collect();
            let Some(role) = role.filter(|_| !allowed.is_empty()) else {
                worker::console_warn!(
                    "{}: {} may not run /{} on #{}",
                    comment.repo.full_name,
                    comment.actor,
                    found.name,
                    comment.number
                );
                continue;
            };
            let args = match &found.args {
                Ok(args) => args,
                Err(err) => {
                    push_reply(&mut replies, err.to_string());
                    continue;
                }
            };
            for wf in allowed {
                let head = match chatops::pin(&wf.options, args, &found.line, run.pull) {
                    Pin::Reply(text) => {
                        push_reply(&mut replies, text);
                        continue;
                    }
                    Pin::Run(head) => head,
                };
                if room == 0 {
                    worker::console_warn!(
                        "{}: #{} is at its cap of active runs, so /{} starts nothing",
                        comment.repo.full_name,
                        comment.number,
                        found.name
                    );
                    break;
                }
                let context = CommandContext {
                    name: &found.name,
                    line: &found.line,
                    words: &args.words,
                    names: &wf.options.args,
                    actor: &comment.actor,
                    role,
                    pull: run.pull.map(|pull| (comment.number, pull)),
                };
                let event = chatops::event_with_command(&comment.payload, &context);
                let chat = store::Chat {
                    issue_number: comment.number,
                    command: Some(store::CommentCommand {
                        comment_id: comment.comment_id,
                        name: found.name.clone(),
                        rocket: wf.options.reaction == CommandReaction::Rocket,
                    }),
                };
                let trigger = Event::Command {
                    name: found.name.clone(),
                    on_pull_request: comment.is_pull_request,
                };
                // One comment can carry several commands, and a workflow
                // makes one run for each delivery.
                let delivery = format!("{}:command:{}", self.delivery, found.name);
                let started = runs::create(
                    self.env,
                    self.db,
                    self.settings,
                    self.tokens,
                    &runs::Request {
                        tenant_id: self.tenant,
                        installation_id: comment.installation_id,
                        repo: &comment.repo,
                        delivery: &delivery,
                        event_name: "issue_comment",
                        event: &event,
                        trigger: &trigger,
                        compile_ref: run.default_ref,
                        git_ref: run.default_ref,
                        sha: run.sha,
                        actor: &comment.actor,
                        head_ref: "",
                        base_ref: "",
                        workflows: Some(std::slice::from_ref(&wf.workflow)),
                        origin: runs::Origin {
                            target: head.as_deref().map(|head_sha| runs::Target {
                                head_sha,
                                refused: None,
                            }),
                            ..comment_origin(comment, run.pull, &chat)
                        },
                    },
                    run.config,
                    self.now,
                )
                .await?;
                room = room.saturating_sub(started.len() as u64);
                ids.extend(started);
            }
        }
        if !ids.is_empty() {
            self.react(comment, chatops::ACCEPTED).await;
        }
        for text in &replies {
            self.reply(comment, text).await;
        }
        Ok(ids)
    }

    /// The built-in `/approve-run --sha=<head>`: a second way into the
    /// approval of a fork's pull request, with the checks of the check
    /// run's button. It is called for the comment's line, in the order
    /// of the comment, after the commenter's role was read and the pull
    /// request fetched, and returns the ids of the runs it approved or
    /// made.
    ///
    /// The commenter needs `write` and is told nothing below it. Then a
    /// plain issue, a closed pull request, a missing or stale pin, a
    /// pull request of this repository and fork runs turned off are
    /// answered with a reply. For a fork's pull request each workflow is
    /// decided on its own, for `pull_request` and for
    /// `pull_request_target` (see [`chatops::decide`]): the waiting
    /// runs at that head are approved (each approval is one statement
    /// that only moves a waiting run, and expired, rejected or
    /// superseded runs stay as they are); a run that is going is left
    /// alone; and a workflow whose runs all ended is made again as a
    /// delivery of its event makes it, counted against the same caps, and
    /// approved by this comment.
    #[allow(clippy::too_many_lines)] // One command, read top to bottom.
    async fn approve_run(
        &self,
        comment: &IssueComment,
        found: &Found,
        role: Option<CommandPermission>,
        pull: Option<&PullInfo>,
    ) -> Result<Vec<u64>> {
        if !chatops::permits(role, CommandPermission::Write) {
            worker::console_warn!(
                "{}: {} may not run /{} on #{}",
                comment.repo.full_name,
                comment.actor,
                found.name,
                comment.number
            );
            return Ok(Vec::new());
        }
        let args = match &found.args {
            Ok(args) => args,
            Err(err) => {
                self.reply(comment, &err.to_string()).await;
                return Ok(Vec::new());
            }
        };
        let Some(pull) = pull else {
            self.reply(comment, chatops::NO_HEAD_COMMIT).await;
            return Ok(Vec::new());
        };
        if pull.state != "open" {
            self.reply(comment, chatops::CLOSED_PULL_REQUEST).await;
            return Ok(Vec::new());
        }
        let head = match chatops::approve_pin(args, &found.line, Some(pull)) {
            Pin::Run(Some(head)) => head,
            Pin::Run(None) => return Ok(Vec::new()),
            Pin::Reply(text) => {
                self.reply(comment, &text).await;
                return Ok(Vec::new());
            }
        };
        let (repo_id, number) = (comment.repo.id, comment.number);
        let policy = store::repo_policy(self.db, repo_id, comment.repo.private).await?;
        let target_approves = target_waits(&policy, true);
        match chatops::unapprovable(pull.fork, policy.fork_runs, target_approves) {
            Some(Unapprovable::NotAFork) => {
                let made = store::pull_runs_at(self.db, repo_id, number, &head).await?;
                if made.is_empty() {
                    self.reply(comment, chatops::NOTHING_TO_APPROVE).await;
                } else {
                    self.react(comment, "+1").await;
                }
                return Ok(Vec::new());
            }
            Some(Unapprovable::ForkRunsOff) => {
                self.reply(comment, fork::FORK_RUNS_OFF).await;
                return Ok(Vec::new());
            }
            None => {}
        }
        // Fork runs off refuse the `pull_request` part only: it is never
        // approved or made again, and the `pull_request_target` part goes
        // on under the `approve` policy.
        let forks_run = policy.fork_runs != ForkRuns::Off;
        let events: &[&str] = if forks_run {
            &["pull_request", "pull_request_target"]
        } else {
            &["pull_request_target"]
        };
        let mut policy = Some(policy);
        // The pull request as it stands now: a push since the runs
        // were made supersedes what waits at older commits, and an
        // approval is only given while it is open at the run's commit.
        let pr = pull_of_comment(comment, pull);
        let record = pull_record(&pr);
        // A snapshot that is not the newest word on the pull request
        // (older than the row, or tied with it) never overwrites the row
        // and cannot decide which runs to cancel. The row decides: the
        // command goes on only if it is open at the pinned head, and says
        // why it stops otherwise.
        if store::upsert_pull(self.db, &record).await? != store::PullWrite::Applied
            && self.stops_on_row(comment, &head, &found.line).await?
        {
            return Ok(Vec::new());
        }
        // The cancelling checks the row in its own statement: a newer
        // head recorded since the pin was checked is not superseded by
        // the pinned one. Nothing ended is also the case of nothing
        // waiting, so the row is read again to tell the two apart.
        if !self
            .end_waiting_at_head(&pr, &head, fork::SUPERSEDED)
            .await?
            && self.stops_on_row(comment, &head, &found.line).await?
        {
            return Ok(Vec::new());
        }
        // Approving needs no config, so what waits is approved before
        // anything is read from GitHub.
        let approved = self.approve_head(comment, &head, events).await?;
        // From here on, an error does not fail the delivery once runs
        // were approved: they stay approved, and a retry of the comment
        // would only say the same again.
        let remade = async {
            let mut ids = approved.clone();
            let made: Vec<_> = store::pull_runs_at(self.db, repo_id, number, &head)
                .await?
                .into_iter()
                .filter(|run| events.contains(&run.event.as_str()))
                .collect();
            // What each event may make again comes from the config it
            // runs from, the head's for `pull_request` and the base
            // branch's, at its head commit, for `pull_request_target`,
            // and from its runs at the head.
            let read_head = async {
                if forks_run {
                    self.config_at(comment.installation_id, &comment.repo, &head)
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            };
            let read_base = async {
                if target_approves {
                    self.base_config(&pr).await
                } else {
                    Ok(None)
                }
            };
            let (head_config, base) = futures_util::future::join(read_head, read_base).await;
            let (head_config, base) = (head_config?, base?);
            let head_parsed = head_config.as_ref().map_or(Ok(None), parsed);
            let base_parsed = base.as_ref().map_or(Ok(None), |(_, config)| parsed(config));
            let mut unusable = Vec::new();
            if head_parsed.is_err() {
                unusable.push(chatops::invalid_head_config(&head));
            }
            if base_parsed.is_err() {
                let sha = base.as_ref().map(|(sha, _)| sha.as_str());
                unusable.push(chatops::invalid_base_config(&pr.base_ref, sha));
            }
            let (head_parsed, base_parsed) =
                (head_parsed.unwrap_or(None), base_parsed.unwrap_or(None));
            let has_runs = |event: &str| made.iter().any(|run| run.event == event);
            let head_actions = planning::remake_actions(has_runs("pull_request"));
            let target_actions = planning::remake_actions(has_runs("pull_request_target"));
            let decide = |changed: &[String]| {
                (
                    chatops::decide(
                        &listed(
                            head_parsed.as_ref(),
                            head_event(&pr.base_ref, changed),
                            head_actions,
                        ),
                        &made,
                        "pull_request",
                        self.now,
                    ),
                    chatops::decide(
                        &listed(
                            base_parsed.as_ref(),
                            target_event(&pr.base_ref, changed),
                            target_actions,
                        ),
                        &made,
                        "pull_request_target",
                        self.now,
                    ),
                )
            };
            let (mut head_decision, mut target_decision) = decide(&[]);
            // The files are fetched once, and only when a workflow that
            // would be made again filters on paths; the listing is then
            // redone with them, so a workflow the files do not trigger
            // is not made.
            let wants_paths = |parsed: &Option<hatchrun_workflow::Config>,
                               names: &[String],
                               actions: &[&str],
                               target: bool| {
                parsed.as_ref().is_some_and(|parsed| {
                    actions.iter().any(|action| {
                        if target {
                            planning::needs_pull_request_target_paths(
                                parsed,
                                action,
                                &pr.base_ref,
                                Some(names),
                            )
                        } else {
                            planning::needs_pull_request_paths(
                                parsed,
                                action,
                                &pr.base_ref,
                                Some(names),
                            )
                        }
                    })
                })
            };
            let mut changed = Vec::new();
            if wants_paths(&head_parsed, &head_decision.remake, head_actions, false)
                || wants_paths(&base_parsed, &target_decision.remake, target_actions, true)
            {
                changed = self.pull_paths(&pr).await?;
                (head_decision, target_decision) = decide(&changed);
            }
            let remakes = !head_decision.remake.is_empty() || !target_decision.remake.is_empty();
            // An unusable config is answered with its own reply, ahead of
            // any other.
            for text in &unusable {
                self.reply(comment, text).await;
            }
            if !remakes && ids.is_empty() {
                if !unusable.is_empty() {
                    self.react(comment, "-1").await;
                } else if !forks_run {
                    self.reply(comment, fork::FORK_RUNS_OFF).await;
                } else if made.is_empty() {
                    self.reply(comment, chatops::NOTHING_TO_RUN).await;
                    self.react(comment, "-1").await;
                } else {
                    self.react(comment, "+1").await;
                }
                return Ok(Vec::new());
            }
            self.react(comment, chatops::ACCEPTED).await;
            if remakes {
                // The comment's delivery made no run of these workflows
                // before, or a retry of it finds the ones it made (a run
                // is one per delivery and workflow) and makes the rest.
                // One call for each event makes all of its workflows, so
                // the admission is counted once for the group. The make
                // calls do not retry: a failure is an error, and the
                // delivery is not asked to come again.
                let delivery = format!("{}:approve-run", self.delivery);
                let mut made_ids = Vec::new();
                let mut refusals: Vec<String> = Vec::new();
                if !head_decision.remake.is_empty()
                    && let Some(config) = &head_config
                {
                    let source = Source {
                        sha: &head,
                        config,
                        parsed: head_parsed.as_ref(),
                        changed: &changed,
                    };
                    let wanted = Wanted::Remake {
                        among: &head_decision.remake,
                        actions: head_actions,
                    };
                    match self
                        .pull_request_head(&pr, true, &mut policy, &delivery, wanted, source)
                        .await?
                    {
                        Outcome::Done(started) => made_ids.extend(started),
                        Outcome::Ignored(reason) => {
                            worker::console_warn!(
                                "{}: /approve-run on #{number} made no run: {reason}",
                                comment.repo.full_name
                            );
                            refusals.push(chatops::refusal_reply(&reason).to_owned());
                        }
                        // The make calls never ask for a retry.
                        Outcome::Retry => {}
                    }
                }
                if !target_decision.remake.is_empty()
                    && let Some((sha, config)) = &base
                {
                    let source = Source {
                        sha,
                        config,
                        parsed: base_parsed.as_ref(),
                        changed: &changed,
                    };
                    let wanted = Wanted::Remake {
                        among: &target_decision.remake,
                        actions: target_actions,
                    };
                    let (started, refused) = self
                        .pull_request_target(&pr, true, &mut policy, &delivery, wanted, source)
                        .await?;
                    made_ids.extend(started);
                    refusals
                        .extend(refused.map(|reason| chatops::refusal_reply(reason).to_owned()));
                }
                refusals.dedup();
                for text in &refusals {
                    self.reply(comment, text).await;
                }
                // The new runs wait for an approval, and this comment is
                // it.
                if !made_ids.is_empty() {
                    ids.extend(self.approve_head(comment, &head, events).await?);
                }
                ids.extend(made_ids);
                if ids.is_empty() {
                    // Nothing could be made: no workflow has a plan for
                    // the actions it is made as.
                    if refusals.is_empty() {
                        self.reply(comment, chatops::NOTHING_TO_RUN).await;
                    }
                    self.react(comment, "-1").await;
                    return Ok(Vec::new());
                }
            }
            ids.sort_unstable();
            ids.dedup();
            self.react(comment, "+1").await;
            Ok::<_, worker::Error>(ids)
        }
        .await;
        match remade {
            Err(err) if !approved.is_empty() => {
                worker::console_error!(
                    "{}: /approve-run on #{number} approved runs, then failed: {err}",
                    comment.repo.full_name
                );
                self.reply(comment, chatops::REMAKE_FAILED).await;
                self.react(comment, "-1").await;
                self.react(comment, "+1").await;
                Ok(approved)
            }
            other => other,
        }
    }

    /// Approves the waiting runs of `events` at `head` for the commenter
    /// and has the cards updated. Returns the ids of the runs approved.
    async fn approve_head(
        &self,
        comment: &IssueComment,
        head: &str,
        events: &[&str],
    ) -> Result<Vec<u64>> {
        let approved = store::approve_at_head(
            self.db,
            self.tenant,
            comment.repo.id,
            comment.number,
            head,
            &comment.actor,
            self.now,
            events,
        )
        .await?;
        if !approved.is_empty()
            && let Some(repo) = store::repo(self.db, comment.repo.id).await?
        {
            fork::after_approval(
                self.env,
                self.db,
                self.settings,
                self.tokens,
                &repo,
                &approved,
                &comment.actor,
            )
            .await;
        }
        Ok(approved.iter().map(|run| run.id).collect())
    }

    /// The workflow config at `sha` of `repo`, read from GitHub.
    async fn config_at(&self, installation_id: u64, repo: &Repo, sha: &str) -> Result<Config> {
        runs::read_config(
            self.tokens,
            self.settings,
            installation_id,
            &repo.full_name,
            sha,
        )
        .await
    }

    /// Starts the workflows that `fired.trigger` matches in `config`,
    /// compiled for `fired.git_ref`.
    async fn fire(&self, fired: &Fired<'_>, config: &Config) -> Result<Outcome> {
        self.fire_with(fired, config, None).await
    }

    /// [`Self::fire`], for runs tied to an issue (`chat`).
    async fn fire_with(
        &self,
        fired: &Fired<'_>,
        config: &Config,
        chat: Option<&store::Chat>,
    ) -> Result<Outcome> {
        let ids = runs::create(
            self.env,
            self.db,
            self.settings,
            self.tokens,
            &runs::Request {
                tenant_id: self.tenant,
                installation_id: fired.installation_id,
                repo: fired.repo,
                delivery: self.delivery,
                event_name: fired.event_name,
                event: fired.payload,
                trigger: &fired.trigger,
                compile_ref: &fired.git_ref,
                git_ref: &fired.git_ref,
                sha: &fired.sha,
                actor: fired.actor,
                head_ref: "",
                base_ref: "",
                workflows: None,
                origin: runs::Origin {
                    chat,
                    ..runs::Origin::own(fired.actor)
                },
            },
            config,
            self.now,
        )
        .await?;
        Ok(Outcome::Done(ids))
    }

    async fn rerun(&self, rerun: &Rerequested) -> Result<Outcome> {
        let Some(job) = store::job(self.db, rerun.job_id).await? else {
            return Ok(Outcome::Ignored("check run of an unknown job".to_owned()));
        };
        let Some(run) = store::run(self.db, job.run_id).await? else {
            return Ok(Outcome::Ignored("check run of an unknown run".to_owned()));
        };
        if run.repo_id != rerun.repo.id || run.tenant_id != self.tenant {
            return Ok(Outcome::Ignored(
                "check run of another repository".to_owned(),
            ));
        }
        // A run whose approval ended (a fork run, or a pull_request_target
        // run of a fork under an approving policy), or a fork run of a
        // repository that turned fork runs off, is not rerun: nothing is
        // reset, and the commit says why in one check run that later
        // refusals update. The not-approved reason needs only the run's
        // row.
        let repo = store::repo(self.db, run.repo_id).await?;
        let policy = match &repo {
            Some(repo) => Some(store::repo_policy(self.db, repo.id, repo.private).await?),
            None => None,
        };
        if let Some(reason) = fork::rerun_refusal(&run, policy.as_ref()) {
            if let Some(repo) = &repo {
                fork::say_rerun_refused(self.tokens, self.settings, repo, &run, reason).await;
            }
            return Ok(Outcome::Ignored(reason.to_owned()));
        }
        let url = format!(
            "https://run/rerun?job={}&run={}&tenant={}&days={}",
            job.id, run.id, run.tenant_id, run.retention_days
        );
        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        let request = worker::Request::new_with_init(&url, &init)?;
        let response = run_stub(self.env, run.id)?
            .fetch_with_request(request)
            .await?;
        match response.status_code() {
            // 204 means nothing to reset, such as a job that still runs.
            200..=299 => Ok(Outcome::Done(vec![run.id])),
            // The earlier attempt's log is still uploading.
            409 => Ok(Outcome::Retry),
            404 => Ok(Outcome::Ignored("run is no longer held".to_owned())),
            status => Err(fail(format!("run {}: rerun answered {status}", run.id))),
        }
    }
}

#[cfg(test)]
mod tests {
    /// The repository the tests deliver for.
    fn repo() -> Repo {
        Repo {
            id: 4242,
            full_name: "octo/widgets".into(),
            default_branch: "main".into(),
            private: false,
        }
    }

    /// A push of `ref` by the test actor.
    fn ref_change(r#ref: &str) -> RefChange {
        RefChange {
            installation_id: 7,
            repo: repo(),
            r#ref: r#ref.into(),
            actor: "octocat".into(),
            payload: json!({ "ref": r#ref }),
        }
    }

    /// The pull request a `pull_request` delivery of `body` reads as.
    fn pull(body: &str) -> PullRequest {
        match intake::parse("pull_request", body.as_bytes()).unwrap() {
            Trigger::PullRequest(pr) => pr,
            other => panic!("not a pull request: {other:?}"),
        }
    }

    #[test]
    fn a_missing_config_is_kept_apart_from_an_unusable_one() {
        assert!(matches!(parsed(&Config::Missing), Ok(None)));
        let good = "workflows:\n  w:\n    on: { push: {} }\n    jobs: [t]\n";
        assert!(matches!(
            parsed(&Config::Text(Ok(good.to_owned()))),
            Ok(Some(_))
        ));
        assert!(parsed(&Config::Text(Ok("not: [valid".to_owned()))).is_err());
    }

    #[test]
    fn a_remake_without_a_parsed_config_makes_nothing() {
        let pr = pull(include_str!("../testdata/pull_request_fork.json"));
        let wanted = Wanted::Remake {
            among: &[],
            actions: planning::remake_actions(false),
        };
        assert!(
            wanted
                .batches(&pr, None, head_event("main", &[]))
                .is_empty()
        );
    }

    #[test]
    fn approve_run_is_found_in_a_comment_without_any_config() {
        // A config that is missing or does not parse knows no command,
        // and the built-in one is still read.
        let sha = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
        let body = format!("/retest\n/approve-run --sha={sha}");
        let found = hatchrun_workflow::command::parse_comment(&body, |_| false);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, [APPROVE_RUN]);
    }

    #[test]
    fn a_bot_login_that_cannot_be_read_skips_the_delivery() {
        assert!(own_sender("hatchrun[bot]", Some("hatchrun[bot]")));
        assert!(!own_sender("dependabot[bot]", Some("hatchrun[bot]")));
        // The lookup failed: the delivery counts as the App's own.
        assert!(own_sender("dependabot[bot]", None));
    }

    #[test]
    fn only_the_apps_own_bot_login_is_its_sender() {
        assert!(is_bot_login("hatchrun[bot]", "hatchrun[bot]"));
        assert!(is_bot_login("Hatchrun[bot]", "hatchrun[bot]"));
        // Another App's bot, a person who made the App's slug their
        // name, and the slug without the suffix are not it.
        assert!(!is_bot_login("dependabot[bot]", "hatchrun[bot]"));
        assert!(!is_bot_login("hatchrun", "hatchrun[bot]"));
        assert!(!is_bot_login("octocat", "hatchrun[bot]"));
    }

    #[test]
    fn a_pull_request_target_run_is_on_the_base_branch_ref_whatever_the_head_is() {
        let fork = pull(include_str!("../testdata/pull_request_fork.json"));
        assert!(fork.fork.is_some());
        // The ref is the base branch's; the commit is looked up on it
        // when the delivery is handled, never taken from the payload (a
        // PullRequest has no base commit to take) or from the head.
        let git_ref = runs::compile_ref_for(&fork);
        assert_eq!(git_ref, "refs/heads/main");
        assert!(!git_ref.contains("pull"), "{git_ref}");
    }

    fn approving() -> RepoPolicy {
        RepoPolicy {
            pull_request_target_head_checkout: policy::HeadCheckout::Approve,
            ..RepoPolicy::default_for(false)
        }
    }

    #[test]
    fn under_approve_a_forks_target_run_waits_and_the_same_repositorys_does_not() {
        let free = Waiting::default();
        // A fork's pull request waits whoever wrote it.
        assert_eq!(
            target_gate(&approving(), true, true, true, free),
            TargetGate::Start(GateStatus::Pending)
        );
        // The repository's own pull request runs at once.
        assert_eq!(
            target_gate(&approving(), false, true, true, free),
            TargetGate::Start(GateStatus::NotGated)
        );
        // Under warn nothing waits, fork or not, closed or not.
        let warn = RepoPolicy::default_for(false);
        for (fork, open, applied) in [
            (true, true, true),
            (true, false, false),
            (false, true, true),
        ] {
            assert_eq!(
                target_gate(&warn, fork, open, applied, free),
                TargetGate::Start(GateStatus::NotGated)
            );
        }
        assert!(target_waits(&approving(), true));
        assert!(!target_waits(&approving(), false));
        assert!(!target_waits(&warn, true));
    }

    #[test]
    fn a_closed_pull_request_is_refused_visibly_and_a_stale_delivery_is_skipped() {
        let free = Waiting::default();
        // Never run unapproved: the closed pull request's workflows are
        // refused with a reason the head shows.
        assert!(matches!(
            target_gate(&approving(), true, false, true, free),
            TargetGate::Refused(reason) if reason.contains("closed")
        ));
        assert_eq!(
            target_gate(&approving(), true, true, false, free),
            TargetGate::Stale
        );
        // Neither applies to a run that does not wait.
        assert_eq!(
            target_gate(&approving(), false, false, false, free),
            TargetGate::Start(GateStatus::NotGated)
        );
    }

    #[test]
    fn a_target_run_waits_for_a_slot_and_is_refused_past_the_caps() {
        let full = Waiting {
            pending: policy::MAX_PENDING,
            ..Waiting::default()
        };
        assert_eq!(
            target_gate(&approving(), true, true, true, full),
            TargetGate::Start(GateStatus::Deferred)
        );
        let flooded = Waiting {
            deferred: policy::MAX_DEFERRED,
            ..full
        };
        assert!(matches!(
            target_gate(&approving(), true, true, true, flooded),
            TargetGate::Refused(reason) if reason.contains("too many")
        ));
        // Fork runs being off does not turn a target run away.
        let off = RepoPolicy {
            fork_runs: policy::ForkRuns::Off,
            ..approving()
        };
        assert_eq!(
            target_gate(&off, true, true, true, Waiting::default()),
            TargetGate::Start(GateStatus::Pending)
        );
    }

    #[test]
    fn the_target_delivery_key_differs_from_the_deliverys() {
        assert_ne!(target_delivery("guid"), "guid");
        assert_eq!(target_delivery("guid"), "guid:target");
    }

    #[test]
    fn a_release_runs_on_its_tag_at_the_tags_commit() {
        let release = Release {
            installation_id: 7,
            repo: repo(),
            action: "published".into(),
            tag: "v1.2.0".into(),
            actor: "octocat".into(),
            payload: json!({ "action": "published" }),
        };
        let fired = release_fired(&release, "abc".into());
        assert_eq!(fired.event_name, "release");
        assert_eq!(fired.git_ref, "refs/tags/v1.2.0");
        assert_eq!(fired.sha, "abc");
        assert_eq!(fired.actor, "octocat");
        assert_eq!(fired.installation_id, 7);
        assert_eq!(fired.payload, &release.payload);
        assert_eq!(
            fired.trigger,
            Event::Release {
                action: "published".into(),
                tag: "v1.2.0".into()
            }
        );
    }

    #[test]
    fn a_created_ref_runs_on_itself_and_a_deleted_one_on_the_default_branch() {
        let created = ref_change("refs/heads/topic/x");
        let fired = create_fired(&created, "def".into());
        assert_eq!(fired.event_name, "create");
        assert_eq!(fired.git_ref, "refs/heads/topic/x");
        assert_eq!(fired.sha, "def");
        assert_eq!(fired.trigger, Event::Create);

        let deleted = ref_change("refs/tags/v0");
        let fired = delete_fired(&deleted, "head".into());
        assert_eq!(fired.event_name, "delete");
        assert_eq!(fired.git_ref, "refs/heads/main");
        assert_eq!(fired.sha, "head");
        assert_eq!(fired.trigger, Event::Delete);
        assert_eq!(fired.payload, &deleted.payload);
    }

    #[test]
    fn a_merge_group_runs_on_its_own_ref_at_its_head_commit() {
        let group = MergeGroup {
            installation_id: 7,
            repo: repo(),
            action: "checks_requested".into(),
            head_sha: "abc".into(),
            head_ref: "refs/heads/gh-readonly-queue/main/pr-5-def".into(),
            actor: "octocat".into(),
            payload: json!({ "action": "checks_requested" }),
        };
        let fired = merge_group_fired(&group);
        assert_eq!(fired.event_name, "merge_group");
        assert_eq!(fired.git_ref, "refs/heads/gh-readonly-queue/main/pr-5-def");
        assert_eq!(fired.sha, "abc");
        assert_eq!(fired.actor, "octocat");
        assert_eq!(fired.payload, &group.payload);
        assert_eq!(
            fired.trigger,
            Event::MergeGroup {
                action: "checks_requested".into()
            }
        );
    }

    #[test]
    fn a_repository_dispatch_runs_on_the_default_branch_head() {
        let dispatch = RepositoryDispatch {
            installation_id: 7,
            repo: repo(),
            action: "deploy".into(),
            actor: "octocat".into(),
            payload: json!({ "action": "deploy", "client_payload": { "x": 1 } }),
        };
        let fired = dispatch_fired(&dispatch, "head".into());
        assert_eq!(fired.event_name, "repository_dispatch");
        assert_eq!(fired.git_ref, "refs/heads/main");
        assert_eq!(fired.sha, "head");
        assert_eq!(fired.payload["client_payload"]["x"], 1);
        assert_eq!(
            fired.trigger,
            Event::RepositoryDispatch {
                action: "deploy".into()
            }
        );
    }

    #[test]
    fn a_release_whose_tag_is_gone_starts_nothing() {
        assert!(matches!(
            release_commit(None),
            Err(Outcome::Ignored(reason)) if reason == "release tag is gone"
        ));
        assert!(matches!(release_commit(Some("abc".into())), Ok(sha) if sha == "abc"));
    }

    use hatchrun_cloud::db::params;
    use hatchrun_cloud::testdb::Db as TestDb;

    use super::*;
    use crate::store::CiBilling;

    fn tenant_db() -> TestDb {
        let db = TestDb::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, \
             updated_at) VALUES ('t1', 1, 'acme', 99, 0, 0)",
            &[],
        );
        db
    }

    fn subscribe(db: &TestDb, status: &str) {
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES ('s1', 't1', $1, 'c', 'p', false, 't', 0)",
            &params![status],
        );
    }

    fn found(db: &TestDb) -> Option<TenantRow> {
        db.wait(store::tenant_for_installation(db.db(), 99))
            .unwrap()
    }

    #[test]
    fn unknown_installation_is_ignored() {
        let db = tenant_db();
        let none = db.wait(store::tenant_for_installation(db.db(), 1)).unwrap();
        assert_eq!(gate(none).unwrap_err(), "no tenant for this installation");
    }

    #[test]
    fn never_subscribed_tenant_passes_to_get_the_card_check_run() {
        let db = tenant_db();
        assert_eq!(gate(found(&db)).unwrap().ci_billing, CiBilling::Never);
    }

    #[test]
    fn beta_tenant_passes_the_gate_as_active() {
        let db = tenant_db();
        db.run("UPDATE tenants SET beta = true", &[]);
        assert_eq!(gate(found(&db)).unwrap().ci_billing, CiBilling::Active);
    }

    #[test]
    fn subscribed_and_lapsed_tenants_pass_the_gate() {
        let db = tenant_db();
        subscribe(&db, "active");
        assert_eq!(gate(found(&db)).unwrap().ci_billing, CiBilling::Active);
        db.run("UPDATE subscriptions SET status = 'canceled'", &[]);
        assert_eq!(gate(found(&db)).unwrap().ci_billing, CiBilling::Lapsed);
    }
}
