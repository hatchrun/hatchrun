// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The reconciler, run by a Cron Trigger every minute. Webhooks get
//! lost, launches die with the request that started them and runners
//! take each other's jobs; each pass puts D1 and the tenants' instances
//! back in line:
//!
//! 1. redeliver the App's webhook deliveries that failed, each once
//!    (see [`deliveries`]);
//! 2. ask GitHub about open jobs that have had no webhook for a while,
//!    and record the ones it finished (see [`sync`]);
//! 3. per tenant with a verified role, describe its live instances,
//!    terminate the ones nothing needs and launch queued jobs again
//!    (see [`plan`]); once a day also read its stack's launch template
//!    again (see [`crate::stack`]) and delete the template versions
//!    launches left behind (see [`crate::aws::fleet::stale`]);
//! 4. refresh the instance type catalog of one region that is due,
//!    through a tenant role there (see [`crate::instance_types`]);
//! 5. fail the lost launches of installations with no AWS connected,
//!    which `launch` does without calling anyone;
//! 6. delete expired sessions and rows past the retention.
//!
//! Every step is safe to repeat and to run beside a webhook: rows
//! change only while they still say what the pass read, and `launch`
//! claims a job before it starts anything. A lease on the `reconciler`
//! row keeps a slow pass from overlapping the next one. A pass makes at
//! most [`SUBREQUESTS`] requests and handles a bounded number of
//! tenants, jobs and deliveries, oldest first; what is left waits for
//! the next minute. It logs one summary line and keeps it, with its
//! time, in the `reconciler` row.

pub mod deliveries;
pub mod plan;
pub mod retry;
pub mod sync;
pub mod time;

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env, console_error, console_log, console_warn};

use self::plan::{Instance, Job, LaunchState, Limits, Plan};
use crate::aws::describe;
use crate::aws::{self, Credentials, ec2, fleet, instances, ssm};
use crate::fetch::{send, send_reading};
use crate::instance_types::{self, store as catalog};
use crate::launch::{self, APP_KEY, AWS_ENDPOINT_URL};
use crate::webhook::{QueuedJob, Status};
use crate::{DB, github, int, jwt, labels, now_ms, oidc};

/// Variable with the App's client id.
const APP_ID: &str = "GITHUB_APP_ID";
/// Variable that replaces the GitHub API URL, for development.
const GITHUB_API_URL: &str = "GITHUB_API_URL";

/// Requests a pass may make. Workers allow 1000 per invocation; launches
/// and the app's own requests are well inside the rest.
const SUBREQUESTS: u32 = 500;
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
/// Pages of deliveries, and redeliveries, per pass.
const DELIVERY_PAGES: u32 = 10;
const REDELIVERIES_PER_PASS: usize = 30;
/// How far each scan of deliveries reaches back past the last one, for
/// attempts GitHub records late.
const DELIVERY_OVERLAP: u64 = 2 * 60 * 1000;
/// How long a redelivery is remembered; GitHub keeps deliveries for
/// three days.
const REDELIVERY_MEMORY: u64 = 4 * 24 * 60 * 60 * 1000;
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
/// template with more is swept again on the next pass.
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
const VERIFIED: &str = "role_arn IS NOT NULL AND region IS NOT NULL \
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
        self.errors += other.errors;
    }
}

