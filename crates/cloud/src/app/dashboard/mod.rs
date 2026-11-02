// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The dashboard of a tenant that is set up.
//!
//! - `GET /api/tenants/<org>/runs` lists recent workflow runs with their
//!   hatchrun jobs and the jobs' instances, newest first, and counts
//!   what is running now. `?status=active|waiting|failed` lists the runs
//!   with a current job in that group, the runs the group's count
//!   counts. `?sort=started|repository&dir=desc|asc` orders the list,
//!   newest first by default; by repository, each repository's runs are
//!   newest first. `?q=<text>` keeps the runs whose repository, workflow
//!   or branch, or one of whose jobs' name, label or instance, contains
//!   the text. `?before=<queued_at>:<id>`, with `:<repository>` when
//!   sorted by repository, is the last run of the page before. The
//!   first page also carries the last day's finished jobs and failed
//!   launches in buckets of 15 minutes.
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
use worker::{Request, Response, console_warn};

use super::tenants::{authorize, show};
use super::{Answer, Ctx, Failure, metrics};
use crate::db::{Param, params};
use crate::jobs::{self, Direction, Group, RunsPage, Sort};
use crate::now_ms;
use crate::settings::Settings;

/// Runs in one page.
const PAGE: u32 = 25;
/// The window the counts of finished and failed jobs cover.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// One bucket of the activity strip.
const BUCKET_MS: u64 = 15 * 60 * 1000;
/// Buckets in a day.
const BUCKETS: usize = (DAY_MS / BUCKET_MS) as usize;

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
    /// When its last current job ended, null while one has not.
    ended_at: Option<u64>,
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

#[derive(Debug, Deserialize)]
struct Page {
    before: Option<String>,
    status: Option<Group>,
    #[serde(default)]
    sort: Sort,
    #[serde(default)]
    dir: Direction,
    q: Option<String>,
}

/// The longest search the list takes.
const SEARCH_MAX: usize = 200;

/// The last run of the page before.
#[derive(Debug, PartialEq, Eq)]
struct Cursor {
    queued_at: u64,
    id: u64,
    /// Set when sorted by repository.
    repository: Option<String>,
    /// When sorted by how long runs took, when the run ended; none for
    /// a run still going.
    ended_at: Option<u64>,
}

/// Reads `?before=<queued_at>:<id>`, and in `sort` order the third part
/// it needs: `:<repository>` by repository, `:<ended_at>` by how long
/// runs took, empty for a run still going. A repository name has no
/// colon, but everything after the second is taken as it is.
fn cursor(value: &str, sort: Sort) -> Option<Cursor> {
    let mut parts = value.splitn(3, ':');
    let queued_at = parts.next()?.parse().ok()?;
    let id = parts.next()?.parse().ok()?;
    let rest = parts.next();
    let (repository, ended_at) = match sort {
        Sort::Started => (None, None),
        Sort::Repository => (Some(rest?.to_owned()), None),
        Sort::Took => match rest? {
            "" => (None, None),
            at => (None, Some(at.parse().ok()?)),
        },
    };
    Some(Cursor {
        queued_at,
        id,
        repository,
        ended_at,
    })
}

/// An `ILIKE` pattern that matches `text` anywhere, with `%`, `_` and the
/// escape character itself taken literally under `ESCAPE '\'`.
fn contains(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
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

async fn jobs_of(ctx: &Ctx<'_>, installation: u64, runs: &[u64]) -> Result<Vec<Job>, Failure> {
    if runs.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(runs).map_err(|err| Failure::new(500, err.to_string()))?;
    Ok(jobs::jobs_of_runs(ctx.db, installation, ids).await?)
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
    let page: Page = req.query().map_err(|_| {
        Failure::new(
            400,
            "status is active, waiting or failed; sort is started, repository or took; \
             dir is asc or desc.",
        )
    })?;
    let before = match page.before.as_deref() {
        Some(value) => Some(cursor(value, page.sort).ok_or_else(|| {
            Failure::new(
                400,
                "before is <queuedAt>:<id>, <queuedAt>:<id>:<repository> by repository \
                     and <queuedAt>:<id>:<endedAt> by took, with no endedAt for a live run.",
            )
        })?),
        None => None,
    };
    let q = page.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if q.is_some_and(|q| q.chars().count() > SEARCH_MAX) {
        return Err(Failure::new(
            400,
            format!("q is at most {SEARCH_MAX} characters."),
        ));
    }
    let fifth = match (&before, page.sort) {
        (
            Some(Cursor {
                repository: Some(repository),
                ..
            }),
            Sort::Repository,
        ) => Param::from(repository.as_str()),
        (
            Some(Cursor {
                ended_at: Some(at), ..
            }),
            Sort::Took,
        ) => Param::from(*at),
        _ => Param::Null,
    };
    // The page, then its jobs; the counts and the activity strip at the
    // same time, on the same connection, in one round trip each.
    let now = now_ms();
    let since = activity_since(now);
    let page = async {
        let runs: Vec<WorkflowRun> = jobs::list_runs(
            ctx.db,
            installation,
            RunsPage {
                group: page.status,
                sort: page.sort,
                dir: page.dir,
                queued_at: before.as_ref().map(|c| c.queued_at),
                id: before.as_ref().map(|c| c.id),
                limit: PAGE,
                fifth,
                search: q.map(contains),
            },
        )
        .await?;
        let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
        let jobs = jobs_of(ctx, installation, &ids).await?;
        Ok::<_, Failure>((runs, jobs))
    };
    let counts = jobs::counts::<Counts>(ctx.db, installation, now.saturating_sub(DAY_MS));
    let strip = async {
        if before.is_some() {
            return Ok(None);
        }
        jobs::activity::<Grouped>(ctx.db, installation, since, BUCKET_MS)
            .await
            .map(Some)
    };
    let (page, counts, strip) = futures_util::join!(page, counts, strip);
    let (runs, jobs) = page?;
    let counts = counts?.unwrap_or_default();
    let mut body = serde_json::json!({
        "runs": list(&ctx.config.github_web, runs, &jobs),
        "counts": counts,
        "region": row.region,
        "pageSize": PAGE,
    });
    if let Some(rows) = strip? {
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
    let run: WorkflowRun = jobs::run(ctx.db, id, installation)
        .await?
        .ok_or_else(missing)?;
    let jobs = jobs_of(ctx, installation, &[id]).await?;
    let since = jobs
        .iter()
        .map(|job| job.queued_at)
        .min()
        .unwrap_or(run.queued_at);
    // Newest attempt first, so a run past the resources cap loses the
    // numbers of its oldest jobs.
    let ids: Vec<u64> = jobs.iter().rev().map(|job| job.id).collect();
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
const SET_SETTINGS: &str = "UPDATE tenants SET max_runners = $2, allowed_regions = $3, \
     updated_at = $4 WHERE id = $1";

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
        .run(
            SET_SETTINGS,
            &params![
                row.id.as_str(),
                checked.max_runners,
                checked.regions_column(),
                now_ms()
            ],
        )
        .await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    show(ctx, &row.org_login).await
}

#[cfg(test)]
mod tests;
