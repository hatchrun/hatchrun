// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Launches a runner instance for a recorded job, after the webhook has
//! been answered.
//!
//! Only tenants with AWS connected and a licence (the beta or a paid
//! subscription) launch; jobs of any other are marked `failed`.
//!
//! The Worker takes the job by moving it from no launch state to
//! `launching`, which only one request can do, and only while the tenant
//! has fewer than `max_runners` runners launching or running. Then:
//!
//! 1. resolve the label's runner profile (see [`crate::profiles`]),
//!    pick its instance types in the tenant's region within what the
//!    connect stack allows, and the image for its architecture;
//! 2. sign an `oidc.hatchrun.io` token for the tenant and trade it for
//!    the tenant role's credentials;
//! 3. register a just-in-time runner with the job's label through the
//!    App installation;
//! 4. start the instance with a fresh broker ticket in its user data:
//!    through EC2 Fleet when the tenant's stack allows it, over every
//!    fitting type of the newest generation in every subnet, skipping
//!    offerings EC2 refused for capacity in the last few minutes and
//!    moving to older generations and then on-demand (see
//!    [`crate::offerings`]); otherwise with `RunInstances`, trying the
//!    subnets in order and then the profile's other instance types,
//!    Spot before on-demand for a Spot profile;
//! 5. store the runner's boot document in the broker under the ticket:
//!    its config, and a token for reporting its CPU and memory to
//!    hatchrun-telemetry (see [`crate::telemetry`]).
//!
//! The config is stored only once the instance exists, as the scaler
//! does. When a step after registration fails, the runner is removed and
//! a started instance terminated, and the job is marked `failed` with
//! what the reconciler does about it (see [`crate::reconcile::retry`]):
//! launch it again after a wait, or once an owner changes the setup,
//! which calls [`recheck`].

use std::fmt::Write;
use std::time::Duration;

use serde::Deserialize;
use worker::{D1Database, Env, Result, console_error, console_log, console_warn};

use crate::aws::{self, Credentials, ec2, sts};
use crate::fetch::send;
use crate::http::Request;
use crate::instance::{Choice, Limits};
use crate::labels::Arch;
use crate::offerings::{self, Candidate, Unavailable};
use crate::profiles::{self, store};
use crate::reconcile::retry::{Cause, Failure};
use crate::webhook::QueuedJob;
use crate::{
    DB, cache, connect, github, instance, instance_types, int, jwt, now_ms, oidc, settings,
    telemetry, webcrypto,
};

/// Variable with the App's client id.
pub(crate) const APP_ID: &str = "GITHUB_APP_ID";
/// Secret with the App's private key in PEM.
pub(crate) const APP_KEY: &str = "GITHUB_APP_PRIVATE_KEY";
/// Variable with the broker URL instances claim their config from.
const BROKER_URL: &str = "BROKER_URL";
/// Secret with the broker's token for storing configs.
const BROKER_TOKEN: &str = "BROKER_TOKEN";
/// Variable that replaces the GitHub API URL, for development.
pub(crate) const GITHUB_API_URL: &str = "GITHUB_API_URL";
/// Variable that replaces the STS and EC2 endpoints, for development.
pub(crate) const AWS_ENDPOINT_URL: &str = "AWS_ENDPOINT_URL";

/// How long a config waits for its instance. The agent gives up after
/// ten minutes.
const BROKER_EXPIRES_IN: u64 = 15 * 60;
/// Attempts at storing a config while the broker is busy.
const BROKER_ATTEMPTS: u32 = 3;

/// A tenant row.
#[derive(Debug, Deserialize)]
struct Tenant {
    id: String,
    role_arn: String,
    region: String,
    launch_template_id: String,
    subnet_ids: String,
    max_runners: u32,
    /// Regions the organization allows, comma separated, or none.
    allowed_regions: Option<String>,
    /// 1 when the tenant holds a licence (see [`LICENSED`]).
    licensed: u8,
    /// What its connect stack allows (see [`Limits::from_columns`]).
    instance_types: Option<String>,
    excluded_instance_types: Option<String>,
    volume_size_limit: Option<u16>,
    root_volume_size: Option<u16>,
    /// 1 when its stack lets the role use EC2 Fleet.
    fleet: u8,
    /// Its stack's cache bucket, version 5 on.
    cache_bucket: Option<String>,
}

/// SQL that is 1 for a tenant row that may launch: in the beta, or
/// with a Polar subscription that holds a licence.
pub(crate) const LICENSED: &str = "(tenants.beta = 1 OR EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')))";

/// The installation's tenant with AWS connected.
fn tenant_query() -> String {
    format!(
        "SELECT id, role_arn, region, launch_template_id, subnet_ids, max_runners, \
         allowed_regions, {LICENSED} AS licensed, instance_types, volume_size_limit, \
         root_volume_size, fleet, excluded_instance_types, cache_bucket \
         FROM tenants WHERE installation_id = ?1 AND role_arn IS NOT NULL \
         AND region IS NOT NULL AND launch_template_id IS NOT NULL \
         AND subnet_ids IS NOT NULL"
    )
}

/// Fails a job nobody took because its tenant cannot launch.
const REFUSE: &str = "UPDATE jobs SET launch_state = 'failed', launch_error = ?2, \
     updated_at = ?3, launch_retry = ?4, next_launch_at = NULL \
     WHERE id = ?1 AND launch_state IS NULL";

