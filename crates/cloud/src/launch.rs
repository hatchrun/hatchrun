// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

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
//! 4. start the instance with a fresh broker ticket in its user data,
//!    trying the tenant's subnets in order and then the profile's other
//!    instance types while they lack capacity, Spot before on-demand
//!    for a Spot profile;
//! 5. store the runner config in the broker under the ticket.
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
use crate::profiles::{self, store};
use crate::reconcile::retry::{Cause, Failure};
use crate::webhook::QueuedJob;
use crate::{DB, github, instance, int, jwt, now_ms, oidc, settings, webcrypto};

/// Variable with the App's client id.
const APP_ID: &str = "GITHUB_APP_ID";
/// Secret with the App's private key in PEM.
pub(crate) const APP_KEY: &str = "GITHUB_APP_PRIVATE_KEY";
/// Variable with the broker URL instances claim their config from.
const BROKER_URL: &str = "BROKER_URL";
/// Secret with the broker's token for storing configs.
const BROKER_TOKEN: &str = "BROKER_TOKEN";
/// Variable that replaces the GitHub API URL, for development.
const GITHUB_API_URL: &str = "GITHUB_API_URL";
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
    volume_size_limit: Option<u16>,
    root_volume_size: Option<u16>,
}

/// SQL that is 1 for a tenant row that may launch: in the beta, or
/// with a Polar subscription that holds a licence.
pub(crate) const LICENSED: &str = "(tenants.beta = 1 OR EXISTS (SELECT 1 FROM subscriptions \
     WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')))";

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
    github_api: String,
    aws_endpoint: Option<String>,
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
            github_api: get(GITHUB_API_URL)
                .unwrap_or_else(|| github::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            aws_endpoint: get(AWS_ENDPOINT_URL),
        })
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
        .prepare(format!(
            "SELECT id, role_arn, region, launch_template_id, subnet_ids, max_runners, \
             allowed_regions, {LICENSED} AS licensed, instance_types, volume_size_limit, \
             root_volume_size \
             FROM tenants WHERE installation_id = ?1 AND role_arn IS NOT NULL \
             AND region IS NOT NULL AND launch_template_id IS NOT NULL \
             AND subnet_ids IS NOT NULL"
        ))
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
            db.prepare(
                "UPDATE jobs SET launch_state = 'failed', launch_error = ?2, updated_at = ?3, \
                 launch_retry = ?4, next_launch_at = NULL \
                 WHERE id = ?1 AND launch_state IS NULL",
            )
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
    db.prepare(
        "UPDATE jobs SET launch_state = ?2, instance_id = ?3, launch_error = ?4, updated_at = ?5, \
         launch_retry = ?6, next_launch_at = ?7 WHERE id = ?1",
    )
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
        .prepare(
            "UPDATE jobs SET launch_state = NULL, launch_retry = NULL, next_launch_at = NULL, \
             updated_at = ?2 \
             WHERE installation_id = (SELECT installation_id FROM tenants WHERE id = ?1) \
             AND status = 'queued' AND launch_state = 'failed' AND launch_retry = 'setup'",
        )
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
    #[derive(Deserialize)]
    struct Claimed {
        launch_attempts: u32,
    }
    let claimed: Option<Claimed> = db
        .prepare(
            "UPDATE jobs SET launch_state = 'launching', runner_name = ?2, updated_at = ?3, \
             launch_attempts = launch_attempts + 1 \
             WHERE id = ?1 AND launch_state IS NULL AND status = 'queued' \
             AND (SELECT COUNT(*) FROM jobs \
                  WHERE installation_id = ?4 AND status <> 'completed' \
                  AND launch_state IN ('launching', 'launched')) < ?5 \
             RETURNING launch_attempts",
        )
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
        tenant.volume_size_limit,
        tenant.root_volume_size,
    );
    let choice = instance::choose(&profile, &tenant.region, &limits)
        .map_err(|err| Failure::new(Cause::NoInstanceType, err))?;
    let settings = Settings::read(env).map_err(|err| Failure::new(Cause::Settings, err))?;
    let arch = choice.arch.as_str();
    let image: Image = db
        .prepare("SELECT image_id, root_device FROM images WHERE region = ?1 AND arch = ?2")
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
        job,
        tenant,
        name,
        &choice,
        &image,
        &credentials,
        &config.encoded_jit_config,
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

