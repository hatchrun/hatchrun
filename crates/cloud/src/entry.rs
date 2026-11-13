// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker's entry point: `fetch`. Another Worker that reuses this
//! crate turns the `worker` feature off and defines its own.

use std::pin::pin;
use std::time::Duration;

use futures_util::future::{Either, select};
use worker::{
    Delay, Env, Headers, Method, Request, Response, ResponseBuilder, Result, console_error, event,
};

use crate::db::{Db, params};
use crate::jwt::PublicKey;
use crate::webhook::Event;
use crate::{app, now_ms, oidc, webhook};

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
/// How long `/healthz` waits for `hatchrun-ci` before it answers 503.
const CI_HEALTH_FOR: Duration = Duration::from_secs(3);

/// Whether `hatchrun-ci` handles deliveries of `event`.
fn forwarded(event: &str) -> bool {
    matches!(
        event,
        "push"
            | "pull_request"
            | "check_run"
            | "repository"
            | "release"
            | "merge_group"
            | "repository_dispatch"
            | "create"
            | "delete"
            | "installation_repositories"
    )
}

/// Whether `hatchrun-ci` serves this request: a path of the CI API on
/// the app's own host. Any other host that reaches this Worker gets the
/// app's answer, which is a 404 for these paths.
fn forwards_api(path: &str, host: Option<&str>, app_host: Option<&str>) -> bool {
    path.starts_with("/api/ci/") && host.is_some() && host == app_host
}

