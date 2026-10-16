// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! One job's CPU and memory, as its runner reported them to
//! hatchrun-telemetry (crates/telemetry).
//!
//! - `GET /api/tenants/<org>/jobs/<id>/metrics` returns the job and its
//!   run, the link to it on GitHub, the tenant's region for the
//!   instance's console page, a summary of its windows and the windows
//!   themselves, oldest first.
//!
//! The run page shows a few of the summary's numbers for each of its
//! jobs, from one query over them all ([`resources`]).
//!
//! The windows live in Workers Analytics Engine, read through the
//! Analytics SQL binding. Every query names the tenant's installation,
//! which comes from the tenant row and never from the request, binds
//! its values as parameters, reads nothing older than the job, and has
//! a `LIMIT`. Analytics Engine may sample a busy dataset; the SQL API
//! weights `COUNT`, `SUM` and `AVG` by each row's sample interval, and
//! the summary uses only those and approximate maxima. Logs stay on
//! GitHub; the app links to the job there.

use js_sys::{JSON, Object, Promise, Reflect};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::Response;

use super::tenants::authorize;
use super::{Answer, Ctx, Failure};
use crate::int;

/// Analytics SQL binding in `wrangler.toml`.
const BINDING: &str = "ANALYTICS_SQL";

#[wasm_bindgen]
extern "C" {
    /// The Analytics SQL binding.
    type AnalyticsSql;

    /// Runs one `SELECT`; answers `{ data, rows, statistics }`.
    #[wasm_bindgen(method, catch)]
    fn query(this: &AnalyticsSql, request: &Object) -> Result<JsValue, JsValue>;
}

/// Most windows a job returns: a job runs eight hours at most, 2880
/// windows of ten seconds.
const MAX_WINDOWS: u32 = 3000;

/// The windows' columns, by the data point order of
/// `hatchrun_telemetry::protocol::Window::doubles`.
const COLUMNS: &str = "double1 AS at, double2 AS seconds, double3 AS cpu_min, \
    double4 AS cpu_avg, double5 AS cpu_max, double6 AS cpu_steal, double7 AS mem_max, \
    double8 AS mem_total, double9 AS cgroup_peak, double10 AS oom_kills, \
    double11 AS disk_used, double12 AS net_rx, double13 AS net_tx";

/// The job's windows. `index1` is the job id and `blob1` the
/// installation that reported it.
const WINDOWS: &str = "FROM events.analyticsEngine.hatchrun_runner_metrics \
    WHERE index1 = $job AND blob1 = $installation AND timestamp >= $since";

/// The job's windows, oldest first. The SQL API resolves column names
/// in `ORDER BY`, not the aliases of `COLUMNS`.
fn windows_query() -> String {
    format!("SELECT {COLUMNS} {WINDOWS} ORDER BY double1 LIMIT {MAX_WINDOWS}")
}

/// One window, as the agent summed it up.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Window {
    /// When the window ended, in Unix seconds.
    at: f64,
    seconds: f64,
    cpu_min: f64,
    cpu_avg: f64,
    cpu_max: f64,
    cpu_steal: f64,
    mem_max: f64,
    mem_total: f64,
    cgroup_peak: f64,
    oom_kills: f64,
    disk_used: f64,
    net_rx: f64,
    net_tx: f64,
}

/// The job's windows together, weighted by sampling. Every aggregate
/// is null when there are no windows.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Summary {
    /// Seconds the windows cover.
    seconds: Option<f64>,
    cpu_avg: Option<f64>,
    cpu_max: Option<f64>,
    cpu_steal: Option<f64>,
    mem_max: Option<f64>,
    mem_total: Option<f64>,
    cgroup_peak: Option<f64>,
    oom_kills: Option<f64>,
    disk_used: Option<f64>,
    net_rx: Option<f64>,
    net_tx: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct Job {
    id: u64,
    repository: String,
    run_id: u64,
    name: String,
    run_attempt: u64,
    label: String,
    status: String,
    conclusion: Option<String>,
    runner_name: Option<String>,
    instance_id: Option<String>,
    queued_at: u64,
    started_at: Option<u64>,
    completed_at: Option<u64>,
    updated_at: u64,
    /// The run's, for the way back to it.
    workflow_name: Option<String>,
    run_number: Option<u64>,
}

