// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! hatchrun-scaler listens to one GitHub Actions runner scale set and
//! launches an EC2 instance for every job assigned to it. Each instance
//! runs hatchrun-agent, which runs the job and powers the instance off.

mod broker;
mod driver;
mod ec2;
mod github;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_config::BehaviorVersion;
use clap::Parser;
use hatchrun_scaler_core::{Config, Scaler};
use hatchrun_scaleset::{ConfigUrl, NewScaleSet, Secret};
use tokio::signal::unix::{SignalKind, signal};
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::broker::Broker;
use crate::driver::{Driver, ensure_scale_set};
use crate::ec2::Ec2;
use crate::github::{Credentials, GitHub, Http};

/// Secrets come from the environment only, so they stay out of `ps`:
/// `HATCHRUN_GITHUB_TOKEN` (a personal access token, or use a GitHub App
/// instead), `HATCHRUN_GITHUB_APP_PRIVATE_KEY` (PEM) and
/// `HATCHRUN_BROKER_TOKEN` (the `BROKER_TOKEN` the broker was deployed
/// with).
#[derive(Parser)]
#[command(version = env!("HATCHRUN_VERSION"), about, after_help = "Secrets are read from the environment: HATCHRUN_GITHUB_TOKEN or HATCHRUN_GITHUB_APP_PRIVATE_KEY, and HATCHRUN_BROKER_TOKEN.")]
struct Args {
    /// Where runners register: `https://github.com/{org}`,
    /// `https://github.com/{owner}/{repo}` or
    /// `https://github.com/enterprises/{enterprise}`.
    #[arg(long, env = "HATCHRUN_GITHUB_URL")]
    github_url: ConfigUrl,
    /// GitHub App client id (or App id). The private key is read from
    /// `HATCHRUN_GITHUB_APP_PRIVATE_KEY`.
    #[arg(
        long,
        env = "HATCHRUN_GITHUB_APP_CLIENT_ID",
        requires = "github_app_installation_id"
    )]
    github_app_client_id: Option<String>,
    /// GitHub App installation id.
    #[arg(long, env = "HATCHRUN_GITHUB_APP_INSTALLATION_ID")]
    github_app_installation_id: Option<u64>,

    /// hatchrun-broker URL. Instances claim their runner config there, so
    /// it never sits in user data.
    #[arg(long, env = "HATCHRUN_BROKER_URL")]
    broker_url: String,

    /// Scale set name, which is also the `runs-on` label.
    #[arg(long, env = "HATCHRUN_SCALE_SET")]
    scale_set: String,
    /// More `runs-on` labels, comma separated. The name is always one.
    #[arg(long, env = "HATCHRUN_LABELS", value_delimiter = ',')]
    labels: Vec<String>,
    /// Runner group the scale set belongs to.
    #[arg(long, env = "HATCHRUN_RUNNER_GROUP", default_value = "Default")]
    runner_group: String,
    /// Keep runners from updating themselves. Set it when the image is
    /// rebuilt with each runner release.
    #[arg(long, env = "HATCHRUN_DISABLE_UPDATE")]
    disable_update: bool,
    /// Runners kept with no jobs.
    #[arg(long, env = "HATCHRUN_MIN_RUNNERS", default_value_t = 0)]
    min_runners: u32,
    /// Most runners at once.
    #[arg(long, env = "HATCHRUN_MAX_RUNNERS", default_value_t = 10)]
    max_runners: u32,
    /// How long a runner may wait for a job before it is replaced.
    #[arg(long, env = "HATCHRUN_IDLE_TIMEOUT", default_value = "15m", value_parser = humantime::parse_duration)]
    idle_timeout: Duration,
    /// Stopped instances kept ready for new runners. They booted once and
    /// read their files, so a job starts sooner on them. Needs an
    /// on-demand launch template: spot instances cannot be stopped this
    /// way.
    #[arg(long, env = "HATCHRUN_WARM_POOL", default_value_t = 0)]
    warm_pool: u32,
    /// How long a pool instance runs after launch before it is stopped.
    #[arg(long, env = "HATCHRUN_WARM_POOL_WARMUP", default_value = "3m", value_parser = humantime::parse_duration)]
    warm_pool_warmup: Duration,
    /// Age at which a pool instance is replaced, so the pool picks up new
    /// images and launch template versions.
    #[arg(long, env = "HATCHRUN_WARM_POOL_MAX_AGE", default_value = "24h", value_parser = humantime::parse_duration)]
    warm_pool_max_age: Duration,
    /// How often to compare runners with instances.
    #[arg(long, env = "HATCHRUN_RECONCILE_INTERVAL", default_value = "1m", value_parser = humantime::parse_duration)]
    reconcile_interval: Duration,

    /// Launch template id (lt-...) or name for runner instances.
    #[arg(long, env = "HATCHRUN_LAUNCH_TEMPLATE")]
    launch_template: String,
    /// Launch template version.
    #[arg(
        long,
        env = "HATCHRUN_LAUNCH_TEMPLATE_VERSION",
        default_value = "$Default"
    )]
    launch_template_version: String,
    /// Subnets to launch in, comma separated, tried in order when one is
    /// out of capacity. Leave empty to use the launch template's network,
    /// and leave network interfaces out of the template when set.
    #[arg(long, env = "HATCHRUN_SUBNETS", value_delimiter = ',')]
    subnets: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Reads a secret that is not accepted as a flag.
