// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker's entry points: `fetch`, `scheduled` and `queue`. Another
//! Worker that reuses this crate turns the `worker` feature off and
//! defines its own.

use worker::{
    Env, Headers, MessageBatch, Method, Request, Response, ResponseBuilder, Result,
    ScheduleContext, ScheduledEvent, console_error, console_warn, event,
};

use crate::db::{Db, params};
use crate::jwt::PublicKey;
use crate::webhook::{Event, Placement, QueuedJob};
use crate::{
    alerts, app, cache, jobs, jwt, labels, launches, now_ms, oidc, profiles, reconcile, runs,
    usage, webhook,
};

/// Static assets binding in `wrangler.toml`.
const ASSETS: &str = "ASSETS";
/// Headers on every page of the app. The app loads nothing from other
/// origins except GitHub avatars, and no other site may frame it.
const SECURITY_HEADERS: [(&str, &str); 5] = [
    (
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' https://avatars.githubusercontent.com data:; \
         style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'self'",
    ),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "strict-origin-when-cross-origin"),
    (
        "Strict-Transport-Security",
        "max-age=31536000; includeSubDomains",
    ),
];
/// Secret holding the App's webhook secret.
const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";
/// Service binding to `hatchrun-ci`.
const CI: &str = "CI";

/// Whether `hatchrun-ci` handles deliveries of `event`.
fn forwarded(event: &str) -> bool {
    matches!(event, "push" | "pull_request" | "check_run")
}

/// Whether `hatchrun-ci` serves `path`: the CI API of the app.
fn forwards_api(path: &str) -> bool {
    path.starts_with("/api/ci/")
}

/// Sends a request under `/api/ci/` to `hatchrun-ci` as it came, cookies
/// and WebSocket upgrade included. `hatchrun-ci` reads the session
/// cookie from the shared database, so no identity travels in a header
/// that a caller could set.
async fn forward_api(env: &Env, req: Request) -> Result<Response> {
    let ci = env.service(CI)?;
    ci.fetch_request(req).await
}

/// Sends the delivery on to `hatchrun-ci` as it came: the same body and
/// the signature, event and delivery headers. Its answer goes back
/// unchanged, so a failure there is a failed delivery that GitHub or the
/// redelivery pass asks for again.
async fn forward(env: &Env, req: &mut Request) -> Result<Response> {
    let Ok(ci) = env.service(CI) else {
        return Ok(
            Response::from_json(&serde_json::json!({ "ignored": "no CI service" }))?
                .with_status(202),
        );
    };
    let headers = Headers::new();
    for name in ["X-Hub-Signature-256", "X-GitHub-Event", "X-GitHub-Delivery"] {
        if let Some(value) = req.headers().get(name)? {
            headers.set(name, &value)?;
        }
    }
    headers.set("Content-Type", "application/json")?;
    let body = req.bytes().await?;
    let mut init = worker::RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(js_sys::Uint8Array::from(body.as_slice()).into()));
    let request = Request::new_with_init("https://ci/github/webhook", &init)?;
    ci.fetch_request(request).await
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: worker::Context) -> Result<Response> {
    if req.url()?.host_str() == Some(oidc::HOST) {
        return issuer(&req, &env);
    }
    let path = req.path();
    if req.method() == Method::Post && path == "/github/webhook" {
        return github_webhook(req, env, &ctx).await;
    }
    if path.starts_with("/v1/cache/") {
        let mut response = cache::serve::handle(req, env).await?;
        response.headers_mut().set("Cache-Control", "no-store")?;
        return Ok(response);
    }
    if forwards_api(&path) {
        return forward_api(&env, req).await;
    }
    if path == "/api" || path.starts_with("/api/") {
        let mut response = app::handle(req, env).await?;
        response.headers_mut().set("Cache-Control", "no-store")?;
        return Ok(response);
    }
    if path == "/healthz" {
        return healthz(&env).await;
    }
    if path.starts_with("/github/") {
        return error("not found", 404);
    }
    assets(req, &env).await
}

/// The Cron Trigger in `wrangler.toml`, every minute: the reconciler.
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    reconcile::run(env).await;
}

/// The launch queue's consumers in `wrangler.toml`: the launches, and
/// the messages that ran out of retries (see [`launches`]).
#[event(queue)]
async fn queue(
    batch: MessageBatch<serde_json::Value>,
    env: Env,
    _ctx: worker::Context,
) -> Result<()> {
    if batch.queue() == launches::DEAD_LETTERS {
        launches::dead_letters(batch, env).await;
    } else {
        launches::consume(batch, env).await;
    }
    Ok(())
}