/// One job of an installation and its run, from D1. Binds the job id
/// and the installation. A job from before runs were recorded has no
/// run until the backfill gives it one.
const JOB: &str = "SELECT jobs.id, jobs.repository, jobs.run_id, jobs.name, jobs.run_attempt, \
    jobs.label, jobs.status, jobs.conclusion, jobs.runner_name, jobs.instance_id, \
    jobs.queued_at, jobs.started_at, jobs.completed_at, jobs.updated_at, \
    runs.workflow_name, runs.run_number \
    FROM jobs LEFT JOIN runs ON runs.id = jobs.run_id \
    WHERE jobs.id = ?1 AND jobs.installation_id = ?2";

/// Most jobs of a run [`resources`] covers, the largest matrix GitHub
/// allows and then some.
const MAX_RESOURCES: usize = 300;

/// What one job used, as the run page lists it.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct Resources {
    /// The job id, as `index1` holds it.
    job: String,
    cpu_avg: Option<f64>,
    cpu_max: Option<f64>,
    mem_max: Option<f64>,
    mem_total: Option<f64>,
    oom_kills: Option<f64>,
}

/// The query for [`resources`]. The ids come from D1, never the
/// request, and are digits only.
fn resources_query(jobs: &[u64]) -> String {
    let ids: Vec<String> = jobs
        .iter()
        .take(MAX_RESOURCES)
        .map(|id| format!("'{id}'"))
        .collect();
    format!(
        "SELECT index1 AS job, AVG(double4) AS cpu_avg, APPROX_MAX(double5) AS cpu_max, \
         APPROX_MAX(double7) AS mem_max, APPROX_MAX(double8) AS mem_total, \
         SUM(double10) AS oom_kills \
         FROM events.analyticsEngine.hatchrun_runner_metrics \
         WHERE blob1 = $installation AND index1 IN ({}) AND timestamp >= $since \
         GROUP BY index1 LIMIT {MAX_RESOURCES}",
        ids.join(", ")
    )
}

/// What each of `jobs` of `installation` used, from `since` on, the
/// oldest job's queue time. Jobs with nothing reported are left out.
pub(crate) async fn resources(
    ctx: &Ctx<'_>,
    installation: u64,
    jobs: &[u64],
    since: u64,
) -> Result<Vec<Resources>, Failure> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let installation = installation.to_string();
    let since = iso(since)?;
    query(
        ctx,
        &resources_query(jobs),
        &[("installation", &installation), ("since", &since)],
    )
    .await
}

/// `ms` as an ISO 8601 string, as the SQL API compares `timestamp`.
fn iso(ms: u64) -> Result<String, Failure> {
    Ok(js_sys::Date::new(&int(ms)?).to_iso_string().into())
}

/// What the SQL binding answers.
#[derive(Deserialize)]
struct Answered<T> {
    data: Vec<T>,
}

/// `GET /api/tenants/<org>/jobs/<id>/metrics`
pub(crate) async fn job(ctx: &Ctx<'_>, org: &str, id: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let missing = || Failure::new(404, format!("no job {id} in {}", row.org_login));
    let (Some(installation), Ok(id)) = (row.installation_id, id.parse::<u64>()) else {
        return Err(missing());
    };
    let job: Job = ctx
        .db
        .prepare(JOB)
        .bind(&[int(id)?, int(installation)?])?
        .first(None)
        .await?
        .ok_or_else(missing)?;

    let id = job.id.to_string();
    let installation = installation.to_string();
    let since = iso(job.queued_at)?;
    let params = [
        ("job", id.as_str()),
        ("installation", installation.as_str()),
        ("since", since.as_str()),
    ];
    let windows: Vec<Window> = query(ctx, &windows_query(), &params).await?;
    let summary: Option<Summary> = query(ctx, SUMMARY, &params).await?.into_iter().next();
    Ok(Response::from_json(&serde_json::json!({
        "githubUrl": github_url(&ctx.config.github_web, &job),
        "region": row.region,
        "job": job,
        "summary": summary.filter(|summary| summary.seconds.is_some_and(|seconds| seconds > 0.0)),
        "windows": distinct(windows),
    }))?)
}

/// The job's page on GitHub, where its logs are.
fn github_url(web: &str, job: &Job) -> String {
    format!(
        "{web}/{}/actions/runs/{}/job/{}",
        job.repository, job.run_id, job.id
    )
}