/// Records how a launch ended.
const RECORD: &str = "UPDATE jobs SET launch_state = ?2, instance_id = ?3, launch_error = ?4, \
     updated_at = ?5, launch_retry = ?6, next_launch_at = ?7 WHERE id = ?1";

/// Puts a tenant's jobs that wait for its setup back in line.
const RECHECK: &str = "UPDATE jobs SET launch_state = NULL, launch_retry = NULL, \
     next_launch_at = NULL, updated_at = ?2 \
     WHERE installation_id = (SELECT installation_id FROM tenants WHERE id = ?1) \
     AND status = 'queued' AND launch_state = 'failed' AND launch_retry = 'setup'";

/// Takes a queued job while the tenant has room, counting the attempt.
const CLAIM: &str = "UPDATE jobs SET launch_state = 'launching', runner_name = ?2, \
     updated_at = ?3, launch_attempts = launch_attempts + 1 \
     WHERE id = ?1 AND launch_state IS NULL AND status = 'queued' \
     AND (SELECT COUNT(*) FROM jobs \
          WHERE installation_id = ?4 AND status <> 'completed' \
          AND launch_state IN ('launching', 'launched')) < ?5 \
     RETURNING launch_attempts";

/// The runner image for a region and architecture.
const IMAGE: &str = "SELECT image_id, root_device FROM images WHERE region = ?1 AND arch = ?2";

/// A row of [`CLAIM`].
#[derive(Deserialize)]
struct Claimed {
    launch_attempts: u32,
}

impl Tenant {
    fn subnets(&self) -> Vec<&str> {
        self.subnet_ids
            .split(',')
            .map(str::trim)
            .filter(|subnet| !subnet.is_empty())
            .collect()
    }
}

/// An image row.
#[derive(Debug, Deserialize)]
struct Image {
    image_id: String,
    root_device: String,
}

/// Settings from the Worker's variables and secrets.
struct Settings {
    app_id: String,
    app_key: Vec<u8>,
    oidc_key: Vec<u8>,
    oidc_kid: String,
    broker_url: String,
    broker_token: String,
    telemetry_key: Vec<u8>,
    telemetry_url: String,
    /// The cache API; without it no runner gets the cache.
    cache_url: Option<String>,
    github_api: String,
    aws_endpoint: Option<String>,
    /// Where an account subscribes to the amd64 and arm64 images.
    marketplace_amd64: String,
    marketplace_arm64: String,
}

