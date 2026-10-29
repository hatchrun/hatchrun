// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The reconciler, run by a Cron Trigger every minute. Webhooks get
//! lost, launch messages run out of retries and runners take each
//! other's jobs; each pass puts the database and the tenants' instances back in
//! line:
//!
//! 1. redeliver the App's webhook deliveries that failed, each once
//!    (see [`deliveries`]);
//! 2. ask GitHub about open jobs that have had no webhook for a while,
//!    and record the ones it finished (see [`sync`]), and fetch the
//!    runs whose details the webhook did not (see [`crate::runs`]);
//! 3. per tenant with a verified role, describe its live instances,
//!    terminate the ones nothing needs and launch queued jobs again
//!    (see [`plan`]); once a day also read its stack's launch template
//!    again (see [`crate::stack`]) and delete the template versions
//!    launches left behind (see [`crate::aws::fleet::stale`]);
//! 4. refresh the instance type catalog of one region that is due,
//!    through a tenant role there (see [`crate::instance_types`]);
//! 5. fail the lost launches of installations with no AWS connected,
//!    which `launch` does without calling anyone;
//! 6. send the seconds of completed jobs of paying tenants to Polar's
//!    meter (see [`crate::usage`]);
//! 7. delete expired sessions and rows past the retention;
//! 8. check the pass and the database and email the alerts that are due
//!    (see [`crate::alerts`]).
//!
//! Every step is safe to repeat and to run beside a webhook: rows
//! change only while they still say what the pass read, and `launch`
//! claims a job before it starts anything. A lease on the `reconciler`
//! row keeps a slow pass from overlapping the next one. A pass makes at
//! most [`SUBREQUESTS`] requests and handles a bounded number of
//! tenants, jobs and deliveries, oldest first; what is left waits for
//! the next minute. It logs one summary line and keeps it, with its
//! time and the last passes' errors, in the `reconciler` row. The
//! alerts run after the row is written and the lease given back, so
//! they never hold up or fail a pass.

pub mod deliveries;
pub mod plan;
pub mod retry;
pub mod sync;
pub mod time;

mod maintenance;
mod remote;
mod tenants;

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use worker::{Env, console_error, console_log, console_warn};

use self::plan::{Instance, Job, LaunchState, Limits, Plan};
use crate::alerts::{self, PassRecord};
use crate::aws::describe;
use crate::aws::{self, Credentials, ec2, fleet, instances, ssm};
use crate::db::{Db, params};
use crate::fetch::{Counted, send, send_reading};
use crate::instance_types::{self, store as catalog};
use crate::jobs::{self, LaunchChange};
use crate::launch::{self, APP_KEY, AWS_ENDPOINT_URL};
use crate::start;
use crate::tokens::{App, Kv, Tokens};
use crate::webhook::{Outcome, QueuedJob, Status};
use crate::{github, jwt, labels, now_ms, oidc, polar, runs, usage};

/// Variable with the App's client id.
const APP_ID: &str = "GITHUB_APP_ID";
/// Secret with Polar's organization access token.
const POLAR_ACCESS_TOKEN: &str = "POLAR_ACCESS_TOKEN";
/// Variable that replaces Polar's API URL, such as Polar's sandbox.
const POLAR_API_URL: &str = "POLAR_API_URL";
/// Variable that replaces the GitHub API URL, for development.
const GITHUB_API_URL: &str = "GITHUB_API_URL";