/// One pass's settings and request budget.
struct Pass<'a> {
    env: &'a Env,
    db: D1Database,
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
    let db = match env.d1(DB) {
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

    let steps: [(&str, Fallible<Summary>); 6] = [
        ("redeliver", pass.redeliver().await),
        ("sync", pass.sync().await),
        ("tenants", pass.tenants().await),
        ("catalog", pass.catalog().await),
        ("unconnected", pass.unconnected().await),
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
    if let Err(err) = record(&pass.db, started, &line).await {
        console_error!("reconcile: record: {err}");
    }
}

/// Takes the lease on the `reconciler` row if no pass holds it.
async fn take_lease(db: &D1Database, now: u64) -> Fallible<bool> {
    let result = db
        .prepare("UPDATE reconciler SET lease_until = ?2 WHERE id = 1 AND lease_until <= ?1")
        .bind(&[int(now).map_err(text)?, int(now + LEASE).map_err(text)?])
        .map_err(text)?
        .run()
        .await
        .map_err(text)?;
    Ok(changes(&result) == 1)
}

/// Keeps the pass's summary and gives the lease back.
async fn record(db: &D1Database, started: u64, summary: &str) -> Fallible<()> {
    db.prepare(
        "UPDATE reconciler SET started_at = ?1, finished_at = ?2, summary = ?3, lease_until = 0 \
         WHERE id = 1",
    )
    .bind(&[
        int(started).map_err(text)?,
        int(now_ms()).map_err(text)?,
        summary.into(),
    ])
    .map_err(text)?
    .run()
    .await
    .map_err(text)?;
    Ok(())
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

    fn secret_der(&self, name: &str) -> Fallible<Vec<u8>> {
        let pem = get(self.env, name).ok_or_else(|| format!("{name} is not set"))?;
        jwt::pkcs8_der(&pem).map_err(|err| format!("{name}: {err}"))
    }

    async fn app_jwt(&self) -> Fallible<String> {
        let app_id = get(self.env, APP_ID).ok_or_else(|| format!("{APP_ID} is not set"))?;
        launch::app_jwt(&app_id, &self.secret_der(APP_KEY)?).await
    }

    async fn installation_token(&self, app_jwt: &str, installation_id: u64) -> Fallible<String> {
        let request = github::installation_token(&self.github_api, installation_id, app_jwt);
        github::token(&send(&request).await?)
    }

    /// Step 1: asks GitHub to deliver failed webhooks again.
    async fn redeliver(&self) -> Fallible<Summary> {
        let mut done = Summary::default();
        let app_jwt = self.app_jwt().await?;
        let mark: Option<u64> = self
            .db
            .prepare("SELECT deliveries_until FROM reconciler WHERE id = 1")
            .first(Some("deliveries_until"))
            .await
            .map_err(text)?;
        let floor = self.now.saturating_sub(self.limits.redeliver_window);
        let since = mark.map_or(floor, |mark| {
            mark.saturating_sub(DELIVERY_OVERLAP).max(floor)
        });

        let (seen, complete) = self.scan(&app_jwt, since).await?;

        let chosen = deliveries::choose(&seen, since);
        let guids: Vec<&str> = chosen.iter().map(|chosen| chosen.guid.as_str()).collect();
        let remembered: HashSet<String> = self
            .db
            .prepare(
                "SELECT guid FROM redeliveries WHERE guid IN (SELECT value FROM json_each(?1))",
            )
            .bind(&[json(&guids)?])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<Guid>()
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
                    .prepare(
                        "UPDATE reconciler SET deliveries_until = \
                         MAX(COALESCE(deliveries_until, 0), ?1) WHERE id = 1",
                    )
                    .bind(&[int(newest).map_err(text)?])
                    .map_err(text)?
                    .run()
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
            .prepare(
                "INSERT INTO redeliveries (guid, delivery_id, event, redelivered_at) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (guid) DO NOTHING",
            )
            .bind(&[
                redelivery.guid.as_str().into(),
                int(redelivery.id).map_err(text)?,
                redelivery.event.as_str().into(),
                int(self.now).map_err(text)?,
            ])
            .map_err(text)?
            .run()
            .await
            .map_err(text)?;
        if changes(&claimed) != 1 {
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
        let open = self
            .db
            .prepare(
                "SELECT id, installation_id, repository, status FROM jobs \
                 WHERE status <> 'completed' AND COALESCE(checked_at, updated_at) < ?1 \
                 ORDER BY COALESCE(checked_at, updated_at) LIMIT ?2",
            )
            .bind(&[int(quiet).map_err(text)?, SYNC_PER_PASS.into()])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<OpenJob>()
            .map_err(text)?;
        if open.is_empty() {
            return Ok(Summary::default());
        }
        let app_jwt = self.app_jwt().await?;
        let mut by_installation: BTreeMap<u64, Vec<OpenJob>> = BTreeMap::new();
        for job in open {
            by_installation
                .entry(job.installation_id)
                .or_default()
                .push(job);
        }
        let results: Vec<Summary> = stream::iter(by_installation)
            .map(|(installation_id, jobs)| self.sync_installation(&app_jwt, installation_id, jobs))
            .buffer_unordered(INSTALLATIONS_AT_ONCE)
            .collect()
            .await;
        let mut done = Summary::default();
        for result in &results {
            done.add(result);
        }
        Ok(done)
    }

    async fn sync_installation(
        &self,
        app_jwt: &str,
        installation_id: u64,
        jobs: Vec<OpenJob>,
    ) -> Summary {
        let mut done = Summary::default();
        if !self.spend(1) {
            return done;
        }
        let token = match self.installation_token(app_jwt, installation_id).await {
            Ok(token) => token,
            Err(err) => {
                console_warn!("reconcile: installation {installation_id}: {err}");
                done.errors += 1;
                for job in &jobs {
                    if let Err(err) = self.checked(job, None).await {
                        console_error!("reconcile: job {}: {err}", job.id);
                    }
                }
                return done;
            }
        };
        for job in &jobs {
            if !self.spend(1) {
                break;
            }
            done.checked += 1;
            let seen = match send(&sync::job(
                &self.github_api,
                &token,
                &job.repository,
                job.id,
            ))
            .await
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
        let now = int(self.now).map_err(text)?;
        let result = self
            .db
            .prepare(
                "UPDATE jobs SET status = ?2, ran_on = COALESCE(?3, ran_on), checked_at = ?4, \
                 updated_at = CASE WHEN status <> ?2 THEN ?4 ELSE updated_at END \
                 WHERE id = ?1 AND status = ?5",
            )
            .bind(&[
                int(job.id).map_err(text)?,
                next.as_str().into(),
                runner.map_or(JsValue::NULL, Into::into),
                now,
                job.status.as_str().into(),
            ])
            .map_err(text)?
            .run()
            .await
            .map_err(text)?;
        let closed = changes(&result) == 1 && next == Status::Completed;
        if closed {
            console_log!("reconcile: job {} completed on GitHub", job.id);
        }
        Ok(closed)
    }

    /// Step 3: every tenant with a verified role, least recently
    /// reconciled first.
    async fn tenants(&self) -> Fallible<Summary> {
        let tenants = self
            .db
            .prepare(format!(
                "SELECT id, installation_id, role_arn, region, launch_template_id, max_runners, \
                 stack_checked_at, versions_swept_at FROM tenants \
                 WHERE {VERIFIED} ORDER BY COALESCE(reconciled_at, 0), id LIMIT ?1"
            ))
            .bind(&[TENANTS_PER_PASS.into()])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<Tenant>()
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
            .prepare("UPDATE tenants SET reconciled_at = ?2 WHERE id = ?1")
            .bind(&[tenant.id.as_str().into(), int(self.now).map_err(text)?])
            .map_err(text)?
            .run()
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
        // from the first page, which now holds the versions left.
        if next.is_some() {
            self.more.set(true);
        } else {
            self.swept(tenant).await?;
        }
        Ok(deleted)
    }

    async fn swept(&self, tenant: &Tenant) -> Fallible<()> {
        self.db
            .prepare("UPDATE tenants SET versions_swept_at = ?2 WHERE id = ?1")
            .bind(&[tenant.id.as_str().into(), int(self.now).map_err(text)?])
            .map_err(text)?
            .run()
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
                    .prepare("UPDATE tenants SET stack_checked_at = ?2 WHERE id = ?1")
                    .bind(&[tenant.id.as_str().into(), int(self.now).map_err(text)?])
                    .map_err(text)?
                    .run()
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
        let rows = self
            .db
            .prepare(
                "SELECT id, label, status, launch_state, runner_name, ran_on, queued_at, \
                 updated_at, launch_retry, next_launch_at FROM jobs WHERE installation_id = ?1 AND (status <> 'completed' \
                 OR id IN (SELECT value FROM json_each(?2)) \
                 OR ran_on IN (SELECT value FROM json_each(?3)))",
            )
            .bind(&[
                int(installation_id).map_err(text)?,
                json(&ids)?,
                json(&runners)?,
            ])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<JobRow>()
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
        let now = int(self.now).map_err(text)?;
        let mut statements: Vec<(D1PreparedStatement, Kind)> = Vec::new();
        for (job_id, instance_id) in &plan.adopt {
            statements.push((
                self.db
                    .prepare(
                        "UPDATE jobs SET launch_state = 'launched', instance_id = ?2, \
                         updated_at = ?3 WHERE id = ?1 AND launch_state = 'launching'",
                    )
                    .bind(&[
                        int(*job_id).map_err(text)?,
                        instance_id.as_str().into(),
                        now.clone(),
                    ])
                    .map_err(text)?,
                Kind::Adopted,
            ));
        }
        for reset in &plan.reset {
            statements.push((
                self.db
                    .prepare(
                        "UPDATE jobs SET launch_state = NULL, runner_name = NULL, \
                         instance_id = NULL, launch_retry = NULL, next_launch_at = NULL, \
                         updated_at = ?3 \
                         WHERE id = ?1 AND status = 'queued' AND launch_state = ?2 \
                         AND runner_name IS ?4",
                    )
                    .bind(&[
                        int(reset.job_id).map_err(text)?,
                        reset.launch_state.as_str().into(),
                        now.clone(),
                        reset
                            .runner_name
                            .as_deref()
                            .map_or(JsValue::NULL, Into::into),
                    ])
                    .map_err(text)?,
                Kind::Reset,
            ));
        }
        for (job_id, runner) in &plan.give_up {
            let reason = format!(
                "runner {runner} waited {} minutes without GitHub handing it the job",
                self.limits.idle_timeout / 60_000
            );
            statements.push((
                self.db
                    .prepare(
                        "UPDATE jobs SET launch_state = 'failed', launch_error = ?3, \
                         launch_retry = NULL, next_launch_at = NULL, \
                         updated_at = ?2 WHERE id = ?1 AND status = 'queued' \
                         AND launch_state = 'launched' AND runner_name = ?4",
                    )
                    .bind(&[
                        int(*job_id).map_err(text)?,
                        now.clone(),
                        reason.as_str().into(),
                        runner.as_str().into(),
                    ])
                    .map_err(text)?,
                Kind::GaveUp,
            ));
        }
        if statements.is_empty() {
            return Ok(());
        }
        let (statements, kinds): (Vec<_>, Vec<_>) = statements.into_iter().unzip();
        let results = self.db.batch(statements).await.map_err(text)?;
        for (result, kind) in results.iter().zip(kinds) {
            if changes(result) != 1 {
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
        let rows = self
            .db
            .prepare(
                "SELECT id, installation_id, repository, run_id, label FROM jobs \
                 WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY queued_at, id",
            )
            .bind(&[json(&ids)?])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<LaunchRow>()
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
            .prepare(format!(
                "SELECT id, role_arn, region FROM tenants WHERE {VERIFIED} \
                 AND NOT EXISTS (SELECT 1 FROM instance_type_regions AS done \
                   WHERE done.region = tenants.region \
                   AND (done.refreshed_at >= ?1 OR done.attempted_at >= ?2)) \
                 ORDER BY COALESCE((SELECT refreshed_at FROM instance_type_regions AS done \
                   WHERE done.region = tenants.region), 0), RANDOM() LIMIT 1"
            ))
            .bind(&[
                int(self.now.saturating_sub(instance_types::FRESH_FOR)).map_err(text)?,
                int(self.now.saturating_sub(instance_types::RETRY_AFTER)).map_err(text)?,
            ])
            .map_err(text)?
            .first(None)
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
        let rows = self
            .db
            .prepare(format!(
                "SELECT id, installation_id, repository, run_id, label FROM jobs \
                 WHERE status = 'queued' AND launch_state IS NULL AND queued_at < ?1 \
                 AND installation_id NOT IN (SELECT installation_id FROM tenants \
                   WHERE installation_id IS NOT NULL AND {VERIFIED}) \
                 ORDER BY queued_at LIMIT ?2"
            ))
            .bind(&[int(before).map_err(text)?, UNCONNECTED_PER_PASS.into()])
            .map_err(text)?
            .all()
            .await
            .map_err(text)?
            .results::<LaunchRow>()
            .map_err(text)?;
        let mut done = Summary::default();
        for job in rows.into_iter().filter_map(queued_job) {
            launch::launch(self.env.clone(), job).await;
            done.unconnected += 1;
        }
        Ok(done)
    }

    /// Step 6: deletes expired sessions, jobs past the retention,
    /// redeliveries GitHub no longer lists and lapsed unavailable
    /// offerings.
    async fn retention(&self) -> Fallible<Summary> {
        let now = int(self.now).map_err(text)?;
        let jobs_before = int(self.now.saturating_sub(self.limits.retention)).map_err(text)?;
        let redeliveries_before = int(self.now.saturating_sub(REDELIVERY_MEMORY)).map_err(text)?;
        let results = self
            .db
            .batch(vec![
                self.db
                    .prepare(
                        "DELETE FROM sessions WHERE key IN \
                         (SELECT key FROM sessions WHERE expires_at < ?1 LIMIT ?2)",
                    )
                    .bind(&[now, DELETE_BATCH.into()])
                    .map_err(text)?,
                self.db
                    .prepare(
                        "DELETE FROM jobs WHERE id IN \
                         (SELECT id FROM jobs WHERE queued_at < ?1 ORDER BY queued_at LIMIT ?2)",
                    )
                    .bind(&[jobs_before, DELETE_BATCH.into()])
                    .map_err(text)?,
                self.db
                    .prepare(
                        "DELETE FROM redeliveries WHERE guid IN \
                         (SELECT guid FROM redeliveries WHERE redelivered_at < ?1 LIMIT ?2)",
                    )
                    .bind(&[redeliveries_before, DELETE_BATCH.into()])
                    .map_err(text)?,
                self.db
                    .prepare("DELETE FROM unavailable_offerings WHERE until < ?1")
                    .bind(&[int(self.now).map_err(text)?])
                    .map_err(text)?,
            ])
            .await
            .map_err(text)?;
        let deleted = |at: usize| {
            results.get(at).map_or(0, |result| {
                u32::try_from(changes(result)).unwrap_or(u32::MAX)
            })
        };
        if (0..3).any(|at| deleted(at) == DELETE_BATCH) {
            self.more.set(true);
        }
        Ok(Summary {
            sessions_deleted: deleted(0),
            jobs_deleted: deleted(1),
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
struct Guid {
    guid: String,
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

fn json<T: Serialize>(values: &T) -> Fallible<JsValue> {
    serde_json::to_string(values)
        .map(Into::into)
        .map_err(|err| err.to_string())
}

fn changes(result: &worker::D1Result) -> usize {
    result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or(0)
}

#[allow(clippy::needless_pass_by_value)]
fn text(err: worker::Error) -> String {
    err.to_string()
}