/// The host of the `APP_URL` variable.
fn app_host(env: &Env) -> Option<String> {
    let url = env.secret("APP_URL").ok()?.to_string();
    worker::Url::parse(&url).ok()?.host_str().map(str::to_owned)
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
/// unchanged: `hatchrun-ci` answers 202 once it stores the delivery and
/// plans it from its queue, and a 503 means the delivery is not stored,
/// so GitHub sends it again.
async fn forward(env: &Env, req: &mut Request) -> Result<Response> {
    let Ok(ci) = env.service(CI) else {
        return error("no CI service", 503);
    };
    let headers = Headers::new();
    for name in ["X-Hub-Signature-256", "X-GitHub-Event", "X-GitHub-Delivery"] {
        let Ok(value) = req.headers().get(name) else {
            return error("the headers did not read", 503);
        };
        if let Some(value) = value {
            headers.set(name, &value)?;
        }
    }
    headers.set("Content-Type", "application/json")?;
    let Ok(body) = req.bytes().await else {
        return error("the body did not read", 503);
    };
    let mut init = worker::RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(js_sys::Uint8Array::from(body.as_slice()).into()));
    let request = Request::new_with_init("https://ci/github/webhook", &init)?;
    ci.fetch_request(request).await
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    if req.url()?.host_str() == Some(oidc::HOST) {
        return issuer(&req, &env);
    }
    let path = req.path();
    if req.method() == Method::Post && path == "/github/webhook" {
        return github_webhook(req, env).await;
    }
    let host = req.url()?.host_str().map(str::to_owned);
    if forwards_api(&path, host.as_deref(), app_host(&env).as_deref()) {
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

/// Whether the database answers and `hatchrun-ci`'s reconciler runs:
/// 200 when both do, 503 when either does not. The CI Worker answers for
/// its reconciler (see `hatchrun-ci`'s alerts), which this Worker cannot
/// see. Both checks run at once; CI's answer is dropped when the
/// database failed.
async fn healthz(env: &Env) -> Result<Response> {
    let database = async {
        let read = async { Db::connect(env).await?.run("SELECT 1", &[]).await };
        match read.await {
            Ok(_) => true,
            Err(err) => {
                console_error!("healthz: {err}");
                false
            }
        }
    };
    let (database, ci) = futures_util::join!(database, ci_health(env));
    if let (true, Err(err)) = (database, &ci) {
        console_error!("healthz: ci: {err}");
    }
    let (status, body) = verdict(database, ci);
    let mut response = Response::from_json(&body)?.with_status(status);
    response.headers_mut().set("Cache-Control", "no-store")?;
    Ok(response)
}

/// `hatchrun-ci`'s `/healthz` over the service binding: its status and
/// body. A CI Worker that does not answer within [`CI_HEALTH_FOR`] is an
/// error.
async fn ci_health(env: &Env) -> std::result::Result<(u16, serde_json::Value), String> {
    let ci = env.service(CI).map_err(|err| err.to_string())?;
    let ask = async {
        let mut answer = ci
            .fetch("https://ci/healthz", None)
            .await
            .map_err(|err| err.to_string())?;
        let status = answer.status_code();
        let body = answer.json().await.unwrap_or(serde_json::Value::Null);
        Ok((status, body))
    };
    match select(pin!(ask), pin!(Delay::from(CI_HEALTH_FOR))).await {
        Either::Left((answer, _)) => answer,
        Either::Right(_) => Err("timed out".to_owned()),
    }
}

/// `/healthz`'s status and body from what the checks found. The
/// database comes first; CI's own answer is passed on.
fn verdict(
    database: bool,
    ci: std::result::Result<(u16, serde_json::Value), String>,
) -> (u16, serde_json::Value) {
    match (database, ci) {
        (false, _) => (503, serde_json::json!({ "status": "database" })),
        (true, Ok((200, _))) => (200, serde_json::json!({ "status": "ok" })),
        (true, Ok((_, body))) => (503, serde_json::json!({ "status": "ci", "ci": body })),
        (true, Err(_)) => (503, serde_json::json!({ "status": "ci" })),
    }
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

async fn github_webhook(mut req: Request, env: Env) -> Result<Response> {
    let Ok(event) = req.headers().get("X-GitHub-Event") else {
        return error("the headers did not read", 503);
    };
    if forwarded(&event.unwrap_or_default()) {
        return forward(&env, &mut req).await;
    }
    let secret = env
        .secret(WEBHOOK_SECRET)
        .map(|secret| secret.to_string())
        .unwrap_or_default();
    // Without the secret nothing can be verified: down for every
    // delivery, so GitHub or the operator sends them again.
    if secret.is_empty() {
        return error("no GITHUB_WEBHOOK_SECRET", 503);
    }
    let (Ok(signature), Ok(kind)) = (
        req.headers().get("X-Hub-Signature-256"),
        req.headers().get("X-GitHub-Event"),
    ) else {
        return error("the headers did not read", 503);
    };
    let kind = kind.unwrap_or_default();
    let Ok(body) = req.bytes().await else {
        return error("the body did not read", 503);
    };
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
        Event::Ignored(reason) => Response::from_json(&serde_json::json!({ "ignored": reason })),
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
    let (keys, problems) = oidc::public_keys(&oidc::SLOTS, |name| {
        env.secret(name)
            .map(|value| value.to_string())
            .ok()
            .filter(|value| !value.is_empty())
    });
    for problem in problems {
        console_error!("{problem}");
    }
    keys
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
        assert!(forwarded("repository"));
        assert!(forwarded("release"));
        assert!(forwarded("merge_group"));
        assert!(forwarded("repository_dispatch"));
        assert!(forwarded("create"));
        assert!(forwarded("delete"));
        assert!(forwarded("installation_repositories"));
        assert!(!forwarded("workflow_job"));
        assert!(!forwarded("installation"));
        assert!(!forwarded(""));
    }

    #[test]
    fn forwards_ci_api_paths() {
        let app = Some("app.hatchrun.io");
        assert!(forwards_api("/api/ci/runs/1", app, app));
        assert!(forwards_api("/api/ci/jobs/2/live", app, app));
        assert!(!forwards_api("/api/session", app, app));
        assert!(!forwards_api("/api/ci", app, app));
        assert!(!forwards_api("/api/cinema", app, app));
    }

    #[test]
    fn forwards_ci_api_only_on_the_app_host() {
        let app = Some("app.hatchrun.io");
        assert!(!forwards_api(
            "/api/ci/runs/1",
            Some("other.hatchrun.io"),
            app
        ));
        assert!(!forwards_api("/api/ci/runs/1", None, app));
        assert!(!forwards_api("/api/ci/runs/1", app, None));
        assert!(!forwards_api("/api/ci/runs/1", None, None));
    }

    #[test]
    fn healthz_needs_the_database_and_ci() {
        let ok = serde_json::json!({ "status": "ok" });
        assert_eq!(verdict(true, Ok((200, ok.clone()))), (200, ok));
        let stale = serde_json::json!({ "status": "stale" });
        assert_eq!(
            verdict(true, Ok((503, stale.clone()))),
            (503, serde_json::json!({ "status": "ci", "ci": stale }))
        );
        assert_eq!(
            verdict(true, Err("no service".to_owned())),
            (503, serde_json::json!({ "status": "ci" }))
        );
        assert_eq!(
            verdict(false, Ok((200, serde_json::Value::Null))),
            (503, serde_json::json!({ "status": "database" }))
        );
    }

    const NOW: u64 = 1_792_140_000_000;

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
