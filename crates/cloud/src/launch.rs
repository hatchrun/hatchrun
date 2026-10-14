// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Launches a runner instance for a recorded job, after the webhook has
//! been answered.
//!
//! The Worker takes the job by moving it from no launch state to
//! `launching`, which only one request can do, and only while the tenant
//! has fewer than `max_runners` runners launching or running. Then:
//!
//! 1. pick the instance type from the label and the image for its
//!    architecture in the tenant's region;
//! 2. sign an `oidc.hatchrun.io` token for the tenant and trade it for
//!    the tenant role's credentials;
//! 3. register a just-in-time runner with the job's label through the
//!    App installation;
//! 4. start the instance with a fresh broker ticket in its user data,
//!    trying the tenant's subnets in order while they lack capacity;
//! 5. store the runner config in the broker under the ticket.
//!
//! The config is stored only once the instance exists, as the scaler
//! does. When a step after registration fails, the runner is removed and
//! a started instance terminated, and the job is marked `failed`.

use std::fmt::Write;
use std::time::Duration;

use serde::Deserialize;
use worker::{D1Database, Env, Result, console_error, console_log, console_warn};

use crate::aws::{self, Credentials, ec2, sts};
use crate::fetch::send;
use crate::http::Request;
use crate::labels::Arch;
use crate::webhook::QueuedJob;
use crate::{DB, github, instance, int, jwt, now_ms, oidc, webcrypto};

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
const AWS_ENDPOINT_URL: &str = "AWS_ENDPOINT_URL";

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
        .prepare(
            "SELECT id, role_arn, region, launch_template_id, subnet_ids, max_runners \
             FROM tenants WHERE installation_id = ?1 AND role_arn IS NOT NULL \
             AND region IS NOT NULL AND launch_template_id IS NOT NULL \
             AND subnet_ids IS NOT NULL",
        )
        .bind(&[int(job.installation_id)?])?
        .first(None)
        .await?;
    let Some(tenant) = tenant else {
        let reason = format!(
            "installation {} has no tenant with AWS connected",
            job.installation_id
        );
        db.prepare(
            "UPDATE jobs SET launch_state = 'failed', launch_error = ?2, updated_at = ?3 \
             WHERE id = ?1 AND launch_state IS NULL",
        )
        .bind(&[int(job.id)?, reason.as_str().into(), int(now_ms())?])?
        .run()
        .await?;
        console_warn!("job {}: {reason}", job.id);
        return Ok(());
    };

    let name = runner_name(job.id, &webcrypto::random_bytes(4)?);
    if !claim(&db, job, &tenant, &name).await? {
        console_log!(
            "job {}: not launching, already taken or tenant {} at {} runners",
            job.id,
            tenant.id,
            tenant.max_runners
        );
        return Ok(());
    }

    let (state, instance_id, error) = match launch_claimed(env, &db, job, &tenant, &name).await {
        Ok(instance_id) => {
            console_log!("job {}: runner {name} on {instance_id}", job.id);
            ("launched", Some(instance_id), None)
        }
        Err(err) => {
            console_error!("job {}: launch failed: {err}", job.id);
            ("failed", None, Some(err))
        }
    };
    db.prepare(
        "UPDATE jobs SET launch_state = ?2, instance_id = ?3, launch_error = ?4, updated_at = ?5 \
         WHERE id = ?1",
    )
    .bind(&[
        int(job.id)?,
        state.into(),
        nullable(instance_id),
        nullable(error),
        int(now_ms())?,
    ])?
    .run()
    .await?;
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
/// tenant has room. D1 runs one statement at a time, so two deliveries of
/// one job cannot both take it.
async fn claim(db: &D1Database, job: &QueuedJob, tenant: &Tenant, name: &str) -> Result<bool> {
    let result = db
        .prepare(
            "UPDATE jobs SET launch_state = 'launching', runner_name = ?2, updated_at = ?3 \
             WHERE id = ?1 AND launch_state IS NULL AND status = 'queued' \
             AND (SELECT COUNT(*) FROM jobs \
                  WHERE installation_id = ?4 AND status <> 'completed' \
                  AND launch_state IN ('launching', 'launched')) < ?5",
        )
        .bind(&[
            int(job.id)?,
            name.into(),
            int(now_ms())?,
            int(job.installation_id)?,
            tenant.max_runners.into(),
        ])?
        .run()
        .await?;
    Ok(result.meta()?.and_then(|meta| meta.changes) == Some(1))
}