/// Requests a pass may make. Workers allow 1000 per invocation; launches
/// and the app's own requests are well inside the rest.
const SUBREQUESTS: u32 = 500;
/// Requests one GitHub call may make with a kept token: the call, and
/// when GitHub refuses the token, a new token and the call again. Each
/// call reserves this much; [`Pass::settle`] gives back what it did not
/// send.
const GITHUB_CALL_COST: u32 = 3;
/// Requests a launch may make: STS, the installation token, the runner,
/// the launch template version and up to four fleets (or
/// `RunInstances` per subnet), the broker's retries and the cleanup.
const LAUNCH_COST: u32 = 16;
/// Tenants per pass, least recently reconciled first.
const TENANTS_PER_PASS: u32 = 50;
/// Tenants reconciled at once.
const TENANTS_AT_ONCE: usize = 4;
/// Launches per tenant per pass.
const LAUNCHES_PER_TENANT: usize = 10;
/// Pages of instances per tenant per pass.
const INSTANCE_PAGES: u32 = 5;
/// Instances per `TerminateInstances` call.
const TERMINATE_BATCH: usize = 100;
/// Open jobs asked about per pass, and installations asked at once.
const SYNC_PER_PASS: u32 = 30;
const INSTALLATIONS_AT_ONCE: usize = 4;
/// Runs whose details are fetched per pass, and how long after the
/// webhook recorded one the reconciler steps in.
const DETAILS_PER_PASS: u32 = 20;
const DETAILS_AFTER: u64 = 60 * 1000;
/// Pages of deliveries, and redeliveries, per pass.
const DELIVERY_PAGES: u32 = 10;
const REDELIVERIES_PER_PASS: usize = 30;
/// How far each scan of deliveries reaches back past the last one, for
/// attempts GitHub records late.
const DELIVERY_OVERLAP: u64 = 2 * 60 * 1000;
/// How long a redelivery is remembered; GitHub keeps deliveries for
/// three days.
const REDELIVERY_MEMORY: u64 = 4 * 24 * 60 * 60 * 1000;
/// How long a cache upload nobody finished keeps its row: the longest a
/// runner instance runs, and then some.
const CACHE_UPLOAD_MEMORY: u64 = 24 * 60 * 60 * 1000;
/// How long runner seconds are kept once Polar has them or never needed
/// them: past a year, so last year's month can be compared.
const USAGE_MEMORY: u64 = 400 * 24 * 60 * 60 * 1000;
/// How far back the meter step looks for completed jobs whose seconds
/// nothing recorded: a day, longer than GitHub retries a webhook and the
/// sync takes to notice a job.
const SWEEP_WINDOW: u64 = 24 * 60 * 60 * 1000;
/// How long a job or run recorded without `expires_at` is kept: the
/// default `retention_days`. Only a Worker from before migration 0019,
/// still running while it applies, records rows without one, and it
/// knew no tenant's retention.
const UNSET_RETENTION: u64 = 30 * 24 * 60 * 60 * 1000;
/// Lost launches of installations with no AWS connected failed per pass.
const UNCONNECTED_PER_PASS: u32 = 50;
/// Rows deleted per table per pass.
const DELETE_BATCH: u32 = 1000;
/// How long a pass holds the lease; a pass that dies holds it no longer.
const LEASE: u64 = 5 * 60 * 1000;
/// Pages of each describe call a catalog refresh reads. A region has
/// about a thousand types: ten pages of types, a few of offerings and
/// Spot prices.
const CATALOG_PAGES: u32 = 20;
/// Requests a catalog refresh may make: STS, the price list and the
/// pages.
const CATALOG_COST: u32 = 2 + 3 * CATALOG_PAGES;
/// How often a tenant's launch template versions are swept.
const SWEEP_EVERY: u64 = 24 * 60 * 60 * 1000;
/// Pages of launch template versions read per sweep, 200 each. A
/// template with more is swept again on the next pass, as long as
/// the pass deleted some.
const VERSION_PAGES: u32 = 10;
/// How long before the oldest launch in flight a version must be made
/// to be swept; more than any launch takes, and any clock skew.
const SWEEP_MARGIN: u64 = 60 * 60 * 1000;
/// EC2's answer to a role without `ec2:DescribeLaunchTemplateVersions`,
/// from a connect stack before version 5.
const DENIED: &[&str] = &["UnauthorizedOperation", "AccessDenied"];
/// What `CloudTrail` shows as the session of the reconciler's calls.
const SESSION: &str = "hatchrun-reconcile";

/// SQL that is true for a tenant row whose role is verified, as
/// `launch` requires.
pub(crate) const VERIFIED: &str = "role_arn IS NOT NULL AND region IS NOT NULL \
     AND launch_template_id IS NOT NULL AND subnet_ids IS NOT NULL";

type Fallible<T> = std::result::Result<T, String>;

/// What a pass did, logged and kept in the `reconciler` row.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    /// Failed deliveries GitHub was asked to deliver again.
    redelivered: u32,
    /// Open jobs GitHub was asked about.
    checked: u32,
    /// Of those, jobs GitHub had finished.
    closed: u32,
    /// Runs whose details were fetched.
    run_details: u32,
    /// Tenants reconciled.
    tenants: u32,
    /// Instances terminated.
    terminated: u32,
    /// Launches undone so the job can launch again, failed ones among
    /// them.
    reset: u32,
    /// Stalled launches whose instance turned up.
    adopted: u32,
    /// Jobs whose runner waited too long, marked failed.
    gave_up: u32,
    /// Jobs launched again.
    launched: u32,
    /// Tenants whose connect stack changed since it was last read.
    stacks_changed: u32,
    /// Launch template versions left by launches, deleted.
    versions_deleted: u32,
    /// Instance types in the region whose catalog was refreshed.
    catalog_types: u32,
    /// Lost launches of installations with no AWS connected, failed.
    unconnected: u32,
    /// Expired sessions deleted.
    sessions_deleted: u32,
    /// Jobs past the retention deleted.
    jobs_deleted: u32,
    /// Jobs whose seconds Polar took.
    metered: u32,
    /// Runs left without jobs, deleted.
    runs_deleted: u32,
    /// Steps or tenants that failed; the log has each.
    errors: u32,
    /// Whether the pass stopped at a limit with work left.
    more: bool,
    /// Requests made.
    subrequests: u32,
    /// How long the pass took.
    ms: u64,
}

