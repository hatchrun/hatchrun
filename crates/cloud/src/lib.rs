// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The Worker behind hatchrun.dev.
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

pub mod aws;
pub mod github;
pub mod http;
pub mod instance;
pub mod jwt;
pub mod labels;
mod launch;
pub mod oidc;
mod webcrypto;
pub mod webhook;

use wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response, Result, console_warn, event};

use crate::webhook::Event;

/// D1 binding in `wrangler.toml`.
pub(crate) const DB: &str = "DB";
/// Secret holding the App's webhook secret.
const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: worker::Context) -> Result<Response> {
    if req.method() == Method::Post && req.path() == "/github/webhook" {
        return github_webhook(req, env, &ctx).await;
    }
    error("not found", 404)
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
        Event::Ignored(reason) => {
            if reason.starts_with(labels::PREFIX) {
                console_warn!("{reason}");
            }
            Response::from_json(&serde_json::json!({ "ignored": reason }))
        }
    }
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