/// Everything after the claim. Returns the instance id.
async fn launch_claimed(
    env: &Env,
    db: &D1Database,
    job: &QueuedJob,
    tenant: &Tenant,
    name: &str,
) -> std::result::Result<String, String> {
    let settings = Settings::read(env)?;
    let choice = instance::choose(&job.spec)?;
    let arch = match choice.arch {
        Arch::Amd64 => "amd64",
        Arch::Arm64 => "arm64",
    };
    let image: Image = db
        .prepare("SELECT image_id, root_device FROM images WHERE region = ?1 AND arch = ?2")
        .bind(&[tenant.region.as_str().into(), arch.into()])
        .map_err(text)?
        .first(None)
        .await
        .map_err(text)?
        .ok_or_else(|| format!("no {arch} image in {}", tenant.region))?;

    let credentials = assume_role(&settings, job, tenant).await?;
    let org = github::owner(&job.repository);
    let token = installation_token(&settings, job.installation_id).await?;
    let config = github::jit_config(
        &send(&github::generate_jit_config(
            &settings.github_api,
            org,
            &token,
            name,
            &job.label,
        ))
        .await?,
    )?;

    let result = start(
        &settings,
        job,
        tenant,
        name,
        &choice.instance_type,
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
    instance_type: &str,
    image: &Image,
    credentials: &Credentials,
    config: &str,
) -> std::result::Result<String, String> {
    let ticket = jwt::b64url(webcrypto::random_bytes(32).map_err(text)?);
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
    let mut last_error = None;
    let mut instance_id = None;
    for (index, subnet) in subnets.into_iter().enumerate() {
        let client_token = format!("{name}-{index}");
        let mut request = ec2::RunInstances {
            launch_template_id: &tenant.launch_template_id,
            image_id: &image.image_id,
            instance_type,
            subnet_id: subnet,
            user_data: &user_data,
            client_token: &client_token,
            tags: &tags,
            spot: job.spec.spot,
            root_volume: job
                .spec
                .disk_gib
                .map(|size| (image.root_device.as_str(), size)),
        }
        .request(&endpoint);
        aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())?;
        match ec2::instance_id(&send(&request).await?) {
            Ok(id) => {
                instance_id = Some(id);
                break;
            }
            Err(err) if ec2::CAPACITY_ERRORS.contains(&err.code.as_str()) => {
                console_warn!(
                    "job {}: {}: {err}",
                    job.id,
                    subnet.unwrap_or("template network")
                );
                last_error = Some(err);
            }
            Err(err) => return Err(format!("RunInstances: {err}")),
        }
    }
    let Some(instance_id) = instance_id else {
        return Err(match last_error {
            Some(err) => format!("no subnet had capacity: {err}"),
            None => "no subnet to launch in".to_owned(),
        });
    };

    if let Err(err) = store_config(settings, &ticket, config).await {
        // Without its config the instance would wait out the agent's
        // timeout before powering off.
        let mut request = ec2::terminate_instances(&endpoint, &instance_id);
        let terminated =
            match aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms()) {
                Ok(()) => match send(&request).await {
                    Ok(response) => ec2::terminated(&response).map_err(|err| err.to_string()),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            };
        if let Err(terminate_err) = terminated {
            console_warn!("terminate {instance_id}: {terminate_err}");
        }
        return Err(err);
    }
    Ok(instance_id)
}

async fn assume_role(
    settings: &Settings,
    job: &QueuedJob,
    tenant: &Tenant,
) -> std::result::Result<Credentials, String> {
    let input = oidc::signing_input(&settings.oidc_kid, &tenant.id, now_ms() / 1000);
    let signature = webcrypto::sign_rs256(&settings.oidc_key, input.as_bytes())
        .await
        .map_err(|err| format!("sign tenant token: {err}"))?;
    let request = sts::assume_role_with_web_identity(
        &aws::endpoint("sts", &tenant.region, settings.aws_endpoint.as_deref()),
        &tenant.role_arn,
        &format!("hatchrun-job-{}", job.id),
        &jwt::token(&input, &signature),
    );
    sts::credentials(&send(&request).await?)
        .map_err(|err| format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn))
}

async fn installation_token(
    settings: &Settings,
    installation_id: u64,
) -> std::result::Result<String, String> {
    let app_jwt = app_jwt(&settings.app_id, &settings.app_key).await?;
    let request = github::installation_token(&settings.github_api, installation_id, &app_jwt);
    github::token(&send(&request).await?)
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
) -> std::result::Result<(), String> {
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
                return Err(format!(
                    "store runner config: broker answered {}: {}",
                    response.status,
                    response.text()
                ));
            }
            Err(err) => last = err,
        }
        if attempt < BROKER_ATTEMPTS {
            worker::Delay::from(Duration::from_secs(u64::from(attempt))).await;
        }
    }
    Err(format!("store runner config: {last}"))
}

/// D1 takes `null` for SQL NULL and refuses `undefined`, which is what
/// `Option::None` becomes.
fn nullable(value: Option<String>) -> wasm_bindgen::JsValue {
    value.map_or(wasm_bindgen::JsValue::NULL, Into::into)
}

#[allow(clippy::needless_pass_by_value)]
fn text(err: worker::Error) -> String {
    err.to_string()
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
    fn subnets_in_order() {
        let tenant = Tenant {
            id: "acme1234".to_owned(),
            role_arn: String::new(),
            region: String::new(),
            launch_template_id: String::new(),
            subnet_ids: "subnet-b, subnet-a,,".to_owned(),
            max_runners: 1,
        };
        assert_eq!(tenant.subnets(), ["subnet-b", "subnet-a"]);
    }
}
