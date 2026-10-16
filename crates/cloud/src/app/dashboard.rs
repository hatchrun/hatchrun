// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

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
/// bucket. A completed job's `updated_at` is when it finished. D1 binds
/// every number as a double, so `?2` is a REAL and the division is
/// float division; the cast truncates it to the bucket index, which is
/// the floor since `updated_at >= ?2`.
fn activity_query() -> String {
    format!(
        "SELECT CAST((updated_at - ?2) / {BUCKET_MS} AS INTEGER) AS bucket, \
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

/// Changes a tenant's settings.
const SET_SETTINGS: &str = "UPDATE tenants SET max_runners = ?2, allowed_regions = ?3, \
     updated_at = ?4 WHERE id = ?1";

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
        .prepare(SET_SETTINGS)
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
    use crate::testdb::{Db, params};

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
        assert!(activity_query().starts_with("SELECT CAST((updated_at - ?2) / "));
        assert!(activity_query().contains(" AS INTEGER) AS bucket, "));
    }

    /// A job as the webhook and the launch leave it.
    struct Seed {
        id: u64,
        status: &'static str,
        launch_state: Option<&'static str>,
        launch_retry: Option<&'static str>,
        queued_at: u64,
        updated_at: u64,
    }

    const INSTALLATION: u64 = 7;
    const NOW: u64 = 1_792_140_000_000 + 7 * 60 * 1000;

    fn seed(db: &Db, jobs: &[Seed]) {
        for job in jobs {
            db.run(
                "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
                 launch_state, runner_name, instance_id, launch_error, launch_attempts, \
                 launch_retry, next_launch_at, queued_at, updated_at) \
                 VALUES (?1, ?2, 'acme/web', ?3, 'hatchrun', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                &params![
                    job.id,
                    INSTALLATION,
                    job.id * 10,
                    job.status,
                    job.launch_state,
                    job.launch_state.map(|_| format!("hatchrun-{}", job.id)),
                    (job.launch_state == Some("launched")).then_some("i-0abc"),
                    (job.launch_state == Some("failed")).then_some("no capacity"),
                    u32::from(job.launch_state.is_some()),
                    job.launch_retry,
                    job.launch_retry.map(|_| job.updated_at + 60_000),
                    job.queued_at,
                    job.updated_at,
                ],
            );
        }
    }

    /// One job of each kind the dashboard tells apart, plus one of
    /// another installation, at times that do not fall on a bucket edge.
    fn jobs() -> Db {
        let db = Db::new();
        let ago = |minutes: u64| NOW - minutes * 60 * 1000 - 123;
        seed(
            &db,
            &[
                Seed {
                    id: 1,
                    status: "queued",
                    launch_state: None,
                    launch_retry: None,
                    queued_at: ago(3),
                    updated_at: ago(3),
                },
                Seed {
                    id: 2,
                    status: "queued",
                    launch_state: Some("launching"),
                    launch_retry: None,
                    queued_at: ago(5),
                    updated_at: ago(4),
                },
                Seed {
                    id: 3,
                    status: "in_progress",
                    launch_state: Some("launched"),
                    launch_retry: None,
                    queued_at: ago(20),
                    updated_at: ago(10),
                },
                Seed {
                    id: 4,
                    status: "completed",
                    launch_state: Some("launched"),
                    launch_retry: None,
                    queued_at: ago(90),
                    updated_at: ago(61),
                },
                Seed {
                    id: 5,
                    status: "queued",
                    launch_state: Some("failed"),
                    launch_retry: Some("backoff"),
                    queued_at: ago(30),
                    updated_at: ago(29),
                },
                Seed {
                    id: 6,
                    status: "queued",
                    launch_state: Some("failed"),
                    launch_retry: Some("setup"),
                    queued_at: ago(40),
                    updated_at: ago(38),
                },
                Seed {
                    id: 7,
                    status: "completed",
                    launch_state: Some("launched"),
                    launch_retry: None,
                    queued_at: ago(3000),
                    updated_at: ago(2990),
                },
            ],
        );
        db.run(
            "UPDATE jobs SET installation_id = ?1 WHERE id = 7",
            &params![INSTALLATION + 1],
        );
        seed(
            &db,
            &[Seed {
                id: 8,
                status: "completed",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(3000),
                updated_at: ago(2990),
            }],
        );
        db
    }

    fn ids(jobs: &[Job]) -> Vec<u64> {
        jobs.iter().map(|job| job.id).collect()
    }

    #[test]
    fn the_list_reads_into_jobs() {
        let db = jobs();
        let all: Vec<Job> = db.all(&list_query(None), &params![INSTALLATION, None::<u64>, PAGE]);
        assert_eq!(ids(&all), [1, 2, 3, 5, 6, 4, 8]);
        let failed = &all[4];
        assert_eq!(failed.launch_error.as_deref(), Some("no capacity"));
        assert_eq!(failed.launch_attempts, 1);
        assert!(failed.next_launch_at.is_some());

        let older: Vec<Job> = db.all(
            &list_query(None),
            &params![INSTALLATION, all[2].queued_at, 2u32],
        );
        assert_eq!(ids(&older), [5, 6]);

        let group = |g| {
            ids(&db.all::<Job>(
                &list_query(Some(g)),
                &params![INSTALLATION, None::<u64>, PAGE],
            ))
        };
        assert_eq!(group(Group::Active), [2, 3]);
        assert_eq!(group(Group::Waiting), [1, 5]);
        assert_eq!(group(Group::Failed), [6]);
    }

    #[test]
    fn the_counts_read_into_counts() {
        let db = jobs();
        let counts: Counts = db
            .first(&counts_query(), &params![INSTALLATION, NOW - DAY_MS])
            .unwrap();
        assert_eq!(
            (
                counts.waiting,
                counts.active,
                counts.running,
                counts.completed_today,
                counts.failed_today
            ),
            (2, 2, 1, 1, 1)
        );
        let none: Counts = db
            .first(&counts_query(), &params![99u64, NOW - DAY_MS])
            .unwrap();
        assert_eq!(none.active, 0);
    }

    #[test]
    fn the_activity_reads_into_buckets() {
        let db = jobs();
        let since = activity_since(NOW);
        let rows: Vec<Grouped> = db.all(&activity_query(), &params![INSTALLATION, since]);
        let got = activity(since, &rows);
        assert_eq!(got.finished.iter().sum::<u32>(), 1);
        assert_eq!(got.failed.iter().sum::<u32>(), 1);
        // 61 minutes back from 7 minutes into the last bucket is four
        // buckets back, 38 minutes back is three.
        assert_eq!(got.finished[BUCKETS - 5], 1);
        assert_eq!(got.failed[BUCKETS - 4], 1);
    }

    /// hatchrun/hatchrun#92: without the cast the bucket is a REAL such
    /// as 1.55, which does not read into the `i64` of `Grouped`.
    #[test]
    fn a_fractional_bucket_fails_to_read() {
        let before = format!(
            "SELECT (updated_at - ?2) / {BUCKET_MS} AS bucket, \
             COUNT(*) FILTER (WHERE status = 'completed') AS finished, \
             COUNT(*) FILTER (WHERE {FAILED}) AS failed \
             FROM jobs WHERE installation_id = ?1 AND updated_at >= ?2 \
             GROUP BY bucket"
        );
        let db = jobs();
        let since = activity_since(NOW);
        let err = db
            .try_all::<Grouped>(&before, &params![INSTALLATION, since])
            .unwrap_err();
        assert!(err.contains("floating point"), "{err}");
    }

    #[test]
    fn the_settings_change_a_tenant() {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
             VALUES ('t1', 1, 'acme', 0, 0)",
            &[],
        );
        let changed = db.run(
            SET_SETTINGS,
            &params!["t1", 3u32, Some("eu-west-1,us-east-1"), NOW],
        );
        assert_eq!(changed, 1);
        let rows = db
            .rows(
                "SELECT max_runners, allowed_regions, updated_at FROM tenants",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["max_runners"], 3);
        assert_eq!(rows[0]["allowed_regions"], "eu-west-1,us-east-1");
        db.run(SET_SETTINGS, &params!["t1", 3u32, None::<&str>, NOW]);
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
