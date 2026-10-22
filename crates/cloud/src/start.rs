// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starts one on-demand or Spot instance in a customer's AWS account
//! from their launch template, and terminates one that will not run.
//!
//! Shared by the hatchrun.io launcher (see [`crate::launch`]) and the
//! CI control plane. The caller says where ([`Target`]) and what
//! ([`Instance`]); this module tries EC2 Fleet or `RunInstances` over
//! the subnets, instance types and markets, and returns the instance id
//! or a [`Failure`] with the cause the retry pass acts on (see
//! [`crate::reconcile::retry`]).

use worker::{console_log, console_warn};

use crate::aws::{self, Credentials, ec2, sts};
use crate::db::Db;
use crate::fetch::send;
use crate::http::{Request, Response};
use crate::instance::Choice;
use crate::offerings::{self, Candidate, Unavailable};
use crate::{github, instance_types, jwt, now_ms, oidc, webcrypto};

pub use crate::reconcile::retry::{Cause, Failure};

/// Where to start an instance.
pub struct Target<'a> {
    /// AWS region.
    pub region: &'a str,
    /// The EC2 endpoint, from [`aws::endpoint`].
    pub endpoint: String,
    /// Credentials of the role that may launch from the template.
    pub credentials: &'a Credentials,
    /// The launch template, `lt-...`.
    pub launch_template_id: &'a str,
    /// Subnets to try in order; `[None]` uses the template's network.
    pub subnets: Vec<Option<&'a str>>,
    /// Start through EC2 Fleet instead of `RunInstances`.
    pub fleet: bool,
    /// Scope of the remembered unavailable offerings
    /// (`unavailable_offerings.tenant_id`).
    pub offerings_scope: &'a str,
}

/// What to start.
pub struct Instance<'a> {
    /// Prefix of the client tokens, and the `Name` tag.
    pub name: &'a str,
    /// Instance types, architecture, capacity and root volume.
    pub choice: &'a Choice,
    /// The image to start.
    pub image_id: &'a str,
    /// The image's root device, for a root volume size.
    pub root_device: &'a str,
    /// User data, before base64.
    pub user_data: &'a str,
    /// Tags for the instance, its volumes and its Spot request.
    pub tags: &'a [(&'a str, &'a str)],
    /// What console lines call the job, such as `job 123`.
    pub log_id: &'a str,
}

/// Starts the instance and returns its id.
///
/// # Errors
///
/// A [`Failure`] for no capacity, an EC2 refusal, or an unreachable EC2.
pub async fn start_instance(
    db: &Db,
    target: &Target<'_>,
    instance: &Instance<'_>,
) -> Result<String, Failure> {
    let run = Run { target, instance };
    if target.fleet {
        fleet(db, &run).await
    } else {
        run_instances(&run).await
    }
}

