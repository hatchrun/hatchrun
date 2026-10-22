// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starts and stops the instances jobs run on.
//!
//! [`Backend::Ec2`] starts them in the CI AWS account through the same
//! code hatchrun.io uses for a tenant's account: it assumes the CI role
//! with an `oidc.hatchrun.io` token whose subject is `tenant:hatchrun-ci`
//! and starts instances of the built-in profile of the job's runner
//! template (see [`profile_for`]), one fresh on-demand instance per job.
//!
//! [`Backend::Local`] calls a launcher daemon on the developer's machine
//! (`hatchrun-ci-dev launcher`), since a Worker cannot start processes.
//! It exists only in tests and in builds with
//! `RUSTFLAGS="--cfg hatchrun_local_launcher"`; the production build has
//! no such code, and [`kind`] refuses it when `DEPLOY_ENV` is
//! `production` as well.

use hatchrun_cloud::aws::{self, Credentials, ec2, instances};
use hatchrun_cloud::db::{Db, params};
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::http::{Request, Response};
use hatchrun_cloud::instance::{Limits, choose, static_offerings};
use hatchrun_cloud::profiles::{Profile, built_in};
use hatchrun_cloud::start::{
    Cause, Failure, Instance, Target, role_credentials, start_instance, terminate_instance,
};
use hatchrun_workflow::Template;
use hatchrun_workflow::templates::Arch as TemplateArch;
use serde::Deserialize;
use worker::console_warn;

use crate::env::Settings;

/// The OIDC subject tenant and the `hatchrun:tenant` tag of CI
/// instances.
pub const TENANT: &str = "hatchrun-ci";

/// The built-in hatchrun.io profile `template` launches:
/// `hatchrun-<vcpu>cpu`, with `-arm64` on arm64. General purpose, 4 GiB
/// per vCPU, on demand.
///
/// # Panics
///
/// Never for a template of [`hatchrun_workflow::templates::TEMPLATES`]:
/// every one has a built-in profile.
#[must_use]
pub fn profile_for(template: &Template) -> Profile {
    let name = match template.arch {
        TemplateArch::Amd64 => format!("hatchrun-{}cpu", template.vcpu),
        TemplateArch::Arm64 => format!("hatchrun-{}cpu-arm64", template.vcpu),
    };
    built_in(&name).unwrap_or_else(|| panic!("no built-in profile {name}"))
}

/// The runner image of region `$1` and architecture `$2`.
const IMAGE: &str = "SELECT image_id, root_device FROM images WHERE region = $1 AND arch = $2";

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// The job the instance runs.
    pub job_id: u64,
    /// The runner name, `ci-<job>-<launch attempt>`.
    pub name: String,
    /// The instance's user data (see [`user_data`]).
    pub user_data: String,
    /// The runner template the job asked for.
    pub template: &'static Template,
}

/// How instances start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// In the CI AWS account.
    Ec2,
    /// Through the local launcher daemon.
    Local,
}

/// The launcher the `LAUNCHER` variable names in `deploy_env`.
///
/// # Errors
///
/// An unknown name, or `local` in production or in a build without the
/// local launcher.
pub fn kind(launcher: &str, deploy_env: &str) -> Result<Kind, String> {
    kind_for(
        launcher,
        deploy_env,
        cfg!(any(test, hatchrun_local_launcher)),
    )
}

/// [`kind`] for a build that has the local launcher or not.
fn kind_for(launcher: &str, deploy_env: &str, has_local: bool) -> Result<Kind, String> {
    match launcher {
        "ec2" => Ok(Kind::Ec2),
        "local" if deploy_env == "production" => {
            Err("the local launcher is refused in production".to_owned())
        }
        "local" if !has_local => Err("this build has no local launcher".to_owned()),
        "local" => Ok(Kind::Local),
        other => Err(format!("unknown LAUNCHER `{other}`")),
    }
}

/// A way to start instances.
pub enum Backend {
    /// The CI AWS account.
    Ec2(Box<Ec2>),
    /// The local launcher daemon.
    #[cfg(any(test, hatchrun_local_launcher))]
    Local(Local),
}

