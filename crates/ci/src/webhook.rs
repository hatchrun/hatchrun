// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /github/webhook`: the `push`, `pull_request`, `check_run`,
//! `release`, `create`, `delete`, `repository` and
//! `installation_repositories` deliveries that `hatchrun-cloud`
//! forwards.
//!
//! A repository that is not enabled for its tenant (see
//! [`crate::installation`]) starts no runs: its deliveries are
//! answered as ignored.

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_cloud::webhook::verify;
use hatchrun_workflow::{Event, MAX_CHANGED};
use serde_json::{Value, json};
use worker::{Env, Method, Request, RequestInit, Response, Result};

use crate::env::{Settings, run_stub};
use crate::github;
use crate::installation;
use crate::intake::{self, PullRequest, Push, RefChange, Release, Repo, Rerequested, Trigger};
use crate::planning;
use crate::runs::{self, Config};
use crate::store::{self, RepoRow, TenantRow};

/// Secret holding the App's webhook secret.
const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";

/// Pages of a pull request's files that are read, 100 files each.
const FILE_PAGES: u32 = 3;

/// Files GitHub lists on a full page.
const PAGE_SIZE: usize = 100;

/// The pull request actions that change what the repository page shows.
const RECORDED_ACTIONS: [&str; 5] = ["opened", "reopened", "synchronize", "edited", "closed"];

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
        Trigger::Rerequested(r) => (r.installation_id, &r.repo),
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
    let outcome = match &trigger {
        Trigger::Push(push) => cx.push(push).await?,
        Trigger::PullRequest(pr) => cx.pull_request(pr).await?,
        Trigger::Release(release) => cx.release(release).await?,
        Trigger::Create(change) => cx.create(change).await?,
        Trigger::Delete(change) => cx.delete(change).await?,
        Trigger::Rerequested(rerun) => cx.rerun(rerun).await?,
        // The row was updated above, with the tenant's other repository
        // writes; nothing runs.
        Trigger::Repository(_) => Outcome::Done(Vec::new()),
        Trigger::Ignored(reason) => Outcome::Ignored(reason.clone()),
        Trigger::InstallationRepositories { .. } => unreachable!("answered above"),
    };
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
                workflow: None,
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

    async fn pull_request(&self, pr: &PullRequest) -> Result<Outcome> {
        if RECORDED_ACTIONS.contains(&pr.action.as_str()) {
            store::upsert_pull(
                self.db,
                &store::NewPull {
                    repo_id: pr.repo.id,
                    number: pr.number,
                    title: pr.title.clone(),
                    head_ref: pr.head_ref.clone(),
                    base_ref: pr.base_ref.clone(),
                    author: pr.author.clone(),
                    state: pr.state.to_owned(),
                    now: self.now,
                },
            )
            .await?;
        }
        let config = runs::read_config(
            self.tokens,
            self.settings,
            pr.installation_id,
            &pr.repo.full_name,
            &pr.head_sha,
        )
        .await?;
        let compile_ref = runs::compile_ref_for(pr);
        // Files cost up to three calls, so they are read only for a
        // workflow that listens to this action and filters on paths.
        let wants_paths = matches!(&config, Config::Text(Ok(source))
            if planning::needs_pull_request_paths(source, &pr.action, &pr.base_ref));
        let mut changed = Vec::new();
        if wants_paths {
            for page in 1..=FILE_PAGES {
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
                let full = names.len() >= PAGE_SIZE;
                changed.extend(names);
                if !full || changed.len() >= MAX_CHANGED {
                    break;
                }
            }
            changed.truncate(MAX_CHANGED);
        }
        let trigger = Event::PullRequest {
            action: pr.action.clone(),
            base_ref: pr.base_ref.clone(),
            changed,
        };
        let git_ref = format!("refs/pull/{}/merge", pr.number);
        let ids = runs::create(
            self.env,
            self.db,
            self.settings,
            self.tokens,
            &runs::Request {
                tenant_id: self.tenant,
                installation_id: pr.installation_id,
                repo: &pr.repo,
                delivery: self.delivery,
                event_name: "pull_request",
                event: &pr.payload,
                trigger: &trigger,
                compile_ref: &compile_ref,
                git_ref: &git_ref,
                sha: &pr.head_sha,
                actor: &pr.actor,
                head_ref: &pr.head_ref,
                base_ref: &pr.base_ref,
                workflow: None,
            },
            &config,
            self.now,
        )
        .await?;
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
                workflow: None,
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
    fn repo() -> Repo {
        Repo {
            id: 4242,
            full_name: "octo/widgets".into(),
            default_branch: "main".into(),
            private: false,
        }
    }

    fn ref_change(r#ref: &str) -> RefChange {
        RefChange {
            installation_id: 7,
            repo: repo(),
            r#ref: r#ref.into(),
            actor: "octocat".into(),
            payload: json!({ "ref": r#ref }),
        }
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