impl Settings {
    fn read(env: &Env) -> std::result::Result<Self, String> {
        let get = |name: &str| {
            env.secret(name)
                .map(|value| value.to_string())
                .ok()
                .filter(|value| !value.is_empty())
        };
        let need = |name: &str| get(name).ok_or_else(|| format!("{name} is not set"));
        let key = |name: &str| jwt::pkcs8_der(&need(name)?).map_err(|err| format!("{name}: {err}"));
        Ok(Self {
            app_id: need(APP_ID)?,
            app_key: key(APP_KEY)?,
            oidc_key: key(oidc::SIGNING_KEY)?,
            oidc_kid: need(oidc::KEY_ID)?,
            broker_url: need(BROKER_URL)?.trim_end_matches('/').to_owned(),
            broker_token: need(BROKER_TOKEN)?,
            telemetry_key: jwt::ed25519_pkcs8_der(&need(telemetry::SIGNING_KEY)?)
                .map_err(|err| format!("{}: {err}", telemetry::SIGNING_KEY))?,
            telemetry_url: need(telemetry::URL)?.trim_end_matches('/').to_owned(),
            cache_url: get(cache::URL).map(|url| url.trim_end_matches('/').to_owned()),
            github_api: get(GITHUB_API_URL)
                .unwrap_or_else(|| github::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            aws_endpoint: get(AWS_ENDPOINT_URL),
            marketplace_amd64: need(connect::MARKETPLACE_AMD64_URL)?,
            marketplace_arm64: need(connect::MARKETPLACE_ARM64_URL)?,
        })
    }

    /// Where an account subscribes to the `arch` image.
    fn marketplace_url(&self, arch: Arch) -> &str {
        match arch {
            Arch::Amd64 => &self.marketplace_amd64,
            Arch::Arm64 => &self.marketplace_arm64,
        }
    }
}

/// Launches a runner for `job` if nothing else has. Errors end up on the
/// job row and in the log; there is nobody to answer.
pub async fn launch(env: Env, job: QueuedJob) {
    if let Err(err) = try_launch(&env, &job).await {
        console_error!("job {}: {err}", job.id);
    }
}

async fn try_launch(env: &Env, job: &QueuedJob) -> Result<()> {
    let db = env.d1(DB)?;
    let tenant: Option<Tenant> = db
        .prepare(tenant_query())
        .bind(&[int(job.installation_id)?])?
        .first(None)
        .await?;
    let tenant = match tenant {
        Some(tenant)
            if tenant.licensed == 1
                && settings::region_allowed(&tenant.region, tenant.allowed_regions.as_deref()) =>
        {
            tenant
        }
        other => {
            let message = match other {
                Some(tenant) if tenant.licensed == 1 => format!(
                    "tenant {} does not allow runners in {}",
                    tenant.id, tenant.region
                ),
                Some(tenant) => format!(
                    "tenant {} has no licence: neither in the beta nor subscribed",
                    tenant.id
                ),
                None => format!(
                    "installation {} has no tenant with AWS connected",
                    job.installation_id
                ),
            };
            let (wait, _) = Failure::new(Cause::Tenant, message.as_str()).outcome(0, now_ms());
            db.prepare(REFUSE)
                .bind(&[
                    int(job.id)?,
                    message.as_str().into(),
                    int(now_ms())?,
                    wait.as_str().into(),
                ])?
                .run()
                .await?;
            console_warn!("job {}: {message}", job.id);
            return Ok(());
        }
    };

    let name = runner_name(job.id, &webcrypto::random_bytes(4)?);
    let Some(attempts) = claim(&db, job, &tenant, &name).await? else {
        console_log!(
            "job {}: not launching, already taken or tenant {} at {} runners",
            job.id,
            tenant.id,
            tenant.max_runners
        );
        return Ok(());
    };

    let launched = launch_claimed(env, &db, job, &tenant, &name).await;
    record(&db, job.id, &name, attempts, launched).await
}

/// Records how the job's `attempts`th launch, of runner `name`, ended.
async fn record(
    db: &D1Database,
    job_id: u64,
    name: &str,
    attempts: u32,
    launched: std::result::Result<String, Failure>,
) -> Result<()> {
    let now = now_ms();
    let (state, instance_id, error, wait, next) = match launched {
        Ok(instance_id) => {
            console_log!("job {job_id}: runner {name} on {instance_id}");
            ("launched", Some(instance_id), None, None, None)
        }
        Err(failure) => {
            let (wait, next) = failure.outcome(attempts, now);
            match next {
                Some(at) => console_error!(
                    "job {job_id}: launch {attempts} failed, trying again in {} s: {failure}",
                    at.saturating_sub(now) / 1000
                ),
                None => console_error!(
                    "job {job_id}: launch {attempts} failed, waiting for the setup to change: \
                     {failure}"
                ),
            }
            ("failed", None, Some(failure.message), Some(wait), next)
        }
    };
    db.prepare(RECORD)
        .bind(&[
            int(job_id)?,
            state.into(),
            nullable(instance_id),
            nullable(error),
            int(now)?,
            nullable(wait.map(|wait| wait.as_str().to_owned())),
            next.map_or(Ok(wasm_bindgen::JsValue::NULL), int)?,
        ])?
        .run()
        .await?;
    Ok(())
}

/// Puts the tenant's queued jobs that wait for a change to its setup
/// back in line, for the reconciler to launch within a minute. Called
/// when an owner verifies the role, edits a profile, changes the
/// settings or gains a licence.
pub(crate) async fn recheck(db: &D1Database, tenant_id: &str) -> Result<()> {
    let result = db
        .prepare(RECHECK)
        .bind(&[tenant_id.into(), int(now_ms())?])?
        .run()
        .await?;
    let changed = result.meta()?.and_then(|meta| meta.changes).unwrap_or(0);
    if changed > 0 {
        console_log!("tenant {tenant_id}: setup changed, launching {changed} waiting jobs again");
    }
    Ok(())
}

/// The runner's name: the job id and a random suffix, since a name
/// GitHub still holds from an earlier attempt cannot be registered again.
fn runner_name(job_id: u64, random: &[u8]) -> String {
    let suffix = random.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    });
    format!("hatchrun-{job_id}-{suffix}")
}

/// Moves the job to `launching` if it is queued, nobody took it, and the
/// tenant has room, and counts the attempt. D1 runs one statement at a
/// time, so two deliveries of one job cannot both take it. Returns the
/// job's launch attempts so far, this one included, or `None` when it
/// was not taken.
async fn claim(
    db: &D1Database,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
) -> Result<Option<u32>> {
    let claimed: Option<Claimed> = db
        .prepare(CLAIM)
        .bind(&[
            int(job.id)?,
            name.into(),
            int(now_ms())?,
            int(job.installation_id)?,
            tenant.max_runners.into(),
        ])?
        .first(None)
        .await?;
    Ok(claimed.map(|claimed| claimed.launch_attempts))
}