impl Backend {
    /// The backend `settings` name.
    ///
    /// # Errors
    ///
    /// A launcher that is unknown, refused, or missing its settings.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)] // Async so a backend may read what it needs.
    pub async fn new(settings: &Settings) -> Result<Backend, String> {
        match kind(&settings.launcher, &settings.deploy_env)? {
            Kind::Ec2 => Ec2::new(settings).map(|ec2| Backend::Ec2(Box::new(ec2))),
            #[cfg(any(test, hatchrun_local_launcher))]
            Kind::Local => Local::new(settings).map(Backend::Local),
            #[cfg(not(any(test, hatchrun_local_launcher)))]
            Kind::Local => Err("this build has no local launcher".to_owned()),
        }
    }

    /// Starts an instance for `spec` and returns its id.
    ///
    /// # Errors
    ///
    /// A [`Failure`] with the cause the retry rules act on.
    pub async fn launch(&self, db: &Db, spec: &LaunchSpec) -> Result<String, Failure> {
        match self {
            Backend::Ec2(ec2) => ec2.launch(db, spec).await,
            #[cfg(any(test, hatchrun_local_launcher))]
            Backend::Local(local) => local.launch(spec).await,
        }
    }

    /// Turns the instance's metadata endpoint off.
    ///
    /// # Errors
    ///
    /// What the backend answered.
    pub async fn metadata_off(&self, instance_id: &str) -> Result<(), String> {
        match self {
            Backend::Ec2(ec2) => ec2.metadata_off(instance_id).await,
            #[cfg(any(test, hatchrun_local_launcher))]
            Backend::Local(local) => local.metadata_off(instance_id).await,
        }
    }

    /// Whether the change made by [`Backend::metadata_off`] is applied.
    ///
    /// # Errors
    ///
    /// What the backend answered.
    pub async fn metadata_applied(&self, instance_id: &str) -> Result<bool, String> {
        match self {
            Backend::Ec2(ec2) => ec2.metadata_applied(instance_id).await,
            #[cfg(any(test, hatchrun_local_launcher))]
            Backend::Local(local) => local.metadata_applied(instance_id).await,
        }
    }

    /// Terminates the instance, logging a failure.
    pub async fn terminate(&self, instance_id: &str) {
        match self {
            Backend::Ec2(ec2) => ec2.terminate(instance_id).await,
            #[cfg(any(test, hatchrun_local_launcher))]
            Backend::Local(local) => local.terminate(instance_id).await,
        }
    }

    /// The instances that are pending or running.
    ///
    /// # Errors
    ///
    /// What the backend answered.
    pub async fn instances(&self) -> Result<Vec<instances::Described>, String> {
        match self {
            Backend::Ec2(ec2) => ec2.instances().await,
            #[cfg(any(test, hatchrun_local_launcher))]
            Backend::Local(local) => local.instances().await,
        }
    }
}

/// The user data of an instance that runs a CI job: the broker to ask
/// for the boot document and the ticket to ask with.
#[must_use]
pub fn user_data(broker_url: &str, ticket: &str) -> String {
    serde_json::json!({ "mode": "ci", "broker": broker_url, "ticket": ticket }).to_string()
}

/// An image row.
#[derive(Debug, Deserialize)]
struct Image {
    image_id: String,
    root_device: String,
}

/// How long before they expire cached credentials stop being used.
const CREDENTIALS_MARGIN_MS: u64 = 60_000;

/// Whether credentials fetched at `fetched_ms` are still good at `now_ms`:
/// until a minute before the session STS granted ends.
pub(crate) fn credentials_fresh(fetched_ms: u64, now_ms: u64) -> bool {
    let lifetime_ms = u64::from(hatchrun_cloud::aws::sts::DURATION_SECONDS) * 1000;
    now_ms >= fetched_ms && now_ms < fetched_ms + lifetime_ms - CREDENTIALS_MARGIN_MS
}

struct CachedCredentials {
    role: String,
    fetched_ms: u64,
    credentials: Credentials,
}

thread_local! {
    static CREDENTIALS: std::cell::RefCell<Option<CachedCredentials>> =
        const { std::cell::RefCell::new(None) };
}

/// Instances in the CI AWS account.
pub struct Ec2 {
    settings: Settings,
}

impl Ec2 {
    fn new(settings: &Settings) -> Result<Self, String> {
        if settings.ci_role_arn.is_empty() {
            return Err("CI_ROLE_ARN is not set".to_owned());
        }
        if settings.ci_launch_template_id.is_empty() {
            return Err("CI_LAUNCH_TEMPLATE_ID is not set".to_owned());
        }
        Ok(Self {
            settings: settings.clone(),
        })
    }