/// Terminates an instance that will not run, logging a failure.
pub async fn terminate_instance(target: &Target<'_>, instance_id: &str) {
    let mut request = ec2::terminate_instances(&target.endpoint, instance_id);
    let terminated = match aws::sign_v4(
        &mut request,
        target.credentials,
        target.region,
        "ec2",
        now_ms(),
    ) {
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

/// One start in progress.
struct Run<'a> {
    target: &'a Target<'a>,
    instance: &'a Instance<'a>,
}

impl Run<'_> {
    /// The markets to try in order, `true` for Spot.
    fn markets(&self) -> &'static [bool] {
        self.instance.choice.capacity.markets()
    }

    fn root_volume(&self) -> Option<(&str, u16)> {
        self.instance
            .choice
            .root_volume
            .map(|size| (self.instance.root_device, size))
    }

    /// Signs and sends an EC2 request.
    async fn send(&self, mut request: Request) -> Result<Response, Failure> {
        aws::sign_v4(
            &mut request,
            self.target.credentials,
            self.target.region,
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
async fn run_instances(run: &Run<'_>) -> Result<String, Failure> {
    let subnets = run.target.subnets.as_slice();
    let tags = run.instance.tags;
    let attempts = run.markets().iter().flat_map(|spot| {
        run.instance
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
        let client_token = format!("{}-{index}", run.instance.name);
        let request = ec2::RunInstances {
            launch_template_id: run.target.launch_template_id,
            image_id: run.instance.image_id,
            instance_type,
            subnet_id: subnet,
            user_data: run.instance.user_data,
            client_token: &client_token,
            tags,
            spot,
            root_volume: run.root_volume(),
        }
        .request(&run.target.endpoint);
        match ec2::instance_id(&run.send(request).await?) {
            Ok(id) => return Ok(id),
            Err(err) if ec2::CAPACITY_ERRORS.contains(&err.code.as_str()) => {
                console_warn!(
                    "{}: {instance_type}{} in {}: {err}",
                    run.instance.log_id,
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
                run.instance.choice.instance_types.join(", ")
            ),
        ),
        None => Failure::new(Cause::Internal, "no subnet to launch in"),
    })
}

/// The offerings a fleet refused for capacity, to skip for a while, or
/// the failure when it refused one for anything else. `last` keeps the
/// latest reason for the job's error.
fn refusals(
    run: &Run<'_>,
    spot: bool,
    refused: Vec<aws::fleet::Refused>,
    last: &mut Option<String>,
) -> Result<Vec<(Unavailable, String)>, Failure> {
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
            "{}: {}{} in {}: {}: {}",
            run.instance.log_id,
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
async fn fleet(db: &Db, run: &Run<'_>) -> Result<String, Failure> {
    let client_token = format!("{}-v", run.instance.name);
    let created = run
        .send(
            aws::fleet::TemplateVersion {
                launch_template_id: run.target.launch_template_id,
                runner_name: run.instance.name,
                image_id: run.instance.image_id,
                user_data: run.instance.user_data,
                root_volume: run.root_volume(),
                tags: run.instance.tags,
                client_token: &client_token,
            }
            .request(&run.target.endpoint),
        )
        .await?;
    let version = aws::fleet::version_number(&created).map_err(|err| {
        Failure::new(
            Cause::Aws(err.code.clone()),
            format!("CreateLaunchTemplateVersion: {err}"),
        )
    })?;
    let result = fleets(db, run, version).await;
    let deleted = run
        .send(aws::fleet::delete_version(
            &run.target.endpoint,
            run.target.launch_template_id,
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
async fn fleets(db: &Db, run: &Run<'_>, version: u64) -> Result<String, Failure> {
    let scope = run.target.offerings_scope;
    let mut unavailable = offerings::store::current(db, scope, now_ms())
        .await
        .map_err(internal)?;
    let candidates: Vec<Candidate> = run
        .instance
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
    let subnets = run.target.subnets.as_slice();
    let mut created = 0;
    let mut last = None;
    for &spot in run.markets() {
        while created < FLEETS_PER_LAUNCH {
            let chosen = offerings::overrides(&candidates, subnets, spot, &unavailable, now_ms());
            if chosen.is_empty() {
                break;
            }
            created += 1;
            let client_token = format!("{}-f{created}", run.instance.name);
            let request = aws::fleet::Fleet {
                launch_template_id: run.target.launch_template_id,
                version,
                overrides: &chosen,
                spot,
                client_token: &client_token,
            }
            .request(&run.target.endpoint);
            let done = aws::fleet::launched(&run.send(request).await?).map_err(|err| {
                Failure::new(Cause::Aws(err.code.clone()), format!("CreateFleet: {err}"))
            })?;
            if let Some((id, instance_type, subnet)) = done.instance {
                console_log!(
                    "{}: {instance_type}{} in {}",
                    run.instance.log_id,
                    if spot { " Spot" } else { "" },
                    subnet.as_deref().unwrap_or("template network")
                );
                return Ok(id);
            }
            let refused = refusals(run, spot, done.refused, &mut last)?;
            offerings::store::remember(db, scope, &refused)
                .await
                .map_err(internal)?;
            unavailable.extend(refused.into_iter().map(|(gone, _)| gone));
        }
    }
    let back = offerings::back_at(&candidates, subnets, run.markets(), &unavailable, now_ms());
    Err(Failure::new(
        Cause::NoCapacity,
        format!(
            "no capacity for {} in any subnet{}",
            run.instance.choice.instance_types.join(", "),
            last.map(|last| format!(": {last}")).unwrap_or_default()
        ),
    )
    .not_before(back))
}

/// Credentials of `role_arn` in `region`, for a token whose subject is
/// `subject_tenant`. `session` shows in the customer's `CloudTrail`.
///
/// # Errors
///
/// STS's error, or a signing failure as an [`aws::Error`] with code
/// `SigningFailed`.
pub async fn role_credentials(
    oidc_key: &[u8],
    oidc_kid: &str,
    aws_endpoint: Option<&str>,
    subject_tenant: &str,
    role_arn: &str,
    region: &str,
    session: &str,
) -> std::result::Result<Credentials, aws::Error> {
    let input = oidc::signing_input(oidc_kid, subject_tenant, now_ms() / 1000);
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

/// The App's JWT, signed with its private key in PKCS#8 DER.
///
/// # Errors
///
/// The signing failure.
pub async fn app_jwt(app_id: &str, pkcs8: &[u8]) -> Result<String, String> {
    let input = github::app_jwt_signing_input(app_id, now_ms() / 1000);
    let signature = webcrypto::sign_rs256(pkcs8, input.as_bytes())
        .await
        .map_err(|err| format!("sign App JWT: {err}"))?;
    Ok(jwt::token(&input, &signature))
}

/// Fails a start on an internal error.
#[allow(clippy::needless_pass_by_value)]
fn internal(err: worker::Error) -> Failure {
    Failure::new(Cause::Internal, err.to_string())
}
