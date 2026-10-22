// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The reconciler, which the Cron Trigger runs every minute under a
//! lease on `ci_reconciler` (see [`run`]). A pass does eight things, in
//! order, and an error in one is logged and does not stop the next:
//!
//! 1. fires the `on: schedule` workflows of the minutes since the last
//!    pass;
//! 2. sends the launches that stalled again, and fails the ones that
//!    never finished;
//! 3. fails the jobs whose runner went quiet;
//! 4. terminates instances that no job needs;
//! 5. keeps the warm pool of each runner template at the size the last
//!    hour's claims ask for ([`crate::pool`]);
//! 6. deletes expired runs and purges their objects;
//! 7. sends metered usage to Polar;
//! 8. re-seals secrets that an old key still seals.
//!
//! Each step decides over rows and the clock with a pure function
//! ([`minutes`], [`fires`], [`stalled`], [`orphans`]) and then acts.
//!
//! A pass is bounded so it stays inside the Worker's CPU time and
//! subrequest limits. It counts the outbound calls it makes (GitHub,
//! run objects, queue, AWS, Polar) against [`BUDGET`], split by
//! [`shares`]: 500 for schedules, then of the rest 3/8 for stalled
//! jobs, 1/8 each for orphans and the warm pool, 1/16 each for usage and
//! re-sealing, and the remainder (100 at [`BUDGET`]) for retention. A step whose share runs out stops and resumes in the next
//! pass. Rows and runs are bounded too: [`MAX_MINUTES`] minutes,
//! [`SCHEDULE_ROWS`] schedule rows, [`MAX_FIXES`] stalled jobs,
//! [`MAX_TERMINATIONS`] terminated instances and [`store::SWEEP_LIMIT`]
//! purged runs. Scheduled runs past [`MAX_FIRES`] in one pass are
//! skipped and logged.

use std::cell::Cell;
use std::collections::HashMap;

use futures_util::future::join_all;
use hatchrun_cloud::aws::instances::Described;
use hatchrun_cloud::ci_wake::{self, Woken};
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::{Counted, send};
use hatchrun_cloud::http::Response;
use hatchrun_cloud::parse_rfc3339;
use hatchrun_cloud::polar::{CI_INGEST_BATCH, CiUsage, ci_ingest, ingested};
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_scaler_core::pool::{Backoff, Pool};
use hatchrun_scaler_core::{Action, Instance, PoolInstance, PoolState};
use hatchrun_workflow::templates::TEMPLATES;
use hatchrun_workflow::{Cron, Event, MAX_RUNNING_JOBS, Template, UtcMinute};
use serde_json::json;
use worker::{Env, Method, RequestInit, console_error, console_log};

use crate::env::{Settings, run_stub};
use crate::github;
use crate::intake::Repo;
use crate::launcher::Backend;
use crate::launches::{self, EndAction, LaunchMessage};
use crate::pool::{self, WINDOW_MS, pool_target};
use crate::run::LAUNCHES;
use crate::runs::{self, Config};
use crate::secrets;
use crate::store::{self, CiBilling, JobRow, PoolRow, ScheduleRow};

/// How long the lease holds, in milliseconds.
const LEASE_MS: u64 = 55_000;
/// The most schedule minutes one pass fires.
pub const MAX_MINUTES: usize = 5;
/// The most scheduled runs one pass starts.
pub const MAX_FIRES: usize = 200;
/// The most schedule rows one pass reads.
pub const SCHEDULE_ROWS: u32 = 1_000;
/// The most stalled jobs one pass handles.
pub const MAX_FIXES: u32 = 50;
/// The subrequests the held-job backstop may spend in the passes that
/// run it, on top of [`BUDGET`] and apart from the stalled-job share.
pub const HELD_BUDGET: u32 = 40;
/// The backstop runs in the passes of every 15th minute.
const HELD_EVERY_MINUTES: u64 = 15;
/// The backstop leaves a held job alone for this long after its last
/// hold.
pub const HELD_QUIET_MS: u64 = 10 * 60_000;
/// The most instances one pass terminates as orphans.
pub const MAX_TERMINATIONS: usize = 50;

/// A queued job nothing launched for this long is sent again.
pub const QUEUED_QUIET_MS: u64 = 60_000;
/// A job launching or launched for this long without moving on fails.
pub const LAUNCH_QUIET_MS: u64 = 600_000;
/// A running job without a heartbeat for this long is lost.
pub const RUNNER_QUIET_MS: u64 = 120_000;
/// An instance whose job ended this long ago is terminated.
const ORPHAN_GRACE_MS: u64 = 180_000;
/// No instance lives longer than this.
const MAX_AGE_MS: u64 = 7 * 3_600_000;

const LAUNCH_DID_NOT_FINISH: &str = "the launch did not finish";
const RUNNER_DID_NOT_START: &str = "the runner did not start";
const LOST_CONTACT: &str = "lost contact with the runner";

/// The subrequests one pass plans to make, below the Worker's limit of
/// 1000 so that DNS, KV and Postgres setup have room.
pub const BUDGET: u32 = 900;

/// What each step of a pass may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shares {
    /// Schedules.
    pub schedules: u32,
    /// Stalled launches and lost runners.
    pub stalled: u32,
    /// Orphan instances.
    pub orphans: u32,
    /// The warm pool.
    pub pool: u32,
    /// Usage sent to Polar.
    pub usage: u32,
    /// Re-sealing secrets under the current key.
    pub reseal: u32,
    /// Retention.
    pub retention: u32,
}

/// Splits `total`: schedules take 5/9 of it, at most 500. Of the rest
/// stalled jobs take 3/8, orphans and the pool 1/8 each, usage and
/// re-sealing 1/16 each, rounded down, and retention the remainder.
/// Schedules cannot take what the other steps need, and retention keeps
/// at least `SWEEP_LIMIT * PURGE_COST` at [`BUDGET`].
#[must_use]
pub fn shares(total: u32) -> Shares {
    let schedules = (total / 9 * 5).min(500);
    let rest = total - schedules;
    let stalled = rest / 8 * 3 + rest % 8 * 3 / 8;
    let orphans = rest / 8;
    let pool = rest / 8;
    let usage = rest / 16;
    let reseal = rest / 16;
    Shares {
        schedules,
        stalled,
        orphans,
        pool,
        usage,
        reseal,
        retention: rest - stalled - orphans - pool - usage - reseal,
    }
}

/// What a step has left to spend.
#[derive(Debug)]
pub struct Budget(Cell<u32>);

impl Budget {
    /// A budget of `n` subrequests.
    #[must_use]
    pub fn new(n: u32) -> Budget {
        Budget(Cell::new(n))
    }

    /// Spends `n`, or nothing when less is left. Returns whether it did.
    pub fn take(&self, n: u32) -> bool {
        match self.0.get().checked_sub(n) {
            Some(left) => {
                self.0.set(left);
                true
            }
            None => false,
        }
    }

    /// What is left.
    #[must_use]
    pub fn left(&self) -> u32 {
        self.0.get()
    }
}

