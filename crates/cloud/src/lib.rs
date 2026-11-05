// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Worker behind hatchrun.io.
//!
//! Customers install the hatchrun GitHub App on their organization. This
//! Worker owns what sits in front of hatchrun CI: the App's webhook
//! endpoint, sign-in with GitHub, the organization setup and billing
//! pages' API, and the OIDC issuer. `hatchrun-ci` (crates/ci) reuses
//! its modules for GitHub, AWS and the database.
//!
//! - `POST /github/webhook` takes the App's webhook deliveries. CI events
//!   go on to `hatchrun-ci`; installation events keep the tenants'
//!   installations current.
//! - `/api/*` on `app.hatchrun.io` is the app's API (see [`app`]), and
//!   `/api/ci/*` goes on to `hatchrun-ci`; every other path there is the
//!   app itself, from the static assets.
//! - `GET /healthz` says whether the database answers, for an uptime
//!   monitor.
//! - `GET /.well-known/openid-configuration` and
//!   `GET /.well-known/jwks.json` on `oidc.hatchrun.io` let STS check
//!   the tokens the Worker signs (see [`oidc`]).

// Without the `worker` feature the entry points are gone and the code only
// they reach has no caller.
#![cfg_attr(not(feature = "worker"), allow(dead_code))]

pub mod actions;
mod app;
pub use app::{Session, UserRow};
pub mod aws;
pub mod cache;
pub mod catalog;
pub mod ci_wake;
pub mod db;
pub mod fetch;
pub mod github;
pub mod http;
pub mod images;
pub mod install;
pub mod instance;
pub mod instance_types;
pub mod jwt;
pub mod labels;
pub mod offerings;
pub mod oidc;
pub mod polar;
pub mod profiles;
pub use retry::delay as launch_retry_delay;
pub use time::format as format_rfc3339;
pub use time::parse as parse_rfc3339;
pub mod retry;
pub mod session;
pub mod signin;
pub mod start;
pub mod tenant;
#[cfg(all(not(target_arch = "wasm32"), any(test, feature = "testdb")))]
pub mod testdb;
pub mod time;
pub mod tokens;
pub mod webcrypto;
pub mod webhook;

#[cfg(feature = "worker")]
mod entry;

#[cfg(not(test))]
pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// Tests run natively, where `worker::Date` cannot be called.
#[cfg(test)]
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}