/// Everything after the claim. Returns the instance id.
async fn launch_claimed(
    env: &Env,
    db: &D1Database,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
) -> std::result::Result<String, Failure> {
    let custom = store::named(db, &tenant.id, &job.spec.profile)
        .await
        .map_err(internal)?;
    let profile =
        profiles::resolve(&job.spec, custom).map_err(|err| Failure::new(Cause::NoProfile, err))?;
    let limits = Limits::from_columns(
        tenant.instance_types.as_deref(),
        tenant.excluded_instance_types.as_deref(),
        tenant.volume_size_limit,
        tenant.root_volume_size,
    );
    let offered = instance_types::store::offered(db, &tenant.region)
        .await
        .map_err(internal)?;
    let choice = instance::choose(&profile, &tenant.region, &offered, &limits)
        .map_err(|err| Failure::new(Cause::NoInstanceType, err))?;
    let settings = Settings::read(env).map_err(|err| Failure::new(Cause::Settings, err))?;
    let arch = choice.arch.as_str();
    let image: Image = db
        .prepare(IMAGE)
        .bind(&[tenant.region.as_str().into(), arch.into()])
        .map_err(internal)?
        .first(None)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            Failure::new(
                Cause::NoImage,
                format!("no {arch} image in {}", tenant.region),
            )
        })?;

    let credentials = assume_role(&settings, job, tenant).await?;
    let org = github::owner(&job.repository);
    let token = installation_token(&settings, job.installation_id).await?;
    let config = github::jit_config(
        &github_send(
            &github::generate_jit_config(&settings.github_api, org, &token, name, &job.label),
            "register runner",
        )
        .await?,
    )
    .map_err(|err| Failure::new(Cause::Internal, err))?;

    let result = start(
        &settings,
        db,
        job,
        tenant,
        name,
        &choice,
        &image,
        &credentials,
        &config.encoded_jit_config,
        profile.extras.iter().any(|extra| extra == cache::EXTRA),
    )
    .await;
    if result.is_err() {
        let removed = send(&github::delete_runner(
            &settings.github_api,
            org,
            &token,
            config.runner.id,
        ))
        .await;
        match removed {
            Ok(response) if response.ok() || response.status == 404 => {}
            Ok(response) => console_warn!(
                "remove runner {name}: GitHub answered {}: {}",
                response.status,
                response.text()
            ),
            Err(err) => console_warn!("remove runner {name}: {err}"),
        }
    }
    result
}

/// Starts the instance and hands its boot document to the broker.
#[allow(clippy::too_many_arguments)]
async fn start(
    settings: &Settings,
    db: &D1Database,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
    choice: &Choice,
    image: &Image,
    credentials: &Credentials,
    config: &str,
    wants_cache: bool,
) -> std::result::Result<String, Failure> {
    let ticket = jwt::b64url(webcrypto::random_bytes(32).map_err(internal)?);
    let token = telemetry_token(settings, job, tenant, name).await?;
    let cache_token = match (&settings.cache_url, &tenant.cache_bucket) {
        (Some(_), Some(_)) if wants_cache => Some(cache_token(settings, job, tenant).await?),
        _ => None,
    };
    let cache = settings
        .cache_url
        .as_deref()
        .zip(cache_token.as_deref())
        .map(|(url, token)| telemetry::Cache { url, token });
    let boot = telemetry::boot_document(config, &settings.telemetry_url, &token, cache);
    let launch = Launch {
        job_id: job.id.to_string(),
        tenant,
        name,
        choice,
        image,
        credentials,
        endpoint: aws::endpoint("ec2", &tenant.region, settings.aws_endpoint.as_deref()),
        user_data: serde_json::json!({ "broker": settings.broker_url, "ticket": ticket })
            .to_string(),
    };
    let started = if tenant.fleet == 1 {
        fleet(db, &launch).await
    } else {
        run_instances(&launch).await
    };
    let instance_id = started.map_err(|failure| match &failure.cause {
        Cause::Aws(code) if code == connect::NOT_SUBSCRIBED => Failure::new(
            failure.cause.clone(),
            connect::subscribe_message(choice.arch.as_str(), settings.marketplace_url(choice.arch)),
        ),
        _ => failure,
    })?;
    if let Err(err) = store_config(settings, &ticket, &boot).await {
        // Without its config the instance would wait out the agent's
        // timeout before powering off.
        terminate(&launch.endpoint, credentials, &tenant.region, &instance_id).await;
        return Err(err);
    }
    Ok(instance_id)
}

/// What starting one runner's instance takes.
struct Launch<'a> {
    job_id: String,
    tenant: &'a Tenant,
    name: &'a str,
    choice: &'a Choice,
    image: &'a Image,
    credentials: &'a Credentials,
    endpoint: String,
    user_data: String,
}

impl Launch<'_> {
    fn tags(&self) -> [(&str, &str); 4] {
        [
            (ec2::TAG_TENANT, self.tenant.id.as_str()),
            (ec2::TAG_JOB_ID, self.job_id.as_str()),
            (ec2::TAG_RUNNER_NAME, self.name),
            ("Name", self.name),
        ]
    }

    /// The tenant's subnets in order, or the template's network.
    fn subnets(&self) -> Vec<Option<&str>> {
        let subnets: Vec<Option<&str>> = self.tenant.subnets().into_iter().map(Some).collect();
        if subnets.is_empty() {
            vec![None]
        } else {
            subnets
        }
    }

    /// The markets to try in order, `true` for Spot.
    fn markets(&self) -> &'static [bool] {
        self.choice.capacity.markets()
    }

    fn root_volume(&self) -> Option<(&str, u16)> {
        self.choice
            .root_volume
            .map(|size| (self.image.root_device.as_str(), size))
    }

    /// Signs and sends an EC2 request.
    async fn send(
        &self,
        mut request: Request,
    ) -> std::result::Result<crate::http::Response, Failure> {
        aws::sign_v4(
            &mut request,
            self.credentials,
            &self.tenant.region,
            "ec2",
            now_ms(),
        )
        .map_err(|err| Failure::new(Cause::Internal, err))?;
        send(&request)
            .await
            .map_err(|err| Failure::new(Cause::Unreachable, err))
    }
}