impl Summary {
    fn add(&mut self, other: &Self) {
        self.redelivered += other.redelivered;
        self.checked += other.checked;
        self.closed += other.closed;
        self.run_details += other.run_details;
        self.tenants += other.tenants;
        self.terminated += other.terminated;
        self.reset += other.reset;
        self.adopted += other.adopted;
        self.gave_up += other.gave_up;
        self.launched += other.launched;
        self.catalog_types += other.catalog_types;
        self.stacks_changed += other.stacks_changed;
        self.versions_deleted += other.versions_deleted;
        self.unconnected += other.unconnected;
        self.sessions_deleted += other.sessions_deleted;
        self.jobs_deleted += other.jobs_deleted;
        self.metered += other.metered;
        self.runs_deleted += other.runs_deleted;
        self.errors += other.errors;
    }
}

/// One pass's settings and request budget.
struct Pass<'a> {
    env: &'a Env,
    db: Db,
    now: u64,
    limits: Limits,
    github_api: String,
    aws_endpoint: Option<String>,
    budget: Cell<u32>,
    more: Cell<bool>,
}

/// Runs one pass. Errors end up in the log and the summary.
pub async fn run(env: Env) {
    let started = now_ms();
    let db = match Db::connect(&env).await {
        Ok(db) => db,
        Err(err) => {
            console_error!("reconcile: {err}");
            return;
        }
    };
    match take_lease(&db, started).await {
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
    let mut summary = Summary::default();
    let limits = Limits::read(|name| get(&env, name)).unwrap_or_else(|err| {
        console_error!("reconcile: {err}; using the defaults");
        summary.errors += 1;
        Limits::default()
    });
    let pass = Pass {
        env: &env,
        db,
        now: started,
        limits,
        github_api: get(&env, GITHUB_API_URL)
            .unwrap_or_else(|| github::API.to_owned())
            .trim_end_matches('/')
            .to_owned(),
        aws_endpoint: get(&env, AWS_ENDPOINT_URL),
        budget: Cell::new(SUBREQUESTS),
        more: Cell::new(false),
    };

    let steps: [(&str, Fallible<Summary>); 8] = [
        ("redeliver", pass.redeliver().await),
        ("sync", pass.sync().await),
        ("details", pass.details().await),
        ("tenants", pass.tenants().await),
        ("catalog", pass.catalog().await),
        ("unconnected", pass.unconnected().await),
        ("meter", pass.meter().await),
        ("retention", pass.retention().await),
    ];
    for (step, result) in steps {
        match result {
            Ok(done) => summary.add(&done),
            Err(err) => {
                console_error!("reconcile: {step}: {err}");
                summary.errors += 1;
            }
        }
    }
    summary.more = pass.more.get();
    summary.subrequests = SUBREQUESTS - pass.budget.get();
    summary.ms = now_ms().saturating_sub(started);
    let line = serde_json::to_string(&summary).unwrap_or_default();
    console_log!("reconcile: {line}");
    let this = PassRecord {
        at: started,
        errors: summary.errors,
        more: summary.more,
    };
    let history = match previous(&pass.db).await {
        Ok(history) => Some(alerts::remember(history, this)),
        Err(err) => {
            console_error!("reconcile: history: {err}");
            None
        }
    };
    if let Err(err) = record(&pass.db, started, &line, history.as_deref()).await {
        console_error!("reconcile: record: {err}");
    }
    alerts::after_pass(&env, &pass.db, now_ms(), history).await;
}

/// The passes before this one, newest last.
pub(crate) const HISTORY: &str = "SELECT history FROM reconciler WHERE id = 1";

/// The history the `reconciler` row keeps.
async fn previous(db: &Db) -> Fallible<Vec<PassRecord>> {
    let row: Option<History> = db.first(HISTORY, &[]).await.map_err(text)?;
    Ok(alerts::history(row.and_then(|row| row.history).as_deref()))
}

/// Takes the lease: `$1` now, `$2` when it lapses.
pub(crate) const TAKE_LEASE: &str =
    "UPDATE reconciler SET lease_until = $2 WHERE id = 1 AND lease_until <= $1";

/// Takes the lease on the `reconciler` row if no pass holds it.
async fn take_lease(db: &Db, now: u64) -> Fallible<bool> {
    let changed = db
        .run(TAKE_LEASE, &params![now, now + LEASE])
        .await
        .map_err(text)?;
    Ok(changed == 1)
}

/// Keeps a pass's summary and the history `$4`, unless that is NULL,
/// and gives the lease back.
pub(crate) const RECORD: &str = "UPDATE reconciler SET started_at = $1, finished_at = $2, summary = $3, \
     history = COALESCE($4, history), lease_until = 0 WHERE id = 1";

/// Keeps the pass's summary and history and gives the lease back.
async fn record(
    db: &Db,
    started: u64,
    summary: &str,
    history: Option<&[PassRecord]>,
) -> Fallible<()> {
    let history = history.map(|history| json(&history)).transpose()?;
    db.run(RECORD, &params![started, now_ms(), summary, history])
        .await
        .map_err(text)?;
    Ok(())
}

/// The newest delivery the last complete scan saw.
const DELIVERIES_UNTIL: &str = "SELECT deliveries_until FROM reconciler WHERE id = 1";

/// The guids, of a JSON array in `$1`, already redelivered.
const REMEMBERED: &str = "SELECT guid FROM redeliveries \
     WHERE guid IN (SELECT value FROM json_array_elements_text($1::json))";

/// Moves the scan's mark forward to `$1`, never back.
const MOVE_DELIVERIES_UNTIL: &str = "UPDATE reconciler SET deliveries_until = \
     GREATEST(COALESCE(deliveries_until, 0), $1) WHERE id = 1";

/// Remembers a redelivery, unless a pass already did.
const CLAIM_REDELIVERY: &str = "INSERT INTO redeliveries (guid, delivery_id, event, \
     redelivered_at) VALUES ($1, $2, $3, $4) ON CONFLICT (guid) DO NOTHING";

/// Tenants with a verified role, least recently reconciled first.
pub(crate) fn tenants_query() -> String {
    format!(
        "SELECT id, installation_id, role_arn, region, launch_template_id, max_runners, \
         stack_checked_at, versions_swept_at FROM tenants \
         WHERE {VERIFIED} ORDER BY COALESCE(reconciled_at, 0), id LIMIT $1"
    )
}

/// Stamps tenant `$1` as reconciled at `$2`.
pub(crate) const MARK_RECONCILED: &str = "UPDATE tenants SET reconciled_at = $2 WHERE id = $1";

/// Stamps tenant `$1`'s template versions as swept at `$2`.
const MARK_SWEPT: &str = "UPDATE tenants SET versions_swept_at = $2 WHERE id = $1";

/// Stamps tenant `$1`'s stack as read at `$2`.
const MARK_STACK_CHECKED: &str = "UPDATE tenants SET stack_checked_at = $2 WHERE id = $1";

/// A tenant in a region whose catalog is due: not refreshed since `$1`
/// nor attempted since `$2`.
pub(crate) fn catalog_query() -> String {
    format!(
        "SELECT id, role_arn, region FROM tenants WHERE {VERIFIED} \
         AND NOT EXISTS (SELECT 1 FROM instance_type_regions AS done \
           WHERE done.region = tenants.region \
           AND (done.refreshed_at >= $1 OR done.attempted_at >= $2)) \
         ORDER BY COALESCE((SELECT refreshed_at FROM instance_type_regions AS done \
           WHERE done.region = tenants.region), 0), RANDOM() LIMIT 1"
    )
}

/// Deletes up to `$2` sessions expired before `$1`.
pub(crate) const DELETE_SESSIONS: &str = "DELETE FROM sessions WHERE key IN \
     (SELECT key FROM sessions WHERE expires_at < $1 LIMIT $2)";

/// How long failed launches are kept.
const FAILED_LAUNCH_MEMORY: u64 = 30 * 24 * 60 * 60 * 1000;
/// How long a low rate limit is kept; the alert reads the last minutes.
const RATE_LIMIT_MEMORY: u64 = 24 * 60 * 60 * 1000;

/// Deletes up to `$2` redeliveries made before `$1`.
pub(crate) const DELETE_REDELIVERIES: &str = "DELETE FROM redeliveries WHERE guid IN \
     (SELECT guid FROM redeliveries WHERE redelivered_at < $1 LIMIT $2)";

/// Deletes unavailable offerings that lapsed before `$1`.
pub(crate) const DELETE_OFFERINGS: &str = "DELETE FROM unavailable_offerings WHERE until < $1";

/// Deletes up to `$2` runner seconds that completed before `$1` and that
/// Polar has, or never needed.
pub(crate) const DELETE_USAGE: &str = "DELETE FROM usage_events WHERE job_id IN \
     (SELECT job_id FROM usage_events WHERE ended_at < $1 \
       AND (NOT billed OR sent_at IS NOT NULL) LIMIT $2)";

/// The budget left once a step that found `before` and left `left`
/// gets back what it reserved beyond the `sent` requests it made.
fn settled(left: u32, before: u32, sent: u32) -> u32 {
    let reserved = before.saturating_sub(left);
    left + reserved.saturating_sub(sent)
}

impl Pass<'_> {
    /// Takes `cost` requests from the budget, or notes that the pass
    /// stopped with work left.
    fn spend(&self, cost: u32) -> bool {
        let left = self.budget.get();
        if left < cost {
            self.more.set(true);
            return false;
        }
        self.budget.set(left - cost);
        true
    }

    /// Gives back what a step reserved since the budget stood at `before`
    /// and did not use, `sent` requests having gone out.
    fn settle(&self, before: u32, sent: u32) {
        self.budget.set(settled(self.budget.get(), before, sent));
    }

    fn secret_der(&self, name: &str) -> Fallible<Vec<u8>> {
        let pem = get(self.env, name).ok_or_else(|| format!("{name} is not set"))?;
        jwt::pkcs8_der(&pem).map_err(|err| format!("{name}: {err}"))
    }

    async fn app_jwt(&self) -> Fallible<String> {
        let app_id = get(self.env, APP_ID).ok_or_else(|| format!("{APP_ID} is not set"))?;
        start::app_jwt(&app_id, &self.secret_der(APP_KEY)?).await
    }

    /// Installation tokens for one step, kept in KV across passes (see
    /// [`crate::tokens`]).
    fn tokens<'c>(&self, calls: &'c Counted) -> Fallible<Tokens<'c, Kv, App, Counted>> {
        Ok(Tokens::new(
            Kv::new(self.env),
            App::read(self.env)?,
            calls,
            self.now,
        ))
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Adopted,
    Reset,
    GaveUp,
}