    /// The CI role's credentials for reads and terminations, shared by
    /// every call in the isolate until shortly before they expire.
    async fn shared_credentials(&self) -> Result<Credentials, aws::Error> {
        let role = self.settings.ci_role_arn.as_str();
        let now = worker::Date::now().as_millis();
        let cached = CREDENTIALS.with(|cell| {
            cell.borrow()
                .as_ref()
                .filter(|cached| cached.role == role && credentials_fresh(cached.fetched_ms, now))
                .map(|cached| cached.credentials.clone())
        });
        if let Some(credentials) = cached {
            return Ok(credentials);
        }
        let credentials = self.credentials("hatchrun-ci").await?;
        CREDENTIALS.with(|cell| {
            *cell.borrow_mut() = Some(CachedCredentials {
                role: role.to_owned(),
                fetched_ms: now,
                credentials: credentials.clone(),
            });
        });
        Ok(credentials)
    }

    /// The CI role's credentials; `session` shows in `CloudTrail`.
    async fn credentials(&self, session: &str) -> Result<Credentials, aws::Error> {
        let settings = &self.settings;
        role_credentials(
            &settings.oidc_key,
            &settings.oidc_kid,
            settings.aws_endpoint.as_deref(),
            TENANT,
            &settings.ci_role_arn,
            &settings.ci_region,
            session,
        )
        .await
    }

    fn endpoint(&self) -> String {
        aws::endpoint(
            "ec2",
            &self.settings.ci_region,
            self.settings.aws_endpoint.as_deref(),
        )
    }

    /// The target of a start with `credentials`.
    fn target<'a>(&'a self, credentials: &'a Credentials) -> Target<'a> {
        let subnets: Vec<Option<&str>> = self
            .settings
            .ci_subnet_ids
            .iter()
            .map(|subnet| Some(subnet.as_str()))
            .collect();
        Target {
            region: &self.settings.ci_region,
            endpoint: self.endpoint(),
            credentials,
            launch_template_id: &self.settings.ci_launch_template_id,
            // `start_instance` needs a subnet to try; `None` is the
            // template's own network.
            subnets: if subnets.is_empty() {
                vec![None]
            } else {
                subnets
            },
            fleet: true,
            offerings_scope: TENANT,
        }
    }

    /// Signs `request` for EC2 and sends it.
    async fn call(
        &self,
        credentials: &Credentials,
        mut request: Request,
    ) -> Result<Response, String> {
        aws::sign_v4(
            &mut request,
            credentials,
            &self.settings.ci_region,
            "ec2",
            worker::Date::now().as_millis(),
        )?;
        send(&request).await
    }

    async fn launch(&self, db: &Db, spec: &LaunchSpec) -> Result<String, Failure> {
        let region = self.settings.ci_region.as_str();
        let profile = profile_for(spec.template);
        let choice = choose(
            &profile,
            region,
            &static_offerings(region),
            &Limits::from_columns(None, None, None, None),
        )
        .map_err(|err| Failure::new(Cause::NoInstanceType, err))?;
        let arch = choice.arch.as_str();
        let image: Image = db
            .first(IMAGE, &params![region, arch])
            .await
            .map_err(|err| Failure::new(Cause::Internal, err.to_string()))?
            .ok_or_else(|| Failure::new(Cause::NoImage, format!("no {arch} image in {region}")))?;
        let credentials = self
            .credentials(&format!("hatchrun-ci-{}", spec.job_id))
            .await
            .map_err(|err| {
                let cause = if err.code == "Unreachable" {
                    Cause::Unreachable
                } else {
                    Cause::Aws(err.code.clone())
                };
                Failure::new(
                    cause,
                    format!(
                        "AssumeRoleWithWebIdentity {}: {err}",
                        self.settings.ci_role_arn
                    ),
                )
            })?;
        let job_id = spec.job_id.to_string();
        let log_id = format!("job {job_id}");
        let tags = [
            (ec2::TAG_TENANT, TENANT),
            (ec2::TAG_JOB_ID, job_id.as_str()),
            (ec2::TAG_RUNNER_NAME, spec.name.as_str()),
        ];
        start_instance(
            db,
            &self.target(&credentials),
            &Instance {
                name: &spec.name,
                choice: &choice,
                image_id: &image.image_id,
                root_device: &image.root_device,
                user_data: &spec.user_data,
                tags: &tags,
                log_id: &log_id,
            },
        )
        .await
    }

