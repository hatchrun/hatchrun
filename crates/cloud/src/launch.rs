// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Launches a runner instance for a recorded job, from the launch queue
//! the webhook sends it to (see [`crate::launches`]).
//!
//! Only tenants with AWS connected that may launch (in the beta, paid,
//! or within their free minutes, see [`crate::usage`]) launch; jobs of
//! any other are marked `failed`.
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
//!    App installation, with its token kept in KV (see
//!    [`crate::tokens`]);
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
use worker::{Env, Result, console_error, console_log, console_warn};

use crate::aws::{self, Credentials, ec2};
use crate::db::{Db, params};
use crate::fetch::{Counted, send};
use crate::http::Request;
use crate::instance::{Choice, Limits};
use crate::labels::Arch;
use crate::profiles::{self, store};
use crate::start::{
    Cause, Failure, Instance, Target, role_credentials, start_instance, terminate_instance,
};
use crate::tokens::{self, App, Kv, Tokens};
use crate::webhook::QueuedJob;
use crate::{
    cache, connect, github, instance, instance_types, jobs, jwt, now_ms, oidc, settings, telemetry,
    webcrypto,
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
    /// Whether the tenant holds a licence (see [`LICENSED`]).
    licensed: bool,
    /// What its connect stack allows (see [`Limits::from_columns`]).
    instance_types: Option<String>,
    excluded_instance_types: Option<String>,
    volume_size_limit: Option<u16>,
    root_volume_size: Option<u16>,
    /// Whether its stack lets the role use EC2 Fleet.
    fleet: bool,
    /// Its stack's cache bucket, version 5 on.
    cache_bucket: Option<String>,
}

/// SQL that is true for a tenant row that may launch (see
/// [`crate::usage::MAY_LAUNCH`]).
pub(crate) const LICENSED: &str = crate::usage::MAY_LAUNCH;

/// The installation's tenant with AWS connected.
pub(crate) fn tenant_query() -> String {
    format!(
        "SELECT id, role_arn, region, launch_template_id, subnet_ids, max_runners, \
         allowed_regions, {LICENSED} AS licensed, instance_types, volume_size_limit, \
         root_volume_size, fleet, excluded_instance_types, cache_bucket \
         FROM tenants WHERE installation_id = $1 AND role_arn IS NOT NULL \
         AND region IS NOT NULL AND launch_template_id IS NOT NULL \
         AND subnet_ids IS NOT NULL"
    )
}

/// The runner image for a region and architecture.
pub(crate) const IMAGE: &str =
    "SELECT image_id, root_device FROM images WHERE region = $1 AND arch = $2";

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

