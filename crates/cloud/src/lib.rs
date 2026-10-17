// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker behind hatchrun.io.
//!
//! Customers install the hatchrun GitHub App on their organization. The
//! App sends a `workflow_job` webhook for every job; the Worker checks the
//! signature, keeps the jobs whose `runs-on` label names one of the
//! organization's runner profiles (see [`labels`] and [`profiles`]) and
//! records them in D1, one row per job, so a webhook GitHub delivers
//! twice is recorded once. It sends each recorded job to the launch
//! queue, whose consumer in this Worker launches a runner instance for
//! it in the tenant's AWS account (see the `launches` and `launch`
//! modules).
//!
//! - `POST /github/webhook` takes the App's webhook deliveries.
//! - `/api/*` on `app.hatchrun.io` is the app's API (see [`app`]); every
//!   other path there is the app itself, from the static assets.
//! - `/v1/cache/*` is the job cache that runners call (see [`cache`]).
//! - `GET /.well-known/openid-configuration` and
//!   `GET /.well-known/jwks.json` on `oidc.hatchrun.io` let STS check
//!   the tokens the Worker signs (see [`oidc`]).
//!
//! A Cron Trigger runs the reconciler every minute, which redelivers
//! failed webhooks, launches lost launches again and terminates
//! instances nothing needs (see the `reconcile` module).

pub mod actions;
mod app;
pub mod aws;
pub mod cache;
pub mod catalog;
pub mod connect;
mod fetch;
pub mod github;
pub mod http;
pub mod images;
pub mod install;
pub mod instance;
pub mod instance_types;
pub mod jwt;
pub mod labels;
mod launch;
mod launches;
pub mod offerings;
pub mod oidc;
pub mod polar;
pub mod profiles;
mod reconcile;
pub mod runs;
pub mod session;
pub mod settings;
pub mod signin;
mod stack;
pub mod telemetry;
pub mod tenant;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod testdb;
mod webcrypto;
pub mod webhook;

use wasm_bindgen::JsValue;
use worker::{
    D1Database, Env, Headers, MessageBatch, Method, Request, Response, ResponseBuilder, Result,
    ScheduleContext, ScheduledEvent, console_error, console_warn, event,
};

use crate::jwt::PublicKey;
use crate::webhook::{Event, Placement, QueuedJob};