/// Starts the instance with `RunInstances`, for a stack that does not
/// allow EC2 Fleet: each instance type in each subnet in turn, Spot
/// before on-demand for a Spot profile, while EC2 has no capacity.
async fn run_instances(launch: &Launch<'_>) -> std::result::Result<String, Failure> {
    let subnets = launch.subnets();
    let tags = launch.tags();
    let subnets = &subnets;
    let attempts = launch.markets().iter().flat_map(|spot| {
        launch
            .choice
            .instance_types
            .iter()
            .flat_map(move |instance_type| {
                subnets
                    .iter()
                    .map(move |subnet| (*spot, instance_type.as_str(), *subnet))
            })
    });
    let mut last_error = None;
    for (index, (spot, instance_type, subnet)) in attempts.enumerate() {
        let client_token = format!("{}-{index}", launch.name);
        let request = ec2::RunInstances {
            launch_template_id: &launch.tenant.launch_template_id,
            image_id: &launch.image.image_id,
            instance_type,
            subnet_id: subnet,
            user_data: &launch.user_data,
            client_token: &client_token,
            tags: &tags,
            spot,
            root_volume: launch.root_volume(),
        }
        .request(&launch.endpoint);
        match ec2::instance_id(&launch.send(request).await?) {
            Ok(id) => return Ok(id),
            Err(err) if ec2::CAPACITY_ERRORS.contains(&err.code.as_str()) => {
                console_warn!(
                    "job {}: {instance_type}{} in {}: {err}",
                    launch.job_id,
                    if spot { " Spot" } else { "" },
                    subnet.unwrap_or("template network")
                );
                last_error = Some(err);
            }
            Err(err) => {
                return Err(Failure::new(
                    Cause::Aws(err.code.clone()),
                    format!("RunInstances {instance_type}: {err}"),
                ));
            }
        }
    }
    Err(match last_error {
        Some(err) => Failure::new(
            Cause::NoCapacity,
            format!(
                "no capacity for {} in any subnet: {err}",
                launch.choice.instance_types.join(", ")
            ),
        ),
        None => Failure::new(Cause::Internal, "no subnet to launch in"),
    })
}

/// The offerings a fleet refused for capacity, to skip for a while, or
/// the failure when it refused one for anything else. `last` keeps the
/// latest reason for the job's error.
fn refusals(
    launch: &Launch<'_>,
    spot: bool,
    refused: Vec<aws::fleet::Refused>,
    last: &mut Option<String>,
) -> std::result::Result<Vec<(Unavailable, String)>, Failure> {
    let until = now_ms() + offerings::UNAVAILABLE_FOR;
    let mut skipped = Vec::new();
    for no in refused {
        if !offerings::is_capacity(&no.code) {
            return Err(Failure::new(
                Cause::Aws(no.code.clone()),
                format!(
                    "CreateFleet {}: {}: {}",
                    no.instance_type, no.code, no.message
                ),
            ));
        }
        console_warn!(
            "job {}: {}{} in {}: {}: {}",
            launch.job_id,
            no.instance_type,
            if spot { " Spot" } else { "" },
            no.subnet_id.as_deref().unwrap_or("template network"),
            no.code,
            no.message
        );
        *last = Some(format!("{}: {}", no.code, no.message));
        skipped.push((
            Unavailable {
                instance_type: no.instance_type,
                subnet_id: no.subnet_id,
                spot,
                until,
            },
            no.code,
        ));
    }
    if skipped.is_empty() {
        return Err(Failure::new(
            Cause::NoCapacity,
            "CreateFleet started no instance and gave no reason",
        ));
    }
    Ok(skipped)
}

/// Fleets one launch may create: a generation or two on Spot, then on
/// on-demand.
const FLEETS_PER_LAUNCH: u32 = 4;

/// Starts the instance with EC2 Fleet from a launch template version
/// made for it, which is deleted after (see [`crate::aws::fleet`] and
/// [`crate::offerings`]).
async fn fleet(db: &D1Database, launch: &Launch<'_>) -> std::result::Result<String, Failure> {
    let client_token = format!("{}-v", launch.name);
    let created = launch
        .send(
            aws::fleet::TemplateVersion {
                launch_template_id: &launch.tenant.launch_template_id,
                runner_name: launch.name,
                image_id: &launch.image.image_id,
                user_data: &launch.user_data,
                root_volume: launch.root_volume(),
                tags: &launch.tags(),
                client_token: &client_token,
            }
            .request(&launch.endpoint),
        )
        .await?;
    let version = aws::fleet::version_number(&created).map_err(|err| {
        Failure::new(
            Cause::Aws(err.code.clone()),
            format!("CreateLaunchTemplateVersion: {err}"),
        )
    })?;
    let result = fleets(db, launch, version).await;
    let deleted = launch
        .send(aws::fleet::delete_version(
            &launch.endpoint,
            &launch.tenant.launch_template_id,
            version,
        ))
        .await;
    match deleted {
        Ok(response) if response.ok() => {}
        Ok(response) => console_warn!(
            "delete launch template version {version}: EC2 answered {}: {}",
            response.status,
            response.text()
        ),
        Err(err) => console_warn!("delete launch template version {version}: {err}"),
    }
    result
}

