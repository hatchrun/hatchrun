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
//! - `/v1/jobs/*` is what the runner agent on a launched instance
//!   calls: its job document, log batches, heartbeats and its result.
//! - `/api/*` serves runs, jobs and logs to the app.
//!
//! A Cron Trigger runs the reconciler every minute (stuck jobs,
//! schedules, expiry), and the `hatchrun-ci-launches` queue starts the
//! instances. Every other path answers 404.

pub mod checks;
pub mod github;
pub mod graph;
pub mod intake;
pub mod planning;
pub mod store;
pub mod token;

use worker::{Context, Env, Request, Response, Result, event};

#[event(fetch)]
async fn fetch(_req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    Response::error("not found", 404)
}