    async fn metadata_off(&self, instance_id: &str) -> Result<(), String> {
        let credentials = self
            .shared_credentials()
            .await
            .map_err(|err| err.to_string())?;
        let response = self
            .call(
                &credentials,
                ec2::metadata_off(&self.endpoint(), instance_id),
            )
            .await?;
        if response.ok() {
            Ok(())
        } else {
            Err(format!(
                "ModifyInstanceMetadataOptions: EC2 answered {}: {}",
                response.status,
                response.text()
            ))
        }
    }

    async fn metadata_applied(&self, instance_id: &str) -> Result<bool, String> {
        let credentials = self
            .shared_credentials()
            .await
            .map_err(|err| err.to_string())?;
        let response = self
            .call(
                &credentials,
                ec2::describe_metadata(&self.endpoint(), instance_id),
            )
            .await?;
        let state = match ec2::metadata_state(&response) {
            Ok(state) => state,
            // Describe is throttled: the change is not seen yet, so the
            // claim polls again after its pause.
            Err(err) if err.code == "RequestLimitExceeded" => return Ok(false),
            Err(err) => return Err(err.to_string()),
        };
        Ok(
            state
                .is_some_and(|state| state.state == "applied" && state.http_endpoint == "disabled"),
        )
    }

    async fn terminate(&self, instance_id: &str) {
        match self.shared_credentials().await {
            Ok(credentials) => {
                terminate_instance(&self.target(&credentials), instance_id).await;
            }
            Err(err) => console_warn!("terminate {instance_id}: {err}"),
        }
    }

    async fn instances(&self) -> Result<Vec<instances::Described>, String> {
        let credentials = self
            .shared_credentials()
            .await
            .map_err(|err| err.to_string())?;
        let endpoint = self.endpoint();
        let mut all = Vec::new();
        let mut next = None;
        loop {
            let response = self
                .call(
                    &credentials,
                    instances::describe(&endpoint, TENANT, next.as_deref()),
                )
                .await?;
            let (page, token) = instances::described(&response).map_err(|err| err.to_string())?;
            all.extend(page);
            match token {
                Some(token) => next = Some(token),
                None => return Ok(all),
            }
        }
    }
}

/// The local launcher daemon.
#[cfg(any(test, hatchrun_local_launcher))]
pub struct Local {
    url: String,
}

#[cfg(any(test, hatchrun_local_launcher))]
impl Local {
    fn new(settings: &Settings) -> Result<Self, String> {
        let url = settings
            .local_launcher_url
            .clone()
            .ok_or_else(|| "LOCAL_LAUNCHER_URL is not set".to_owned())?;
        Ok(Self { url })
    }

