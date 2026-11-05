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
//!   `check_run` deliveries that `hatchrun-cloud` forwards.
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
//!
//! A Cron Trigger runs the reconciler every minute (stuck jobs,
//! schedules, expiry), and the `hatchrun-ci-launches` queue starts the
//! instances. Every other path answers 404.

pub mod access;
pub mod agent;
pub mod api;
pub mod artifacts;
pub mod billing;
pub mod cache;
pub mod chain;
pub mod checks;
pub mod concurrency;
pub mod env;
pub mod environments;
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
pub mod pool;
pub mod reconcile;
pub mod run;
pub mod runs;
pub mod secrets;
pub mod settings_api;
pub mod store;
pub mod token;
pub mod webhook;

use worker::{
    Context, Env, MessageBatch, Method, Request, Response, Result, ScheduleContext, ScheduledEvent,
    event,
};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    if req.url()?.host_str() == Some(oidc::HOST) {
        return oidc::issuer(&req, &env);
    }
    if req.method() == Method::Post && req.path() == "/github/webhook" {
        return webhook::handle(req, &env).await;
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

/// The launch queue and its dead-letter queue (see [`launches`]).
#[event(queue)]
async fn queue(
    batch: MessageBatch<launches::LaunchMessage>,
    env: Env,
    _ctx: Context,
) -> Result<()> {
    if batch.queue() == launches::DEAD_LETTERS {
        launches::dead_letters(batch, env).await;
    } else {
        launches::consume(batch, env).await;
    }
    Ok(())
}

/// The Cron Trigger, every minute: see [`reconcile`].
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    reconcile::run(env).await;
}