/// Subrequests of reading a repository's head and config: a token (its
/// cache and GitHub), the commit and the file.
const HEAD_COST: u32 = 4;
/// Of starting a run: its object, and the two GitHub calls of a config
/// report.
const START_COST: u32 = 3;
/// Of ending a job: its object.
const FAIL_COST: u32 = 1;
/// Of sending a launch message.
const REQUEUE_COST: u32 = 1;
/// Of terminating an instance: credentials and the call.
const TERMINATE_COST: u32 = 2;
/// Of listing instances: credentials and the first page.
const LIST_COST: u32 = 2;
/// Of purging a run's object.
const PURGE_COST: u32 = 1;
/// Of stopping a pool instance: credentials and the call.
const STOP_COST: u32 = 2;
/// Of launching a pool instance: a launch template version, a fleet or
/// two, and the version's deletion.
const POOL_LAUNCH_COST: u32 = 5;

/// The name of the runner of launch `launch` of attempt `attempt` of job
/// `job`, which is also the instance's `hatchrun:runner-name` tag.
#[must_use]
pub fn runner_name(job: u64, attempt: u32, launch: u32) -> String {
    format!("ci-{job}-{attempt}-{launch}")
}

/// The attempt and launch a [`runner_name`] holds.
fn runner_position(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.strip_prefix("ci-")?.splitn(3, '-');
    parts.next()?.parse::<u64>().ok()?;
    let attempt = parts.next()?.parse().ok()?;
    let launch = parts.next()?.parse().ok()?;
    Some((attempt, launch))
}

/// What to do about a stalled job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    /// Send its launch message again.
    Requeue(u64),
    /// End it as failed with this message.
    Fail(u64, &'static str),
    /// Terminate the instance, then end the job as failed.
    TerminateAndFail(u64, String, &'static str),
}

/// What to do about each job in `rows` at `now`.
#[must_use]
pub fn stalled(rows: &[JobRow], now: u64) -> Vec<Fix> {
    rows.iter().filter_map(|row| fix(row, now)).collect()
}

fn fix(row: &JobRow, now: u64) -> Option<Fix> {
    let quiet_since = |since: u64, limit: u64| since < now.saturating_sub(limit);
    match row.status.as_str() {
        // A held job is woken, not requeued: see [`wake_held`].
        "queued"
            if row.held.is_none()
                && (row.next_launch_at.is_some_and(|at| at <= now)
                    || quiet_since(row.updated_at, QUEUED_QUIET_MS)) =>
        {
            Some(Fix::Requeue(row.id))
        }
        "launching" if quiet_since(row.updated_at, LAUNCH_QUIET_MS) => {
            Some(Fix::Fail(row.id, LAUNCH_DID_NOT_FINISH))
        }
        "launched" if quiet_since(row.updated_at, LAUNCH_QUIET_MS) => {
            Some(terminate_and_fail(row, RUNNER_DID_NOT_START))
        }
        "running"
            if quiet_since(
                row.heartbeat_at
                    .or(row.started_at)
                    .unwrap_or(row.updated_at),
                RUNNER_QUIET_MS,
            ) =>
        {
            Some(terminate_and_fail(row, LOST_CONTACT))
        }
        _ => None,
    }
}

fn terminate_and_fail(row: &JobRow, message: &'static str) -> Fix {
    match &row.instance_id {
        Some(instance) => Fix::TerminateAndFail(row.id, instance.clone(), message),
        None => Fix::Fail(row.id, message),
    }
}

/// The ids of the instances in `described` that no job needs: the job is
/// unknown (no tag, or no row in `rows`), ended more than 180 s before
/// `now`, a later attempt or launch of the job replaced the instance
/// (its runner name says which it is), or the instance started more
/// than 7 hours ago. A launch time that does not parse counts as old.
/// An instance with a pool tag belongs to the warm pool, which keeps it.
#[must_use]
pub fn orphans(described: &[Described], rows: &[JobRow], now: u64) -> Vec<String> {
    let jobs: HashMap<u64, &JobRow> = rows.iter().map(|row| (row.id, row)).collect();
    described
        .iter()
        .filter(|instance| instance.pool.is_none())
        .filter(|instance| {
            let old = parse_rfc3339(&instance.launch_time)
                .is_none_or(|at| at < now.saturating_sub(MAX_AGE_MS));
            let row = instance
                .job_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok())
                .and_then(|id| jobs.get(&id));
            let finished = |row: &JobRow| {
                row.status == "completed"
                    && row
                        .completed_at
                        .is_none_or(|at| at < now.saturating_sub(ORPHAN_GRACE_MS))
            };
            let superseded = |row: &JobRow| {
                instance
                    .runner_name
                    .as_deref()
                    .and_then(runner_position)
                    .is_some_and(|(attempt, launch)| {
                        attempt < row.attempt
                            || (attempt == row.attempt && launch < row.launch_attempts)
                    })
            };
            old || row.is_none_or(|row| finished(row) || superseded(row))
        })
        .map(|instance| instance.id.clone())
        .collect()
}

/// The schedule minutes to fire at `now`, as Unix milliseconds at the
/// start of each minute, oldest first: those after `last` up to the
/// current one, the latest [`MAX_MINUTES`] of them. Without a `last`
/// only the current minute fires.
#[must_use]
pub fn minutes(last: Option<u64>, now: u64) -> Vec<u64> {
    let current = now / 60_000 * 60_000;
    let first = match last {
        Some(last) => (last / 60_000 + 1) * 60_000,
        None => current,
    };
    let from = first.max(current.saturating_sub(60_000 * (MAX_MINUTES as u64 - 1)));
    (from..=current).step_by(60_000).collect()
}

/// One schedule firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fire {
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The workflow's file name.
    pub workflow: String,
    /// The cron expression that matched.
    pub cron: String,
    /// The minute, in Unix milliseconds.
    pub minute_ms: u64,
}

impl Fire {
    /// The delivery key of the run it starts: one run per workflow and
    /// minute, however often the pass repeats.
    #[must_use]
    pub fn delivery(&self) -> String {
        format!(
            "schedule:{}:{}:{}:{}",
            self.repo_id, self.workflow, self.cron, self.minute_ms
        )
    }
}