const SUMMARY: &str = "SELECT SUM(double2) AS seconds, \
    AVG(double4) AS cpu_avg, APPROX_MAX(double5) AS cpu_max, AVG(double6) AS cpu_steal, \
    APPROX_MAX(double7) AS mem_max, APPROX_MAX(double8) AS mem_total, \
    APPROX_MAX(double9) AS cgroup_peak, SUM(double10) AS oom_kills, \
    APPROX_MAX(double11) AS disk_used, SUM(double12) AS net_rx, SUM(double13) AS net_tx \
    FROM events.analyticsEngine.hatchrun_runner_metrics \
    WHERE index1 = $job AND blob1 = $installation AND timestamp >= $since LIMIT 1";

/// Runs `sql` through the Analytics SQL binding with its named
/// parameters: the installation, the lower time bound as an ISO 8601
/// string, and for one job's queries the job.
async fn query<T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'_>,
    sql: &str,
    params: &[(&str, &str)],
) -> Result<Vec<T>, Failure> {
    let failed = |err: JsValue| {
        Failure::new(
            502,
            format!(
                "metrics query: {}",
                err.as_string()
                    .or_else(|| err
                        .dyn_ref::<js_sys::Error>()
                        .map(|err| String::from(err.message())))
                    .unwrap_or_else(|| format!("{err:?}"))
            ),
        )
    };
    let binding = Reflect::get(ctx.env, &BINDING.into()).map_err(failed)?;
    if binding.is_undefined() {
        return Err(Failure::new(500, format!("no {BINDING} binding")));
    }
    let named = Object::new();
    for &(name, value) in params {
        Reflect::set(&named, &name.into(), &value.into()).map_err(failed)?;
    }
    let request = Object::new();
    Reflect::set(&request, &"query".into(), &sql.into()).map_err(failed)?;
    Reflect::set(&request, &"params".into(), &named).map_err(failed)?;
    // The binding is an RPC stub. Its answer is a thenable RpcPromise,
    // not a Promise, and anything read off the stub, `query.call`
    // included, is a remote method; only `binding.query(request)` works.
    let answered = binding
        .unchecked_into::<AnalyticsSql>()
        .query(&request)
        .map_err(failed)?;
    let answer = JsFuture::from(Promise::resolve(&answered))
        .await
        .map_err(failed)?;
    let text: String = JSON::stringify(&answer).map_err(failed)?.into();
    let answered: Answered<T> = serde_json::from_str(&text)
        .map_err(|err| Failure::new(502, format!("metrics query answer: {err}")))?;
    Ok(answered.data)
}

