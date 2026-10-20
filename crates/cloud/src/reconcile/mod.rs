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
//! 6. send the minutes of completed jobs of paying tenants to Polar's
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
/// How long job minutes are kept once Polar has them or never needed
/// them: past a year, so last year's month can be compared.
const USAGE_MEMORY: u64 = 400 * 24 * 60 * 60 * 1000;
/// How far back the meter step looks for completed jobs whose minutes
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
    /// Jobs whose minutes Polar took.
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

/// Deletes up to `$2` job minutes that completed before `$1` and that
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
        launch::app_jwt(&app_id, &self.secret_der(APP_KEY)?).await
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

    /// Step 1: asks GitHub to deliver failed webhooks again.
    async fn redeliver(&self) -> Fallible<Summary> {
        let mut done = Summary::default();
        let app_jwt = self.app_jwt().await?;
        let mark = self
            .db
            .first::<Mark>(DELIVERIES_UNTIL, &[])
            .await
            .map_err(text)?
            .and_then(|row| row.deliveries_until);
        let floor = self.now.saturating_sub(self.limits.redeliver_window);
        let since = mark.map_or(floor, |mark| {
            mark.saturating_sub(DELIVERY_OVERLAP).max(floor)
        });

        let (seen, complete) = self.scan(&app_jwt, since).await?;

        let chosen = deliveries::choose(&seen, since);
        let guids: Vec<&str> = chosen.iter().map(|chosen| chosen.guid.as_str()).collect();
        let remembered: HashSet<String> = self
            .db
            .all::<Guid>(REMEMBERED, &params![json(&guids)?])
            .await
            .map_err(text)?
            .into_iter()
            .map(|row| row.guid)
            .collect();
        let pending: Vec<_> = chosen
            .into_iter()
            .filter(|chosen| !remembered.contains(&chosen.guid))
            .collect();
        let mut handled = 0;
        for redelivery in pending.iter().take(REDELIVERIES_PER_PASS) {
            if !self.spend(1) {
                break;
            }
            handled += 1;
            match self.ask_again(&app_jwt, redelivery).await? {
                Some(true) => done.redelivered += 1,
                Some(false) => done.errors += 1,
                None => {}
            }
        }

        // The next scan starts where this one ended, once it saw every
        // delivery back to `since` and asked for every failed one.
        if complete && handled == pending.len() {
            if let Some(newest) = seen.iter().filter_map(deliveries::Delivery::at).max() {
                self.db
                    .run(MOVE_DELIVERIES_UNTIL, &params![newest])
                    .await
                    .map_err(text)?;
            }
        } else {
            self.more.set(true);
        }
        Ok(done)
    }

    /// Asks GitHub to deliver `redelivery` again, unless a pass already
    /// did. Whether GitHub agreed, or `None` when it was not asked.
    async fn ask_again(
        &self,
        app_jwt: &str,
        redelivery: &deliveries::Redelivery,
    ) -> Fallible<Option<bool>> {
        // Remembered first, so two passes cannot both ask.
        let claimed = self
            .db
            .run(
                CLAIM_REDELIVERY,
                &params![
                    redelivery.guid.as_str(),
                    redelivery.id,
                    redelivery.event.as_str(),
                    self.now,
                ],
            )
            .await
            .map_err(text)?;
        if claimed != 1 {
            return Ok(None);
        }
        let request = deliveries::redeliver(&self.github_api, app_jwt, redelivery.id);
        let failure = match send(&request).await {
            Ok(response) if response.ok() => {
                console_log!(
                    "reconcile: redelivered {} {} (attempt {})",
                    redelivery.event,
                    redelivery.guid,
                    redelivery.id
                );
                return Ok(Some(true));
            }
            Ok(response) => format!("GitHub answered {}: {}", response.status, response.text()),
            Err(err) => err,
        };
        console_warn!("reconcile: redeliver {}: {failure}", redelivery.guid);
        Ok(Some(false))
    }

    /// The App's deliveries, newest first, back to `since` or as far as
    /// the pass's limits reach; and whether they reached it.
    async fn scan(&self, app_jwt: &str, since: u64) -> Fallible<(Vec<deliveries::Delivery>, bool)> {
        let mut seen = Vec::new();
        let mut next = Some(deliveries::first_page(&self.github_api));
        let mut complete = false;
        for _ in 0..DELIVERY_PAGES {
            let Some(url) = next.clone() else {
                complete = true;
                break;
            };
            if !self.spend(1) {
                break;
            }
            let (response, link) =
                send_reading(&deliveries::list(&url, app_jwt), Some("Link")).await?;
            let page = deliveries::parse(&response)?;
            let reached = page.is_empty()
                || page
                    .iter()
                    .any(|delivery| delivery.at().is_some_and(|at| at < since));
            seen.extend(page);
            if reached {
                complete = true;
                break;
            }
            next = deliveries::next_page(link.as_deref());
        }
        complete |= next.is_none();
        Ok((seen, complete))
    }

    /// Step 2: asks GitHub about open jobs that have gone quiet.
    async fn sync(&self) -> Fallible<Summary> {
        let quiet = self.now.saturating_sub(self.limits.sync_after);
        let open = jobs::open_jobs::<OpenJob>(&self.db, quiet, SYNC_PER_PASS)
            .await
            .map_err(text)?;
        if open.is_empty() {
            return Ok(Summary::default());
        }
        let calls = Counted::default();
        let tokens = self.tokens(&calls)?;
        let before = self.budget.get();
        let mut by_installation: BTreeMap<u64, Vec<OpenJob>> = BTreeMap::new();
        for job in open {
            by_installation
                .entry(job.installation_id)
                .or_default()
                .push(job);
        }
        let results: Vec<Summary> = stream::iter(by_installation)
            .map(|(installation_id, jobs)| self.sync_installation(&tokens, installation_id, jobs))
            .buffer_unordered(INSTALLATIONS_AT_ONCE)
            .collect()
            .await;
        self.settle(before, calls.sent());
        alerts::note_rate_limits(&self.db, &calls, self.now).await;
        let mut done = Summary::default();
        for result in &results {
            done.add(result);
        }
        Ok(done)
    }

    async fn sync_installation(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        installation_id: u64,
        jobs: Vec<OpenJob>,
    ) -> Summary {
        let mut done = Summary::default();
        if !self.spend(1) {
            return done;
        }
        if let Err(err) = tokens.token(installation_id).await {
            console_warn!("reconcile: installation {installation_id}: {err}");
            done.errors += 1;
            for job in &jobs {
                if let Err(err) = self.checked(job, None).await {
                    console_error!("reconcile: job {}: {err}", job.id);
                }
            }
            return done;
        }
        for job in &jobs {
            if !self.spend(GITHUB_CALL_COST) {
                break;
            }
            done.checked += 1;
            let seen = match tokens
                .send(installation_id, |token| {
                    sync::job(&self.github_api, token, &job.repository, job.id)
                })
                .await
                .map_err(|err| err.to_string())
                .and_then(|response| sync::parse(&response))
            {
                Ok(seen) => Some(seen),
                Err(err) => {
                    console_warn!("reconcile: job {}: {err}", job.id);
                    done.errors += 1;
                    None
                }
            };
            match self.checked(job, seen.as_ref()).await {
                Ok(true) => done.closed += 1,
                Ok(false) => {}
                Err(err) => {
                    console_error!("reconcile: job {}: {err}", job.id);
                    done.errors += 1;
                }
            }
        }
        done
    }

    /// Records what GitHub said about `job`, or only that it was asked.
    /// Returns whether the job is now completed.
    async fn checked(&self, job: &OpenJob, seen: Option<&sync::Seen>) -> Fallible<bool> {
        let stored = status(&job.status).unwrap_or(Status::Queued);
        let next = seen.map_or(stored, |seen| sync::advance(stored, seen.status));
        let runner = seen.and_then(|seen| seen.runner_name.as_deref());
        let outcome = seen.map(|seen| &seen.outcome);
        let changed = jobs::record_check(
            &self.db,
            job.id,
            next.as_str(),
            runner,
            job.status.as_str(),
            outcome,
            self.now,
        )
        .await
        .map_err(text)?;
        let closed = changed == 1 && next == Status::Completed;
        if closed {
            console_log!("reconcile: job {} completed on GitHub", job.id);
            if let Some(sync::Seen {
                outcome:
                    Outcome {
                        started_at: Some(started),
                        completed_at: Some(completed),
                        ..
                    },
                ..
            }) = seen
            {
                usage::record(&self.db, job.id, *started, *completed, self.now)
                    .await
                    .map_err(text)?;
            }
        }
        Ok(closed)
    }

    /// Step 2, continued: fetches the details of runs the webhook could
    /// not, oldest first.
    async fn details(&self) -> Fallible<Summary> {
        let before = self.now.saturating_sub(DETAILS_AFTER);
        let runs =
            jobs::runs_without_details::<RunWithoutDetails>(&self.db, before, DETAILS_PER_PASS)
                .await
                .map_err(text)?;
        let done = Summary::default();
        if runs.is_empty() {
            return Ok(done);
        }
        let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
        jobs::details_tried(&self.db, &json(&ids)?, self.now)
            .await
            .map_err(text)?;
        let calls = Counted::default();
        let tokens = self.tokens(&calls)?;
        let before = self.budget.get();
        let mut by_installation: BTreeMap<u64, Vec<RunWithoutDetails>> = BTreeMap::new();
        for run in runs {
            by_installation
                .entry(run.installation_id)
                .or_default()
                .push(run);
        }
        let fetched = self.fetch_details(&tokens, by_installation, done).await;
        self.settle(before, calls.sent());
        alerts::note_rate_limits(&self.db, &calls, self.now).await;
        fetched
    }

    /// Fetches and records the details of `runs`, by installation.
    async fn fetch_details(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        by_installation: BTreeMap<u64, Vec<RunWithoutDetails>>,
        mut done: Summary,
    ) -> Fallible<Summary> {
        for (installation_id, runs) in by_installation {
            if !self.spend(1) {
                break;
            }
            if let Err(err) = tokens.token(installation_id).await {
                console_warn!("reconcile: installation {installation_id}: {err}");
                done.errors += 1;
                continue;
            }
            for run in runs {
                if !self.spend(GITHUB_CALL_COST) {
                    break;
                }
                match tokens
                    .send(installation_id, |token| {
                        github::workflow_run(&self.github_api, token, &run.repository, run.id)
                    })
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|response| runs::parse(&response))
                {
                    Ok(details) => {
                        jobs::record_details(&self.db, run.id, &details, now_ms())
                            .await
                            .map_err(text)?;
                        done.run_details += 1;
                    }
                    Err(err) => {
                        console_warn!("reconcile: run {}: {err}", run.id);
                        done.errors += 1;
                    }
                }
            }
        }
        Ok(done)
    }

    /// Step 3: every tenant with a verified role, least recently
    /// reconciled first.
    async fn tenants(&self) -> Fallible<Summary> {
        let tenants = self
            .db
            .all::<Tenant>(&tenants_query(), &params![TENANTS_PER_PASS])
            .await
            .map_err(text)?;
        if tenants.len() == TENANTS_PER_PASS as usize {
            self.more.set(true);
        }
        let oidc_key = self.secret_der(oidc::SIGNING_KEY);
        let oidc_kid = get(self.env, oidc::KEY_ID);
        let oidc = match (oidc_key, oidc_kid) {
            (Ok(key), Some(kid)) => Ok((key, kid)),
            (Err(err), _) => Err(err),
            (_, None) => Err(format!("{} is not set", oidc::KEY_ID)),
        };
        let results: Vec<Summary> = stream::iter(&tenants)
            .map(|tenant| self.tenant(tenant, oidc.as_ref()))
            .buffer_unordered(TENANTS_AT_ONCE)
            .collect()
            .await;
        let mut done = Summary::default();
        for result in &results {
            done.add(result);
        }
        Ok(done)
    }

    async fn tenant(&self, tenant: &Tenant, oidc: Result<&(Vec<u8>, String), &String>) -> Summary {
        let mut done = Summary {
            tenants: 1,
            ..Summary::default()
        };
        if let Err(err) = self.reconcile_tenant(tenant, oidc, &mut done).await {
            console_error!("reconcile: tenant {}: {err}", tenant.id);
            done.errors += 1;
        }
        done
    }

    async fn reconcile_tenant(
        &self,
        tenant: &Tenant,
        oidc: Result<&(Vec<u8>, String), &String>,
        done: &mut Summary,
    ) -> Fallible<()> {
        // Moved to the back of the queue first, so a tenant that fails
        // every time does not hold up the rest.
        self.db
            .run(MARK_RECONCILED, &params![tenant.id.as_str(), self.now])
            .await
            .map_err(text)?;

        let endpoint = aws::endpoint("ec2", &tenant.region, self.aws_endpoint.as_deref());
        let live = match oidc.map_err(Clone::clone) {
            Ok((key, kid)) => self.live_instances(tenant, key, kid, &endpoint).await,
            Err(err) => Err(err),
        };
        let (credentials, instances) = match live {
            Ok((credentials, instances)) => (Some(credentials), Some(instances)),
            Err(err) => {
                // Jobs that never launched can still launch; `launch`
                // records why if the role fails it too.
                console_warn!("reconcile: tenant {}: {err}", tenant.id);
                done.errors += 1;
                (None, None)
            }
        };
        if let Some(credentials) = &credentials
            && self
                .now
                .saturating_sub(tenant.stack_checked_at.unwrap_or(0))
                > crate::stack::CHECK_EVERY
            && self.spend(1)
        {
            match self.check_stack(tenant, credentials).await {
                Ok(true) => done.stacks_changed += 1,
                Ok(false) => {}
                Err(err) => {
                    console_warn!("reconcile: tenant {}: stack: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
        let jobs = self.tenant_jobs(tenant, instances.as_deref()).await?;
        let plan = plan::plan(
            self.now,
            &self.limits,
            tenant.max_runners,
            instances.as_deref(),
            &jobs,
        );
        if let Some(credentials) = &credentials {
            self.terminate(tenant, credentials, &endpoint, &plan, done)
                .await;
        }
        self.update_jobs(tenant, &plan, done).await?;
        if let Some(credentials) = &credentials
            && self
                .now
                .saturating_sub(tenant.versions_swept_at.unwrap_or(0))
                > SWEEP_EVERY
        {
            match self
                .sweep_versions(tenant, credentials, &endpoint, &jobs)
                .await
            {
                Ok(deleted) => done.versions_deleted += deleted,
                Err(err) => {
                    console_warn!("reconcile: tenant {}: template versions: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
        self.launch(tenant, &plan, done).await
    }

    /// Deletes the launch template versions that launches made and did
    /// not delete, because the Worker died between the two. A version
    /// made before the oldest launch in flight, less a margin, belongs
    /// to no launch still running. Returns how many it deleted.
    async fn sweep_versions(
        &self,
        tenant: &Tenant,
        credentials: &Credentials,
        endpoint: &str,
        jobs: &[Job],
    ) -> Fallible<u32> {
        // A job is queued before it is claimed and its version made.
        let in_flight = jobs
            .iter()
            .filter(|job| job.launch_state == Some(LaunchState::Launching))
            .map(|job| job.queued_at)
            .min()
            .unwrap_or(self.now);
        let before = in_flight.min(self.now).saturating_sub(SWEEP_MARGIN);
        let template = tenant.launch_template_id.as_str();
        let mut found = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..VERSION_PAGES {
            if !self.spend(1) {
                return Ok(0);
            }
            let mut request = fleet::describe_versions(endpoint, template, next.as_deref());
            aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())?;
            match fleet::versions(&send(&request).await?) {
                Ok((page, token)) => {
                    found.extend(page);
                    next = token;
                }
                Err(err) if DENIED.contains(&err.code.as_str()) => {
                    console_log!(
                        "reconcile: tenant {}: connect stack before version 5 may not list \
                         template versions; skipped",
                        tenant.id
                    );
                    self.swept(tenant).await?;
                    return Ok(0);
                }
                Err(err) => {
                    // Stamped all the same, so a failing call is not made
                    // every minute.
                    self.swept(tenant).await?;
                    return Err(format!("DescribeLaunchTemplateVersions: {err}"));
                }
            }
            if next.is_none() {
                break;
            }
        }
        let mut deleted = 0;
        for batch in fleet::stale(&found, before).chunks(fleet::DELETE_BATCH) {
            if !self.spend(1) {
                return Ok(deleted);
            }
            let mut request = fleet::delete_versions(endpoint, template, batch);
            aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())?;
            let done = fleet::deleted(&send(&request).await?)
                .map_err(|err| format!("DeleteLaunchTemplateVersions: {err}"))?;
            for (version, reason) in &done.failed {
                console_warn!(
                    "reconcile: tenant {}: delete template version {version}: {reason}",
                    tenant.id
                );
            }
            deleted += u32::try_from(done.deleted.len()).unwrap_or(u32::MAX);
        }
        if deleted > 0 {
            console_log!(
                "reconcile: tenant {}: deleted {deleted} template versions left by launches",
                tenant.id
            );
        }
        // A template with more pages is swept again on the next pass,
        // from the first page, which now holds the versions left. One
        // that deleted nothing would read the same pages again, so it
        // waits a day like the rest.
        if next.is_some() && deleted > 0 {
            self.more.set(true);
        } else {
            self.swept(tenant).await?;
        }
        Ok(deleted)
    }

    async fn swept(&self, tenant: &Tenant) -> Fallible<()> {
        self.db
            .run(MARK_SWEPT, &params![tenant.id.as_str(), self.now])
            .await
            .map_err(text)?;
        Ok(())
    }

    /// Reads the tenant's stack settings again and records what they
    /// say. Returns whether they changed; a change puts jobs waiting for
    /// the setup back in line.
    async fn check_stack(&self, tenant: &Tenant, credentials: &Credentials) -> Fallible<bool> {
        let endpoint = aws::endpoint("ssm", &tenant.region, self.aws_endpoint.as_deref());
        let mut request =
            ssm::get_parameter(&endpoint, &crate::connect::parameter_name(&tenant.id));
        aws::sign_v4(&mut request, credentials, &tenant.region, "ssm", now_ms())?;
        let stack = ssm::value(&send(&request).await?)
            .map_err(|err| format!("GetParameter: {err}"))
            .and_then(|value| crate::connect::Stack::parse(&value));
        let stack = match stack {
            Ok(stack) => stack,
            Err(err) => {
                // Stamped all the same, so a deleted stack is not asked about
                // every minute; launches say what is wrong.
                self.db
                    .run(MARK_STACK_CHECKED, &params![tenant.id.as_str(), self.now])
                    .await
                    .map_err(text)?;
                return Err(err);
            }
        };
        let changed = crate::stack::record(&self.db, &tenant.id, &stack, self.now)
            .await
            .map_err(text)?;
        if changed {
            console_log!(
                "reconcile: tenant {}: connect stack changed, template version {}",
                tenant.id,
                stack.version
            );
            launch::recheck(&self.db, &tenant.id).await.map_err(text)?;
        }
        Ok(changed)
    }

    /// The tenant role's credentials and its pending and running
    /// instances.
    async fn live_instances(
        &self,
        tenant: &Tenant,
        oidc_key: &[u8],
        oidc_kid: &str,
        endpoint: &str,
    ) -> Fallible<(Credentials, Vec<Instance>)> {
        if !self.spend(2) {
            return Err("out of requests for this pass".to_owned());
        }
        let credentials = launch::tenant_credentials(
            oidc_key,
            oidc_kid,
            self.aws_endpoint.as_deref(),
            &tenant.id,
            &tenant.role_arn,
            &tenant.region,
            SESSION,
        )
        .await
        .map_err(|err| format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn))?;
        let mut found = Vec::new();
        let mut next: Option<String> = None;
        for page in 0..INSTANCE_PAGES {
            if page > 0 && !self.spend(1) {
                return Err("out of requests while describing instances".to_owned());
            }
            let mut request = instances::describe(endpoint, &tenant.id, next.as_deref());
            aws::sign_v4(&mut request, &credentials, &tenant.region, "ec2", now_ms())?;
            let (described, token) = instances::described(&send(&request).await?)
                .map_err(|err| format!("DescribeInstances: {err}"))?;
            found.extend(described);
            next = token;
            if next.is_none() {
                break;
            }
        }
        if next.is_some() {
            // Deciding on part of the fleet could terminate a runner
            // whose job is on the next page.
            return Err(format!(
                "more than {INSTANCE_PAGES} pages of instances; not deciding on part of them"
            ));
        }
        let instances = found
            .into_iter()
            .filter_map(|described| {
                let launched_at = time::parse(&described.launch_time);
                if launched_at.is_none() {
                    console_warn!(
                        "reconcile: {}: unreadable launch time {:?}",
                        described.id,
                        described.launch_time
                    );
                }
                Some(Instance {
                    launched_at: launched_at?,
                    job_id: described.job_id.and_then(|id| id.parse().ok()),
                    runner_name: described.runner_name,
                    id: described.id,
                })
            })
            .collect();
        Ok((credentials, instances))
    }

    /// The tenant's open jobs, and the finished ones its instances name.
    async fn tenant_jobs(
        &self,
        tenant: &Tenant,
        instances: Option<&[Instance]>,
    ) -> Fallible<Vec<Job>> {
        let Some(installation_id) = tenant.installation_id else {
            return Ok(Vec::new());
        };
        let instances = instances.unwrap_or_default();
        let ids: Vec<u64> = instances
            .iter()
            .filter_map(|instance| instance.job_id)
            .collect();
        let runners: Vec<&str> = instances
            .iter()
            .filter_map(|instance| instance.runner_name.as_deref())
            .collect();
        let rows =
            jobs::tenant_jobs::<JobRow>(&self.db, installation_id, &json(&ids)?, &json(&runners)?)
                .await
                .map_err(text)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(Job {
                    id: row.id,
                    label: row.label,
                    status: status(&row.status)?,
                    launch_state: row.launch_state.as_deref().and_then(LaunchState::parse),
                    runner_name: row.runner_name,
                    ran_on: row.ran_on,
                    queued_at: row.queued_at,
                    updated_at: row.updated_at,
                    launch_retry: row.launch_retry.as_deref().and_then(retry::Wait::parse),
                    next_launch_at: row.next_launch_at,
                })
            })
            .collect())
    }

    async fn terminate(
        &self,
        tenant: &Tenant,
        credentials: &Credentials,
        endpoint: &str,
        plan: &Plan,
        done: &mut Summary,
    ) {
        for batch in plan.terminate.chunks(TERMINATE_BATCH) {
            if !self.spend(1) {
                return;
            }
            let ids: Vec<&str> = batch.iter().map(|(id, _)| id.as_str()).collect();
            let listed = batch
                .iter()
                .map(|(id, reason)| format!("{id} ({})", reason.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            let mut request = instances::terminate(endpoint, &ids);
            let result =
                match aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms()) {
                    Ok(()) => send(&request).await.and_then(|response| {
                        ec2::terminated(&response).map_err(|err| err.to_string())
                    }),
                    Err(err) => Err(err),
                };
            match result {
                Ok(()) => {
                    console_log!("reconcile: tenant {}: terminated {listed}", tenant.id);
                    done.terminated += u32::try_from(batch.len()).unwrap_or(u32::MAX);
                }
                Err(err) => {
                    console_warn!("reconcile: tenant {}: terminate {listed}: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
    }

    /// Adopts, undoes and gives up launches, each only while the row
    /// still says what the plan saw.
    async fn update_jobs(&self, tenant: &Tenant, plan: &Plan, done: &mut Summary) -> Fallible<()> {
        let mut changes: Vec<(LaunchChange, Kind)> = Vec::new();
        for (job_id, instance_id) in &plan.adopt {
            changes.push((
                LaunchChange::Adopt {
                    id: *job_id,
                    instance_id: instance_id.as_str(),
                },
                Kind::Adopted,
            ));
        }
        for reset in &plan.reset {
            changes.push((
                LaunchChange::Reset {
                    id: reset.job_id,
                    launch_state: reset.launch_state.as_str(),
                    runner_name: reset.runner_name.as_deref(),
                },
                Kind::Reset,
            ));
        }
        for (job_id, runner) in &plan.give_up {
            let reason = format!(
                "runner {runner} waited {} minutes without GitHub handing it the job",
                self.limits.idle_timeout / 60_000
            );
            changes.push((
                LaunchChange::GiveUp {
                    id: *job_id,
                    reason,
                    runner: runner.as_str(),
                },
                Kind::GaveUp,
            ));
        }
        if changes.is_empty() {
            return Ok(());
        }
        let (changes, kinds): (Vec<_>, Vec<_>) = changes.into_iter().unzip();
        let results = jobs::update_launches(&self.db, &changes, self.now)
            .await
            .map_err(text)?;
        for (changed, kind) in results.into_iter().zip(kinds) {
            if changed != 1 {
                continue;
            }
            match kind {
                Kind::Adopted => done.adopted += 1,
                Kind::Reset => done.reset += 1,
                Kind::GaveUp => done.gave_up += 1,
            }
        }
        if done.adopted + done.reset + done.gave_up > 0 {
            console_log!(
                "reconcile: tenant {}: adopted {}, reset {}, gave up {}",
                tenant.id,
                done.adopted,
                done.reset,
                done.gave_up
            );
        }
        Ok(())
    }

    /// Launches the plan's jobs through `launch`, which claims each
    /// first and stops at `max_runners`.
    async fn launch(&self, tenant: &Tenant, plan: &Plan, done: &mut Summary) -> Fallible<()> {
        if plan.launch.len() > LAUNCHES_PER_TENANT {
            self.more.set(true);
        }
        let ids: Vec<u64> = plan
            .launch
            .iter()
            .copied()
            .take(LAUNCHES_PER_TENANT)
            .collect();
        for job in self.queued_jobs(&ids).await? {
            if !self.spend(LAUNCH_COST) {
                break;
            }
            console_log!(
                "reconcile: tenant {}: launching job {} again",
                tenant.id,
                job.id
            );
            launch::launch(self.env.clone(), job).await;
            done.launched += 1;
        }
        Ok(())
    }

    /// Jobs as `launch` takes them, oldest first.
    async fn queued_jobs(&self, ids: &[u64]) -> Fallible<Vec<QueuedJob>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = jobs::queued_jobs::<LaunchRow>(&self.db, &json(&ids)?)
            .await
            .map_err(text)?;
        Ok(rows.into_iter().filter_map(queued_job).collect())
    }

    /// Step 4: refreshes the catalog of the region least recently
    /// refreshed among those that are due, through the role of a tenant
    /// there chosen at random, so one tenant whose role refuses does not
    /// hold the region back.
    async fn catalog(&self) -> Fallible<Summary> {
        let due: Option<CatalogTenant> = self
            .db
            .first(
                &catalog_query(),
                &params![
                    self.now.saturating_sub(instance_types::FRESH_FOR),
                    self.now.saturating_sub(instance_types::RETRY_AFTER),
                ],
            )
            .await
            .map_err(text)?;
        let Some(tenant) = due else {
            return Ok(Summary::default());
        };
        if !self.spend(CATALOG_COST) {
            return Ok(Summary::default());
        }
        match self.refresh_catalog(&tenant).await {
            Ok(entries) => {
                catalog::replace(&self.db, &tenant.region, &entries, self.now)
                    .await
                    .map_err(text)?;
                console_log!(
                    "reconcile: catalog of {}: {} instance types",
                    tenant.region,
                    entries.len()
                );
                Ok(Summary {
                    catalog_types: u32::try_from(entries.len()).unwrap_or(u32::MAX),
                    ..Summary::default()
                })
            }
            Err(err) => {
                let err = format!("through tenant {}: {err}", tenant.id);
                catalog::failed(&self.db, &tenant.region, &err, self.now)
                    .await
                    .map_err(text)?;
                Err(format!("catalog of {}: {err}", tenant.region))
            }
        }
    }

    /// Reads `tenant`'s region's instance types, offerings and prices.
    async fn refresh_catalog(
        &self,
        tenant: &CatalogTenant,
    ) -> Fallible<Vec<instance_types::Entry>> {
        let price_url = instance_types::price_list_url(&tenant.region)
            .ok_or_else(|| format!("no price list for {}", tenant.region))?;
        let key = self.secret_der(oidc::SIGNING_KEY)?;
        let kid =
            get(self.env, oidc::KEY_ID).ok_or_else(|| format!("{} is not set", oidc::KEY_ID))?;
        let credentials = launch::tenant_credentials(
            &key,
            &kid,
            self.aws_endpoint.as_deref(),
            &tenant.id,
            &tenant.role_arn,
            &tenant.region,
            SESSION,
        )
        .await
        .map_err(|err| format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn))?;
        let endpoint = aws::endpoint("ec2", &tenant.region, self.aws_endpoint.as_deref());
        let call = |mut request: crate::http::Request| {
            let credentials = &credentials;
            let region = tenant.region.as_str();
            async move {
                aws::sign_v4(&mut request, credentials, region, "ec2", now_ms())?;
                send(&request).await
            }
        };

        let mut types = Vec::new();
        let mut next = None;
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::instance_types(&endpoint, next.as_deref())).await?;
            let (page, token) = describe::described_types(&response)
                .map_err(|err| format!("DescribeInstanceTypes: {err}"))?;
            types.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        let mut offerings = Vec::new();
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::offerings(&endpoint, next.as_deref())).await?;
            let (page, token) = describe::described_offerings(&response)
                .map_err(|err| format!("DescribeInstanceTypeOfferings: {err}"))?;
            offerings.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        let now = time::format(self.now);
        let mut spot = Vec::new();
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::spot_prices(&endpoint, &now, next.as_deref())).await?;
            let (page, token) = describe::described_spot_prices(&response)
                .map_err(|err| format!("DescribeSpotPriceHistory: {err}"))?;
            spot.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        if next.is_some() {
            return Err(format!(
                "more than {CATALOG_PAGES} pages of one describe call"
            ));
        }
        let prices = send(&crate::http::Request::new("GET", price_url)).await?;
        if !prices.ok() {
            return Err(format!("price list answered {}", prices.status));
        }
        let on_demand = instance_types::on_demand_prices(&prices.body)?;
        Ok(instance_types::merge(types, &offerings, &on_demand, &spot))
    }

    /// Step 5: fails the lost launches of installations with no AWS
    /// connected. `launch` finds no tenant and says so on the job.
    async fn unconnected(&self) -> Fallible<Summary> {
        let before = self.now.saturating_sub(self.limits.queued_grace);
        let rows = jobs::unconnected::<LaunchRow>(&self.db, before, UNCONNECTED_PER_PASS)
            .await
            .map_err(text)?;
        let mut done = Summary::default();
        for job in rows.into_iter().filter_map(queued_job) {
            launch::launch(self.env.clone(), job).await;
            done.unconnected += 1;
        }
        Ok(done)
    }

    /// Step 6: records the minutes of jobs that completed in the last
    /// [`SWEEP_WINDOW`] with none recorded, then sends the minutes Polar
    /// has not taken yet, oldest first,
    /// one batch a pass. Without Polar's token nothing is sent and the
    /// events wait.
    async fn meter(&self) -> Fallible<Summary> {
        let swept = usage::sweep(
            &self.db,
            self.now.saturating_sub(SWEEP_WINDOW),
            DELETE_BATCH,
            self.now,
        )
        .await
        .map_err(text)?;
        if swept > 0 {
            console_log!("reconcile: recorded the minutes of {swept} jobs no webhook recorded");
        }
        let Some(token) = get(self.env, POLAR_ACCESS_TOKEN) else {
            return Ok(Summary::default());
        };
        let batch = u32::try_from(polar::INGEST_BATCH).unwrap_or(u32::MAX);
        let unsent = usage::unsent(&self.db, batch).await.map_err(text)?;
        if unsent.is_empty() || !self.spend(1) {
            return Ok(Summary::default());
        }
        let api = get(self.env, POLAR_API_URL)
            .unwrap_or_else(|| polar::API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let events: Vec<polar::Minutes<'_>> = unsent
            .iter()
            .map(|event| polar::Minutes {
                job_id: event.job_id,
                tenant: &event.tenant_id,
                minutes: event.minutes,
                at: time::format(event.ended_at),
            })
            .collect();
        polar::ingested(&send(&polar::ingest(&api, &token, &events)).await?)?;
        let jobs: Vec<u64> = unsent.iter().map(|event| event.job_id).collect();
        usage::sent(&self.db, &jobs, self.now).await.map_err(text)?;
        if unsent.len() == polar::INGEST_BATCH {
            self.more.set(true);
        }
        Ok(Summary {
            metered: u32::try_from(jobs.len()).unwrap_or(u32::MAX),
            ..Summary::default()
        })
    }

    /// Step 7: deletes expired sessions, jobs past the retention,
    /// redeliveries GitHub no longer lists, lapsed unavailable offerings,
    /// cache uploads nobody finished, whose parts S3's lifecycle rule
    /// removes after a day, job minutes older than [`USAGE_MEMORY`]
    /// that Polar has or never needed, and old failed launches and rate
    /// limits.
    async fn retention(&self) -> Fallible<Summary> {
        let now = self.now;
        let results = self
            .db
            .batch(&[
                // Before the deletes, so a row without an expiry that
                // is past it goes in the same pass.
                (jobs::BACKFILL_JOBS, params![UNSET_RETENTION, DELETE_BATCH]),
                (jobs::BACKFILL_RUNS, params![UNSET_RETENTION, DELETE_BATCH]),
                (DELETE_SESSIONS, params![now, DELETE_BATCH]),
                (jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
                (
                    DELETE_REDELIVERIES,
                    params![now.saturating_sub(REDELIVERY_MEMORY), DELETE_BATCH],
                ),
                (DELETE_OFFERINGS, params![now]),
                (
                    jobs::DELETE_UPLOADS,
                    params![now.saturating_sub(CACHE_UPLOAD_MEMORY), DELETE_BATCH],
                ),
                (
                    alerts::DELETE_FAILED,
                    params![now.saturating_sub(FAILED_LAUNCH_MEMORY), DELETE_BATCH],
                ),
                (
                    alerts::DELETE_RATE_LIMITS,
                    params![now.saturating_sub(RATE_LIMIT_MEMORY)],
                ),
                // After the jobs, so a run goes with its last job.
                (jobs::DELETE_RUNS, params![now, DELETE_BATCH]),
                (
                    DELETE_USAGE,
                    params![now.saturating_sub(USAGE_MEMORY), DELETE_BATCH],
                ),
            ])
            .await
            .map_err(text)?;
        let deleted = |at: usize| {
            results
                .get(at)
                .map_or(0, |changed| u32::try_from(*changed).unwrap_or(u32::MAX))
        };
        if [0, 1, 2, 3, 4, 6, 7, 9, 10]
            .into_iter()
            .any(|at| deleted(at) == DELETE_BATCH)
        {
            self.more.set(true);
        }
        Ok(Summary {
            sessions_deleted: deleted(2),
            jobs_deleted: deleted(3),
            runs_deleted: deleted(9),
            ..Summary::default()
        })
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
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_800_000_000_000;

    fn job(db: &Db, id: u64, installation_id: u64, status: &str, queued_at: u64) {
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at) VALUES ($1, $2, 'acme/app', $3, 'hatchrun', $4, $5, $5)",
            &params![id, installation_id, id * 10, status, queued_at],
        );
    }

    fn tenant(db: &Db, id: &str, installation_id: Option<u64>, region: Option<&str>) {
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             launch_template_id, subnet_ids, created_at, updated_at) \
             VALUES ($1, $2, $1, $3, 'arn:aws:iam::1:role/r', $4, 'lt-1', 'subnet-1', $5, $5)",
            &params![
                id,
                installation_id.unwrap_or(0) + 1000,
                installation_id,
                region,
                NOW
            ],
        );
    }

    fn value(db: &Db, sql: &str, params: &[crate::testdb::Param]) -> Value {
        db.rows(sql, params).unwrap()[0]
            .values()
            .next()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_step_gives_back_what_it_did_not_send() {
        // Two calls reserved three each and sent one each.
        assert_eq!(settled(494, 500, 2), 498);
        // A refused token took all three of one call.
        assert_eq!(settled(494, 500, 4), 496);
        assert_eq!(settled(500, 500, 0), 500);
    }

    #[test]
    fn the_lease_is_taken_once_and_given_back() {
        let db = Db::new();
        let take = params![NOW, NOW + LEASE];
        assert_eq!(db.run(TAKE_LEASE, &take), 1);
        assert_eq!(db.run(TAKE_LEASE, &params![NOW + 1, NOW + 1 + LEASE]), 0);
        let history = r#"[{"at":1,"errors":0,"more":false}]"#;
        assert_eq!(
            db.run(RECORD, &params![NOW, NOW + 5, r#"{"tenants":1}"#, history]),
            1
        );
        let row = &db
            .rows(
                "SELECT lease_until, started_at, finished_at, summary, history FROM reconciler",
                &[],
            )
            .unwrap()[0];
        assert_eq!(row["lease_until"], 0);
        assert_eq!(row["started_at"], NOW);
        assert_eq!(row["finished_at"], NOW + 5);
        assert_eq!(row["summary"], r#"{"tenants":1}"#);
        assert_eq!(row["history"], history);
        assert_eq!(db.run(TAKE_LEASE, &take), 1);
        // A pass that could not read the history keeps the one there.
        db.run(RECORD, &params![NOW, NOW + 6, "{}", None::<&str>]);
        let kept: Option<String> = db.column(HISTORY, &[], "history");
        assert_eq!(kept.as_deref(), Some(history));
    }

    #[test]
    fn the_delivery_mark_only_moves_forward() {
        let db = Db::new();
        assert_eq!(
            db.column::<Option<u64>>(DELIVERIES_UNTIL, &[], "deliveries_until"),
            Some(None)
        );
        assert_eq!(db.run(MOVE_DELIVERIES_UNTIL, &params![NOW]), 1);
        db.run(MOVE_DELIVERIES_UNTIL, &params![NOW - 10]);
        assert_eq!(
            db.column::<Option<u64>>(DELIVERIES_UNTIL, &[], "deliveries_until"),
            Some(Some(NOW))
        );
    }

    #[test]
    fn a_redelivery_is_claimed_once() {
        let db = Db::new();
        let claim = params!["g-1", 77u64, "workflow_job", NOW];
        assert_eq!(db.run(CLAIM_REDELIVERY, &claim), 1);
        assert_eq!(db.run(CLAIM_REDELIVERY, &claim), 0);
        let guids = serde_json::to_string(&["g-1", "g-2"]).unwrap();
        let remembered: Vec<Guid> = db.all(REMEMBERED, &params![guids]);
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].guid, "g-1");
        let empty: Vec<Guid> = db.all(REMEMBERED, &params!["[]"]);
        assert!(empty.is_empty());
    }

    #[test]
    fn runs_without_details_are_fetched_after_a_minute() {
        let db = Db::new();
        for (id, created_at) in [
            (1u64, NOW - 2 * DETAILS_AFTER),
            (2, NOW),
            (3, NOW - DETAILS_AFTER - 1),
        ] {
            db.run(
                "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
                 queued_at, created_at, updated_at) VALUES ($1, 9, 'acme/app', 'a', 1, $2, $2, $2)",
                &params![id, created_at],
            );
        }
        let due = |db: &Db| -> Vec<u64> {
            db.all::<RunWithoutDetails>(
                jobs::RUNS_WITHOUT_DETAILS,
                &params![NOW - DETAILS_AFTER, DETAILS_PER_PASS],
            )
            .into_iter()
            .map(|run| run.id)
            .collect()
        };
        assert_eq!(due(&db), [1, 3]);
        db.run(
            jobs::DETAILS,
            &params![
                1u64,
                None::<u64>,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                NOW
            ],
        );
        assert_eq!(due(&db), [3]);
        // Tried and failed: it waits its turn behind the others.
        assert_eq!(db.run(jobs::DETAILS_TRIED, &params![NOW, "[3]"]), 1);
        assert_eq!(due(&db), Vec::<u64>::new());
        let later: Vec<u64> = db
            .all::<RunWithoutDetails>(
                jobs::RUNS_WITHOUT_DETAILS,
                &params![NOW + DETAILS_AFTER + 1, DETAILS_PER_PASS],
            )
            .into_iter()
            .map(|run| run.id)
            .collect();
        assert_eq!(later, [2, 3]);
    }

    #[test]
    fn quiet_open_jobs_are_checked_and_recorded() {
        let db = Db::new();
        job(&db, 1, 9, "queued", NOW - 3000);
        job(&db, 2, 9, "in_progress", NOW - 2000);
        job(&db, 3, 9, "completed", NOW - 4000);
        job(&db, 4, 9, "queued", NOW);
        let open: Vec<OpenJob> = db.all(jobs::OPEN_JOBS, &params![NOW - 1000, SYNC_PER_PASS]);
        let ids: Vec<u64> = open.iter().map(|job| job.id).collect();
        assert_eq!(ids, [1, 2]);
        assert_eq!(open[0].installation_id, 9);
        assert_eq!(open[0].repository, "acme/app");
        assert_eq!(open[0].status, "queued");

        // As `checked`: completed by runner r-1.
        let done = params![
            1u64,
            "completed",
            Some("r-1"),
            NOW,
            "queued",
            Some("success"),
            Some(NOW - 2500),
            Some(NOW - 100)
        ];
        assert_eq!(db.run(jobs::RECORD_CHECK, &done), 1);
        // The row moved on, so the same update matches nothing.
        assert_eq!(db.run(jobs::RECORD_CHECK, &done), 0);
        // Only asked, the status unchanged and no runner.
        let asked = params![
            2u64,
            "in_progress",
            None::<&str>,
            NOW,
            "in_progress",
            None::<&str>,
            None::<u64>,
            None::<u64>
        ];
        assert_eq!(db.run(jobs::RECORD_CHECK, &asked), 1);
        let rows = db
            .rows(
                "SELECT id, status, ran_on, checked_at, updated_at FROM jobs \
                 WHERE id IN (1, 2) ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[0]["ran_on"], "r-1");
        let ended = db
            .rows(
                "SELECT conclusion, completed_at FROM jobs WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(ended[0]["conclusion"], "success");
        assert_eq!(ended[0]["completed_at"], NOW - 100);
        assert_eq!(rows[0]["updated_at"], NOW);
        assert_eq!(rows[1]["ran_on"], Value::Null);
        assert_eq!(rows[1]["checked_at"], NOW);
        assert_eq!(rows[1]["updated_at"], NOW - 2000);
        let open: Vec<OpenJob> = db.all(jobs::OPEN_JOBS, &params![NOW - 1000, SYNC_PER_PASS]);
        assert!(open.is_empty());
    }

    #[test]
    fn verified_tenants_are_reconciled_oldest_first() {
        let db = Db::new();
        tenant(&db, "a", Some(1), Some("eu-west-1"));
        tenant(&db, "b", Some(2), Some("eu-west-1"));
        tenant(&db, "c", Some(3), None);
        db.run(MARK_RECONCILED, &params!["a", NOW]);
        db.run(MARK_SWEPT, &params!["b", NOW - 1]);
        db.run(MARK_STACK_CHECKED, &params!["b", NOW - 2]);
        let tenants: Vec<Tenant> = db.all(&tenants_query(), &params![TENANTS_PER_PASS]);
        let ids: Vec<&str> = tenants.iter().map(|tenant| tenant.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        let b = &tenants[0];
        assert_eq!(b.installation_id, Some(2));
        assert_eq!(b.role_arn, "arn:aws:iam::1:role/r");
        assert_eq!(b.region, "eu-west-1");
        assert_eq!(b.launch_template_id, "lt-1");
        assert_eq!(b.max_runners, 10);
        assert_eq!(b.versions_swept_at, Some(NOW - 1));
        assert_eq!(b.stack_checked_at, Some(NOW - 2));
        assert_eq!(tenants[1].versions_swept_at, None);
    }

    #[test]
    fn a_tenants_jobs_include_the_finished_ones_its_instances_name() {
        let db = Db::new();
        job(&db, 1, 9, "queued", NOW - 30);
        job(&db, 2, 9, "completed", NOW - 20);
        job(&db, 3, 9, "completed", NOW - 10);
        job(&db, 4, 9, "completed", NOW - 5);
        job(&db, 5, 8, "queued", NOW);
        db.run(
            "UPDATE jobs SET ran_on = 'r-3', launch_state = 'launched', runner_name = 'r-3', \
             launch_retry = 'backoff', next_launch_at = $1 WHERE id = 3",
            &params![NOW + 60_000],
        );
        let ids = serde_json::to_string(&[2u64]).unwrap();
        let runners = serde_json::to_string(&["r-3"]).unwrap();
        let mut rows: Vec<JobRow> = db.all(jobs::TENANT_JOBS, &params![9u64, ids, runners]);
        rows.sort_by_key(|row| row.id);
        let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        let three = &rows[2];
        assert_eq!(three.label, "hatchrun");
        assert_eq!(three.status, "completed");
        assert_eq!(three.launch_state.as_deref(), Some("launched"));
        assert_eq!(three.runner_name.as_deref(), Some("r-3"));
        assert_eq!(three.ran_on.as_deref(), Some("r-3"));
        assert_eq!(three.queued_at, NOW - 10);
        assert_eq!(three.updated_at, NOW - 10);
        assert_eq!(three.launch_retry.as_deref(), Some("backoff"));
        assert_eq!(three.next_launch_at, Some(NOW + 60_000));

        // No instances: empty arrays.
        let rows: Vec<JobRow> = db.all(jobs::TENANT_JOBS, &params![9u64, "[]", "[]"]);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn job_updates_apply_only_while_the_row_matches() {
        let db = Db::new();
        for id in 1..=4 {
            job(&db, id, 9, "queued", NOW - 100);
        }
        db.run(
            "UPDATE jobs SET launch_state = 'launching', runner_name = 'r-1' WHERE id = 1",
            &[],
        );
        db.run(
            "UPDATE jobs SET launch_state = 'failed', launch_retry = 'setup', \
             next_launch_at = 5 WHERE id = 2",
            &[],
        );
        db.run(
            "UPDATE jobs SET launch_state = 'launched', runner_name = 'r-3' WHERE id = 3",
            &[],
        );
        db.run(
            "UPDATE jobs SET launch_state = 'launched', runner_name = 'r-4' WHERE id = 4",
            &[],
        );
        let adopt = params![1u64, "i-1", NOW];
        let reset = params![2u64, "failed", NOW, None::<&str>];
        let reset_named = params![4u64, "launched", NOW, Some("r-4")];
        let give_up = params![3u64, NOW, "runner r-3 waited 10 minutes", "r-3"];
        let changed = db
            .batch(&[
                (jobs::ADOPT, adopt.clone()),
                (jobs::RESET, reset.clone()),
                (jobs::RESET, reset_named.clone()),
                (jobs::GIVE_UP, give_up.clone()),
            ])
            .unwrap();
        assert_eq!(changed, [1, 1, 1, 1]);
        let rows = db
            .rows(
                "SELECT launch_state, runner_name, instance_id, launch_error, launch_retry, \
                 next_launch_at, updated_at FROM jobs ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["launch_state"], "launched");
        assert_eq!(rows[0]["instance_id"], "i-1");
        assert_eq!(rows[1]["launch_state"], Value::Null);
        assert_eq!(rows[1]["launch_retry"], Value::Null);
        assert_eq!(rows[1]["next_launch_at"], Value::Null);
        assert_eq!(rows[2]["launch_state"], "failed");
        assert_eq!(rows[2]["launch_error"], "runner r-3 waited 10 minutes");
        assert_eq!(rows[3]["launch_state"], Value::Null);
        assert_eq!(rows[3]["runner_name"], Value::Null);
        assert!(rows.iter().all(|row| row["updated_at"] == NOW));

        // The rows moved on; the same statements change nothing.
        for (sql, params) in [
            (jobs::ADOPT, adopt),
            (jobs::RESET, reset),
            (jobs::RESET, reset_named),
            (jobs::GIVE_UP, give_up),
        ] {
            assert_eq!(db.run(sql, &params), 0, "{sql}");
        }
    }

    #[test]
    fn queued_jobs_read_as_launch_takes_them() {
        let db = Db::new();
        job(&db, 1, 9, "queued", NOW);
        job(&db, 2, 9, "queued", NOW - 10);
        job(&db, 3, 9, "queued", NOW - 20);
        let ids = serde_json::to_string(&[1u64, 2]).unwrap();
        let rows: Vec<LaunchRow> = db.all(jobs::QUEUED_JOBS, &params![ids]);
        let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, [2, 1]);
        assert_eq!(rows[0].installation_id, 9);
        assert_eq!(rows[0].repository, "acme/app");
        assert_eq!(rows[0].run_id, 20);
        assert_eq!(rows[0].label, "hatchrun");
    }

    #[test]
    fn the_catalog_picks_a_region_that_is_due() {
        let db = Db::new();
        tenant(&db, "a", Some(1), Some("eu-west-1"));
        tenant(&db, "b", Some(2), Some("us-east-1"));
        tenant(&db, "c", Some(3), None);
        let fresh = NOW - instance_types::FRESH_FOR;
        let retry = NOW - instance_types::RETRY_AFTER;
        let due = params![fresh, retry];
        db.run(
            "INSERT INTO instance_type_regions (region, refreshed_at, attempted_at) \
             VALUES ('eu-west-1', $1, $1)",
            &params![NOW],
        );
        let picked: Option<CatalogTenant> = db.first(&catalog_query(), &due);
        let picked = picked.unwrap();
        assert_eq!(picked.id, "b");
        assert_eq!(picked.region, "us-east-1");
        assert_eq!(picked.role_arn, "arn:aws:iam::1:role/r");
        db.run(
            "INSERT INTO instance_type_regions (region, attempted_at) VALUES ('us-east-1', $1)",
            &params![NOW],
        );
        assert!(db.first::<CatalogTenant>(&catalog_query(), &due).is_none());
    }

    #[test]
    fn lost_launches_without_aws_are_found() {
        let db = Db::new();
        tenant(&db, "a", Some(1), Some("eu-west-1"));
        tenant(&db, "c", Some(3), None);
        job(&db, 10, 1, "queued", NOW - 100_000);
        job(&db, 11, 2, "queued", NOW - 100_000);
        job(&db, 12, 3, "queued", NOW - 200_000);
        job(&db, 13, 2, "queued", NOW);
        let rows: Vec<LaunchRow> = db.all(
            &jobs::unconnected_query(),
            &params![NOW - 60_000, UNCONNECTED_PER_PASS],
        );
        let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, [12, 11]);
    }

    #[test]
    fn retention_deletes_old_rows() {
        let db = Db::new();
        db.run(
            "INSERT INTO users (id, login, avatar_url, created_at, updated_at) \
             VALUES (1, 'u', 'a', 0, 0)",
            &[],
        );
        for (key, expires_at) in [("old", NOW - 1), ("new", NOW + 1)] {
            db.run(
                "INSERT INTO sessions (key, user_id, github_token, created_at, expires_at) \
                 VALUES ($1, 1, 't', 0, $2)",
                &params![key, expires_at],
            );
        }
        job(&db, 1, 9, "completed", NOW - 100);
        job(&db, 2, 9, "completed", NOW);
        db.run(
            "UPDATE jobs SET expires_at = CASE id WHEN 1 THEN $1::bigint ELSE $2::bigint END WHERE id IN (1, 2)",
            &params![NOW - 1, NOW + 1],
        );
        // Run 10 loses its only job; run 20 expired too but keeps one.
        for (id, expires_at) in [(10u64, NOW - 1), (20, NOW - 1), (30, NOW + 1)] {
            db.run(
                "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
                 queued_at, created_at, updated_at, expires_at) \
                 VALUES ($1, 9, 'acme/app', 'a', 1, 0, 0, 0, $2)",
                &params![id, expires_at],
            );
        }
        for (guid, at) in [("old", NOW - REDELIVERY_MEMORY - 1), ("new", NOW)] {
            db.run(CLAIM_REDELIVERY, &params![guid, 1u64, "e", at]);
        }
        for (subnet, until) in [("s-old", NOW - 1), ("s-new", NOW + 1)] {
            db.run(
                "INSERT INTO unavailable_offerings \
                 (tenant_id, instance_type, subnet_id, spot, code, until) \
                 VALUES ('a', 'm8g.large', $1, false, 'c', $2)",
                &params![subnet, until],
            );
        }
        for (id, at) in [("old", NOW - CACHE_UPLOAD_MEMORY - 1), ("new", NOW)] {
            db.run(
                "INSERT INTO cache_uploads (id, job_id, object_key, upload_id, created_at) \
                 VALUES ($1, 1, 'k', 'u', $2)",
                &params![id, at],
            );
        }
        // As `retention` binds them.
        let deleted = db
            .batch(&[
                (DELETE_SESSIONS, params![NOW, DELETE_BATCH]),
                (jobs::DELETE_JOBS, params![NOW, DELETE_BATCH]),
                (
                    DELETE_REDELIVERIES,
                    params![NOW - REDELIVERY_MEMORY, DELETE_BATCH],
                ),
                (DELETE_OFFERINGS, params![NOW]),
                (
                    jobs::DELETE_UPLOADS,
                    params![NOW - CACHE_UPLOAD_MEMORY, DELETE_BATCH],
                ),
                (jobs::DELETE_RUNS, params![NOW, DELETE_BATCH]),
            ])
            .unwrap();
        assert_eq!(deleted, [1, 1, 1, 1, 1, 1]);
        assert_eq!(
            value(
                &db,
                "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM runs",
                &[]
            ),
            "20,30"
        );
        for (table, key) in [
            ("sessions", "key"),
            ("redeliveries", "guid"),
            ("unavailable_offerings", "subnet_id"),
            ("cache_uploads", "id"),
        ] {
            let left = value(
                &db,
                &format!("SELECT string_agg({key}::text, ',') AS kept FROM {table}"),
                &[],
            );
            assert!(left == "new" || left == "s-new", "{table} kept {left}");
        }
        assert_eq!(
            value(
                &db,
                "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
                &[]
            ),
            "2"
        );
    }

    #[test]
    fn retention_deletes_a_batch_at_most() {
        let db = Db::new();
        for id in 0..3u64 {
            job(&db, id + 1, 9, "completed", id);
        }
        db.run(
            "UPDATE jobs SET expires_at = queued_at WHERE expires_at IS NULL",
            &[],
        );
        assert_eq!(db.run(jobs::DELETE_JOBS, &params![NOW, 2u32]), 2);
        assert_eq!(
            value(
                &db,
                "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
                &[]
            ),
            "3"
        );
    }

    #[test]
    fn jobs_expire_after_their_tenants_retention() {
        const DAY: u64 = 24 * 60 * 60 * 1000;
        let db = Db::new();
        // Installation 1 keeps 90 days, 2 the default 30, 3 has no
        // tenant.
        tenant(&db, "a", Some(1), Some("eu-west-1"));
        tenant(&db, "b", Some(2), Some("eu-west-1"));
        db.run("UPDATE tenants SET retention_days = 90 WHERE id = 'a'", &[]);
        for installation in 1..=3u64 {
            // As the webhook records a job of run `installation`.
            db.batch(&[
                (
                    crate::jobs::RUN,
                    params![
                        installation,
                        installation,
                        "acme/app",
                        None::<&str>,
                        None::<&str>,
                        "a",
                        1u64,
                        NOW
                    ],
                ),
                (
                    crate::jobs::QUEUED,
                    params![
                        installation,
                        installation,
                        "acme/app",
                        installation,
                        "hatchrun",
                        NOW,
                        "build",
                        1u64
                    ],
                ),
            ])
            .unwrap();
        }
        let expiry = |table: &str| {
            value(
                &db,
                &format!(
                    "SELECT string_agg(expires_at::text, ',' ORDER BY id) AS expiry FROM {table}"
                ),
                &[],
            )
        };
        let want = format!("{},{},{}", NOW + 90 * DAY, NOW + 30 * DAY, NOW + 30 * DAY);
        assert_eq!(expiry("jobs"), want);
        assert_eq!(expiry("runs"), want);

        let retention = |now: u64| {
            db.batch(&[
                (jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
                (jobs::DELETE_RUNS, params![now, DELETE_BATCH]),
            ])
            .unwrap()
        };
        assert_eq!(retention(NOW + 30 * DAY), [0, 0]);
        assert_eq!(retention(NOW + 30 * DAY + 1), [2, 2]);
        assert_eq!(
            value(
                &db,
                "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
                &[]
            ),
            "1"
        );
        assert_eq!(
            value(
                &db,
                "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM runs",
                &[]
            ),
            "1"
        );
        assert_eq!(retention(NOW + 90 * DAY), [0, 0]);
        assert_eq!(retention(NOW + 90 * DAY + 1), [1, 1]);
    }

    #[test]
    fn rows_without_an_expiry_get_the_default() {
        let db = Db::new();
        // As the Worker from before migration 0019 recorded them: an
        // old job and run, and a recent job.
        job(&db, 1, 9, "completed", NOW - UNSET_RETENTION - 1);
        job(&db, 2, 9, "queued", NOW);
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
             queued_at, created_at, updated_at) VALUES (10, 9, 'acme/app', 'a', 1, $1, $1, $1)",
            &params![NOW - UNSET_RETENTION - 1],
        );
        // As `retention` binds them.
        let pass = |db: &Db| {
            db.batch(&[
                (jobs::BACKFILL_JOBS, params![UNSET_RETENTION, DELETE_BATCH]),
                (jobs::BACKFILL_RUNS, params![UNSET_RETENTION, DELETE_BATCH]),
                (jobs::DELETE_JOBS, params![NOW, DELETE_BATCH]),
                (jobs::DELETE_RUNS, params![NOW, DELETE_BATCH]),
            ])
            .unwrap()
        };
        assert_eq!(pass(&db), [2, 1, 1, 1]);
        let left = db.rows("SELECT id, expires_at FROM jobs", &[]).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["id"], 2);
        assert_eq!(left[0]["expires_at"], NOW + UNSET_RETENTION);
        assert_eq!(value(&db, "SELECT COUNT(*) AS n FROM runs", &[]), 0);
        // Filled in: the next pass has nothing to do.
        assert_eq!(pass(&db), [0, 0, 0, 0]);
    }

    #[test]
    fn retention_days_stay_between_one_and_ninety() {
        let db = Db::new();
        tenant(&db, "a", Some(1), Some("eu-west-1"));
        for days in [0u32, 91] {
            assert!(
                db.try_run(
                    "UPDATE tenants SET retention_days = $1 WHERE id = 'a'",
                    &params![days]
                )
                .is_err(),
                "{days}"
            );
        }
        assert_eq!(
            db.run(
                "UPDATE tenants SET retention_days = $1 WHERE id = 'a'",
                &params![1u32]
            ),
            1
        );
    }

    #[test]
    fn job_queries_read_through_indexes() {
        let db = Db::new();
        // On these empty tables the planner would read a whole index
        // in no useful order and sort; a large table reads the indexes
        // below in order.
        db.run("SET enable_bitmapscan = off", &[]);
        db.run("SET enable_mergejoin = off", &[]);
        db.run("SET enable_hashjoin = off", &[]);
        let plans = [
            (
                "OPEN_JOBS",
                db.plan(jobs::OPEN_JOBS, &params![NOW, SYNC_PER_PASS]),
                &["jobs_unchecked"][..],
            ),
            (
                "TENANT_JOBS",
                db.plan(jobs::TENANT_JOBS, &params![9u64, "[1]", r#"["r-1"]"#]),
                &["jobs_open", "jobs_pkey", "jobs_ran_on"][..],
            ),
            (
                "DELETE_JOBS",
                db.plan(jobs::DELETE_JOBS, &params![NOW, DELETE_BATCH]),
                &["jobs_expiry"][..],
            ),
            (
                "DELETE_RUNS",
                db.plan(jobs::DELETE_RUNS, &params![NOW, DELETE_BATCH]),
                &["runs_expiry", "jobs_run"][..],
            ),
            (
                "BACKFILL_JOBS",
                db.plan(jobs::BACKFILL_JOBS, &params![UNSET_RETENTION, DELETE_BATCH]),
                &["jobs_expiry", "(expires_at IS NULL)"][..],
            ),
            (
                "BACKFILL_RUNS",
                db.plan(jobs::BACKFILL_RUNS, &params![UNSET_RETENTION, DELETE_BATCH]),
                &["runs_expiry", "(expires_at IS NULL)"][..],
            ),
            (
                "DELETE_USAGE",
                db.plan(DELETE_USAGE, &params![NOW, DELETE_BATCH]),
                &["usage_events_done"][..],
            ),
        ];
        for (name, plan, indexes) in plans {
            assert!(!plan.contains("Seq Scan"), "{name}\n{plan}");
            assert!(!plan.contains("Sort"), "{name}\n{plan}");
            for index in indexes {
                assert!(plan.contains(index), "{name} without {index}\n{plan}");
            }
        }
    }
}