/// Whether the reconciler is running: 200 while its last pass is
/// recent and the passes are not all failing, 503 otherwise, or when
/// the database does not answer.
async fn healthz(env: &Env) -> Result<Response> {
    let read = async {
        Db::connect(env)
            .await?
            .first::<alerts::HealthRow>(alerts::HEALTH, &[])
            .await
    };
    let (status, body) = match read.await {
        Ok(row) => alerts::health(&row.unwrap_or_default(), now_ms()),
        Err(err) => {
            console_error!("healthz: {err}");
            (503, serde_json::json!({ "status": "database" }))
        }
    };
    let mut response = Response::from_json(&body)?.with_status(status);
    response.headers_mut().set("Cache-Control", "no-store")?;
    Ok(response)
}

/// The app's static assets. Every path that is not a file is the app's
/// `index.html`, so the app's own routes load it.
async fn assets(req: Request, env: &Env) -> Result<Response> {
    let response = env.assets(ASSETS)?.fetch_request(req).await?;
    let status = response.status_code();
    let headers = Headers::new();
    for (name, value) in response.headers().entries() {
        headers.append(&name, &value)?;
    }
    for (name, value) in SECURITY_HEADERS {
        headers.set(name, value)?;
    }
    let (_, body) = response.into_parts();
    Ok(ResponseBuilder::new()
        .with_status(status)
        .with_headers(headers)
        .body(body))
}

/// Detaches the tenant from an uninstalled installation.
const UNINSTALLED: &str =
    "UPDATE tenants SET installation_id = NULL, updated_at = $2 WHERE installation_id = $1";

/// Gives the organization's tenant its new installation.
const INSTALLED: &str =
    "UPDATE tenants SET installation_id = $1, updated_at = $3 WHERE org_id = $2";

/// Records a queued job and its run. The run's first job fetches the
/// rest of the run from GitHub; the reconciler tries again if that
/// fails.
async fn record_queued(
    db: &Db,
    env: &Env,
    ctx: &worker::Context,
    job: &QueuedJob,
    placement: &Placement,
    now: u64,
) -> Result<()> {
    if jobs::record_queued(db, job, placement, now).await? {
        ctx.wait_until(runs::fetch(
            env.clone(),
            job.installation_id,
            job.repository.clone(),
            job.run_id,
        ));
    }
    Ok(())
}

async fn github_webhook(mut req: Request, env: Env, ctx: &worker::Context) -> Result<Response> {
    if forwarded(&req.headers().get("X-GitHub-Event")?.unwrap_or_default()) {
        return forward(&env, &mut req).await;
    }
    let secret = env
        .secret(WEBHOOK_SECRET)
        .map(|secret| secret.to_string())
        .unwrap_or_default();
    if secret.is_empty() {
        return error("no GITHUB_WEBHOOK_SECRET", 500);
    }
    let signature = req.headers().get("X-Hub-Signature-256")?;
    let kind = req.headers().get("X-GitHub-Event")?.unwrap_or_default();
    let body = req.bytes().await?;
    if !webhook::verify(secret.as_bytes(), &body, signature.as_deref()) {
        return error("bad signature", 401);
    }
    let event = match webhook::parse(&kind, &body) {
        Ok(event) => event,
        Err(err) => return error(&format!("bad {kind} payload: {err}"), 400),
    };

    let db = Db::connect(&env).await?;
    let now = now_ms();
    match event {
        Event::Queued(job, placement) => {
            if let Some(reason) =
                profiles::store::unknown(&db, job.installation_id, &job.spec).await?
            {
                let reason = format!("{}: {reason}", job.label);
                console_warn!("{reason}");
                return Response::from_json(&serde_json::json!({ "ignored": reason }));
            }
            record_queued(&db, &env, ctx, &job, &placement, now).await?;
            // A repeated delivery comes here too; the launch claims the
            // job first, so only one of them launches. The job is
            // recorded either way, and the reconciler launches it when
            // the queue does not take the message.
            let queued = match env.queue(launches::QUEUE) {
                Ok(queue) => launches::enqueue(&queue, &job).await,
                Err(err) => Err(err.to_string()),
            };
            if let Err(err) = queued {
                console_error!("job {}: not queued for launch: {err}", job.id);
            }
            Ok(Response::from_json(&serde_json::json!({ "recorded": job.id }))?.with_status(202))
        }
        Event::Progress {
            id,
            status,
            runner,
            outcome,
        } => {
            // Deliveries can arrive out of order; a completed job stays
            // completed. The runner shows the reconciler which instance
            // ran the job (see `reconcile`).
            jobs::progress(&db, id, status.as_str(), runner.as_deref(), &outcome, now).await?;
            if status == webhook::Status::Completed
                && let (Some(started), Some(completed)) = (outcome.started_at, outcome.completed_at)
            {
                usage::record(&db, id, started, completed, now_ms()).await?;
            }
            Ok(Response::empty()?.with_status(204))
        }
        Event::Uninstalled { installation_id } => {
            db.run(UNINSTALLED, &params![installation_id, now]).await?;
            Ok(Response::empty()?.with_status(204))
        }
        Event::Installed {
            installation_id,
            account_id,
        } => {
            // A tenant made before an uninstall gets the new installation;
            // an account without a tenant waits for an owner to set it up.
            db.run(INSTALLED, &params![installation_id, account_id, now])
                .await?;
            Ok(Response::empty()?.with_status(204))
        }
        Event::Ignored(reason) => {
            if reason.starts_with(labels::PREFIX) {
                console_warn!("{reason}");
            }
            Response::from_json(&serde_json::json!({ "ignored": reason }))
        }
    }
}