/// The firings of `schedules` in `minutes`, oldest minute first, at most
/// [`MAX_FIRES`]; the second answer is how many were left out. A cron
/// that does not parse never fires.
#[must_use]
pub fn fires(schedules: &[ScheduleRow], minutes: &[u64]) -> (Vec<Fire>, usize) {
    let parsed: Vec<(&ScheduleRow, Cron)> = schedules
        .iter()
        .filter_map(|row| Some((row, Cron::parse(&row.cron).ok()?)))
        .collect();
    let mut fired = Vec::new();
    let mut skipped = 0;
    for &minute_ms in minutes {
        let at = UtcMinute::from_unix(minute_ms / 1000);
        for (row, cron) in &parsed {
            if !cron.matches(&at) {
                continue;
            }
            if fired.len() < MAX_FIRES {
                fired.push(Fire {
                    repo_id: row.repo_id,
                    workflow: row.workflow.clone(),
                    cron: row.cron.clone(),
                    minute_ms,
                });
            } else {
                skipped += 1;
            }
        }
    }
    (fired, skipped)
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// One reconciler pass. Skipped when an earlier pass holds the lease.
pub async fn run(env: Env) {
    let db = match Db::connect(&env).await {
        Ok(db) => db,
        Err(err) => {
            console_error!("reconcile: database: {err}");
            return;
        }
    };
    let now = now_ms();
    match store::lease(&db, now, now + LEASE_MS).await {
        Ok(true) => {}
        Ok(false) => {
            console_log!("reconcile: skipped, an earlier pass holds the lease");
            return;
        }
        Err(err) => {
            console_error!("reconcile: lease: {err}");
            return;
        }
    }
    let settings = match Settings::read(&env) {
        Ok(settings) => settings,
        Err(err) => {
            console_error!("reconcile: {err}");
            return;
        }
    };
    let share = shares(BUDGET);
    if let Err(err) = fire_schedules(&env, &db, &settings, &Budget::new(share.schedules), now).await
    {
        console_error!("reconcile: schedules: {err}");
    }
    let backend = Backend::new(&settings).await;
    if let Err(err) = &backend {
        console_error!("reconcile: no launcher backend: {err}");
    }
    if let Err(err) = fix_stalled(
        &env,
        &db,
        backend.as_ref().ok(),
        &Budget::new(share.stalled),
        now,
    )
    .await
    {
        console_error!("reconcile: stalled jobs: {err}");
    }
    if held_backstop_due(now)
        && let Err(err) = wake_held(&env, &db, &Budget::new(HELD_BUDGET), now).await
    {
        console_error!("reconcile: held jobs: {err}");
    }
    if let Ok(backend) = &backend
        && let Err(err) = end_orphans(&db, backend, &Budget::new(share.orphans), now).await
    {
        console_error!("reconcile: orphans: {err}");
    }
    if let Ok(backend) = &backend
        && let Err(err) = keep_pool(&db, backend, &Budget::new(share.pool), now).await
    {
        console_error!("reconcile: warm pool: {err}");
    }
    if let Err(err) = retain(&env, &db, &Budget::new(share.retention), now).await {
        console_error!("reconcile: retention: {err}");
    }
    if let Err(err) = report_usage(&db, &settings, &Budget::new(share.usage), now).await {
        console_error!("reconcile: usage: {err}");
    }
    if let Err(err) = reseal_secrets(&env, &db, &Budget::new(share.reseal)).await {
        console_error!("reconcile: reseal: {err}");
    }
}

/// A repository ready to start scheduled runs: where it is now, the
/// head of its default branch and the config there.
struct Head {
    repo: Repo,
    tenant_id: String,
    installation_id: u64,
    git_ref: String,
    sha: String,
    config: Config,
}

/// Step 1. When `budget` runs out the pass stops before the first fire it
/// cannot pay for; the next pass starts again from that minute, and the
/// runs already made are skipped without cost.
async fn fire_schedules(
    env: &Env,
    db: &Db,
    settings: &Settings,
    budget: &Budget,
    now: u64,
) -> Result<(), String> {
    let last = store::last_minute(db).await.map_err(|e| e.to_string())?;
    let due = minutes(last, now);
    let Some(&newest) = due.last() else {
        return Ok(());
    };
    let rows = store::schedules(db, SCHEDULE_ROWS)
        .await
        .map_err(|e| e.to_string())?;
    if rows.len() >= SCHEDULE_ROWS as usize {
        console_error!("reconcile: more than {SCHEDULE_ROWS} schedules, the rest do not fire");
    }
    let (fired, skipped) = fires(&rows, &due);
    if skipped > 0 {
        console_error!("reconcile: {skipped} scheduled runs over the limit of {MAX_FIRES} skipped");
    }
    let github = Counted::default();
    let tokens = Tokens::new(
        Kv::new(env),
        App::new(
            settings.github_api.clone(),
            settings.app_id.clone(),
            settings.app_key.clone(),
        ),
        &github,
        now,
    );
    // The repository, its head and its config are read once per pass.
    let mut heads: HashMap<u64, Option<Head>> = HashMap::new();
    let mut stopped_at = None;
    for fire in &fired {
        if let std::collections::hash_map::Entry::Vacant(slot) = heads.entry(fire.repo_id) {
            if !budget.take(HEAD_COST) {
                stopped_at = Some(fire.minute_ms);
                break;
            }
            let head = match head(db, settings, &tokens, fire.repo_id).await {
                Ok(head) => head,
                Err(err) => {
                    console_error!("reconcile: repository {}: {err}", fire.repo_id);
                    None
                }
            };
            slot.insert(head);
        }
        let Some(Some(head)) = heads.get(&fire.repo_id) else {
            continue;
        };
        let made = store::run_for_delivery(db, &fire.delivery(), &fire.workflow)
            .await
            .map_err(|e| e.to_string())?;
        if made.is_some_and(|run| run.status != "queued") {
            continue;
        }
        if !budget.take(START_COST) {
            stopped_at = Some(fire.minute_ms);
            break;
        }
        if let Err(err) = start(env, db, settings, &tokens, head, fire, now).await {
            console_error!(
                "reconcile: schedule {} of {}: {err}",
                fire.cron,
                head.repo.full_name
            );
        }
    }
    let done = match stopped_at {
        Some(minute) => {
            console_error!("reconcile: schedule budget spent, resuming at minute {minute}");
            minute - 60_000
        }
        None => newest,
    };
    store::set_last_minute(db, done)
        .await
        .map_err(|e| e.to_string())
}

/// The head of repository `repo_id`'s default branch, under the tenant
/// that owns the repository now (a transfer leaves old schedules
/// behind). `None` when the repository or its tenant is gone.
async fn head(
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    repo_id: u64,
) -> Result<Option<Head>, String> {
    let Some(row) = store::repo(db, repo_id).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    match store::tenant(db, &row.tenant_id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(tenant) if tenant.ci_billing == CiBilling::Active => {}
        _ => return Ok(None),
    }
    let response = tokens
        .send(row.installation_id, |token| {
            github::commit_sha(
                &settings.github_api,
                &row.full_name,
                &format!("heads/{}", row.default_branch),
                token,
            )
        })
        .await
        .map_err(|e| e.to_string())?;
    let sha = github::sha_of(&response)?;
    let config = runs::read_config(tokens, settings, row.installation_id, &row.full_name, &sha)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Head {
        git_ref: format!("refs/heads/{}", row.default_branch),
        repo: Repo {
            id: row.id,
            full_name: row.full_name,
            default_branch: row.default_branch,
            private: row.private,
        },
        tenant_id: row.tenant_id,
        installation_id: row.installation_id,
        sha,
        config,
    }))
}