#[derive(Deserialize)]
struct History {
    history: Option<String>,
}

#[derive(Deserialize)]
struct Mark {
    deliveries_until: Option<u64>,
}

#[derive(Deserialize)]
struct Guid {
    guid: String,
}

#[derive(Deserialize)]
struct RunWithoutDetails {
    id: u64,
    installation_id: u64,
    repository: String,
}

#[derive(Deserialize)]
struct OpenJob {
    id: u64,
    installation_id: u64,
    repository: String,
    status: String,
}

#[derive(Deserialize)]
struct Tenant {
    id: String,
    installation_id: Option<u64>,
    role_arn: String,
    region: String,
    launch_template_id: String,
    max_runners: u32,
    stack_checked_at: Option<u64>,
    versions_swept_at: Option<u64>,
}

#[derive(Deserialize)]
struct CatalogTenant {
    id: String,
    role_arn: String,
    region: String,
}

#[derive(Deserialize)]
struct JobRow {
    id: u64,
    label: String,
    status: String,
    launch_state: Option<String>,
    runner_name: Option<String>,
    ran_on: Option<String>,
    queued_at: u64,
    updated_at: u64,
    launch_retry: Option<String>,
    next_launch_at: Option<u64>,
}

#[derive(Deserialize)]
struct LaunchRow {
    id: u64,
    installation_id: u64,
    repository: String,
    run_id: u64,
    label: String,
}

/// A job row as `launch` takes it; the label was checked when the job
/// was recorded.
#[allow(clippy::needless_pass_by_value)]
fn queued_job(row: LaunchRow) -> Option<QueuedJob> {
    match labels::parse(&row.label) {
        Ok(spec) => Some(QueuedJob {
            id: row.id,
            run_id: row.run_id,
            installation_id: row.installation_id,
            repository: row.repository,
            label: row.label,
            spec,
        }),
        Err(err) => {
            console_warn!("reconcile: job {}: {}: {err}", row.id, row.label);
            None
        }
    }
}

fn status(value: &str) -> Option<Status> {
    [Status::Queued, Status::InProgress, Status::Completed]
        .into_iter()
        .find(|status| status.as_str() == value)
}

/// A variable or secret, when set and not empty.
fn get(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|value| value.to_string())
        .ok()
        .filter(|value| !value.is_empty())
}

/// `values` as JSON text, for the queries that take a JSON array.
fn json<T: Serialize>(values: &T) -> Fallible<String> {
    serde_json::to_string(values).map_err(|err| err.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn text(err: worker::Error) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests;