/// D1 binding in `wrangler.toml`.
pub(crate) const DB: &str = "DB";
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
    if path == "/api" || path.starts_with("/api/") {
        let mut response = app::handle(req, env).await?;
        response.headers_mut().set("Cache-Control", "no-store")?;
        return Ok(response);
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

/// The launch queue's consumer in `wrangler.toml` (see [`launches`]).
#[event(queue)]
async fn queue(
    batch: MessageBatch<launches::Launch>,
    env: Env,
    _ctx: worker::Context,
) -> Result<()> {
    launches::consume(batch, env).await;
    Ok(())
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

/// Records a run the first time one of its jobs is queued. It expires
/// with its jobs, `retention_days` after it was queued (see
/// [`QUEUED`]).
const RUN: &str = "INSERT INTO runs (id, installation_id, repository, workflow_name, \
     head_branch, head_sha, attempt, queued_at, created_at, updated_at, expires_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8, ?8 + COALESCE((SELECT retention_days \
       FROM tenants WHERE installation_id = ?2), 30) * 86400000) \
     ON CONFLICT (id) DO NOTHING";

/// Moves a run on to attempt `?2` when its first job is queued. A late
/// delivery from an earlier attempt changes nothing. The run expires
/// from the new attempt, as its jobs do.
const RUN_ATTEMPT: &str = "UPDATE runs SET attempt = ?2, queued_at = ?3, head_sha = ?4, \
     head_branch = ?5, updated_at = ?3, expires_at = ?3 + COALESCE((SELECT retention_days \
       FROM tenants WHERE tenants.installation_id = runs.installation_id), 30) * 86400000 \
     WHERE id = ?1 AND attempt < ?2";

/// Records a queued job once, however often GitHub delivers it. It
/// expires after the `retention_days` of the installation's tenant, or
/// that column's default of 30 days for an installation without one.
/// The lookup rides along with the insert, so the webhook makes no
/// extra round trip for it.
const QUEUED: &str = "INSERT INTO jobs \
     (id, installation_id, repository, run_id, label, status, queued_at, updated_at, \
     name, run_attempt, expires_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6, ?7, ?8, ?6 + COALESCE((SELECT retention_days \
       FROM tenants WHERE installation_id = ?2), 30) * 86400000) \
     ON CONFLICT (id) DO NOTHING";

/// Moves a job on unless it completed, with GitHub's conclusion and
/// times when the event has them.
const PROGRESS: &str = "UPDATE jobs SET status = ?1, updated_at = ?2, \
     ran_on = COALESCE(?4, ran_on), conclusion = COALESCE(?5, conclusion), \
     started_at = COALESCE(?6, started_at), completed_at = COALESCE(?7, completed_at) \
     WHERE id = ?3 AND status <> 'completed'";

/// Detaches the tenant from an uninstalled installation.
const UNINSTALLED: &str =
    "UPDATE tenants SET installation_id = NULL, updated_at = ?2 WHERE installation_id = ?1";

/// Gives the organization's tenant its new installation.
const INSTALLED: &str =
    "UPDATE tenants SET installation_id = ?1, updated_at = ?3 WHERE org_id = ?2";

/// Records a queued job and its run. The run's first job fetches the
/// rest of the run from GitHub; the reconciler tries again if that
/// fails.
async fn record_queued(
    db: &D1Database,
    env: &Env,
    ctx: &worker::Context,
    job: &QueuedJob,
    placement: &Placement,
    now: JsValue,
) -> Result<()> {
    let text = |value: &Option<String>| value.as_deref().map_or(JsValue::NULL, Into::into);
    let recorded = db
        .batch(vec![
            db.prepare(RUN).bind(&[
                int(job.run_id)?,
                int(job.installation_id)?,
                job.repository.as_str().into(),
                text(&placement.workflow_name),
                text(&placement.head_branch),
                placement.head_sha.as_str().into(),
                int(placement.attempt)?,
                now.clone(),
            ])?,
            db.prepare(RUN_ATTEMPT).bind(&[
                int(job.run_id)?,
                int(placement.attempt)?,
                now.clone(),
                placement.head_sha.as_str().into(),
                text(&placement.head_branch),
            ])?,
            db.prepare(QUEUED).bind(&[
                int(job.id)?,
                int(job.installation_id)?,
                job.repository.as_str().into(),
                int(job.run_id)?,
                job.label.as_str().into(),
                now,
                placement.name.as_str().into(),
                int(placement.attempt)?,
            ])?,
        ])
        .await?;
    if recorded.first().is_some_and(|result| changes(result) == 1) {
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

    let db = env.d1(DB)?;
    let now = int(now_ms())?;
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
            if let Err(err) = launches::enqueue(&env.queue(launches::QUEUE)?, &job).await {
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
            let time = |value: Option<u64>| value.map_or(Ok(JsValue::NULL), int);
            db.prepare(PROGRESS)
                .bind(&[
                    status.as_str().into(),
                    now,
                    int(id)?,
                    runner.map_or(JsValue::NULL, Into::into),
                    outcome.conclusion.map_or(JsValue::NULL, Into::into),
                    time(outcome.started_at)?,
                    time(outcome.completed_at)?,
                ])?
                .run()
                .await?;
            Ok(Response::empty()?.with_status(204))
        }
        Event::Uninstalled { installation_id } => {
            db.prepare(UNINSTALLED)
                .bind(&[int(installation_id)?, now])?
                .run()
                .await?;
            Ok(Response::empty()?.with_status(204))
        }
        Event::Installed {
            installation_id,
            account_id,
        } => {
            // A tenant made before an uninstall gets the new installation;
            // an account without a tenant waits for an owner to set it up.
            db.prepare(INSTALLED)
                .bind(&[int(installation_id)?, int(account_id)?, now])?
                .run()
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

/// D1 takes numbers as JavaScript doubles, exact up to 2^53. GitHub ids
/// are far below that; anything above it is refused rather than rounded.
pub(crate) fn int(value: u64) -> Result<JsValue> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    if value > MAX_SAFE {
        return Err(worker::Error::RustError(format!(
            "{value} is too large for D1"
        )));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(JsValue::from_f64(value as f64))
}

fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}

/// Rows a statement changed.
pub(crate) fn changes(result: &worker::D1Result) -> usize {
    result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or(0)
}

pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;

    fn queued(db: &Db, id: u64, label: &str, now: u64) -> usize {
        db.run(
            QUEUED,
            &params![id, 42u64, "acme/app", 77u64, label, now, "build", 1u64],
        )
    }

    fn progress(db: &Db, id: u64, status: &str, runner: Option<&str>, now: u64) -> usize {
        db.run(
            PROGRESS,
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
                RUN,
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
            (RUN_ATTEMPT, params![77u64, attempt, now, sha, "main"]),
            (
                QUEUED,
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
        // A re-run: new job ids, attempt 2 and the list's place moves.
        assert_eq!(job_of_run(&db, 3, 2, "a2", NOW + 100), [0, 1, 1]);
        // A late delivery of attempt 1 leaves the run on attempt 2.
        assert_eq!(job_of_run(&db, 4, 1, "a1", NOW + 200), [0, 0, 1]);
        let rows = db
            .rows(
                "SELECT attempt, queued_at, head_sha, workflow_name, expires_at FROM runs",
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
                    PROGRESS,
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
                "SELECT label, status, ran_on, queued_at, updated_at, typeof(updated_at) AS t \
                 FROM jobs",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["label"], "hatchrun");
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[0]["ran_on"], "hatchrun-5-ab");
        assert_eq!(rows[0]["queued_at"], NOW);
        assert_eq!(rows[0]["updated_at"], NOW + 3);
        assert_eq!(rows[0]["t"], "integer");
        assert_eq!(progress(&db, 6, "in_progress", None, NOW), 0);
    }

    #[test]
    fn installs_move_the_tenant() {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
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
