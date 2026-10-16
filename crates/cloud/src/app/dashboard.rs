// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The dashboard of a tenant that is set up.
//!
//! - `GET /api/tenants/<org>/runs` lists recent workflow runs with their
//!   hatchrun jobs and the jobs' instances, newest first, and counts
//!   what is running now. `?status=active|waiting|failed` lists the runs
//!   with a current job in that group, the runs the group's count
//!   counts. `?before=<queued_at>:<id>` pages back. The first page also
//!   carries the last day's finished jobs and failed launches in buckets
//!   of 15 minutes.
//! - `GET /api/tenants/<org>/runs/<id>` returns one run with the jobs of
//!   every attempt and what each job used of its instance.
//! - `PUT /api/tenants/<org>/settings` with `{"maxRunners",
//!   "allowedRegions"}` changes the settings.
//!
//! A run's current jobs are the newest attempt of each job name, as
//! GitHub shows a run: after re-running its failed jobs, the jobs that
//! passed in the first attempt are still part of it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{Request, Response, console_warn};

use super::tenants::{authorize, show};
use super::{Answer, Ctx, Failure, metrics};
use crate::settings::Settings;
use crate::{int, now_ms};

/// Runs in one page.
const PAGE: u32 = 25;
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
/// A job no later attempt of its run replaced, for a job named `j`.
const CURRENT: &str = "NOT EXISTS (SELECT 1 FROM jobs AS later \
     WHERE later.run_id = j.run_id AND later.name = j.name \
     AND later.run_attempt > j.run_attempt)";

const RUN_COLUMNS: &str = "id, repository, workflow_name, head_branch, head_sha, attempt, \
     queued_at, run_number, event, display_title, actor, path";

const JOB_COLUMNS: &str = "id, run_id, name, run_attempt, label, status, conclusion, \
     launch_state, runner_name, instance_id, launch_error, launch_attempts, launch_retry, \
     next_launch_at, queued_at, started_at, completed_at, updated_at";

/// A run, from `runs`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct WorkflowRun {
    id: u64,
    repository: String,
    workflow_name: Option<String>,
    head_branch: Option<String>,
    head_sha: String,
    /// The newest attempt.
    attempt: u64,
    /// When the newest attempt's first job was queued.
    queued_at: u64,
    /// The rest is null until the run's details are fetched.
    run_number: Option<u64>,
    event: Option<String>,
    display_title: Option<String>,
    actor: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Job {
    id: u64,
    run_id: u64,
    name: String,
    run_attempt: u64,
    label: String,
    status: String,
    /// GitHub's, once completed.
    conclusion: Option<String>,
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
    started_at: Option<u64>,
    completed_at: Option<u64>,
    updated_at: u64,
}

/// A run in the list: its current jobs, and how many older attempts'
/// jobs the run page has besides.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    #[serde(flatten)]
    run: WorkflowRun,
    github_url: String,
    jobs: Vec<Job>,
    older_jobs: usize,
}

/// A run on its own page, with the jobs of every attempt.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Detailed {
    #[serde(flatten)]
    run: WorkflowRun,
    github_url: String,
    jobs: Vec<Attempted>,
}

/// A job on the run page, and whether it is current.
#[derive(Debug, Serialize)]
struct Attempted {
    #[serde(flatten)]
    job: Job,
    current: bool,
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
    /// The runs with a job in each group, as the status tabs list them.
    waiting_runs: u32,
    active_runs: u32,
    failed_runs_today: u32,
}

/// A status tab of the runs table.
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
    before: Option<String>,
    status: Option<Group>,
}

