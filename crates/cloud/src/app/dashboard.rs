// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The dashboard of a tenant that is set up.
//!
//! - `GET /api/tenants/<org>/jobs` lists recent jobs and their
//!   instances, newest first, and counts what is running now.
//!   `?status=active|waiting|failed` lists one group, the same jobs the
//!   count of that group counts. `?before=<queued_at>` pages back. The
//!   first page also carries the last day's finished and failed jobs
//!   in buckets of 15 minutes.
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
/// One bucket of the activity strip.
const BUCKET_MS: u64 = 15 * 60 * 1000;
/// Buckets in a day.
const BUCKETS: usize = (DAY_MS / BUCKET_MS) as usize;

/// Runners launching or running.
const ACTIVE: &str = "status <> 'completed' AND launch_state IN ('launching', 'launched')";
/// Queued and not taken, or waiting to launch again after a failed
/// launch.
const WAITING: &str = "status = 'queued' AND (launch_state IS NULL \
     OR (launch_state = 'failed' AND launch_retry = 'backoff'))";
/// Launches that failed and are not tried again on a timer. The leading
/// `launch_state = 'failed'` lets SQLite use the `jobs_failed` index.
const FAILED: &str = "launch_state = 'failed' \
     AND NOT (status = 'queued' AND launch_retry IS 'backoff')";

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
    /// Of the active ones, those running a job; the rest are booting.
    running: u32,
    /// Finished in the last day.
    completed_today: u32,
    /// Launches that failed in the last day and are not tried again
    /// on a timer.
    failed_today: u32,
}

/// A status tab of the jobs table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Group {
    Active,
    Waiting,
    Failed,
}