fn optional_secret_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{name} is not valid UTF-8"),
    }
}

fn secret_env(name: &str) -> Result<String> {
    optional_secret_env(name)?.with_context(|| format!("{name} is required"))
}

async fn run(args: Args) -> Result<()> {
    driver::validate(args.min_runners, args.max_runners)?;
    if args.warm_pool > 0 && args.warm_pool_max_age <= args.warm_pool_warmup {
        anyhow::bail!("--warm-pool-max-age must be longer than --warm-pool-warmup");
    }
    let store = Broker::new(
        &args.broker_url,
        Secret::new(secret_env("HATCHRUN_BROKER_TOKEN")?),
    )?;
    let mut terminate = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("listen for SIGINT")?;
    let shutdown = async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    };

    let token = optional_secret_env("HATCHRUN_GITHUB_TOKEN")?;
    let credentials = match (token, args.github_app_client_id) {
        (Some(_), Some(_)) => {
            bail!("HATCHRUN_GITHUB_TOKEN conflicts with --github-app-client-id")
        }
        (Some(token), None) => Credentials::Token(Secret::new(token)),
        (None, Some(client_id)) => Credentials::app(
            client_id,
            args.github_app_installation_id.unwrap_or_default(),
            &secret_env("HATCHRUN_GITHUB_APP_PRIVATE_KEY")?,
        )?,
        (None, None) => bail!("set HATCHRUN_GITHUB_TOKEN or --github-app-client-id"),
    };
    let github = GitHub::new(Http::new()?, args.github_url, credentials);
    let actions = github.connect(driver::now()).await?;

    let scale_set = ensure_scale_set(
        &github,
        &actions,
        &args.runner_group,
        NewScaleSet {
            name: args.scale_set.clone(),
            runner_group_id: 0,
            labels: if args.labels.is_empty() {
                Vec::new()
            } else {
                std::iter::once(args.scale_set.clone())
                    .chain(args.labels)
                    .collect()
            },
            disable_update: args.disable_update,
        },
    )
    .await?;

    let aws = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let cloud = Ec2 {
        client: aws_sdk_ec2::Client::new(&aws),
        launch_template: args.launch_template,
        launch_template_version: args.launch_template_version,
        subnets: args.subnets,
        scale_set_id: scale_set.id,
        scale_set_name: scale_set.name.clone(),
    };
    let scaler = Scaler::new(Config {
        name_prefix: scale_set.name.clone(),
        min_runners: args.min_runners,
        max_runners: args.max_runners,
        idle_timeout: args.idle_timeout.as_secs(),
        pool_size: args.warm_pool,
        pool_warmup: args.warm_pool_warmup.as_secs(),
        pool_max_age: args.warm_pool_max_age.as_secs(),
        ..Config::default()
    });
    let owner = std::env::var("HOSTNAME").unwrap_or_else(|_| "hatchrun-scaler".into());

    Driver::start(github, actions, cloud, store, scaler, scale_set.id, &owner)
        .await?
        .run(shutdown, args.reconcile_interval)
        .await
}
