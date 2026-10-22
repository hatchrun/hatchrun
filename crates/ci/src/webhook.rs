// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /github/webhook`: the `push`, `pull_request` and `check_run`
//! deliveries that `hatchrun-cloud` forwards.

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_cloud::webhook::verify;
use hatchrun_workflow::{Event, MAX_CHANGED};
use serde_json::json;
use worker::{Env, Method, Request, RequestInit, Response, Result};

use crate::env::{Settings, run_stub};
use crate::github;
use crate::intake::{self, PullRequest, Push, Rerequested, Trigger};
use crate::planning;
use crate::runs::{self, Config};
use crate::store::{self, CiBilling, RepoRow, TenantRow};

/// Secret holding the App's webhook secret.
const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";

/// Pages of a pull request's files that are read, 100 files each.
const FILE_PAGES: u32 = 3;

/// Files GitHub lists on a full page.
const PAGE_SIZE: usize = 100;

const NEVER_SUBSCRIBED: &str = "this organization has no hatchrun CI subscription";

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

/// The tenant of a delivery, or why the delivery is ignored. A tenant
/// that never had a CI subscription is ignored before anything reads
/// its config; one whose subscription lapsed passes, and
/// [`runs::create`] answers it with the card check run.
fn gate(tenant: Option<TenantRow>) -> std::result::Result<TenantRow, &'static str> {
    match tenant {
        None => Err("no tenant for this installation"),
        Some(tenant) if tenant.ci_billing == CiBilling::Never => Err(NEVER_SUBSCRIBED),
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
    let secret = env
        .secret(WEBHOOK_SECRET)
        .map(|secret| secret.to_string())
        .unwrap_or_default();
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
    let (installation_id, repo) = match &trigger {
        Trigger::Ignored(reason) => return reply(&Outcome::Ignored(reason.clone())),
        Trigger::Push(p) => (p.installation_id, &p.repo),
        Trigger::PullRequest(p) => (p.installation_id, &p.repo),
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
        Trigger::Rerequested(rerun) => cx.rerun(rerun).await?,
        // The row was updated above, with the tenant's other repository
        // writes; nothing runs.
        Trigger::Repository(_) => Outcome::Done(Vec::new()),
        Trigger::Ignored(reason) => Outcome::Ignored(reason.clone()),
    };
    if !matches!(outcome, Outcome::Retry) {
        store::record_delivery(&db, guid, kind, now).await?;
    }
    reply(&outcome)
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
            "https://run/rerun?job={}&run={}&tenant={}",
            job.id, run.id, run.tenant_id
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
    use hatchrun_cloud::db::params;
    use hatchrun_cloud::testdb::Db as TestDb;

    use super::*;

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
             product, cancel_at_period_end, modified_at, updated_at) \
             VALUES ('s1', 't1', $1, 'c', 'p', 'ci', false, 't', 0)",
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
    fn never_subscribed_tenant_is_ignored_before_the_config() {
        let db = tenant_db();
        assert_eq!(gate(found(&db)).unwrap_err(), NEVER_SUBSCRIBED);
        // A runners subscription is not a CI one.
        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             product, cancel_at_period_end, modified_at, updated_at) \
             VALUES ('r1', 't1', 'active', 'c', 'p', 'runners', false, 't', 0)",
            &[],
        );
        assert_eq!(gate(found(&db)).unwrap_err(), NEVER_SUBSCRIBED);
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
