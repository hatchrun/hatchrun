// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Takes CPU and memory reports from hosted runners into Workers
//! Analytics Engine, at `telemetry.hatchrun.io`.
//!
//! hatchrun-cloud hands each runner it launches a token, an `EdDSA` JWT
//! naming the tenant, installation, job and runner, signed with a key
//! that signs nothing else. This Worker holds only the public half of
//! that key and the Analytics Engine binding: no database, no App key,
//! no OIDC key. It trusts the token for who is reporting and the body
//! for nothing but the numbers.
//!
//! - `POST /v1/metrics` with `Authorization: Bearer {token}` and a
//!   report of up to 60 ten-second windows (see [`protocol::Report`])
//!   writes one data point per window and answers 204.
//!
//! Each data point has the job id as `index1`, which Analytics Engine
//! samples by, so one job's points are kept or sampled together. The
//! blobs are the installation id, the instance type, the architecture
//! and the agent version; the doubles are the window's numbers in the
//! order of [`protocol::Window::doubles`].

pub mod protocol;

use worker::{Env, Method, Request, Response, Result, event};

use crate::protocol::{MAX_BODY, Report};

/// Analytics Engine binding in `wrangler.toml`.
const DATASET: &str = "METRICS";
/// Variable with the token public keys (see [`protocol::public_keys`]).
const PUBLIC_KEYS: &str = "TELEMETRY_PUBLIC_KEYS";

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    if req.path() != "/v1/metrics" {
        return error("not found", 404);
    }
    if req.method() != Method::Post {
        return error("method not allowed", 405);
    }
    let keys = match env
        .var(PUBLIC_KEYS)
        .map_err(|err| err.to_string())
        .and_then(|keys| protocol::public_keys(&keys.to_string()))
    {
        Ok(keys) => keys,
        Err(err) => {
            worker::console_error!("{PUBLIC_KEYS}: {err}");
            return error("telemetry has no public key", 500);
        }
    };
    let now = worker::Date::now().as_millis() / 1000;
    let claims = match protocol::verify(req.headers().get("Authorization")?.as_deref(), &keys, now)
    {
        Ok(claims) => claims,
        Err(refusal) => return error(refusal.message, refusal.status),
    };
    let declared = req
        .headers()
        .get("Content-Length")?
        .and_then(|length| length.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_BODY) {
        return error("body too large", 413);
    }
    let report = match Report::parse(&req.bytes().await?, now) {
        Ok(report) => report,
        Err(message) => return error(&message, 400),
    };

    let dataset = env.analytics_engine(DATASET)?;
    let index = claims.job_id.to_string();
    let blobs = report.blobs(&claims);
    for window in &report.windows {
        worker::AnalyticsEngineDataPointBuilder::new()
            .indexes([index.as_str()])
            .blobs(blobs.iter().map(String::as_str))
            .doubles(window.doubles())
            .write_to(&dataset)?;
    }
    Ok(Response::empty()?.with_status(204))
}

fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}
