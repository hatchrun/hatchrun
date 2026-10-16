// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! One job's CPU and memory, as its runner reported them to
//! hatchrun-telemetry (crates/telemetry).
//!
//! - `GET /api/tenants/<org>/jobs/<id>/metrics` returns the job, the
//!   link to it on GitHub, the tenant's region for the instance's
//!   console page, a summary of its windows and the windows
//!   themselves, oldest first.
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
    label: String,
    status: String,
    runner_name: Option<String>,
    instance_id: Option<String>,
    queued_at: u64,
    updated_at: u64,
}

/// One job of an installation, from D1. Binds the job id and the
/// installation.
const JOB: &str = "SELECT id, repository, run_id, label, status, runner_name, instance_id, \
    queued_at, updated_at FROM jobs WHERE id = ?1 AND installation_id = ?2";

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

    let params = Params {
        job: job.id.to_string(),
        installation: installation.to_string(),
        since: js_sys::Date::new(&int(job.queued_at)?)
            .to_iso_string()
            .into(),
    };
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

/// The named parameters of every query.
struct Params {
    job: String,
    installation: String,
    /// The job's queue time as an ISO 8601 string, the lower time bound.
    since: String,
}

/// Runs `sql` through the Analytics SQL binding.
async fn query<T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'_>,
    sql: &str,
    params: &Params,
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
    for (name, value) in [
        ("job", &params.job),
        ("installation", &params.installation),
        ("since", &params.since),
    ] {
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
        let job: Job = db.first(JOB, &params![7u64, 3u64]).unwrap();
        assert_eq!(job.id, 7);
        assert_eq!(job.run_id, 9);
        assert_eq!(job.queued_at, queued_at);
        assert_eq!(job.updated_at, queued_at + 60_000);
        assert_eq!(job.instance_id.as_deref(), Some("i-0123456789abcdef0"));
        assert!(db.first::<Job>(JOB, &params![7u64, 4u64]).is_none());
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
            label: "hatchrun-2cpu".into(),
            status: "completed".into(),
            runner_name: None,
            instance_id: None,
            queued_at: 0,
            updated_at: 0,
        };
        assert_eq!(
            github_url("https://github.com", &job),
            "https://github.com/acme/app/actions/runs/3/job/7"
        );
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