/// Creates fleets from template `version` until one starts the
/// instance, remembering the offerings EC2 refuses for capacity.
async fn fleets(
    db: &D1Database,
    launch: &Launch<'_>,
    version: u64,
) -> std::result::Result<String, Failure> {
    let tenant = &launch.tenant.id;
    let mut unavailable = offerings::store::current(db, tenant, now_ms())
        .await
        .map_err(internal)?;
    let candidates: Vec<Candidate> = launch
        .choice
        .instance_types
        .iter()
        .map(|name| Candidate {
            generation: instance_types::split(name)
                .and_then(|parts| parts.generation)
                .unwrap_or(0),
            instance_type: name.clone(),
        })
        .collect();
    let subnets = launch.subnets();
    let mut created = 0;
    let mut last = None;
    for &spot in launch.markets() {
        while created < FLEETS_PER_LAUNCH {
            let chosen = offerings::overrides(&candidates, &subnets, spot, &unavailable, now_ms());
            if chosen.is_empty() {
                break;
            }
            created += 1;
            let client_token = format!("{}-f{created}", launch.name);
            let request = aws::fleet::Fleet {
                launch_template_id: &launch.tenant.launch_template_id,
                version,
                overrides: &chosen,
                spot,
                client_token: &client_token,
            }
            .request(&launch.endpoint);
            let done = aws::fleet::launched(&launch.send(request).await?).map_err(|err| {
                Failure::new(Cause::Aws(err.code.clone()), format!("CreateFleet: {err}"))
            })?;
            if let Some((id, instance_type, subnet)) = done.instance {
                console_log!(
                    "job {}: {instance_type}{} in {}",
                    launch.job_id,
                    if spot { " Spot" } else { "" },
                    subnet.as_deref().unwrap_or("template network")
                );
                return Ok(id);
            }
            let refused = refusals(launch, spot, done.refused, &mut last)?;
            offerings::store::remember(db, tenant, &refused)
                .await
                .map_err(internal)?;
            unavailable.extend(refused.into_iter().map(|(gone, _)| gone));
        }
    }
    let back = offerings::back_at(
        &candidates,
        &subnets,
        launch.markets(),
        &unavailable,
        now_ms(),
    );
    Err(Failure::new(
        Cause::NoCapacity,
        format!(
            "no capacity for {} in any subnet{}",
            launch.choice.instance_types.join(", "),
            last.map(|last| format!(": {last}")).unwrap_or_default()
        ),
    )
    .not_before(back))
}

/// Terminates an instance that will not run, logging a failure.
async fn terminate(endpoint: &str, credentials: &Credentials, region: &str, instance_id: &str) {
    let mut request = ec2::terminate_instances(endpoint, instance_id);
    let terminated = match aws::sign_v4(&mut request, credentials, region, "ec2", now_ms()) {
        Ok(()) => match send(&request).await {
            Ok(response) => ec2::terminated(&response).map_err(|err| err.to_string()),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    };
    if let Err(err) = terminated {
        console_warn!("terminate {instance_id}: {err}");
    }
}

async fn assume_role(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
) -> std::result::Result<Credentials, Failure> {
    tenant_credentials(
        &settings.oidc_key,
        &settings.oidc_kid,
        settings.aws_endpoint.as_deref(),
        &tenant.id,
        &tenant.role_arn,
        &tenant.region,
        &format!("hatchrun-job-{}", job.id),
    )
    .await
    .map_err(|err| {
        Failure::new(
            Cause::Aws(err.code.clone()),
            format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn),
        )
    })
}

/// Credentials of `role_arn` in `region`, for a token that names
/// `tenant`. `session` shows in the customer's `CloudTrail`.
///
/// # Errors
///
/// STS's error, or a signing failure as an [`aws::Error`] with code
/// `SigningFailed`.
pub(crate) async fn tenant_credentials(
    oidc_key: &[u8],
    oidc_kid: &str,
    aws_endpoint: Option<&str>,
    tenant: &str,
    role_arn: &str,
    region: &str,
    session: &str,
) -> std::result::Result<Credentials, aws::Error> {
    let input = oidc::signing_input(oidc_kid, tenant, now_ms() / 1000);
    let signature = webcrypto::sign_rs256(oidc_key, input.as_bytes())
        .await
        .map_err(|err| aws::Error {
            status: 0,
            code: "SigningFailed".to_owned(),
            message: format!("sign tenant token: {err}"),
        })?;
    let request = sts::assume_role_with_web_identity(
        &aws::endpoint("sts", region, aws_endpoint),
        role_arn,
        session,
        &jwt::token(&input, &signature),
    );
    let response = send(&request).await.map_err(|err| aws::Error {
        status: 0,
        code: "Unreachable".to_owned(),
        message: err,
    })?;
    sts::credentials(&response)
}

/// The GitHub API and a token of installation `installation_id`.
pub(crate) async fn github_token(
    env: &Env,
    installation_id: u64,
) -> std::result::Result<(String, String), String> {
    let settings = Settings::read(env)?;
    let token = installation_token(&settings, installation_id)
        .await
        .map_err(|err| err.to_string())?;
    Ok((settings.github_api, token))
}

async fn installation_token(
    settings: &Settings,
    installation_id: u64,
) -> std::result::Result<String, Failure> {
    let app_jwt = app_jwt(&settings.app_id, &settings.app_key)
        .await
        .map_err(|err| Failure::new(Cause::Settings, err))?;
    let request = github::installation_token(&settings.github_api, installation_id, &app_jwt);
    github::token(&github_send(&request, "installation token").await?)
        .map_err(|err| Failure::new(Cause::Internal, err))
}

