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
//! twice is recorded once. After answering, it launches a runner
//! instance for the job in the tenant's AWS account (see the `launch`
//! module).
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
pub mod offerings;
pub mod oidc;
pub mod polar;
pub mod profiles;
mod reconcile;
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
    Env, Headers, Method, Request, Response, ResponseBuilder, Result, ScheduleContext,
    ScheduledEvent, console_error, console_warn, event,
};

use crate::jwt::PublicKey;
use crate::webhook::Event;

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

/// Records a queued job once, however often GitHub delivers it.
const QUEUED: &str = "INSERT INTO jobs \
     (id, installation_id, repository, run_id, label, status, queued_at, updated_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6) \
     ON CONFLICT (id) DO NOTHING";

/// Moves a job on unless it completed.
const PROGRESS: &str = "UPDATE jobs SET status = ?1, updated_at = ?2, \
     ran_on = COALESCE(?4, ran_on) WHERE id = ?3 AND status <> 'completed'";

/// Detaches the tenant from an uninstalled installation.
const UNINSTALLED: &str =
    "UPDATE tenants SET installation_id = NULL, updated_at = ?2 WHERE installation_id = ?1";

/// Gives the organization's tenant its new installation.
const INSTALLED: &str =
    "UPDATE tenants SET installation_id = ?1, updated_at = ?3 WHERE org_id = ?2";

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
        Event::Queued(job) => {
            if let Some(reason) =
                profiles::store::unknown(&db, job.installation_id, &job.spec).await?
            {
                let reason = format!("{}: {reason}", job.label);
                console_warn!("{reason}");
                return Response::from_json(&serde_json::json!({ "ignored": reason }));
            }
            db.prepare(QUEUED)
                .bind(&[
                    int(job.id)?,
                    int(job.installation_id)?,
                    job.repository.as_str().into(),
                    int(job.run_id)?,
                    job.label.as_str().into(),
                    now,
                ])?
                .run()
                .await?;
            // A repeated delivery comes here too; the launch claims the
            // job first, so only one of them launches.
            let id = job.id;
            ctx.wait_until(launch::launch(env, job));
            Ok(Response::from_json(&serde_json::json!({ "recorded": id }))?.with_status(202))
        }
        Event::Progress { id, status, runner } => {
            // Deliveries can arrive out of order; a completed job stays
            // completed. The runner shows the reconciler which instance
            // ran the job (see `reconcile`).
            db.prepare(PROGRESS)
                .bind(&[
                    status.as_str().into(),
                    now,
                    int(id)?,
                    runner.map_or(JsValue::NULL, Into::into),
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

pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;

    fn queued(db: &Db, id: u64, label: &str, now: u64) -> usize {
        db.run(QUEUED, &params![id, 42u64, "acme/app", 77u64, label, now])
    }

    fn progress(db: &Db, id: u64, status: &str, runner: Option<&str>, now: u64) -> usize {
        db.run(PROGRESS, &params![status, now, id, runner])
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
