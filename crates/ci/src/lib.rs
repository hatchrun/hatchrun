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
//! - `/api/ci/*` serves runs, jobs and logs to the app and takes its
//!   dispatch and cancel requests; `hatchrun-cloud` forwards them.
//!
//! A Cron Trigger runs the reconciler every minute (stuck jobs,
//! schedules, expiry), and the `hatchrun-ci-launches` queue starts the
//! instances. Every other path answers 404.

pub mod access;
pub mod agent;
pub mod api;
pub mod billing;
pub mod checks;
pub mod env;
pub mod environments;
pub mod github;
pub mod graph;
pub mod intake;
pub mod launcher;
pub mod launches;
pub mod logs;
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
    if req.method() == Method::Post && req.path() == "/github/webhook" {
        return webhook::handle(req, &env).await;
    }
    if req.path().starts_with("/api/ci/") {
        return api::handle(req, &env).await;
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