/// Sends a request to GitHub; an error answer fails on its status.
async fn github_send(
    request: &Request,
    what: &str,
) -> std::result::Result<crate::http::Response, Failure> {
    let response = send(request)
        .await
        .map_err(|err| Failure::new(Cause::Unreachable, err))?;
    if !response.ok() {
        return Err(Failure::new(
            Cause::GitHub(response.status),
            format!(
                "{what}: GitHub answered {}: {}",
                response.status,
                response.text()
            ),
        ));
    }
    Ok(response)
}

/// The runner's token for hatchrun-telemetry.
async fn telemetry_token(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
) -> std::result::Result<String, Failure> {
    let runner = telemetry::Runner {
        tenant: &tenant.id,
        installation_id: job.installation_id,
        job_id: job.id,
        runner_name: name,
    };
    let input = telemetry::signing_input(runner, now_ms() / 1000);
    let signature = webcrypto::sign_ed25519(&settings.telemetry_key, input.as_bytes())
        .await
        .map_err(|err| Failure::new(Cause::Settings, format!("sign telemetry token: {err}")))?;
    Ok(jwt::token(&input, &signature))
}

/// The runner's token for the job cache, signed by the telemetry key
/// with its own audience.
async fn cache_token(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
) -> std::result::Result<String, Failure> {
    let job = cache::Job {
        tenant: &tenant.id,
        installation_id: job.installation_id,
        job_id: job.id,
    };
    let input = cache::signing_input(job, now_ms() / 1000);
    let signature = webcrypto::sign_ed25519(&settings.telemetry_key, input.as_bytes())
        .await
        .map_err(|err| Failure::new(Cause::Settings, format!("sign cache token: {err}")))?;
    Ok(jwt::token(&input, &signature))
}

/// The App's JWT, signed with its private key in PKCS#8 DER.
pub(crate) async fn app_jwt(app_id: &str, pkcs8: &[u8]) -> std::result::Result<String, String> {
    let input = github::app_jwt_signing_input(app_id, now_ms() / 1000);
    let signature = webcrypto::sign_rs256(pkcs8, input.as_bytes())
        .await
        .map_err(|err| format!("sign App JWT: {err}"))?;
    Ok(jwt::token(&input, &signature))
}

/// Stores the boot document under the ticket, retrying while the broker
/// is busy.
async fn store_config(
    settings: &Settings,
    ticket: &str,
    boot: &str,
) -> std::result::Result<(), Failure> {
    let request = Request::new(
        "PUT",
        format!("{}/v1/tickets/{ticket}", settings.broker_url),
    )
    .header("Authorization", format!("Bearer {}", settings.broker_token))
    .header("Content-Type", "application/json")
    .body(serde_json::json!({ "config": boot, "expiresIn": BROKER_EXPIRES_IN }).to_string());
    let mut last = String::new();
    for attempt in 1..=BROKER_ATTEMPTS {
        match send(&request).await {
            Ok(response) if response.ok() => return Ok(()),
            Ok(response) if response.status >= 500 || response.status == 429 => {
                last = format!("broker answered {}", response.status);
            }
            Ok(response) => {
                return Err(Failure::new(
                    Cause::Broker,
                    format!(
                        "store runner config: broker answered {}: {}",
                        response.status,
                        response.text()
                    ),
                ));
            }
            Err(err) => last = err,
        }
        if attempt < BROKER_ATTEMPTS {
            worker::Delay::from(Duration::from_secs(u64::from(attempt))).await;
        }
    }
    Err(Failure::new(
        Cause::Broker,
        format!("store runner config: {last}"),
    ))
}

/// D1 takes `null` for SQL NULL and refuses `undefined`, which is what
/// `Option::None` becomes.
fn nullable(value: Option<String>) -> wasm_bindgen::JsValue {
    value.map_or(wasm_bindgen::JsValue::NULL, Into::into)
}