/// The windows by end time, each once. A report the agent sent again
/// after a lost answer writes its windows twice.
fn distinct(mut windows: Vec<Window>) -> Vec<Window> {
    windows.sort_by(|a, b| a.at.total_cmp(&b.at));
    windows.dedup_by(|a, b| a.at.total_cmp(&b.at).is_eq());
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    #[test]
    fn reads_a_job_of_the_installation() {
        let db = Db::new();
        let queued_at = 1_792_140_000_000u64;
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, launch_state, runner_name, instance_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            &params![
                7u64,
                3u64,
                "acme/app",
                9u64,
                "hatchrun-2cpu",
                "completed",
                queued_at,
                queued_at + 60_000,
                "launched",
                "hatchrun-7",
                "i-0123456789abcdef0",
            ],
        );
        db.run(
            "INSERT INTO runs (id, installation_id, repository, workflow_name, head_sha, attempt, \
             queued_at, run_number, created_at, updated_at) \
             VALUES (9, 3, 'acme/app', 'Smoke test', 'ab', 1, ?1, 412, ?1, ?1)",
            &params![queued_at],
        );
        let job: Job = db.first(JOB, &params![7u64, 3u64]).unwrap();
        assert_eq!(job.id, 7);
        assert_eq!(job.run_id, 9);
        assert_eq!(job.workflow_name.as_deref(), Some("Smoke test"));
        assert_eq!(job.run_number, Some(412));
        assert_eq!(job.queued_at, queued_at);
        assert_eq!(job.updated_at, queued_at + 60_000);
        assert_eq!(job.instance_id.as_deref(), Some("i-0123456789abcdef0"));
        assert!(db.first::<Job>(JOB, &params![7u64, 4u64]).is_none());
        // A job from before runs were recorded, until the backfill.
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at) VALUES (8, 3, 'acme/app', 99, 'hatchrun', 'completed', 1, 2)",
            &[],
        );
        let old: Job = db.first(JOB, &params![8u64, 3u64]).unwrap();
        assert_eq!((old.workflow_name, old.run_number), (None, None));
    }

    fn window(at: f64) -> Window {
        Window {
            at,
            seconds: 10.0,
            cpu_min: 1.0,
            cpu_avg: 2.0,
            cpu_max: 3.0,
            cpu_steal: 0.0,
            mem_max: 1.0,
            mem_total: 2.0,
            cgroup_peak: 1.0,
            oom_kills: 0.0,
            disk_used: 1.0,
            net_rx: 0.0,
            net_tx: 0.0,
        }
    }

    #[test]
    fn windows_come_once_and_in_order() {
        let at = |windows: Vec<Window>| windows.iter().map(|w| w.at).collect::<Vec<_>>();
        assert_eq!(
            at(distinct(vec![
                window(30.0),
                window(10.0),
                window(20.0),
                window(10.0)
            ])),
            [10.0, 20.0, 30.0]
        );
    }

    #[test]
    fn links_to_the_job_on_github() {
        let job = Job {
            id: 7,
            repository: "acme/app".into(),
            run_id: 3,
            name: "build".into(),
            run_attempt: 1,
            label: "hatchrun-2cpu".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            runner_name: None,
            instance_id: None,
            queued_at: 0,
            started_at: None,
            completed_at: None,
            updated_at: 0,
            workflow_name: None,
            run_number: None,
        };
        assert_eq!(
            github_url("https://github.com", &job),
            "https://github.com/acme/app/actions/runs/3/job/7"
        );
    }

    #[test]
    fn resources_cover_the_run_s_jobs_at_most() {
        let sql = resources_query(&[7, 8]);
        assert!(sql.contains("index1 IN ('7', '8')"), "{sql}");
        assert!(sql.contains("blob1 = $installation"), "{sql}");
        assert!(sql.contains("timestamp >= $since"), "{sql}");
        assert!(sql.contains(" GROUP BY index1 LIMIT "), "{sql}");
        assert!(!sql.replace("APPROX_MAX(", "").contains("MAX("), "{sql}");
        let many: Vec<u64> = (1..=1000).collect();
        assert_eq!(
            resources_query(&many).matches('\'').count(),
            2 * MAX_RESOURCES
        );
        let answered: Answered<Resources> = serde_json::from_str(
            r#"{"data":[{"job":"7","cpu_avg":40.5,"cpu_max":99,"mem_max":1024,
                "mem_total":4096,"oom_kills":0}]}"#,
        )
        .unwrap();
        assert_eq!(answered.data[0].job, "7");
        assert_eq!(answered.data[0].cpu_avg, Some(40.5));
    }

    #[test]
    fn every_query_is_bounded_and_scoped() {
        let windows = windows_query();
        assert!(
            windows.contains(" ORDER BY double1 "),
            "the SQL API sorts by column, not alias: {windows}"
        );
        for sql in [windows.as_str(), SUMMARY] {
            assert!(sql.contains("blob1 = $installation"), "{sql}");
            assert!(sql.contains("index1 = $job"), "{sql}");
            assert!(sql.contains("timestamp >= $since"), "{sql}");
            assert!(sql.contains(" LIMIT "), "{sql}");
            assert!(
                !sql.replace("APPROX_MAX(", "").contains("MAX("),
                "exact maxima are refused on sampled data: {sql}"
            );
        }
    }

    #[test]
    fn an_empty_summary_is_all_null() {
        let answered: Answered<Summary> = serde_json::from_str(
            r#"{"data":[{"seconds":null,"cpu_avg":null,"cpu_max":null,"cpu_steal":null,
                "mem_max":null,"mem_total":null,"cgroup_peak":null,"oom_kills":null,
                "disk_used":null,"net_rx":null,"net_tx":null}]}"#,
        )
        .unwrap();
        assert_eq!(answered.data, [Summary::default()]);
    }

    #[test]
    fn reads_rows_the_binding_answers() {
        let answered: Answered<Window> = serde_json::from_str(
            r#"{"data":[{"at":10,"seconds":10,"cpu_min":1,"cpu_avg":2,"cpu_max":3,
                "cpu_steal":0,"mem_max":1,"mem_total":2,"cgroup_peak":1,"oom_kills":0,
                "disk_used":1,"net_rx":0,"net_tx":0}],"rows":1,"statistics":{}}"#,
        )
        .unwrap();
        assert_eq!(answered.data, [window(10.0)]);
    }
}