    async fn call(&self, request: Request, expect: u16) -> Result<Response, String> {
        let response = send(&request).await?;
        if response.status == expect {
            Ok(response)
        } else {
            Err(format!(
                "the local launcher answered {}: {}",
                response.status,
                response.text()
            ))
        }
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<String, Failure> {
        #[derive(Deserialize)]
        struct Started {
            instance_id: String,
        }
        let body = serde_json::json!({
            "name": spec.name,
            "job_id": spec.job_id,
            "template": spec.template.labels[0],
            "user_data": spec.user_data,
        });
        let request = Request::new("POST", format!("{}/instances", self.url))
            .header("Content-Type", "application/json")
            .body(body.to_string());
        let response = self
            .call(request, 200)
            .await
            .map_err(|err| Failure::new(Cause::Unreachable, err))?;
        serde_json::from_slice::<Started>(&response.body)
            .map(|started| started.instance_id)
            .map_err(|err| Failure::new(Cause::Internal, format!("local launcher: {err}")))
    }

    async fn metadata_off(&self, instance_id: &str) -> Result<(), String> {
        let request = Request::new(
            "POST",
            format!("{}/instances/{instance_id}/metadata-off", self.url),
        );
        self.call(request, 204).await.map(|_| ())
    }

    async fn metadata_applied(&self, instance_id: &str) -> Result<bool, String> {
        #[derive(Deserialize)]
        struct State {
            metadata: String,
        }
        let request = Request::new("GET", format!("{}/instances/{instance_id}", self.url));
        let response = self.call(request, 200).await?;
        let state: State = serde_json::from_slice(&response.body).map_err(|err| err.to_string())?;
        Ok(state.metadata == "applied")
    }

    async fn terminate(&self, instance_id: &str) {
        let request = Request::new("DELETE", format!("{}/instances/{instance_id}", self.url));
        if let Err(err) = self.call(request, 204).await {
            console_warn!("terminate {instance_id}: {err}");
        }
    }

    async fn instances(&self) -> Result<Vec<instances::Described>, String> {
        #[derive(Deserialize)]
        struct Listed {
            id: String,
            state: String,
            launch_time: String,
            job_id: Option<String>,
            runner_name: Option<String>,
        }
        let request = Request::new("GET", format!("{}/instances", self.url));
        let response = self.call(request, 200).await?;
        let listed: Vec<Listed> =
            serde_json::from_slice(&response.body).map_err(|err| err.to_string())?;
        Ok(listed
            .into_iter()
            .map(|item| instances::Described {
                id: item.id,
                state: item.state,
                launch_time: item.launch_time,
                job_id: item.job_id,
                runner_name: item.runner_name,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use hatchrun_cloud::labels::Arch;
    use hatchrun_cloud::profiles::Capacity;
    use hatchrun_workflow::templates::{TEMPLATES, template};

    use super::*;

    #[test]
    fn cached_credentials_are_used_until_a_minute_before_they_expire() {
        let start = 1_000_000;
        assert!(credentials_fresh(start, start));
        assert!(credentials_fresh(start, start + 839_999));
        assert!(!credentials_fresh(start, start + 840_000), "900 s - 60 s");
        assert!(!credentials_fresh(start, start + 900_000));
        assert!(
            !credentials_fresh(start, start - 1),
            "a clock that went back"
        );
    }

    #[test]
    fn local_is_refused_in_production() {
        let err = kind("local", "production").unwrap_err();
        assert!(err.contains("refused in production"), "{err}");
    }

    #[test]
    fn local_is_allowed_in_development() {
        assert_eq!(kind("local", "development"), Ok(Kind::Local));
    }

    #[test]
    fn a_build_without_the_local_launcher_lacks_it() {
        assert_eq!(
            kind_for("local", "development", false),
            Err("this build has no local launcher".to_owned())
        );
        assert_eq!(
            kind_for("local", "production", false),
            Err("the local launcher is refused in production".to_owned())
        );
        assert_eq!(kind_for("ec2", "production", false), Ok(Kind::Ec2));
    }

    #[test]
    fn ec2_is_the_default() {
        assert_eq!(kind("ec2", "production"), Ok(Kind::Ec2));
        assert_eq!(kind("ec2", "development"), Ok(Kind::Ec2));
        assert!(kind("docker", "development").is_err());
    }

    #[test]
    fn user_data_selects_the_ci_path() {
        let data: serde_json::Value =
            serde_json::from_str(&user_data("https://broker.example", "t1")).unwrap();
        assert_eq!(
            data,
            serde_json::json!({
                "mode": "ci",
                "broker": "https://broker.example",
                "ticket": "t1",
            })
        );
    }

    #[test]
    fn every_template_has_a_built_in_profile() {
        for t in TEMPLATES {
            let p = profile_for(t);
            let arch = match t.arch {
                TemplateArch::Amd64 => Arch::Amd64,
                TemplateArch::Arm64 => Arch::Arm64,
            };
            assert_eq!(p.arch, arch, "{}", t.labels[0]);
            assert_eq!(p.cpu_min, u16::try_from(t.vcpu).unwrap());
            assert_eq!(p.cpu_max, Some(u16::try_from(t.vcpu).unwrap()));
            assert_eq!(p.memory_min, u16::try_from(t.memory_gib).unwrap());
            assert_eq!(p.capacity, Capacity::OnDemand);
            assert_eq!(p.categories, ["m"]);
        }
    }

    #[test]
    fn arm_names_the_arm64_profile() {
        let t = template("ubuntu-26.04-8core-arm").unwrap();
        assert_eq!(profile_for(t).name, "hatchrun-8cpu-arm64");
        let t = template("ubuntu-latest").unwrap();
        assert_eq!(profile_for(t).name, "hatchrun-2cpu");
    }

    #[test]
    fn every_template_picks_an_instance_of_its_size() {
        for t in TEMPLATES {
            let choice = choose(
                &profile_for(t),
                "us-east-1",
                &static_offerings("us-east-1"),
                &Limits::from_columns(None, None, None, None),
            )
            .unwrap_or_else(|err| panic!("{}: {err}", t.labels[0]));
            assert_eq!(u32::from(choice.vcpus), t.vcpu, "{}", t.labels[0]);
            assert_eq!(choice.memory_gib, t.memory_gib, "{}", t.labels[0]);
            assert_eq!(choice.capacity.markets(), [false]);
        }
    }
}