/// Starts the instance and hands its config to the broker.
#[allow(clippy::too_many_arguments)]
async fn start(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
    choice: &Choice,
    image: &Image,
    credentials: &Credentials,
    config: &str,
) -> std::result::Result<String, Failure> {
    let ticket = jwt::b64url(webcrypto::random_bytes(32).map_err(internal)?);
    let user_data =
        serde_json::json!({ "broker": settings.broker_url, "ticket": ticket }).to_string();
    let job_id = job.id.to_string();
    let tags = [
        (ec2::TAG_TENANT, tenant.id.as_str()),
        (ec2::TAG_JOB_ID, job_id.as_str()),
        (ec2::TAG_RUNNER_NAME, name),
        ("Name", name),
    ];
    let endpoint = aws::endpoint("ec2", &tenant.region, settings.aws_endpoint.as_deref());

    let mut subnets: Vec<Option<&str>> = tenant.subnets().into_iter().map(Some).collect();
    if subnets.is_empty() {
        subnets.push(None);
    }
    let subnets = &subnets;
    let markets: &[bool] = if choice.spot {
        &[true, false]
    } else {
        &[false]
    };
    let attempts = markets.iter().flat_map(|spot| {
        choice.instance_types.iter().flat_map(move |instance_type| {
            subnets
                .iter()
                .map(move |subnet| (*spot, instance_type.as_str(), *subnet))
        })
    });
    let mut last_error = None;
    let mut instance_id = None;
    for (index, (spot, instance_type, subnet)) in attempts.enumerate() {
        let client_token = format!("{name}-{index}");
        let mut request = ec2::RunInstances {
            launch_template_id: &tenant.launch_template_id,
            image_id: &image.image_id,
            instance_type,
            subnet_id: subnet,
            user_data: &user_data,
            client_token: &client_token,
            tags: &tags,
            spot,
            root_volume: choice
                .root_volume
                .map(|size| (image.root_device.as_str(), size)),
        }
        .request(&endpoint);
        aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())
            .map_err(|err| Failure::new(Cause::Internal, err))?;
        let response = send(&request)
            .await
            .map_err(|err| Failure::new(Cause::Unreachable, err))?;
        match ec2::instance_id(&response) {
            Ok(id) => {
                instance_id = Some(id);
                break;
            }
            Err(err) if ec2::CAPACITY_ERRORS.contains(&err.code.as_str()) => {
                console_warn!(
                    "job {}: {instance_type}{} in {}: {err}",
                    job.id,
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
    let Some(instance_id) = instance_id else {
        return Err(match last_error {
            Some(err) => Failure::new(
                Cause::NoCapacity,
                format!(
                    "no capacity for {} in any subnet: {err}",
                    choice.instance_types.join(", ")
                ),
            ),
            None => Failure::new(Cause::Internal, "no subnet to launch in"),
        });
    };

    if let Err(err) = store_config(settings, &ticket, config).await {
        // Without its config the instance would wait out the agent's
        // timeout before powering off.
        terminate(&endpoint, credentials, &tenant.region, &instance_id).await;
        return Err(err);
    }
    Ok(instance_id)
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

/// The App's JWT, signed with its private key in PKCS#8 DER.
pub(crate) async fn app_jwt(app_id: &str, pkcs8: &[u8]) -> std::result::Result<String, String> {
    let input = github::app_jwt_signing_input(app_id, now_ms() / 1000);
    let signature = webcrypto::sign_rs256(pkcs8, input.as_bytes())
        .await
        .map_err(|err| format!("sign App JWT: {err}"))?;
    Ok(jwt::token(&input, &signature))
}

/// Stores the config under the ticket, retrying while the broker is busy.
async fn store_config(
    settings: &Settings,
    ticket: &str,
    config: &str,
) -> std::result::Result<(), Failure> {
    let request = Request::new(
        "PUT",
        format!("{}/v1/tickets/{ticket}", settings.broker_url),
    )
    .header("Authorization", format!("Bearer {}", settings.broker_token))
    .header("Content-Type", "application/json")
    .body(serde_json::json!({ "config": config, "expiresIn": BROKER_EXPIRES_IN }).to_string());
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
            volume_size_limit: None,
            root_volume_size: None,
        };
        assert_eq!(tenant.subnets(), ["subnet-b", "subnet-a"]);
    }
}
