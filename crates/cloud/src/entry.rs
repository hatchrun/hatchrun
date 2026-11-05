// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker's entry point: `fetch`. Another Worker that reuses this
//! crate turns the `worker` feature off and defines its own.

use worker::{
    Env, Headers, Method, Request, Response, ResponseBuilder, Result, console_error, event,
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

/// Whether `hatchrun-ci` handles deliveries of `event`.
fn forwarded(event: &str) -> bool {
    matches!(
        event,
        "push"
            | "pull_request"
            | "check_run"
            | "repository"
            | "release"
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

/// Whether the database answers: 200 when it does, 503 when it does
/// not.
async fn healthz(env: &Env) -> Result<Response> {
    let read = async { Db::connect(env).await?.run("SELECT 1", &[]).await };
    let (status, body) = match read.await {
        Ok(_) => (200, serde_json::json!({ "status": "ok" })),
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

async fn github_webhook(mut req: Request, env: Env) -> Result<Response> {
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