/// The issuer's discovery document and JWKS.
fn issuer(req: &Request, env: &Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return error("method not allowed", 405);
    }
    let body = match req.path().as_str() {
        oidc::DISCOVERY_PATH => oidc::discovery(),
        oidc::JWKS_PATH => {
            let keys = published_keys(env);
            if keys.is_empty() {
                return error("no signing key", 500);
            }
            oidc::jwks(&keys)
        }
        _ => return error("not found", 404),
    };
    let mut response = Response::from_json(&body)?;
    response
        .headers_mut()
        .set("Cache-Control", oidc::CACHE_CONTROL)?;
    Ok(response)
}

/// The public keys of the issuer's slots that have a `kid`. A slot whose
/// key is missing or unreadable is left out and logged, so a bad next
/// key cannot take the signing key's JWKS down with it.
fn published_keys(env: &Env) -> Vec<(String, PublicKey)> {
    let get = |name: &str| {
        env.secret(name)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| !value.is_empty())
    };
    oidc::SLOTS
        .iter()
        .filter_map(|(key, kid)| {
            let kid = get(kid)?;
            let public = get(key)
                .ok_or_else(|| "is not set".to_owned())
                .and_then(|pem| {
                    jwt::pkcs8_der(&pem)
                        .and_then(|der| jwt::public_key(&der))
                        .map_err(|err| err.to_string())
                });
            match public {
                Ok(public) => Some((kid, public)),
                Err(err) => {
                    console_error!("{key} (kid {kid}): {err}");
                    None
                }
            }
        })
        .collect()
}

fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testdb::Db;

    #[test]
    fn forwards_orchestrator_events() {
        assert!(forwarded("push"));
        assert!(forwarded("pull_request"));
        assert!(forwarded("check_run"));
        assert!(!forwarded("workflow_job"));
        assert!(!forwarded("installation"));
        assert!(!forwarded(""));
    }

    #[test]
    fn forwards_ci_api_paths() {
        assert!(forwards_api("/api/ci/runs/1"));
        assert!(forwards_api("/api/ci/jobs/2/live"));
        assert!(!forwards_api("/api/session"));
        assert!(!forwards_api("/api/ci"));
        assert!(!forwards_api("/api/cinema"));
        assert!(!forwards_api("/v1/cache/x"));
    }

    const NOW: u64 = 1_792_140_000_000;

    fn queued(db: &Db, id: u64, label: &str, now: u64) -> usize {
        db.run(
            jobs::QUEUED,
            &params![id, 42u64, "acme/app", 77u64, label, now, "build", 1u64],
        )
    }

    fn progress(db: &Db, id: u64, status: &str, runner: Option<&str>, now: u64) -> usize {
        db.run(
            jobs::PROGRESS,
            &params![
                status,
                now,
                id,
                runner,
                None::<&str>,
                None::<u64>,
                None::<u64>
            ],
        )
    }

    /// The webhook's batch for a job of run 77 in attempt `attempt`:
    /// rows changed by each statement.
    fn job_of_run(db: &Db, id: u64, attempt: u64, sha: &str, now: u64) -> Vec<usize> {
        db.batch(&[
            (
                jobs::RUN,
                params![
                    77u64,
                    42u64,
                    "acme/app",
                    "Smoke test",
                    "main",
                    sha,
                    attempt,
                    now
                ],
            ),
            (jobs::RUN_ATTEMPT, params![77u64, attempt, now, sha, "main"]),
            (
                jobs::QUEUED,
                params![
                    id,
                    42u64,
                    "acme/app",
                    77u64,
                    "hatchrun",
                    now,
                    "Save (host)",
                    attempt
                ],
            ),
        ])
        .unwrap()
    }

    #[test]
    fn a_run_moves_to_its_newest_attempt() {
        let db = Db::new();
        assert_eq!(job_of_run(&db, 1, 1, "a1", NOW), [1, 0, 1]);
        assert_eq!(job_of_run(&db, 2, 1, "a1", NOW + 5), [0, 0, 1]);
        db.run(
            "UPDATE runs SET cache_write = '1/b/x', cache_reads = '[\"1/b/x\"]' WHERE id = 77",
            &[],
        );
        // A re-run: new job ids, attempt 2 and the list's place moves.
        assert_eq!(job_of_run(&db, 3, 2, "a2", NOW + 100), [0, 1, 1]);
        // A late delivery of attempt 1 leaves the run on attempt 2.
        assert_eq!(job_of_run(&db, 4, 1, "a1", NOW + 200), [0, 0, 1]);
        let rows = db
            .rows(
                "SELECT attempt, queued_at, head_sha, workflow_name, expires_at, cache_write, \
                 cache_reads FROM runs",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["attempt"], 2);
        assert_eq!(rows[0]["queued_at"], NOW + 100);
        assert_eq!(rows[0]["head_sha"], "a2");
        assert_eq!(rows[0]["workflow_name"], "Smoke test");
        // No tenant: the default 30 days, from the newest attempt.
        assert_eq!(rows[0]["expires_at"], NOW + 100 + 30 * 24 * 60 * 60 * 1000);
        // The re-run works out its cache scopes again.
        assert!(rows[0]["cache_write"].is_null() && rows[0]["cache_reads"].is_null());
        let attempts = db
            .rows("SELECT name, run_attempt FROM jobs ORDER BY id", &[])
            .unwrap();
        assert_eq!(attempts[2]["run_attempt"], 2);
        assert_eq!(attempts[2]["name"], "Save (host)");
    }

    #[test]
    fn progress_records_how_a_job_ended() {
        let db = Db::new();
        queued(&db, 5, "hatchrun", NOW);
        let progress =
            |status: &str, conclusion: Option<&str>, started: Option<u64>, ended: Option<u64>| {
                db.run(
                    jobs::PROGRESS,
                    &params![status, NOW, 5u64, None::<&str>, conclusion, started, ended],
                )
            };
        assert_eq!(progress("in_progress", None, Some(NOW + 1), None), 1);
        assert_eq!(
            progress("completed", Some("failure"), None, Some(NOW + 9)),
            1
        );
        let rows = db
            .rows("SELECT conclusion, started_at, completed_at FROM jobs", &[])
            .unwrap();
        assert_eq!(rows[0]["conclusion"], "failure");
        assert_eq!(rows[0]["started_at"], NOW + 1);
        assert_eq!(rows[0]["completed_at"], NOW + 9);
    }

    #[test]
    fn a_job_is_recorded_once_and_stays_completed() {
        let db = Db::new();
        assert_eq!(queued(&db, 5, "hatchrun", NOW), 1);
        assert_eq!(queued(&db, 5, "other", NOW + 1), 0);

        assert_eq!(
            progress(&db, 5, "in_progress", Some("hatchrun-5-ab"), NOW + 2),
            1
        );
        // A completed delivery without a runner keeps the one recorded.
        assert_eq!(progress(&db, 5, "completed", None, NOW + 3), 1);
        assert_eq!(progress(&db, 5, "in_progress", Some("other"), NOW + 4), 0);
        let rows = db
            .rows(
                "SELECT label, status, ran_on, queued_at, updated_at FROM jobs",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["label"], "hatchrun");
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[0]["ran_on"], "hatchrun-5-ab");
        assert_eq!(rows[0]["queued_at"], NOW);
        assert_eq!(rows[0]["updated_at"], NOW + 3);
        assert_eq!(progress(&db, 6, "in_progress", None, NOW), 0);
    }

    #[test]
    fn installs_move_the_tenant() {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $5)",
            &params!["t1", 9u64, "acme", 42u64, 1u64],
        );
        assert_eq!(db.run(UNINSTALLED, &params![42u64, NOW]), 1);
        let installation: Option<Option<u64>> = db.column(
            "SELECT installation_id FROM tenants",
            &[],
            "installation_id",
        );
        assert_eq!(installation, Some(None));
        assert_eq!(db.run(INSTALLED, &params![43u64, 9u64, NOW]), 1);
        assert_eq!(db.run(INSTALLED, &params![44u64, 10u64, NOW]), 0);
        let installation: Option<u64> = db.column(
            "SELECT installation_id FROM tenants",
            &[],
            "installation_id",
        );
        assert_eq!(installation, Some(43));
    }
}