/// Launches a runner for `job` if nothing else has. Returns once the
/// outcome is decided: launched, refused, taken by another launch, or
/// failed and recorded on the job row. An error means the launch could
/// not get that far, such as the database not answering before the claim, and is
/// safe to try again: the claim keeps a second try from launching twice.
pub(crate) async fn try_launch(env: &Env, job: &QueuedJob) -> Result<()> {
    let db = Db::connect(env).await?;
    let tenant: Option<Tenant> = db
        .first(&tenant_query(), &params![job.installation_id])
        .await?;
    let tenant = match tenant {
        Some(tenant)
            if tenant.licensed
                && settings::region_allowed(&tenant.region, tenant.allowed_regions.as_deref()) =>
        {
            tenant
        }
        other => {
            let message = match other {
                Some(tenant) if tenant.licensed => format!(
                    "tenant {} does not allow runners in {}",
                    tenant.id, tenant.region
                ),
                Some(tenant) => format!(
                    "tenant {} used its {} free minutes this month and the grace period \
                     after them: add a card in the app to keep launching",
                    tenant.id,
                    crate::usage::FREE_MINUTES
                ),
                None => format!(
                    "installation {} has no tenant with AWS connected",
                    job.installation_id
                ),
            };
            let (wait, _) = Failure::new(Cause::Tenant, message.as_str()).outcome(0, now_ms());
            jobs::refuse(&db, job.id, message.as_str(), now_ms(), wait.as_str()).await?;
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

    let calls = Counted::default();
    let launched = launch_claimed(env, &db, job, &tenant, &name, &calls).await;
    crate::alerts::note_rate_limits(&db, &calls, now_ms()).await;
    record(&db, job.id, &name, attempts, launched, calls.sent()).await
}

/// Records how the job's `attempts`th launch, of runner `name`, ended
/// after `calls` GitHub calls.
async fn record(
    db: &Db,
    job_id: u64,
    name: &str,
    attempts: u32,
    launched: std::result::Result<String, Failure>,
    calls: u32,
) -> Result<()> {
    let now = now_ms();
    let (state, instance_id, error, wait, next) = match launched {
        Ok(instance_id) => {
            console_log!("job {job_id}: runner {name} on {instance_id}, {calls} GitHub calls");
            ("launched", Some(instance_id), None, None, None)
        }
        Err(failure) => {
            let (wait, next) = failure.outcome(attempts, now);
            match next {
                Some(at) => console_error!(
                    "job {job_id}: launch {attempts} failed after {calls} GitHub calls, \
                     trying again in {} s: {failure}",
                    at.saturating_sub(now) / 1000
                ),
                None => console_error!(
                    "job {job_id}: launch {attempts} failed after {calls} GitHub calls, \
                     waiting for the setup to change: {failure}"
                ),
            }
            ("failed", None, Some(failure.message), Some(wait), next)
        }
    };
    jobs::record_launch(
        db,
        job_id,
        jobs::Launched {
            state,
            instance_id,
            error,
            wait: wait.map(super::reconcile::retry::Wait::as_str),
            next_launch_at: next,
        },
        now,
    )
    .await
}

/// Puts the tenant's queued jobs that wait for a change to its setup
/// back in line, for the reconciler to launch within a minute. Called
/// when an owner verifies the role, edits a profile, changes the
/// settings or gains a licence.
pub(crate) async fn recheck(db: &Db, tenant_id: &str) -> Result<()> {
    let changed = jobs::recheck(db, tenant_id, now_ms()).await?;
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
/// tenant has room, and counts the attempt (see [`jobs::claim`]).
/// Returns the job's launch attempts so far, this one included, or
/// `None` when it was not taken.
async fn claim(db: &Db, job: &QueuedJob, tenant: &Tenant, name: &str) -> Result<Option<u32>> {
    jobs::claim(
        db,
        job.id,
        name,
        now_ms(),
        job.installation_id,
        tenant.max_runners,
    )
    .await
}

/// Everything after the claim, with GitHub calls sent through `calls`.
/// Returns the instance id.
async fn launch_claimed(
    env: &Env,
    db: &Db,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
    calls: &Counted,
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
        .first(IMAGE, &params![tenant.region.as_str(), arch])
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            Failure::new(
                Cause::NoImage,
                format!("no {arch} image in {}", tenant.region),
            )
        })?;

    let credentials = assume_role(&settings, job, tenant).await?;
    let api = settings.github_api.as_str();
    let org = github::owner(&job.repository);
    let app = App::new(
        settings.github_api.clone(),
        settings.app_id.clone(),
        settings.app_key.clone(),
    );
    let tokens = Tokens::new(Kv::new(env), app, calls, now_ms());
    let registered = tokens
        .send(job.installation_id, |token| {
            github::generate_jit_config(api, org, token, name, &job.label)
        })
        .await
        .map_err(token_failure)?;
    let config = github::jit_config(&checked(registered, "register runner")?)
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
        let removed = tokens
            .send(job.installation_id, |token| {
                github::delete_runner(api, org, token, config.runner.id)
            })
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
    db: &Db,
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
    let user_data =
        serde_json::json!({ "broker": settings.broker_url, "ticket": ticket }).to_string();
    let job_id = job.id.to_string();
    let log_id = format!("job {job_id}");
    let tags = [
        (ec2::TAG_TENANT, tenant.id.as_str()),
        (ec2::TAG_JOB_ID, job_id.as_str()),
        (ec2::TAG_RUNNER_NAME, name),
        ("Name", name),
    ];
    let subnets: Vec<Option<&str>> = tenant.subnets().into_iter().map(Some).collect();
    let target = Target {
        region: &tenant.region,
        endpoint: aws::endpoint("ec2", &tenant.region, settings.aws_endpoint.as_deref()),
        credentials,
        launch_template_id: &tenant.launch_template_id,
        subnets: if subnets.is_empty() {
            vec![None]
        } else {
            subnets
        },
        fleet: tenant.fleet,
        offerings_scope: &tenant.id,
    };
    let instance = Instance {
        name,
        choice,
        image_id: &image.image_id,
        root_device: &image.root_device,
        user_data: &user_data,
        tags: &tags,
        log_id: &log_id,
    };
    let instance_id =
        start_instance(db, &target, &instance)
            .await
            .map_err(|failure| match &failure.cause {
                Cause::Aws(code) if code == connect::NOT_SUBSCRIBED => Failure::new(
                    failure.cause.clone(),
                    connect::subscribe_message(
                        choice.arch.as_str(),
                        settings.marketplace_url(choice.arch),
                    ),
                ),
                _ => failure,
            })?;
    if let Err(err) = store_config(settings, &ticket, &boot).await {
        // Without its config the instance would wait out the agent's
        // timeout before powering off.
        terminate_instance(&target, &instance_id).await;
        return Err(err);
    }
    Ok(instance_id)
}

async fn assume_role(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
) -> std::result::Result<Credentials, Failure> {
    role_credentials(
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

/// A launch failure for a token GitHub would not mint.
fn token_failure(err: tokens::Error) -> Failure {
    match err {
        tokens::Error::Settings(message) => Failure::new(Cause::Settings, message),
        tokens::Error::Unreachable(message) => Failure::new(Cause::Unreachable, message),
        tokens::Error::GitHub { status, message } => Failure::new(Cause::GitHub(status), message),
        tokens::Error::Answer(message) => Failure::new(Cause::Internal, message),
    }
}

/// GitHub's answer to `what`; an error answer fails on its status.
fn checked(
    response: crate::http::Response,
    what: &str,
) -> std::result::Result<crate::http::Response, Failure> {
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $14)",
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
                beta,
                200u32,
                30u32,
                true,
                1u64
            ],
        );
        db
    }

    fn job(db: &Db, id: u64, status: &str) {
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
            &params![id, INSTALLATION, "acme/app", 77u64, "hatchrun", status, NOW],
        );
    }

    fn claim(db: &Db, id: u64, max_runners: u32) -> Option<u32> {
        db.wait(jobs::claim(
            db.db(),
            id,
            &format!("hatchrun-{id}-00"),
            NOW,
            INSTALLATION,
            max_runners,
        ))
        .unwrap()
    }

    #[test]
    fn a_connected_tenant_reads() {
        let db = seed(false, 2);
        let tenant: Tenant = db.first(&tenant_query(), &params![INSTALLATION]).unwrap();
        assert_eq!(tenant.id, "t1");
        assert_eq!(tenant.max_runners, 2);
        // Unpaid, within its free minutes.
        assert!(tenant.licensed);
        assert_eq!(tenant.volume_size_limit, Some(200));
        assert_eq!(tenant.root_volume_size, Some(30));
        assert!(tenant.fleet);
        assert!(tenant.instance_types.is_none());

        // Past them and the grace period after them.
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        db.run(
            "INSERT INTO usage_events (job_id, tenant_id, minutes, ended_at, billed) \
             VALUES (1, 't1', $1, $2, false)",
            &params![crate::usage::FREE_MINUTES, now],
        );
        db.run(
            "UPDATE tenants SET free_used_up_at = $1 WHERE id = 't1'",
            &params![now - crate::usage::GRACE_MS - 60_000],
        );
        let tenant: Tenant = db.first(&tenant_query(), &params![INSTALLATION]).unwrap();
        assert!(!tenant.licensed);

        db.run(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             product, cancel_at_period_end, modified_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, 'runners', $6, $7, $8)",
            &params!["s1", "t1", "active", "c1", "p1", false, "2026-10-01", NOW],
        );
        let tenant: Tenant = db.first(&tenant_query(), &params![INSTALLATION]).unwrap();
        assert!(tenant.licensed);
        assert!(
            db.first::<Tenant>(&tenant_query(), &params![INSTALLATION + 1])
                .is_none()
        );
    }

    #[test]
    fn a_claim_counts_runners_through_the_open_jobs() {
        let db = seed(true, 1);
        let plan = db.plan(
            jobs::CLAIM,
            &params![1u64, "hatchrun-1-00", NOW, INSTALLATION, 1u32],
        );
        assert!(plan.contains("jobs_open on jobs"), "{plan}");
        assert!(!plan.contains("Seq Scan"), "{plan}");
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
            jobs::RECORD_LAUNCH,
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
            jobs::RECORD_LAUNCH,
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
                jobs::REFUSE,
                &params![id, "tenant t1 has no licence", NOW, "setup"],
            )
        };
        assert_eq!(refuse(1), 1);
        assert_eq!(refuse(1), 0);
        assert_eq!(refuse(2), 1);
        // Only the queued one launches again.
        assert_eq!(db.run(jobs::RECHECK, &params!["t1", NOW + 1]), 1);
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
             VALUES ($1, $2, $3, $4, $5)",
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
            licensed: true,
            instance_types: None,
            excluded_instance_types: None,
            volume_size_limit: None,
            root_volume_size: None,
            fleet: false,
            cache_bucket: None,
        };
        assert_eq!(tenant.subnets(), ["subnet-b", "subnet-a"]);
    }
}
