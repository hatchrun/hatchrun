// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! hatchrun-agent runs on a runner instance at boot. It fetches the
//! instance's just-in-time runner config, runs exactly one GitHub Actions job
//! and exits. The systemd unit powers the instance off when it exits, whether
//! the job ran or the agent failed. On hatchrun.io launches it reports the
//! instance's CPU and memory while the job runs (see [`telemetry`]), and
//! serves the job cache from the tenant's bucket when the launch says
//! (see [`cache`]).

mod broker;
mod cache;
mod envfile;
mod exec;
mod imds;
mod metrics;
mod runner;
mod source;
mod ssm;
mod telemetry;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_config::{BehaviorVersion, Region};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::broker::HttpBroker;
use crate::cache::Proxy;
use crate::cache::patch::{self, Direction};
use crate::imds::Imds;
use crate::metrics::Sampler;
use crate::runner::Runner;
use crate::source::Resolver;
use crate::ssm::Ssm;
use crate::telemetry::{Machine, Reporter};

#[derive(Parser)]
#[command(version = env!("HATCHRUN_VERSION"), about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
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

#[derive(clap::Subcommand)]
enum Command {
    /// Mark the instance as launched as a runner, for hatchrun-launch.service.
    ///
    /// Writes the mark file when user data or tags hold a runner config. An
    /// instance launched any other way, by hand or by the AWS Marketplace
    /// scan, gets no mark, so hatchrun-agent.service stays off instead of
    /// powering it off. When the metadata cannot be read the instance is
    /// marked, so a runner never sits idle and billed.
    Detect {
        /// File to create on a runner launch.
        #[arg(long, default_value = "/run/hatchrun/launch")]
        mark: PathBuf,
    },
    /// Run one job from a job document, without GitHub's runner.
    ///
    /// Exits 0 when the job succeeds, 1 when it does not, and 2 when the
    /// document cannot be read or describes an approval job.
    Exec {
        /// The job document, as JSON.
        job_file: PathBuf,
        /// Work directory, laid out like GitHub's hosted runners.
        #[arg(long, default_value = "/home/runner/work")]
        work_dir: PathBuf,
        /// User steps run as. Pass an empty value to keep the agent's user.
        #[arg(long, default_value = "runner")]
        runner_user: String,
        /// KEY=VALUE file that becomes the steps' environment.
        #[arg(long, default_value = "/etc/environment")]
        env_file: PathBuf,
        /// Node binary that runs JavaScript actions.
        #[arg(long)]
        node: Option<PathBuf>,
        /// Directory of prefetched actions.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Where to write the job result, as JSON.
        #[arg(long)]
        result: Option<PathBuf>,
        /// How long one `timeout-minutes` minute lasts. Tests shorten it.
        #[arg(long, default_value = "60s", value_parser = humantime::parse_duration, hide = true)]
        minute: Duration,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    if let Some(Command::Exec {
        job_file,
        work_dir,
        runner_user,
        env_file,
        node,
        store,
        result,
        minute,
    }) = &args.command
    {
        let opts = exec::Options {
            work_dir: work_dir.clone(),
            runner_user: Some(runner_user.clone()).filter(|user| !user.is_empty()),
            env_file: env_file.clone(),
            node: node.clone(),
            store: store.clone(),
            tool_cache: PathBuf::from("/opt/hostedtoolcache"),
            minute: *minute,
        };
        return exec_command(job_file, &opts, result.as_deref()).await;
    }
    let result = match &args.command {
        Some(Command::Detect { mark }) => detect(&args.imds_endpoint, mark).await,
        Some(Command::Exec { .. }) => unreachable!("handled above"),
        None => run(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the job file. Exit codes: 0 success, 1 the job did not succeed
/// or the run failed, 2 the document cannot be used.
async fn exec_command(job_file: &Path, opts: &exec::Options, result: Option<&Path>) -> ExitCode {
    let doc = match std::fs::read(job_file)
        .with_context(|| format!("read {}", job_file.display()))
        .and_then(|bytes| {
            serde_json::from_slice::<hatchrun_workflow::JobDocument>(&bytes)
                .with_context(|| format!("parse {}", job_file.display()))
        }) {
        Ok(doc) => doc,
        Err(err) => {
            error!("{err:#}");
            return ExitCode::from(2);
        }
    };
    if matches!(doc.job.kind, hatchrun_workflow::JobKind::Approval) {
        error!("approval jobs do not run on a runner");
        return ExitCode::from(2);
    }
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(err) => {
            error!("listen for SIGTERM: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Each SIGTERM bumps the count. The first cancels the job, the
    // second stops it for good.
    let (requests, cancel) = tokio::sync::watch::channel(0u32);
    tokio::spawn(async move {
        while terminate.recv().await.is_some() {
            requests.send_modify(|n| *n += 1);
        }
    });
    let mut sink = exec::sink::TextSink::new(std::io::stdout());
    let outcome = exec::run_job(doc, opts, &mut sink, cancel).await;
    let job = match outcome {
        Ok(job) => job,
        Err(err) => {
            error!("{err:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = result {
        let written = serde_json::to_vec(&job)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
            });
        if let Err(err) = written {
            error!("{err:#}");
            return ExitCode::FAILURE;
        }
    }
    if job.conclusion == hatchrun_workflow::Conclusion::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn detect(imds_endpoint: &str, mark: &Path) -> Result<()> {
    let launch = match Imds::new(imds_endpoint) {
        Ok(metadata) => source::is_runner_launch(&metadata).await,
        Err(err) => Err(err),
    };
    let launch = launch.unwrap_or_else(|err| {
        warn!("cannot tell how the instance was launched, treating it as a runner: {err:#}");
        true
    });
    if !launch {
        info!("not launched as a runner, hatchrun-agent stays off");
        return Ok(());
    }
    if let Some(dir) = mark.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(mark, b"").with_context(|| format!("write {}", mark.display()))
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

    let boot = tokio::select! {
        result = tokio::time::timeout(args.config_timeout, resolver.boot()) => {
            result.map_err(|_| anyhow!("no runner config after {:?}", args.config_timeout))??
        }
        () = &mut shutdown => bail!("stopped before the runner config arrived"),
    };

    let reporter = match boot.telemetry {
        Some(telemetry) => start_reporter(&resolver.metadata, telemetry).await,
        None => None,
    };

    let cache = match boot.cache {
        Some(cache) => start_cache(&resolver.metadata, cache, &args.runner_dir).await,
        None => None,
    };
    let env = cache
        .as_ref()
        .map(|proxy| {
            vec![
                ("ACTIONS_RESULTS_URL".to_owned(), proxy.url().to_owned()),
                ("ACTIONS_CACHE_SERVICE_V2".to_owned(), "true".to_owned()),
            ]
        })
        .unwrap_or_default();

    let runner = Runner {
        dir: args.runner_dir,
        user: Some(args.runner_user).filter(|user| !user.is_empty()),
        env_file: args.env_file,
        stop_timeout: args.stop_timeout,
        env,
    };
    info!(dir = %runner.dir.display(), user = runner.user.as_deref().unwrap_or("(agent)"), "starting runner");
    let run = runner.run(&boot.jit_config, shutdown);
    let status = match reporter {
        Some(mut reporter) => {
            let status = tokio::select! {
                status = run => status,
                never = reporter.collect() => match never {},
            };
            reporter.finish().await;
            status
        }
        None => run.await,
    };
    if let Some(proxy) = cache {
        proxy.stop().await;
    }
    let status = status?;
    if !status.success() {
        bail!("runner exited with {status}");
    }
    info!("runner exited");
    Ok(())
}

/// The cache proxy for a hosted launch, with the runner pointed at it.
/// The runner is patched only once the proxy passed its health check; a
/// proxy that cannot start or fails it is logged and skipped, the runner
/// stays stock, and the job uses GitHub's cache.
async fn start_cache(metadata: &Imds, cache: source::Cache, runner_dir: &Path) -> Option<Proxy> {
    let started = async {
        let ip = tokio::time::timeout(Duration::from_secs(2), metadata.private_ip())
            .await
            .map_err(|_| anyhow!("instance metadata has no local-ipv4 after 2s"))??;
        let proxy = Proxy::start(cache, ip).await?;
        proxy.check().await?;
        patch::patch_runner(runner_dir, Direction::Redirect)?;
        anyhow::Ok(proxy)
    };
    match started.await {
        Ok(proxy) => {
            info!(url = proxy.url(), "serving the job cache");
            Some(proxy)
        }
        Err(err) => {
            warn!("not serving the job cache, the job uses GitHub's: {err:#}");
            if let Err(err) = patch::patch_runner(runner_dir, Direction::Restore) {
                warn!("{err:#}");
            }
            None
        }
    }
}

/// The metrics reporter for a hosted launch. A reporter that cannot
/// start is logged and skipped; the job runs either way.
async fn start_reporter(metadata: &Imds, telemetry: source::Telemetry) -> Option<Reporter> {
    // Metadata answered for the user data a moment ago; the job does not
    // wait long for it a second time.
    let instance_type =
        match tokio::time::timeout(Duration::from_secs(2), metadata.instance_type()).await {
            Ok(Ok(instance_type)) => instance_type,
            Ok(Err(err)) => {
                warn!("{err:#}");
                "unknown".to_owned()
            }
            Err(_) => {
                warn!("instance metadata has no instance-type after 2s");
                "unknown".to_owned()
            }
        };
    let machine = Machine {
        instance_type,
        arch: std::env::consts::ARCH,
        agent_version: env!("HATCHRUN_VERSION"),
    };
    match Reporter::new(telemetry, machine, Sampler::new("/")) {
        Ok(reporter) => {
            info!("reporting metrics");
            Some(reporter)
        }
        Err(err) => {
            warn!("not reporting metrics: {err:#}");
            None
        }
    }
}