/// Reads `?before=<queued_at>:<id>`, the last run of the page before.
fn cursor(value: &str) -> Option<(u64, u64)> {
    let (queued_at, id) = value.split_once(':')?;
    Some((queued_at.parse().ok()?, id.parse().ok()?))
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

/// The query for one page of runs, of one group or of all. A run is in
/// a group when one of its current jobs is; the inner query starts from
/// the jobs, through their indexes, so a tenant with few failures does
/// not read every run.
fn list_query(group: Option<Group>) -> String {
    let filter = group.map_or(String::new(), |g| {
        format!(
            " AND id IN (SELECT j.run_id FROM jobs AS j \
             WHERE j.installation_id = ?1 AND {} AND {CURRENT})",
            g.predicate()
        )
    });
    format!(
        "SELECT {RUN_COLUMNS} FROM runs \
         WHERE installation_id = ?1 AND (?2 IS NULL OR (queued_at, id) < (?2, ?3)){filter} \
         ORDER BY queued_at DESC, id DESC LIMIT ?4"
    )
}

/// The jobs, of every attempt, of the runs in the JSON array `?2`.
fn jobs_query() -> String {
    format!(
        "SELECT {JOB_COLUMNS} FROM jobs \
         WHERE installation_id = ?1 AND run_id IN (SELECT value FROM json_each(?2)) \
         ORDER BY run_id, run_attempt, queued_at, id"
    )
}

/// One run of an installation.
fn run_query() -> String {
    format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1 AND installation_id = ?2")
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
         COUNT(*) FILTER (WHERE updated_at >= ?2 AND ({FAILED})) AS failed_today, \
         COUNT(DISTINCT run_id) FILTER (WHERE {WAITING}) AS waiting_runs, \
         COUNT(DISTINCT run_id) FILTER (WHERE {ACTIVE}) AS active_runs, \
         COUNT(DISTINCT run_id) FILTER (WHERE updated_at >= ?2 AND ({FAILED}) AND {CURRENT}) \
           AS failed_runs_today \
         FROM jobs AS j WHERE installation_id = ?1 \
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

/// Whether each job is current: no job of the same run and name has a
/// later attempt.
fn current(jobs: &[Job]) -> Vec<bool> {
    let mut newest: HashMap<(u64, &str), u64> = HashMap::new();
    for job in jobs {
        let attempt = newest.entry((job.run_id, &job.name)).or_default();
        *attempt = (*attempt).max(job.run_attempt);
    }
    jobs.iter()
        .map(|job| newest.get(&(job.run_id, job.name.as_str())) == Some(&job.run_attempt))
        .collect()
}

/// The run on GitHub.
fn github_url(web: &str, run: &WorkflowRun) -> String {
    format!("{web}/{}/actions/runs/{}", run.repository, run.id)
}

/// Puts each run's current jobs under it.
fn list(web: &str, runs: Vec<WorkflowRun>, jobs: &[Job]) -> Vec<Listed> {
    let flags = current(jobs);
    let mut of_run: HashMap<u64, (Vec<Job>, usize)> = HashMap::new();
    for (job, current) in jobs.iter().zip(flags) {
        let entry = of_run.entry(job.run_id).or_default();
        if current {
            entry.0.push(job.clone());
        } else {
            entry.1 += 1;
        }
    }
    runs.into_iter()
        .map(|run| {
            let (jobs, older_jobs) = of_run.remove(&run.id).unwrap_or_default();
            Listed {
                github_url: github_url(web, &run),
                run,
                jobs,
                older_jobs,
            }
        })
        .collect()
}

async fn jobs_of(ctx: &Ctx<'_>, installation: &JsValue, runs: &[u64]) -> Result<Vec<Job>, Failure> {
    if runs.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(runs).map_err(|err| Failure::new(500, err.to_string()))?;
    Ok(ctx
        .db
        .prepare(jobs_query())
        .bind(&[installation.clone(), ids.into()])?
        .all()
        .await?
        .results::<Job>()?)
}

/// `GET /api/tenants/<org>/runs`
pub(crate) async fn runs(ctx: &Ctx<'_>, org: &str, req: &Request) -> Answer {
    let row = authorize(ctx, org).await?;
    let Some(installation) = row.installation_id else {
        return Ok(Response::from_json(&serde_json::json!({
            "runs": [],
            "counts": Counts::default(),
            "region": row.region,
        }))?);
    };
    let page: Page = req
        .query()
        .map_err(|_| Failure::new(400, "status is one of active, waiting or failed."))?;
    let before = match page.before.as_deref() {
        Some(value) => {
            Some(cursor(value).ok_or_else(|| Failure::new(400, "before is <queuedAt>:<id>."))?)
        }
        None => None,
    };
    let installation = int(installation)?;
    let runs = ctx
        .db
        .prepare(list_query(page.status))
        .bind(&[
            installation.clone(),
            before.map_or(Ok(JsValue::NULL), |(at, _)| int(at))?,
            before.map_or(Ok(JsValue::NULL), |(_, id)| int(id))?,
            PAGE.into(),
        ])?
        .all()
        .await?
        .results::<WorkflowRun>()?;
    let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
    let jobs = jobs_of(ctx, &installation, &ids).await?;
    let now = now_ms();
    let counts: Counts = ctx
        .db
        .prepare(counts_query())
        .bind(&[installation.clone(), int(now.saturating_sub(DAY_MS))?])?
        .first(None)
        .await?
        .unwrap_or_default();
    let mut body = serde_json::json!({
        "runs": list(&ctx.config.github_web, runs, &jobs),
        "counts": counts,
        "region": row.region,
        "pageSize": PAGE,
    });
    if before.is_none() {
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

/// `GET /api/tenants/<org>/runs/<id>`
pub(crate) async fn run(ctx: &Ctx<'_>, org: &str, id: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let missing = || Failure::new(404, format!("no run {id} in {}", row.org_login));
    let (Some(installation), Ok(id)) = (row.installation_id, id.parse::<u64>()) else {
        return Err(missing());
    };
    let run: WorkflowRun = ctx
        .db
        .prepare(run_query())
        .bind(&[int(id)?, int(installation)?])?
        .first(None)
        .await?
        .ok_or_else(missing)?;
    let jobs = jobs_of(ctx, &int(installation)?, &[id]).await?;
    let since = jobs
        .iter()
        .map(|job| job.queued_at)
        .min()
        .unwrap_or(run.queued_at);
    let ids: Vec<u64> = jobs.iter().map(|job| job.id).collect();
    // The run page stands without its numbers.
    let resources = match metrics::resources(ctx, installation, &ids, since).await {
        Ok(resources) => Some(resources),
        Err(err) => {
            console_warn!("run {id}: {}", err.message);
            None
        }
    };
    let flags = current(&jobs);
    let jobs: Vec<Attempted> = jobs
        .into_iter()
        .zip(flags)
        .map(|(job, current)| Attempted { job, current })
        .collect();
    Ok(Response::from_json(&serde_json::json!({
        "run": Detailed {
            github_url: github_url(&ctx.config.github_web, &run),
            run,
            jobs,
        },
        "region": row.region,
        "resources": resources,
    }))?)
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
            assert!(list.contains(&format!(" AND {} AND {CURRENT})", group.predicate())));
            assert!(counts.contains(group.predicate()));
        }
        let all = list_query(None);
        assert!([ACTIVE, WAITING, FAILED].iter().all(|p| !all.contains(p)));
    }

    #[test]
    fn a_cursor_is_a_time_and_a_run() {
        assert_eq!(
            cursor("1792140000000:18234567890"),
            Some((1_792_140_000_000, 18_234_567_890))
        );
        assert_eq!(cursor("1792140000000"), None);
        assert_eq!(cursor("x:1"), None);
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
            jobs_query(),
            counts_query(),
            activity_query(),
        ] {
            assert!(query.contains("WHERE installation_id = ?1 "), "{query}");
        }
        assert!(list_query(Some(Group::Failed)).contains("WHERE j.installation_id = ?1 "));
        assert!(run_query().ends_with("WHERE id = ?1 AND installation_id = ?2"));
        assert!(list_query(None).contains(" LIMIT ?4"));
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

    /// Each job in a run of its own, `id * 10`, unless the test adds
    /// more jobs to it.
    fn seed(db: &Db, jobs: &[Seed]) {
        for job in jobs {
            db.run(
                "INSERT INTO runs (id, installation_id, repository, workflow_name, head_sha, \
                 attempt, queued_at, created_at, updated_at) \
                 VALUES (?1, ?2, 'acme/web', 'CI', 'ab', 1, ?3, ?3, ?3) ON CONFLICT (id) DO NOTHING",
                &params![job.id * 10, INSTALLATION, job.queued_at],
            );
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
        db.run(
            "UPDATE runs SET installation_id = ?1 WHERE id = 70",
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

    fn ids(runs: &[WorkflowRun]) -> Vec<u64> {
        runs.iter().map(|run| run.id).collect()
    }

    fn page(
        db: &Db,
        group: Option<Group>,
        before: Option<(u64, u64)>,
        limit: u32,
    ) -> Vec<WorkflowRun> {
        db.all(
            &list_query(group),
            &params![
                INSTALLATION,
                before.map(|(at, _)| at),
                before.map(|(_, id)| id),
                limit
            ],
        )
    }

    fn jobs_of(db: &Db, runs: &[u64]) -> Vec<Job> {
        db.all(
            &jobs_query(),
            &params![INSTALLATION, serde_json::to_string(runs).unwrap()],
        )
    }

    #[test]
    fn the_list_reads_into_runs() {
        let db = jobs();
        let all = page(&db, None, None, PAGE);
        assert_eq!(ids(&all), [10, 20, 30, 50, 60, 40, 80]);
        assert_eq!(all[0].workflow_name.as_deref(), Some("CI"));
        let jobs = jobs_of(&db, &ids(&all));
        assert_eq!(jobs.len(), 7);
        let failed = jobs.iter().find(|job| job.id == 5).unwrap();
        assert_eq!(failed.launch_error.as_deref(), Some("no capacity"));
        assert_eq!(failed.launch_attempts, 1);
        assert!(failed.next_launch_at.is_some());

        let older = page(&db, None, Some((all[2].queued_at, all[2].id)), 2);
        assert_eq!(ids(&older), [50, 60]);

        let group = |g| ids(&page(&db, Some(g), None, PAGE));
        assert_eq!(group(Group::Active), [20, 30]);
        assert_eq!(group(Group::Waiting), [10, 50]);
        assert_eq!(group(Group::Failed), [60]);
    }

    /// Run 100: two jobs in attempt 1, one of which never launched; the
    /// re-run of that one in attempt 2 is running.
    fn rerun(db: &Db) {
        let ago = |minutes: u64| NOW - minutes * 60 * 1000;
        seed(
            db,
            &[
                Seed {
                    id: 11,
                    status: "completed",
                    launch_state: Some("launched"),
                    launch_retry: None,
                    queued_at: ago(50),
                    updated_at: ago(45),
                },
                Seed {
                    id: 12,
                    status: "completed",
                    launch_state: Some("failed"),
                    launch_retry: None,
                    queued_at: ago(50),
                    updated_at: ago(44),
                },
                Seed {
                    id: 13,
                    status: "in_progress",
                    launch_state: Some("launched"),
                    launch_retry: None,
                    queued_at: ago(2),
                    updated_at: ago(1),
                },
            ],
        );
        db.run(
            "UPDATE jobs SET run_id = 100, name = CASE id WHEN 11 THEN 'Save' ELSE 'Restore' END, \
             run_attempt = CASE id WHEN 13 THEN 2 ELSE 1 END WHERE id IN (11, 12, 13)",
            &[],
        );
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, queued_at, \
             created_at, updated_at) VALUES (100, ?1, 'acme/web', 'ab', 2, ?2, ?2, ?2)",
            &params![INSTALLATION, ago(2)],
        );
        db.run("DELETE FROM runs WHERE id IN (110, 120, 130)", &[]);
    }

    #[test]
    fn a_rerun_replaces_only_the_jobs_it_runs_again() {
        let db = Db::new();
        rerun(&db);
        let jobs = jobs_of(&db, &[100]);
        assert_eq!(
            jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            [11, 12, 13]
        );
        assert_eq!(current(&jobs), [true, false, true]);

        let runs = page(&db, None, None, PAGE);
        let listed = list("https://github.com", runs, &jobs);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            [11, 13]
        );
        assert_eq!(listed[0].older_jobs, 1);
        assert_eq!(
            listed[0].github_url,
            "https://github.com/acme/web/actions/runs/100"
        );

        // The failed launch was replaced, so the run is active, not failed.
        assert_eq!(ids(&page(&db, Some(Group::Active), None, PAGE)), [100]);
        assert!(page(&db, Some(Group::Failed), None, PAGE).is_empty());
        let counts: Counts = db
            .first(&counts_query(), &params![INSTALLATION, NOW - DAY_MS])
            .unwrap();
        assert_eq!((counts.active_runs, counts.failed_runs_today), (1, 0));
        assert_eq!(counts.failed_today, 1);
    }

    #[test]
    fn a_listed_run_reads_as_the_app_expects() {
        let db = Db::new();
        rerun(&db);
        let listed = list(
            "https://github.com",
            page(&db, None, None, PAGE),
            &jobs_of(&db, &[100]),
        );
        let value = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(value["id"], 100);
        assert_eq!(value["attempt"], 2);
        assert_eq!(value["olderJobs"], 1);
        assert_eq!(value["runNumber"], serde_json::Value::Null);
        assert_eq!(value["jobs"][1]["runAttempt"], 2);
        assert_eq!(value["jobs"][1]["name"], "Restore");
    }

    #[test]
    fn tabs_start_from_the_job_indexes() {
        let db = jobs();
        let plan = |sql: &str| -> String {
            db.rows(
                &format!("EXPLAIN QUERY PLAN {sql}"),
                &params![INSTALLATION, None::<u64>, None::<u64>, PAGE],
            )
            .unwrap()
            .iter()
            .map(|row| row["detail"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
        };
        let failed = plan(&list_query(Some(Group::Failed)));
        // Through jobs_failed or jobs_launch, never every job, and the
        // later attempts through jobs_run.
        assert!(failed.contains("launch_state=?"), "{failed}");
        assert!(!failed.contains("SCAN"), "{failed}");
        assert!(failed.contains("jobs_run"), "{failed}");
        let all = plan(&list_query(None));
        assert!(all.contains("runs_recent"), "{all}");
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
        assert_eq!(
            (
                counts.waiting_runs,
                counts.active_runs,
                counts.failed_runs_today
            ),
            (2, 2, 1)
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
