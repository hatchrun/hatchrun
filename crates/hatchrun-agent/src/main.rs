// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! hatchrun-agent runs on a runner instance at boot. It fetches the
//! instance's just-in-time runner config, runs exactly one GitHub Actions job
//! and exits. The systemd unit powers the instance off when it exits, whether
//! the job ran or the agent failed.

mod broker;
mod envfile;
mod imds;
mod runner;
mod source;
mod ssm;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_config::{BehaviorVersion, Region};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::broker::HttpBroker;
use crate::imds::Imds;
use crate::runner::Runner;
use crate::source::Resolver;
use crate::ssm::Ssm;

#[derive(Parser)]
#[command(version = env!("HATCHRUN_VERSION"), about)]
struct Args {
    /// Directory holding the runner's run.sh.
    #[arg(long, default_value = "/home/runner")]
    runner_dir: PathBuf,
    /// User the runner runs as. Pass an empty value to keep the agent's user.
    #[arg(long, default_value = "runner")]
    runner_user: String,
    /// KEY=VALUE file that becomes the runner's environment.
    #[arg(long, default_value = "/etc/environment")]
    env_file: PathBuf,
    /// How long to wait for the runner config before giving up.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    config_timeout: Duration,
    /// How often to check for a runner config parameter that does not exist yet.
    #[arg(long, default_value = "1s", value_parser = humantime::parse_duration)]
    poll_interval: Duration,
    /// How long the runner gets to exit after SIGTERM.
    #[arg(long, default_value = "30s", value_parser = humantime::parse_duration)]
    stop_timeout: Duration,
    /// Instance metadata endpoint.
    #[arg(long, default_value = imds::DEFAULT_ENDPOINT, hide = true)]
    imds_endpoint: String,
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

async fn run(args: Args) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let shutdown = async move {
        terminate.recv().await;
    };
    tokio::pin!(shutdown);

    let metadata = Imds::new(&args.imds_endpoint)?;
    let region = metadata.region().await?;
    let aws = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    let resolver = Resolver {
        metadata,
        parameters: Ssm::new(aws_sdk_ssm::Client::new(&aws)),
        broker: HttpBroker::new()?,
        poll_interval: args.poll_interval,
    };

    let jit_config = tokio::select! {
        result = tokio::time::timeout(args.config_timeout, resolver.jit_config()) => {
            result.map_err(|_| anyhow!("no runner config after {:?}", args.config_timeout))??
        }
        () = &mut shutdown => bail!("stopped before the runner config arrived"),
    };

    let runner = Runner {
        dir: args.runner_dir,
        user: Some(args.runner_user).filter(|user| !user.is_empty()),
        env_file: args.env_file,
        stop_timeout: args.stop_timeout,
    };
    info!(dir = %runner.dir.display(), user = runner.user.as_deref().unwrap_or("(agent)"), "starting runner");
    let status = runner.run(&jit_config, shutdown).await?;
    if !status.success() {
        bail!("runner exited with {status}");
    }
    info!("runner exited");
    Ok(())
}