impl Group {
    fn predicate(self) -> &'static str {
        match self {
            Group::Active => ACTIVE,
            Group::Waiting => WAITING,
            Group::Failed => FAILED,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Page {
    before: Option<u64>,
    status: Option<Group>,
}

/// Finished and failed jobs per bucket over the last day, oldest first.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Activity {
    since: u64,
    bucket_ms: u64,
    finished: Vec<u32>,
    failed: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct Grouped {
    bucket: i64,
    finished: u32,
    failed: u32,
}

/// The query for one page of jobs, of one group or of all.
fn list_query(group: Option<Group>) -> String {
    let filter = group.map_or(String::new(), |g| format!(" AND {}", g.predicate()));
    format!(
        "SELECT id, repository, run_id, label, status, launch_state, runner_name, \
         instance_id, launch_error, launch_attempts, launch_retry, next_launch_at, \
         queued_at, updated_at FROM jobs \
         WHERE installation_id = ?1 AND (?2 IS NULL OR queued_at < ?2){filter} \
         ORDER BY queued_at DESC LIMIT ?3"
    )
}

/// The query for the counts. `?2` is the start of the last day.
fn counts_query() -> String {
    format!(
        "SELECT \
         COUNT(*) FILTER (WHERE {WAITING}) AS waiting, \
         COUNT(*) FILTER (WHERE {ACTIVE}) AS active, \
         COUNT(*) FILTER (WHERE status = 'in_progress' AND ({ACTIVE})) AS running, \
         COUNT(*) FILTER (WHERE status = 'completed' AND updated_at >= ?2) \
           AS completed_today, \
         COUNT(*) FILTER (WHERE updated_at >= ?2 AND ({FAILED})) AS failed_today \
         FROM jobs WHERE installation_id = ?1 \
         AND (status <> 'completed' OR updated_at >= ?2)"
    )
}

/// The query for the activity buckets. `?2` is the start of the first
/// bucket. A completed job's `updated_at` is when it finished.
fn activity_query() -> String {
    format!(
        "SELECT (updated_at - ?2) / {BUCKET_MS} AS bucket, \
         COUNT(*) FILTER (WHERE status = 'completed') AS finished, \
         COUNT(*) FILTER (WHERE {FAILED}) AS failed \
         FROM jobs WHERE installation_id = ?1 AND updated_at >= ?2 \
         GROUP BY bucket"
    )
}

/// The first bucket's start: a day back from the bucket `now` falls in,
/// so the last bucket holds `now` and buckets line up between refreshes.
fn activity_since(now: u64) -> u64 {
    (now / BUCKET_MS + 1).saturating_sub(BUCKETS as u64) * BUCKET_MS
}

/// Lays the grouped rows out in a full day of buckets, empty ones 0.
fn activity(since: u64, rows: &[Grouped]) -> Activity {
    let mut finished = vec![0; BUCKETS];
    let mut failed = vec![0; BUCKETS];
    for row in rows {
        // A clock that ran ahead can put a row past the last bucket.
        let Some(i) = usize::try_from(row.bucket).ok().filter(|&i| i < BUCKETS) else {
            continue;
        };
        finished[i] = row.finished;
        failed[i] = row.failed;
    }
    Activity {
        since,
        bucket_ms: BUCKET_MS,
        finished,
        failed,
    }
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
    let page: Page = req
        .query()
        .map_err(|_| Failure::new(400, "status is one of active, waiting or failed."))?;
    let installation = int(installation)?;
    let jobs = ctx
        .db
        .prepare(list_query(page.status))
        .bind(&[
            installation.clone(),
            page.before.map_or(Ok(JsValue::NULL), int)?,
            PAGE.into(),
        ])?
        .all()
        .await?
        .results::<Job>()?;
    let now = now_ms();
    let counts: Counts = ctx
        .db
        .prepare(counts_query())
        .bind(&[installation.clone(), int(now.saturating_sub(DAY_MS))?])?
        .first(None)
        .await?
        .unwrap_or_default();
    let mut body = serde_json::json!({
        "jobs": jobs,
        "counts": counts,
        "region": row.region,
        "pageSize": PAGE,
    });
    if page.before.is_none() {
        let since = activity_since(now);
        let rows = ctx
            .db
            .prepare(activity_query())
            .bind(&[installation, int(since)?])?
            .all()
            .await?
            .results::<Grouped>()?;
        body["activity"] = serde_json::json!(activity(since, &rows));
    }
    Ok(Response::from_json(&body)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_a_group() {
        let page: Page = serde_json::from_value(serde_json::json!({"status": "failed"})).unwrap();
        assert_eq!(page.status, Some(Group::Failed));
        let page: Page = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(page.status, None);
        assert!(serde_json::from_value::<Page>(serde_json::json!({"status": "done"})).is_err());
    }

    #[test]
    fn a_group_lists_what_its_count_counts() {
        let counts = counts_query();
        for group in [Group::Active, Group::Waiting, Group::Failed] {
            let list = list_query(Some(group));
            assert!(list.contains(&format!(" AND {} ORDER BY", group.predicate())));
            assert!(counts.contains(group.predicate()));
        }
        let all = list_query(None);
        assert!([ACTIVE, WAITING, FAILED].iter().all(|p| !all.contains(p)));
    }

    #[test]
    fn failed_jobs_list_from_their_own_index() {
        // A partial index serves a query only when the query's WHERE
        // implies the index's.
        assert!(FAILED.starts_with("launch_state = 'failed' AND "));
    }

    #[test]
    fn every_query_is_scoped_to_the_installation() {
        for query in [
            list_query(None),
            list_query(Some(Group::Failed)),
            counts_query(),
            activity_query(),
        ] {
            assert!(query.contains("WHERE installation_id = ?1 "), "{query}");
        }
        assert!(list_query(None).contains(" LIMIT ?3"));
        assert!(activity_query().contains("updated_at >= ?2"));
    }

    #[test]
    fn the_last_bucket_holds_now() {
        let now = 1_792_140_000_000 + 7 * 60 * 1000;
        let since = activity_since(now);
        assert_eq!(since % BUCKET_MS, 0);
        let last = since + (BUCKETS as u64 - 1) * BUCKET_MS;
        assert!((last..last + BUCKET_MS).contains(&now));
        assert_eq!(activity_since(0), 0);
    }

    #[test]
    fn empty_buckets_are_zero() {
        let rows = [
            Grouped {
                bucket: 0,
                finished: 3,
                failed: 1,
            },
            Grouped {
                bucket: 95,
                finished: 2,
                failed: 0,
            },
            Grouped {
                bucket: 96,
                finished: 9,
                failed: 9,
            },
            Grouped {
                bucket: -1,
                finished: 9,
                failed: 9,
            },
        ];
        let got = activity(1000, &rows);
        assert_eq!(got.since, 1000);
        assert_eq!(got.bucket_ms, BUCKET_MS);
        assert_eq!(got.finished.len(), BUCKETS);
        assert_eq!(got.failed.len(), BUCKETS);
        assert_eq!((got.finished[0], got.failed[0]), (3, 1));
        assert_eq!(got.finished[95], 2);
        assert_eq!(got.finished.iter().sum::<u32>(), 5);
        assert_eq!(got.failed.iter().sum::<u32>(), 1);
    }
}