async fn start(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    head: &Head,
    fire: &Fire,
    now: u64,
) -> Result<(), String> {
    let payload = json!({
        "schedule": fire.cron,
        "repository": { "full_name": head.repo.full_name },
    });
    let trigger = Event::Schedule {
        cron: fire.cron.clone(),
    };
    let delivery = fire.delivery();
    let ids = runs::create(
        env,
        db,
        settings,
        tokens,
        &runs::Request {
            tenant_id: &head.tenant_id,
            installation_id: head.installation_id,
            repo: &head.repo,
            delivery: &delivery,
            event_name: "schedule",
            event: &payload,
            trigger: &trigger,
            compile_ref: &head.git_ref,
            git_ref: &head.git_ref,
            sha: &head.sha,
            actor: "schedule",
            head_ref: "",
            base_ref: "",
            workflow: Some(&fire.workflow),
        },
        &head.config,
        now,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !ids.is_empty() {
        console_log!(
            "reconcile: {} {}: started runs {ids:?}",
            head.repo.full_name,
            fire.cron
        );
    }
    Ok(())
}

/// Steps 2 and 3. Stops when `budget` cannot pay for the next fix; the
/// rest are still stalled next pass.
async fn fix_stalled(
    env: &Env,
    db: &Db,
    backend: Option<&Backend>,
    budget: &Budget,
    now: u64,
) -> Result<(), String> {
    let rows = store::stalled_jobs(db, now, MAX_FIXES)
        .await
        .map_err(|e| e.to_string())?;
    let by_id: HashMap<u64, &JobRow> = rows.iter().map(|row| (row.id, row)).collect();
    for fix in stalled(&rows, now) {
        let cost = match &fix {
            Fix::Requeue(_) => REQUEUE_COST,
            Fix::Fail(..) => FAIL_COST,
            Fix::TerminateAndFail(..) => TERMINATE_COST + FAIL_COST,
        };
        if !budget.take(cost) {
            console_error!("reconcile: stalled-job budget spent, the rest wait");
            break;
        }
        let (id, result) = match fix {
            Fix::Requeue(id) => (id, requeue(env, by_id[&id]).await),
            Fix::Fail(id, message) => (id, fail_job(env, db, by_id[&id], message, now).await),
            Fix::TerminateAndFail(id, instance, message) => {
                if let Some(backend) = backend {
                    backend.terminate(&instance).await;
                }
                (id, fail_job(env, db, by_id[&id], message, now).await)
            }
        };
        if let Err(err) = result {
            console_error!("reconcile: job {id}: {err}");
        }
    }
    Ok(())
}

/// Whether the pass at `now_ms` runs the held-job backstop.
#[must_use]
pub fn held_backstop_due(now_ms: u64) -> bool {
    (now_ms / 60_000).is_multiple_of(HELD_EVERY_MINUTES)
}

/// The jobs of `woken` that `budget` pays a message for, in order.
#[must_use]
pub fn paid_wakes(woken: &[Woken], budget: &Budget) -> Vec<Woken> {
    woken
        .iter()
        .copied()
        .take_while(|_| budget.take(REQUEUE_COST))
        .collect()
}

/// The slow backstop for held jobs, which the stalled-job step leaves
/// alone. Wakes come from the Polar webhook (card), the end of a job
/// (concurrency) and the owner raising the cap; this catches a wake that
/// was lost, and releases spend-cap holds when the month turns. It sends
/// the oldest held jobs of each tenant, at most [`MAX_RUNNING_JOBS`]
/// each, that nobody looked at for [`HELD_QUIET_MS`], paid from its own
/// `budget`. The consumer checks the caps again and holds a job that
/// still waits.
async fn wake_held(env: &Env, db: &Db, budget: &Budget, now: u64) -> Result<(), String> {
    let stale = ci_wake::stale(
        db,
        now.saturating_sub(HELD_QUIET_MS),
        MAX_RUNNING_JOBS,
        HELD_BUDGET,
    )
    .await
    .map_err(|e| e.to_string())?;
    let wakes = paid_wakes(&stale, budget);
    if wakes.len() < stale.len() {
        console_error!("reconcile: held-job budget spent, the rest wait");
    }
    launches::send_wakes(env, &wakes).await
}

async fn requeue(env: &Env, row: &JobRow) -> Result<(), String> {
    env.queue(LAUNCHES)
        .map_err(|e| e.to_string())?
        .send(LaunchMessage {
            job_id: row.id,
            attempt: row.attempt,
        })
        .await
        .map_err(|e| e.to_string())
}

/// Ends the attempt of `row` as failed through its run's object. When
/// nobody holds the run, the row is completed here, and the run with it
/// once every job is over. A failed ask is left for the next pass.
async fn fail_job(env: &Env, db: &Db, row: &JobRow, message: &str, now: u64) -> Result<(), String> {
    let answer = launches::post_failed(env, db, row, message).await;
    match launches::end_action(&answer) {
        EndAction::Done => Ok(()),
        EndAction::Retry => Err(format!("could not end it: {answer:?}")),
        EndAction::GiveUp => {
            console_error!("reconcile: job {}: no run object, failing the row", row.id);
            let ended =
                store::finish_job(db, row.id, row.attempt, "failure", &row.steps, None, now)
                    .await
                    .map_err(|e| e.to_string())?;
            if !ended {
                return Ok(());
            }
            let Some(run) = store::run(db, row.run_id)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Ok(());
            };
            launches::wake_oldest(env, db, &run.tenant_id).await;
            let jobs = store::jobs_for_run(db, run.id)
                .await
                .map_err(|e| e.to_string())?;
            if jobs.iter().all(|job| job.status == "completed") {
                store::finish_run(db, run.id, run.run_attempt, "failure", now)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

/// Step 4. Terminates as many orphans as `budget` pays for.
async fn end_orphans(db: &Db, backend: &Backend, budget: &Budget, now: u64) -> Result<(), String> {
    if !budget.take(LIST_COST) {
        return Ok(());
    }
    let described = backend.instances().await?;
    if described.is_empty() {
        return Ok(());
    }
    let ids: Vec<u64> = described
        .iter()
        .filter_map(|instance| instance.job_id.as_deref()?.parse().ok())
        .collect();
    let rows = store::jobs_by_ids(db, &ids)
        .await
        .map_err(|e| e.to_string())?;
    let mut doomed = orphans(&described, &rows, now);
    let affordable = (budget.left() / TERMINATE_COST) as usize;
    let now_count = MAX_TERMINATIONS.min(affordable);
    if doomed.len() > now_count {
        console_log!(
            "reconcile: {} orphans, terminating {now_count} now",
            doomed.len()
        );
        doomed.truncate(now_count);
    }
    budget.take(TERMINATE_COST * u32::try_from(doomed.len()).unwrap_or(0));
    join_all(doomed.iter().map(|id| async move {
        console_log!("reconcile: terminating orphan {id}");
        backend.terminate(id).await;
    }))
    .await;
    Ok(())
}

/// Step 5. Keeps the warm pool of each runner template at the size its
/// claims in the last hour ask for (see [`crate::pool`]). Skipped for a
/// backend without a pool. One list of the pool instances serves every
/// template; an error in one template's pool is logged and does not stop
/// the next. What `budget` does not pay for waits for the next pass.
async fn keep_pool(db: &Db, backend: &Backend, budget: &Budget, now: u64) -> Result<(), String> {
    if !backend.has_pool() || !budget.take(LIST_COST) {
        return Ok(());
    }
    let described = backend.pool_instances().await?;
    let claims = store::claims_by_bucket(db, now.saturating_sub(WINDOW_MS))
        .await
        .map_err(|err| err.to_string())?;
    let demand = pool::demand(&claims);
    let mut pass = PoolPass {
        db,
        backend,
        budget,
        now,
        // Launch failures back off across the templates of one pass.
        backoff: pool::backoff(),
    };
    for template in TEMPLATES {
        let size = demand
            .get(template.labels[0])
            .map_or(0, |claims| pool_target(claims));
        let held = pool::inventory(&described, template);
        if let Err(err) = pass.keep(template, size, &held).await {
            console_error!("reconcile: pool {}: {err}", template.labels[0]);
        }
    }
    Ok(())
}

/// What the pools share in one pass.
struct PoolPass<'a> {
    db: &'a Db,
    backend: &'a Backend,
    budget: &'a Budget,
    now: u64,
    backoff: Backoff,
}

fn sql(err: &worker::Error) -> String {
    err.to_string()
}

impl PoolPass<'_> {
    /// One template's pool: restores it from its rows and what EC2
    /// lists, resizes it, ends and stops what it says, launches what it
    /// lacks, and saves it. An instance a job took meanwhile has lost its
    /// row (and soon its pool tag): a row that cannot be deleted or moved
    /// means it is the job's, and it is left alone.
    async fn keep(
        &mut self,
        template: &'static Template,
        size: u32,
        held: &[Instance],
    ) -> Result<(), String> {
        let label = template.labels[0];
        let now = self.now;
        let rows = store::pool_rows(self.db, label)
            .await
            .map_err(|err| sql(&err))?;
        if size == 0 && rows.is_empty() && held.is_empty() {
            return Ok(());
        }
        let mut pool = Pool::restore(
            pool::config(template, size),
            rows.iter().filter_map(pool::instance),
        );
        pool.reconcile(held, &|_| false, now);
        self.settle(template, &rows, held, &pool).await?;
        let mut unchanged = Vec::new();
        let starved = self.act(&mut pool, &mut unchanged).await?;
        // A pool short of its size launches, unless something it was to
        // end or stop is still there.
        if !starved {
            self.fill(template, &mut pool).await?;
        }
        let changed: Vec<_> = pool
            .instances()
            .iter()
            .filter(|(id, _)| !unchanged.contains(id))
            .map(|(id, pooled)| pool::row(template, id, pooled))
            .filter(|row| !rows.contains(row))
            .collect();
        store::save_pool(self.db, label, &changed, &[])
            .await
            .map_err(|err| sql(&err))
    }

    /// Reconciles the rows with what EC2 lists: drops the rows whose
    /// instance is gone, records a running instance no row knows (a lost
    /// row), and ends a stopped one that is too old to be a job's.
    async fn settle(
        &self,
        template: &'static Template,
        rows: &[PoolRow],
        held: &[Instance],
        pool: &Pool,
    ) -> Result<(), String> {
        let label = template.labels[0];
        for row in rows {
            if !pool.instances().contains_key(&row.instance_id) {
                store::drop_pool(self.db, &row.instance_id)
                    .await
                    .map_err(|err| sql(&err))?;
            }
        }
        let recorded = |id: &str| rows.iter().any(|row| row.instance_id == id);
        let unrecorded = pool::unrecorded(held, &recorded, self.now);
        let adopted: Vec<_> = unrecorded
            .running
            .iter()
            .map(|(id, launched_at)| {
                let pooled = PoolInstance {
                    state: PoolState::Warming,
                    launched_at: *launched_at,
                    since: *launched_at,
                };
                pool::row(template, id, &pooled)
            })
            .collect();
        if !adopted.is_empty() {
            store::save_pool(self.db, label, &[], &adopted)
                .await
                .map_err(|err| sql(&err))?;
        }
        for id in &unrecorded.stale {
            if self.budget.take(TERMINATE_COST) {
                console_log!("reconcile: terminating stale pool instance {id}");
                self.backend.terminate(id).await;
            }
        }
        Ok(())
    }

    /// Ends and stops what the pool's upkeep says. Each is done only when
    /// the row says the instance is still the pool's. The ids of stops
    /// that did not happen go to `unchanged`, so their rows are not
    /// updated. Returns whether the budget left something undone.
    async fn act(&self, pool: &mut Pool, unchanged: &mut Vec<String>) -> Result<bool, String> {
        let mut starved = false;
        for action in pool.upkeep(self.now) {
            match action {
                Action::Terminate { instance_id } => {
                    if !self.budget.take(TERMINATE_COST) {
                        starved = true;
                    } else if store::drop_pool(self.db, &instance_id)
                        .await
                        .map_err(|err| sql(&err))?
                    {
                        console_log!("reconcile: ending pool instance {instance_id}");
                        self.backend.terminate(&instance_id).await;
                    } else {
                        console_log!("reconcile: pool instance {instance_id} went to a job");
                    }
                }
                Action::Stop { instance_id } => {
                    unchanged.push(instance_id.clone());
                    if !self.budget.take(STOP_COST) {
                        starved = true;
                    } else if self.stop(&instance_id).await? {
                        unchanged.retain(|id| id != &instance_id);
                    }
                }
                Action::Launch { .. } | Action::LaunchPool { .. } | Action::RemoveRunner { .. } => {
                }
            }
        }
        Ok(starved)
    }

    /// Stops a warmed-up instance. Returns whether it was stopped; the
    /// row goes back to warming when EC2 refused, and is left alone when
    /// the instance is not the pool's any more.
    async fn stop(&self, instance_id: &str) -> Result<bool, String> {
        let now = self.now;
        if !store::move_pool(self.db, instance_id, "warming", "stopping", now)
            .await
            .map_err(|err| sql(&err))?
        {
            return Ok(false);
        }
        match self.backend.stop(instance_id).await {
            Ok(()) => Ok(true),
            Err(err) => {
                console_error!("reconcile: stop pool instance {instance_id}: {err}");
                store::move_pool(self.db, instance_id, "stopping", "warming", now)
                    .await
                    .map_err(|err| sql(&err))?;
                Ok(false)
            }
        }
    }

    /// Launches the instances a pool lacks, each saved as it starts. A
    /// failed launch ends the pass for the pool: the rest would fail the
    /// same way, and the next pass tries again.
    async fn fill(&mut self, template: &'static Template, pool: &mut Pool) -> Result<(), String> {
        let label = template.labels[0];
        let now = self.now;
        let mut names = pool::names(template);
        for action in pool.refill(now, &self.backoff, &mut names) {
            let Action::LaunchPool { name } = action else {
                continue;
            };
            if !self.budget.take(POOL_LAUNCH_COST) {
                break;
            }
            match self.backend.launch_pool(self.db, template, &name).await {
                Ok(instance_id) => {
                    pool.launched(&name, instance_id.clone(), now, &mut self.backoff);
                    console_log!("reconcile: launched pool instance {instance_id} for {label}");
                    if let Some(pooled) = pool.instances().get(&instance_id) {
                        let launched = pool::row(template, &instance_id, pooled);
                        store::save_pool(self.db, label, &[], &[launched])
                            .await
                            .map_err(|err| sql(&err))?;
                    }
                }
                Err(failure) => {
                    console_error!(
                        "reconcile: launch pool instance {name}: {}",
                        failure.message
                    );
                    pool.launch_failed(&name, now, &mut self.backoff);
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Step 6. A sweep takes at most [`store::SWEEP_LIMIT`] runs and one
/// subrequest each purges them, which `budget` covers.
async fn retain(env: &Env, db: &Db, budget: &Budget, now: u64) -> Result<(), String> {
    if budget.left() < store::SWEEP_LIMIT * PURGE_COST {
        return Ok(());
    }
    let expired = store::sweep(db, now).await.map_err(|e| e.to_string())?;
    budget.take(PURGE_COST * u32::try_from(expired.len()).unwrap_or(0));
    join_all(expired.iter().map(|&run_id| async move {
        if let Err(err) = purge(env, run_id).await {
            console_error!("reconcile: purge run {run_id}: {err}");
        }
    }))
    .await;
    Ok(())
}

/// Of one ingest call to Polar.
const INGEST_COST: u32 = 1;

/// The events of `rows`, in their order, with the units priced when the
/// attempt completed.
fn usage_events(rows: &[store::UnsentUsage]) -> Vec<CiUsage<'_>> {
    rows.iter()
        .map(|row| CiUsage {
            job_id: row.job_id,
            attempt: row.attempt,
            tenant: &row.tenant_id,
            template: &row.template,
            minutes: row.minutes,
            units: row.units,
            at: hatchrun_cloud::format_rfc3339(row.ended_at),
        })
        .collect()
}

/// What Polar's answer to a batch of `sent` events means.
#[derive(Debug, PartialEq, Eq)]
enum Ingest {
    /// Polar took every event, new or duplicate.
    Taken,
    /// Polar refused the batch for what is in it: split it.
    Refused(String),
    /// Nothing says the events are at fault (a transport error, a 429,
    /// a 5xx, or the token): send the same batch again later.
    Retry(String),
}

/// Reads the answer to a batch of `sent` events. A 2xx that took fewer
/// events than were sent counts as a refusal. A 4xx is a refusal
/// except 401, 403, 408 and 429, which are about the caller.
fn judge(answer: &Result<Response, String>, sent: usize) -> Ingest {
    let response = match answer {
        Ok(response) => response,
        Err(err) => return Ingest::Retry(err.clone()),
    };
    match response.status {
        200..=299 => match ingested(response) {
            Ok(taken) if usize::try_from(taken).ok() == Some(sent) => Ingest::Taken,
            Ok(taken) => Ingest::Refused(format!("Polar took {taken} of {sent} events")),
            Err(err) => Ingest::Refused(err),
        },
        401 | 403 | 408 | 429 => Ingest::Retry(format!("Polar answered {}", response.status)),
        400..=499 => Ingest::Refused(format!(
            "Polar answered {}: {}",
            response.status,
            response.text().chars().take(300).collect::<String>()
        )),
        status => Ingest::Retry(format!("Polar answered {status}")),
    }
}

/// What to do with a refused batch of `rows`.
#[derive(Debug, PartialEq, Eq)]
enum Split {
    /// Send each half on its own.
    Halves(std::ops::Range<usize>, std::ops::Range<usize>),
    /// A single row is the one Polar refuses.
    Bad(usize),
}

fn split(rows: &std::ops::Range<usize>) -> Split {
    if rows.len() <= 1 {
        return Split::Bad(rows.start);
    }
    let middle = rows.start + rows.len() / 2;
    Split::Halves(rows.start..middle, middle..rows.end)
}

/// Step 6. Sends up to [`CI_INGEST_BATCH`] unsent usage rows to Polar
/// and marks them sent only when Polar took them. Each event is keyed
/// by job and attempt, so a batch sent again after a lost answer is
/// counted once. A batch Polar refuses for its content is split until
/// the refused rows are alone; those are quarantined with Polar's
/// reason and never sent again. A transport error, a 429 or a 5xx ends
/// the step and the next pass sends the rest again. Without a Polar
/// token nothing is sent.
async fn report_usage(
    db: &Db,
    settings: &Settings,
    budget: &Budget,
    now: u64,
) -> Result<(), String> {
    if settings.polar_token.is_empty() || !budget.take(INGEST_COST) {
        return Ok(());
    }
    let limit = u32::try_from(CI_INGEST_BATCH).unwrap_or(u32::MAX);
    let rows = store::unsent_usage(db, limit)
        .await
        .map_err(|e| e.to_string())?;
    let keys = |range: &std::ops::Range<usize>| -> Vec<(u64, u32)> {
        rows[range.clone()]
            .iter()
            .map(|r| (r.job_id, r.attempt))
            .collect()
    };
    let mut pending = std::iter::once(0..rows.len()).collect::<Vec<_>>();
    let mut first = true;
    while let Some(range) = pending.pop() {
        if range.is_empty() {
            continue;
        }
        if !std::mem::take(&mut first) && !budget.take(INGEST_COST) {
            break;
        }
        let events = usage_events(&rows[range.clone()]);
        let answer = send(&ci_ingest(
            &settings.polar_api,
            &settings.polar_token,
            &events,
        ))
        .await;
        match judge(&answer, range.len()) {
            Ingest::Taken => store::usage_sent(db, &keys(&range), now)
                .await
                .map_err(|e| e.to_string())?,
            Ingest::Retry(err) => return Err(err),
            Ingest::Refused(reason) => match split(&range) {
                Split::Halves(left, right) => {
                    pending.push(right);
                    pending.push(left);
                }
                Split::Bad(at) => {
                    let row = &rows[at];
                    console_error!(
                        "reconcile: usage of job {} attempt {} (tenant {}) quarantined: {reason}",
                        row.job_id,
                        row.attempt,
                        row.tenant_id
                    );
                    store::usage_rejected(db, &[(row.job_id, row.attempt)], &reason, now)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            },
        }
    }
    Ok(())
}

/// Step 7. Takes up to the budget's worth of secrets that are not
/// sealed with the current key (one unit per row), opens each with the
/// keyring and seals it again with the current key. A row that does not
/// open is logged by name and key id, never by value, and stays for an
/// operator; a row set again meanwhile is left as it is.
async fn reseal_secrets(env: &Env, db: &Db, budget: &Budget) -> Result<(), String> {
    if budget.left() == 0 {
        return Ok(());
    }
    let keyring = secrets::Keyring::load(env).await?;
    let rows = store::stale_secrets(db, &keyring.current.id, budget.left())
        .await
        .map_err(|e| e.to_string())?;
    for row in rows {
        if !budget.take(1) {
            break;
        }
        let aad = secrets::Aad {
            tenant: &row.tenant,
            repo_id: row.repo_id,
            name: &row.name,
        };
        let sealed = secrets::fresh_nonce().and_then(|nonce| {
            secrets::reseal(&keyring, &row.key_id, &aad, &row.sealed, nonce)
                .map_err(|err| err.to_string())
        });
        match sealed {
            Ok(sealed) => {
                store::reseal(db, &row, &keyring.current.id, &sealed)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(err) => console_error!(
                "reconcile: secret {} of tenant {} (key {}) not re-sealed: {err}",
                row.name,
                row.tenant,
                row.key_id
            ),
        }
    }
    Ok(())
}

async fn purge(env: &Env, run_id: u64) -> worker::Result<()> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    let request = worker::Request::new_with_init("https://run/purge", &init)?;
    let response = run_stub(env, run_id)?.fetch_with_request(request).await?;
    match response.status_code() {
        200..=299 => Ok(()),
        status => Err(worker::Error::RustError(format!("answered {status}"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const MIN: u64 = 60_000;
    const NOW: u64 = 1_792_676_700_000; // 2026-10-22 13:45:00 UTC

    fn row(status: &str) -> JobRow {
        let mut row: JobRow = serde_json::from_value(json!({
            "id": 1, "run_id": 2, "plan_id": "a", "name": "a", "status": status,
            "conclusion": null, "instance_id": null, "attempt": 1,
            "launch_attempts": 1, "next_launch_at": null,
            "timeout_minutes": 10, "updated_at": NOW, "heartbeat_at": null,
            "started_at": null, "completed_at": null, "steps": "[]", "log_key": null,
            "template": "ubuntu-latest", "token_permissions": "{}",
            "secret_refs": "{\"all\":false,\"names\":[]}",
        }))
        .unwrap();
        row.updated_at = NOW;
        row
    }

    #[test]
    fn a_held_job_is_woken_not_requeued() {
        let mut held = row("queued");
        held.held = Some("concurrency".into());
        held.next_launch_at = Some(NOW - 1);
        held.updated_at = NOW - 3_600_000;
        assert_eq!(stalled(&[held.clone()], NOW), []);
        held.held = None;
        assert_eq!(stalled(&[held], NOW), [Fix::Requeue(1)]);
    }

    #[test]
    fn the_backstop_has_its_own_small_budget() {
        let woken: Vec<Woken> = (1..=100)
            .map(|job_id| Woken { job_id, attempt: 1 })
            .collect();
        // A stalled-job budget that ran out leaves the backstop's alone.
        let stalled_budget = Budget::new(shares(BUDGET).stalled);
        assert!(stalled_budget.take(shares(BUDGET).stalled));
        assert!(!stalled_budget.take(1));
        let held_budget = Budget::new(HELD_BUDGET);
        let paid = paid_wakes(&woken, &held_budget);
        assert_eq!(paid.len(), HELD_BUDGET as usize);
        assert_eq!(paid[0].job_id, 1, "oldest first");
        assert_eq!(held_budget.left(), 0);
        assert_eq!(paid_wakes(&woken, &held_budget), []);
        const { assert!(HELD_BUDGET + BUDGET < 1_000) }
    }

    #[test]
    fn the_backstop_runs_every_fifteen_minutes() {
        let minute = |m: u64| m * 60_000;
        assert!(held_backstop_due(minute(60)));
        assert!(held_backstop_due(minute(75) + 59_999));
        assert!(!held_backstop_due(minute(76)));
        assert!(!held_backstop_due(minute(89)));
        assert_eq!((0..60).filter(|m| held_backstop_due(minute(*m))).count(), 4);
    }

    #[test]
    fn requeues_a_quiet_queued_job() {
        let mut quiet = row("queued");
        quiet.updated_at = NOW - 61_000;
        assert_eq!(stalled(&[quiet], NOW), [Fix::Requeue(1)]);
        let mut due = row("queued");
        due.next_launch_at = Some(NOW);
        assert_eq!(stalled(&[due], NOW), [Fix::Requeue(1)]);
        let mut later = row("queued");
        later.next_launch_at = Some(NOW + 1);
        later.updated_at = NOW - 30_000;
        assert_eq!(stalled(&[later], NOW), []);
        assert_eq!(stalled(&[row("queued")], NOW), []);
    }

    #[test]
    fn fails_launches_that_never_finished() {
        let mut launching = row("launching");
        launching.updated_at = NOW - 601_000;
        assert_eq!(
            stalled(&[launching.clone()], NOW),
            [Fix::Fail(1, "the launch did not finish")]
        );
        launching.updated_at = NOW - 599_000;
        assert_eq!(stalled(&[launching], NOW), []);
        let mut launched = row("launched");
        launched.updated_at = NOW - 601_000;
        assert_eq!(
            stalled(&[launched.clone()], NOW),
            [Fix::Fail(1, "the runner did not start")]
        );
        launched.instance_id = Some("i-1".into());
        assert_eq!(
            stalled(&[launched], NOW),
            [Fix::TerminateAndFail(
                1,
                "i-1".into(),
                "the runner did not start"
            )]
        );
    }

    #[test]
    fn fails_a_silent_runner() {
        let mut running = row("running");
        running.instance_id = Some("i-1".into());
        running.started_at = Some(NOW - 500_000);
        running.heartbeat_at = Some(NOW - 121_000);
        assert_eq!(
            stalled(&[running.clone()], NOW),
            [Fix::TerminateAndFail(
                1,
                "i-1".into(),
                "lost contact with the runner"
            )]
        );
        running.heartbeat_at = Some(NOW - 119_000);
        assert_eq!(stalled(&[running.clone()], NOW), []);
        // No heartbeat yet: the start counts.
        running.heartbeat_at = None;
        assert_eq!(stalled(&[running.clone()], NOW).len(), 1);
        running.started_at = Some(NOW - 10_000);
        assert_eq!(stalled(&[running], NOW), []);
        for status in ["waiting", "completed"] {
            let mut old = row(status);
            old.updated_at = 0;
            assert_eq!(stalled(&[old], NOW), [], "{status}");
        }
    }

    fn instance(id: &str, job: Option<&str>, launched: &str) -> Described {
        named(id, job, launched, None)
    }

    fn named(id: &str, job: Option<&str>, launched: &str, runner: Option<&str>) -> Described {
        Described {
            id: id.into(),
            state: "running".into(),
            launch_time: launched.into(),
            job_id: job.map(str::to_owned),
            runner_name: runner.map(str::to_owned),
            pool: None,
        }
    }

    #[test]
    fn terminates_unknown_and_finished_instances_only() {
        let fresh = "2026-10-22T13:40:00.000Z";
        let mut running = row("running");
        running.id = 1;
        let mut done_long = row("completed");
        done_long.id = 2;
        done_long.completed_at = Some(NOW - 181_000);
        let mut done_just = row("completed");
        done_just.id = 3;
        done_just.completed_at = Some(NOW - 60_000);
        let rows = [running, done_long, done_just];
        let described = [
            instance("i-running", Some("1"), fresh),
            instance("i-done-long", Some("2"), fresh),
            instance("i-done-just", Some("3"), fresh),
            instance("i-unknown", Some("99"), fresh),
            instance("i-untagged", None, fresh),
            instance("i-garbled", Some("x"), fresh),
            instance("i-old", Some("1"), "2026-10-22T06:44:00.000Z"),
            instance("i-bad-time", Some("1"), "yesterday"),
            instance("i-edge", Some("1"), "2026-10-22T06:45:01Z"),
        ];
        assert_eq!(
            orphans(&described, &rows, NOW),
            [
                "i-done-long",
                "i-unknown",
                "i-untagged",
                "i-garbled",
                "i-old",
                "i-bad-time"
            ]
        );
    }

    #[test]
    fn orphans_skip_pool_instances() {
        let mut pooled = instance("i-pool", None, "2020-01-01T00:00:00Z");
        pooled.pool = Some("ubuntu-latest".into());
        let described = [pooled, instance("i-untagged", None, "2026-10-22T13:40:00Z")];
        assert_eq!(orphans(&described, &[], NOW), ["i-untagged"]);
    }

    #[test]
    fn fires_each_missed_minute_up_to_five() {
        assert_eq!(minutes(Some(NOW - 2 * MIN), NOW), [NOW - MIN, NOW]);
        assert_eq!(minutes(Some(NOW), NOW), [] as [u64; 0]);
        assert_eq!(minutes(None, NOW + 7_000), [NOW]);
        assert_eq!(
            minutes(Some(NOW - 30 * MIN), NOW),
            [NOW - 4 * MIN, NOW - 3 * MIN, NOW - 2 * MIN, NOW - MIN, NOW]
        );
        // A last minute that is not on the boundary counts from the next one.
        assert_eq!(minutes(Some(NOW - MIN + 5_000), NOW), [NOW]);

        let every = |cron: &str, workflow: &str| ScheduleRow {
            repo_id: 7,
            workflow: workflow.into(),
            cron: cron.into(),
        };
        let rows = [
            every("* * * * *", "ci"),
            every("*/15 * * * *", "slow"),
            every("nonsense", "bad"),
        ];
        let due = minutes(Some(NOW - 8 * MIN), NOW);
        let (fired, skipped) = fires(&rows, &due);
        assert_eq!(skipped, 0);
        // 13:41 to 13:45: every minute for ci, and :45 for slow.
        assert_eq!(fired.iter().filter(|f| f.workflow == "ci").count(), 5);
        let slow: Vec<_> = fired.iter().filter(|f| f.workflow == "slow").collect();
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].minute_ms, NOW);
        assert_eq!(
            slow[0].delivery(),
            format!("schedule:7:slow:*/15 * * * *:{NOW}")
        );
        assert!(fired.iter().all(|f| f.workflow != "bad"));
    }

    #[test]
    fn firings_stop_at_the_limit() {
        let rows: Vec<ScheduleRow> = (0..130)
            .map(|repo_id| ScheduleRow {
                repo_id,
                workflow: "ci".into(),
                cron: "* * * * *".into(),
            })
            .collect();
        let due = minutes(Some(NOW - 2 * MIN), NOW);
        let (fired, skipped) = fires(&rows, &due);
        assert_eq!(fired.len(), 200);
        assert_eq!(skipped, 60);
        assert!(fired[..130].iter().all(|f| f.minute_ms == NOW - MIN));
    }

    #[test]
    fn terminates_instances_a_later_launch_or_attempt_replaced() {
        let fresh = "2026-10-22T13:40:00Z";
        let mut running = row("running");
        running.id = 5;
        running.attempt = 2;
        running.launch_attempts = 2;
        let described = [
            named("i-current", Some("5"), fresh, Some("ci-5-2-2")),
            named("i-old-launch", Some("5"), fresh, Some("ci-5-2-1")),
            named("i-old-attempt", Some("5"), fresh, Some("ci-5-1-3")),
            named("i-newer", Some("5"), fresh, Some("ci-5-3-1")),
            named("i-odd-name", Some("5"), fresh, Some("custom")),
            named("i-no-name", Some("5"), fresh, None),
        ];
        assert_eq!(
            orphans(&described, &[running], NOW),
            ["i-old-launch", "i-old-attempt"]
        );
        assert_eq!(runner_name(5, 2, 1), "ci-5-2-1");
        assert_eq!(runner_position("ci-5-2-1"), Some((2, 1)));
        assert_eq!(runner_position("ci-5-x-1"), None);
    }

    #[test]
    fn shares_split_the_budget() {
        let sum = |s: Shares| {
            s.schedules + s.stalled + s.orphans + s.pool + s.usage + s.reseal + s.retention
        };
        let share = shares(BUDGET);
        assert_eq!(
            share,
            Shares {
                schedules: 500,
                stalled: 150,
                orphans: 50,
                pool: 50,
                usage: 25,
                reseal: 25,
                retention: 100,
            }
        );
        assert_eq!(sum(share), BUDGET);
        assert!(share.retention >= store::SWEEP_LIMIT * PURGE_COST);
        assert!(share.reseal > 0 && share.usage > 0);
        for total in [0, 1, 8, 100, 900, 5_000] {
            let share = shares(total);
            assert_eq!(sum(share), total, "{total}");
            assert!(share.schedules <= 500);
        }
    }

    fn usage_row(job_id: u64, template: &str, minutes: u64, units: u64) -> store::UnsentUsage {
        store::UnsentUsage {
            job_id,
            attempt: 2,
            tenant_id: "t1".into(),
            template: template.into(),
            minutes,
            units,
            ended_at: NOW,
        }
    }

    #[test]
    fn usage_events_carry_the_units_recorded_at_completion() {
        let rows = [
            usage_row(7, "ubuntu-26.04-8core", 5, 20),
            usage_row(8, "retired", 3, 3),
        ];
        let events = usage_events(&rows);
        assert_eq!((events[0].job_id, events[0].attempt), (7, 2));
        assert_eq!(events[0].units, 20);
        assert_eq!(events[0].at, "2026-10-22T13:45:00Z");
        assert_eq!(events[1].units, 3);
    }

    #[allow(clippy::unnecessary_wraps)] // The argument of `judge`.
    fn answer(status: u16, body: &str) -> Result<Response, String> {
        Ok(Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn polars_answer_decides_between_split_and_retry() {
        let taken = answer(200, r#"{"inserted":2,"duplicates":1}"#);
        assert_eq!(judge(&taken, 3), Ingest::Taken);
        assert!(matches!(judge(&taken, 4), Ingest::Refused(_)));
        for status in [400, 404, 409, 413, 422] {
            assert!(
                matches!(judge(&answer(status, "bad"), 3), Ingest::Refused(_)),
                "{status}"
            );
        }
        for status in [401, 403, 408, 429, 500, 502, 503] {
            assert!(
                matches!(judge(&answer(status, ""), 3), Ingest::Retry(_)),
                "{status}"
            );
        }
        assert!(matches!(
            judge(&Err("connection reset".into()), 3),
            Ingest::Retry(_)
        ));
    }

    #[test]
    fn a_refused_batch_splits_down_to_the_bad_row() {
        assert_eq!(split(&(0..5)), Split::Halves(0..2, 2..5));
        assert_eq!(split(&(2..4)), Split::Halves(2..3, 3..4));
        assert_eq!(split(&(3..4)), Split::Bad(3));
        // Polar refuses any batch that holds row 6 of 8: the search
        // isolates it and takes the rest.
        let bad = 6;
        let (mut taken, mut quarantined, mut calls) = (Vec::new(), Vec::new(), 0);
        let mut pending = std::iter::once(0..8).collect::<Vec<_>>();
        while let Some(range) = pending.pop() {
            calls += 1;
            if range.contains(&bad) {
                match split(&range) {
                    Split::Halves(left, right) => {
                        pending.push(right);
                        pending.push(left);
                    }
                    Split::Bad(at) => quarantined.push(at),
                }
            } else {
                taken.extend(range);
            }
        }
        taken.sort_unstable();
        assert_eq!(taken, [0, 1, 2, 3, 4, 5, 7]);
        assert_eq!(quarantined, [6]);
        assert!(calls <= 2 * 3 + 1, "{calls}");
        // A full batch with one bad row fits the usage share.
        assert!(2 * 7 < shares(BUDGET).usage);
    }

    #[test]
    fn a_budget_spends_all_or_nothing() {
        let budget = Budget::new(5);
        assert!(budget.take(3));
        assert!(!budget.take(3));
        assert_eq!(budget.left(), 2);
        assert!(budget.take(2));
        assert!(!budget.take(1));
    }
}