#[allow(clippy::needless_pass_by_value)]
fn internal(err: worker::Error) -> Failure {
    Failure::new(Cause::Internal, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;
    const INSTALLATION: u64 = 42;

    /// A connected tenant, in the beta or not, as the app leaves it.
    fn seed(beta: bool, max_runners: u32) -> Db {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             launch_template_id, subnet_ids, max_runners, beta, volume_size_limit, \
             root_volume_size, fleet, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
            &params![
                "t1",
                9u64,
                "acme",
                INSTALLATION,
                "arn:aws:iam::1:role/r",
                "ap-south-1",
                "lt-1",
                "subnet-a,subnet-b",
                max_runners,
                u32::from(beta),
                200u32,
                30u32,
                1u32,
                1u64
            ],
        );
        db
    }

    fn job(db: &Db, id: u64, status: &str) {
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            &params![id, INSTALLATION, "acme/app", 77u64, "hatchrun", status, NOW],
        );
    }

    fn claim(db: &Db, id: u64, max_runners: u32) -> Option<u32> {
        db.first::<Claimed>(
            CLAIM,
            &params![
                id,
                format!("hatchrun-{id}-00"),
                NOW,
                INSTALLATION,
                max_runners
            ],
        )
        .map(|claimed| claimed.launch_attempts)
    }

    #[test]
    fn a_connected_tenant_reads() {
        let db = seed(false, 2);
        let tenant: Tenant = db.first(&tenant_query(), &params![INSTALLATION]).unwrap();
        assert_eq!(tenant.id, "t1");
        assert_eq!(tenant.max_runners, 2);
        assert_eq!(tenant.licensed, 0);
        assert_eq!(tenant.volume_size_limit, Some(200));
        assert_eq!(tenant.root_volume_size, Some(30));
        assert_eq!(tenant.fleet, 1);
        assert!(tenant.instance_types.is_none());

        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             cancel_at_period_end, modified_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            &params!["s1", "t1", "active", "c1", "p1", 0u32, "2026-10-01", NOW],
        );
        let tenant: Tenant = db.first(&tenant_query(), &params![INSTALLATION]).unwrap();
        assert_eq!(tenant.licensed, 1);
        assert!(
            db.first::<Tenant>(&tenant_query(), &params![INSTALLATION + 1])
                .is_none()
        );
    }

    #[test]
    fn a_claim_counts_runners_through_the_open_jobs() {
        let db = seed(true, 1);
        let plan = db.plan(
            CLAIM,
            &params![1u64, "hatchrun-1-00", NOW, INSTALLATION, 1u32],
        );
        assert!(plan.contains("INDEX jobs_open"), "{plan}");
        assert!(!plan.contains("SCAN jobs"), "{plan}");
    }

    #[test]
    fn claims_stop_at_max_runners() {
        let db = seed(true, 1);
        job(&db, 1, "queued");
        job(&db, 2, "queued");
        assert_eq!(claim(&db, 1, 1), Some(1));
        // Taken already, and the tenant is full.
        assert_eq!(claim(&db, 1, 1), None);
        assert_eq!(claim(&db, 2, 1), None);

        db.run(
            RECORD,
            &params![
                1u64,
                "failed",
                None::<String>,
                Some("no capacity".to_owned()),
                NOW,
                Some("backoff".to_owned()),
                Some(NOW + 30_000)
            ],
        );
        assert_eq!(claim(&db, 2, 1), Some(1));
        db.run(
            RECORD,
            &params![
                2u64,
                "launched",
                Some("i-1".to_owned()),
                None::<String>,
                NOW,
                None::<String>,
                None::<u64>
            ],
        );
        let rows = db
            .rows(
                "SELECT launch_state, instance_id, next_launch_at FROM jobs ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["launch_state"], "failed");
        assert_eq!(rows[0]["next_launch_at"], NOW + 30_000);
        assert_eq!(rows[1]["launch_state"], "launched");
        assert_eq!(rows[1]["instance_id"], "i-1");

        // The retry takes the job again and counts a second attempt.
        db.run("UPDATE jobs SET launch_state = NULL WHERE id = 1", &[]);
        assert_eq!(claim(&db, 1, 2), Some(2));
    }

    #[test]
    fn refused_jobs_wait_for_the_setup() {
        let db = seed(false, 1);
        job(&db, 1, "queued");
        job(&db, 2, "completed");
        let refuse = |id: u64| {
            db.run(
                REFUSE,
                &params![id, "tenant t1 has no licence", NOW, "setup"],
            )
        };
        assert_eq!(refuse(1), 1);
        assert_eq!(refuse(1), 0);
        assert_eq!(refuse(2), 1);
        // Only the queued one launches again.
        assert_eq!(db.run(RECHECK, &params!["t1", NOW + 1]), 1);
        let rows = db
            .rows(
                "SELECT launch_state, launch_retry FROM jobs ORDER BY id",
                &[],
            )
            .unwrap();
        assert!(rows[0]["launch_state"].is_null() && rows[0]["launch_retry"].is_null());
        assert_eq!(rows[1]["launch_state"], "failed");
        assert_eq!(claim(&db, 1, 1), Some(1));
    }

    #[test]
    fn images_by_region_and_arch() {
        let db = Db::new();
        db.run(
            "INSERT INTO images (region, arch, image_id, root_device, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &params!["ap-south-1", "arm64", "ami-1", "/dev/sda1", NOW],
        );
        let image: Image = db.first(IMAGE, &params!["ap-south-1", "arm64"]).unwrap();
        assert_eq!(image.image_id, "ami-1");
        assert_eq!(image.root_device, "/dev/sda1");
        assert!(
            db.first::<Image>(IMAGE, &params!["ap-south-1", "amd64"])
                .is_none()
        );
    }

    #[test]
    fn runner_names_are_unique_per_attempt() {
        assert_eq!(
            runner_name(7, &[0xab, 0x01, 0, 0xff]),
            "hatchrun-7-ab0100ff"
        );
    }

    #[test]
    fn licences_follow_polar() {
        for status in crate::polar::LICENSED {
            assert!(LICENSED.contains(&format!("'{status}'")), "{status}");
        }
    }

    #[test]
    fn subnets_in_order() {
        let tenant = Tenant {
            id: "acme1234".to_owned(),
            role_arn: String::new(),
            region: String::new(),
            launch_template_id: String::new(),
            subnet_ids: "subnet-b, subnet-a,,".to_owned(),
            max_runners: 1,
            allowed_regions: None,
            licensed: 1,
            instance_types: None,
            excluded_instance_types: None,
            volume_size_limit: None,
            root_volume_size: None,
            fleet: 0,
            cache_bucket: None,
        };
        assert_eq!(tenant.subnets(), ["subnet-b", "subnet-a"]);
    }
}
