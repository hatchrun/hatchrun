// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The Worker behind hatchrun.io.
//!
//! Customers install the hatchrun GitHub App on their organization. The
//! App sends a `workflow_job` webhook for every job; the Worker checks the
//! signature, keeps the jobs whose `runs-on` label asks for a hatchrun
//! runner (see [`labels`]) and records them in D1, one row per job, so a
//! webhook GitHub delivers twice is recorded once. After answering, it
//! launches a runner instance for the job in the tenant's AWS account
//! (see the `launch` module).
//!
//! - `POST /github/webhook` takes the App's webhook deliveries.
//! - `/api/*` on `app.hatchrun.io` is the app's API (see [`app`]); every
//!   other path there is the app itself, from the static assets.
//! - `GET /.well-known/openid-configuration` and
//!   `GET /.well-known/jwks.json` on `oidc.hatchrun.io` let STS check
//!   the tokens the Worker signs (see [`oidc`]).

mod app;
pub mod aws;
pub mod connect;
mod fetch;
pub mod github;
pub mod http;
pub mod instance;
pub mod jwt;
pub mod labels;
mod launch;
pub mod oidc;
pub mod polar;
pub mod session;
pub mod signin;
pub mod tenant;
mod webcrypto;
pub mod webhook;

use wasm_bindgen::JsValue;
use worker::{
    Env, Headers, Method, Request, Response, ResponseBuilder, Result, console_error, console_warn,
    event,
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
            db.prepare(
                "INSERT INTO jobs \
                 (id, installation_id, repository, run_id, label, status, queued_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6) \
                 ON CONFLICT (id) DO NOTHING",
            )
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
        Event::Progress { id, status } => {
            // Deliveries can arrive out of order; a completed job stays
            // completed.
            db.prepare(
                "UPDATE jobs SET status = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND status <> 'completed'",
            )
            .bind(&[status.as_str().into(), now, int(id)?])?
            .run()
            .await?;
            Ok(Response::empty()?.with_status(204))
        }
        Event::Uninstalled { installation_id } => {
            db.prepare("UPDATE tenants SET installation_id = NULL, updated_at = ?2 WHERE installation_id = ?1")
                .bind(&[int(installation_id)?, now])?
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
