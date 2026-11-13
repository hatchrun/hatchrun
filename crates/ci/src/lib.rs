// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker behind ci.hatchrun.io, hatchrun's own replacement for
//! GitHub Actions.
//!
//! It shares its Postgres, KV namespace and GitHub App with
//! `hatchrun-cloud` and reuses that crate's modules. Routes, as they
//! land:
//!
//! - `POST /github/webhook` takes the `push`, `pull_request` and
//!   `check_run` deliveries that `hatchrun-cloud` forwards. It stores
//!   each verified delivery, answers 202, and the
//!   `hatchrun-ci-deliveries` queue plans it.
//! - `POST /v1/agent/{claim,events,result}` is what the runner agent on
//!   a launched instance calls: its job document, log batches and
//!   heartbeats, and its result.
//! - `GET /v1/oidc/token` gives a job with `id-token: write` its OIDC
//!   token (see [`oidc`]); the same Worker serves the issuer's discovery
//!   document and JWKS on `token.ci.hatchrun.io`.
//! - `/v1/agent/artifacts/*` is the artifact service behind
//!   `actions/upload-artifact` and `download-artifact` (see [`artifacts`]).
//! - `/v1/agent/cache/*` is the cache service behind `actions/cache`, with
//!   its entries in the same bucket (see [`cache`]).
//! - `/api/ci/*` serves runs, jobs and logs to the app and takes its
//!   dispatch and cancel requests; `hatchrun-cloud` forwards them.
//! - `GET /healthz` answers 200 while the database answers and the
//!   reconciler's last pass is recent, 503 otherwise. Only the service
//!   binding's host reaches it; `hatchrun-cloud`'s `/healthz` asks it
//!   (see [`alerts`]).
//!
//! A Cron Trigger runs the reconciler every minute (stuck jobs,
//! schedules, expiry, deliveries), and the `hatchrun-ci-launches` queue
//! starts the instances. The webhook stores each verified delivery and
//! the `hatchrun-ci-deliveries` queue plans it. Alerts about the reconciler, failed launches, GitHub's
//! rate limit and the database go out by email (see [`alerts`]). Every
//! other path answers 404.

pub mod access;
pub mod agent;
mod alerts;
pub mod api;
pub mod artifacts;
pub mod billing;
pub mod cache;
pub mod chain;
pub mod chatops;
pub mod checks;
pub mod concurrency;
pub mod deliveries;
pub mod env;
pub mod environments;
pub mod fork;
pub mod github;
pub mod graph;
pub mod installation;
pub mod intake;
pub mod launcher;
pub mod launches;
pub mod logs;
pub mod oidc;
pub mod org_api;
pub mod page;
pub mod planning;
pub mod policy;
pub mod pool;
pub mod reconcile;
pub mod remote;
pub mod run;
pub mod runs;
pub mod secrets;
pub mod settings_api;
pub mod store;
pub mod token;
pub mod trust;
pub mod webhook;

use hatchrun_cloud::db::Db;
use worker::{
    Context, Env, MessageBatch, Method, Request, Response, Result, ScheduleContext, ScheduledEvent,
    console_error, event,
};

/// The host `hatchrun-cloud` calls through the service binding.
const INTERNAL_HOST: &str = "ci";

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    if req.url()?.host_str() == Some(oidc::HOST) {
        return oidc::issuer(&req, &env);
    }
    if req.method() == Method::Post && req.path() == "/github/webhook" {
        return webhook::handle(req, &env).await;
    }
    if req.method() == Method::Get && req.path() == "/healthz" {
        // Public on ci.hatchrun.io it would open a database connection
        // per hit; only the service binding's host gets an answer.
        if req.url()?.host_str() != Some(INTERNAL_HOST) {
            return Response::error("not found", 404);
        }
        return healthz(&env).await;
    }
    if req.path().starts_with("/api/ci/") {
        return api::handle(req, &env).await;
    }
    if req.path().starts_with("/v1/oidc/") {
        return oidc::handle(req, &env).await;
    }
    if req.path().starts_with("/v1/agent/") {
        return agent::handle(req, &env).await;
    }
    Response::error("not found", 404)
}

/// Whether the database answers and the reconciler runs: 200 when it
/// does, 503 when it does not (see [`alerts::health`]).
async fn healthz(env: &Env) -> Result<Response> {
    let read = async {
        Db::connect(env)
            .await?
            .first::<alerts::HealthRow>(alerts::HEALTH, &[])
            .await
    };
    let (status, body) = match read.await {
        Ok(row) => alerts::health(&row.unwrap_or_default(), worker::Date::now().as_millis()),
        Err(err) => {
            console_error!("healthz: {err}");
            (503, serde_json::json!({ "status": "database" }))
        }
    };
    let mut response = Response::from_json(&body)?.with_status(status);
    response.headers_mut().set("Cache-Control", "no-store")?;
    Ok(response)
}

/// The launch and delivery queues and their dead-letter queues (see
/// [`launches`] and [`deliveries`]). Each queue's messages read as its
/// own type: the batch arrives untyped and is decoded per queue, so a
/// batch of one queue never reads as the other's.
#[event(queue)]
async fn queue(batch: MessageBatch<serde_json::Value>, env: Env, _ctx: Context) -> Result<()> {
    match batch.queue().as_str() {
        launches::QUEUE => launches::consume(batch, env).await,
        launches::DEAD_LETTERS => launches::dead_letters(batch, env).await,
        deliveries::QUEUE => deliveries::consume(batch, env).await,
        deliveries::DEAD_LETTERS => deliveries::dead_letters(&batch),
        other => {
            // No consumer reads this queue: retrying would loop
            // forever, so the batch is logged and acked.
            console_error!("queue {other}: no consumer, acking the batch");
            batch.ack_all();
        }
    }
    Ok(())
}

/// The Cron Trigger, every minute: see [`reconcile`].
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    reconcile::run(env).await;
}
