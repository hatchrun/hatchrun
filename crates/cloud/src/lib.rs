// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker behind hatchrun.io.
//!
//! Customers install the hatchrun GitHub App on their organization. The
//! App sends a `workflow_job` webhook for every job; the Worker checks the
//! signature, keeps the jobs whose `runs-on` label names one of the
//! organization's runner profiles (see [`labels`] and [`profiles`]) and
//! records them in Postgres, one row per job, so a webhook GitHub delivers
//! twice is recorded once. It sends each recorded job to the launch
//! queue, whose consumer in this Worker launches a runner instance for
//! it in the tenant's AWS account (see the `launches` and `launch`
//! modules).
//!
//! - `POST /github/webhook` takes the App's webhook deliveries.
//! - `/api/*` on `app.hatchrun.io` is the app's API (see [`app`]); every
//!   other path there is the app itself, from the static assets.
//! - `/v1/cache/*` is the job cache that runners call (see [`cache`]).
//! - `GET /healthz` says whether the reconciler is running, for an
//!   uptime monitor (see the `alerts` module).
//! - `GET /.well-known/openid-configuration` and
//!   `GET /.well-known/jwks.json` on `oidc.hatchrun.io` let STS check
//!   the tokens the Worker signs (see [`oidc`]).
//!
//! A Cron Trigger runs the reconciler every minute, which redelivers
//! failed webhooks, launches lost launches again and terminates
//! instances nothing needs (see the `reconcile` module), then emails
//! the alerts that are due (see the `alerts` module).

// Without the `worker` feature the entry points are gone and the code only
// they reach has no caller.
#![cfg_attr(not(feature = "worker"), allow(dead_code))]

pub mod actions;
mod alerts;
mod app;
pub use app::{Session, UserRow};
pub mod aws;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod bench;
pub mod cache;
pub mod catalog;
pub mod ci_wake;
pub mod connect;
pub mod db;
pub mod fetch;
pub mod github;
pub mod http;
pub mod images;
pub mod install;
pub mod instance;
pub mod instance_types;
mod jobs;
pub mod jwt;
pub mod labels;
mod launch;
mod launches;
pub mod offerings;
pub mod oidc;
pub mod polar;
pub mod profiles;
mod reconcile;
pub use reconcile::time::format as format_rfc3339;
pub use reconcile::time::parse as parse_rfc3339;
pub mod runs;
pub mod session;
pub mod settings;
pub mod signin;
mod stack;
pub mod start;
pub mod telemetry;
pub mod tenant;
#[cfg(all(not(target_arch = "wasm32"), any(test, feature = "testdb")))]
pub mod testdb;
pub mod tokens;
pub mod usage;
pub mod webcrypto;
pub mod webhook;

#[cfg(feature = "worker")]
mod entry;

pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}
