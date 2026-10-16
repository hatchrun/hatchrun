// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The dashboard of a tenant that is set up.
//!
//! - `GET /api/tenants/<org>/jobs` lists recent jobs and their
//!   instances, newest first, and counts what is running now.
//!   `?before=<queued_at>` pages back.
//! - `PUT /api/tenants/<org>/settings` with `{"maxRunners",
//!   "allowedRegions"}` changes the settings.

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{Request, Response};

use super::tenants::{authorize, show};
use super::{Answer, Ctx, Failure};
use crate::settings::Settings;
use crate::{int, now_ms};

/// Jobs in one page.
const PAGE: u32 = 50;
/// The window the counts of finished and failed jobs cover.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Job {
    id: u64,
    repository: String,
    run_id: u64,
    label: String,
    status: String,
    launch_state: Option<String>,
    runner_name: Option<String>,
    instance_id: Option<String>,
    launch_error: Option<String>,
    /// Launches started for the job.
    launch_attempts: u32,
    /// After a failed launch: `backoff` to launch again at
    /// `next_launch_at`, `setup` to wait for the setup to change.
    launch_retry: Option<String>,
    next_launch_at: Option<u64>,
    queued_at: u64,
    updated_at: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Counts {
    /// Queued and not taken: waiting for room under max runners, for
    /// the launch to start, or to launch again after a failed launch.
    waiting: u32,
    /// Runners launching or running.
    active: u32,
    /// Finished in the last day.
    completed_today: u32,
    /// Launches that failed in the last day and are not tried again
    /// on a timer.
    failed_today: u32,
}

#[derive(Debug, Deserialize)]
struct Page {
    before: Option<u64>,
}

/// `GET /api/tenants/<org>/jobs`
pub(crate) async fn jobs(ctx: &Ctx<'_>, org: &str, req: &Request) -> Answer {
    let row = authorize(ctx, org).await?;
    let Some(installation) = row.installation_id else {
        return Ok(Response::from_json(&serde_json::json!({
            "jobs": [],
            "counts": Counts::default(),
            "region": row.region,
        }))?);
    };
    let page: Page = req.query()?;
    let installation = int(installation)?;
    let jobs = ctx
        .db
        .prepare(
            "SELECT id, repository, run_id, label, status, launch_state, runner_name, \
             instance_id, launch_error, launch_attempts, launch_retry, next_launch_at, \
             queued_at, updated_at FROM jobs \
             WHERE installation_id = ?1 AND (?2 IS NULL OR queued_at < ?2) \
             ORDER BY queued_at DESC LIMIT ?3",
        )
        .bind(&[
            installation.clone(),
            page.before.map_or(Ok(JsValue::NULL), int)?,
            PAGE.into(),
        ])?
        .all()
        .await?
        .results::<Job>()?;
    let since = int(now_ms().saturating_sub(DAY_MS))?;
    let counts: Counts = ctx
        .db
        .prepare(
            "SELECT \
             COUNT(*) FILTER (WHERE status = 'queued' AND (launch_state IS NULL \
               OR (launch_state = 'failed' AND launch_retry = 'backoff'))) AS waiting, \
             COUNT(*) FILTER (WHERE status <> 'completed' \
               AND launch_state IN ('launching', 'launched')) AS active, \
             COUNT(*) FILTER (WHERE status = 'completed' AND updated_at >= ?2) \
               AS completed_today, \
             COUNT(*) FILTER (WHERE launch_state = 'failed' AND updated_at >= ?2 \
               AND NOT (status = 'queued' AND launch_retry IS 'backoff')) AS failed_today \
             FROM jobs WHERE installation_id = ?1 \
             AND (status <> 'completed' OR updated_at >= ?2)",
        )
        .bind(&[installation, since])?
        .first(None)
        .await?
        .unwrap_or_default();
    Ok(Response::from_json(&serde_json::json!({
        "jobs": jobs,
        "counts": counts,
        "region": row.region,
        "pageSize": PAGE,
    }))?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Change {
    max_runners: u32,
    allowed_regions: Option<Vec<String>>,
}

/// `PUT /api/tenants/<org>/settings`
pub(crate) async fn settings(ctx: &Ctx<'_>, org: &str, body: Change) -> Answer {
    let row = authorize(ctx, org).await?;
    let checked = Settings {
        max_runners: body.max_runners,
        allowed_regions: body.allowed_regions,
    }
    .validate()
    .map_err(|err| Failure::new(422, err))?;
    if let (Some(region), Some(allowed)) = (&row.region, &checked.allowed_regions)
        && !allowed.contains(region)
    {
        return Err(Failure::new(
            422,
            format!(
                "AWS is connected in {region}. Keep it allowed, or connect another region first."
            ),
        ));
    }
    ctx.db
        .prepare(
            "UPDATE tenants SET max_runners = ?2, allowed_regions = ?3, updated_at = ?4 \
             WHERE id = ?1",
        )
        .bind(&[
            row.id.as_str().into(),
            checked.max_runners.into(),
            checked.regions_column().map_or(JsValue::NULL, Into::into),
            int(now_ms())?,
        ])?
        .run()
        .await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    show(ctx, &row.org_login).await
}
