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
mod ci;
mod envfile;
mod exec;
mod imds;
mod metrics;
mod runner;
mod source;
mod split;
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
use crate::source::Metadata as _;
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
        /// Directory that holds the archives of cache, workspace and
        /// test result steps.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Where the agent keeps what the runner user must not reach,
        /// such as the staging area for cache and workspace archives.
        /// Created if missing; every directory on the path must belong
        /// to the agent or root and be closed to others.
        #[arg(long, default_value = "/var/lib/hatchrun-agent")]
        state_dir: PathBuf,
        /// Where to write the job result, as JSON.
        #[arg(long)]
        result: Option<PathBuf>,
        /// How long one `timeout-minutes` minute lasts. Tests shorten it.
        #[arg(long, default_value = "60s", value_parser = humantime::parse_duration, hide = true)]
        minute: Duration,
    },
    /// Run one job for the control plane.
    ///
    /// Claims the boot ticket named by the user data, claims the job from
    /// the control plane, runs it and reports the result. The user data
    /// comes from `--user-data`, else from instance metadata. Exits 0
    /// when the job succeeds or had already ended and 1 otherwise.
    Ci {
        /// File holding the user data, as JSON. Without it the agent
        /// reads the instance's user data.
        #[arg(long)]
        user_data: Option<PathBuf>,
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
        /// Where the agent keeps what the runner user must not reach.
        #[arg(long, default_value = "/var/lib/hatchrun-agent")]
        state_dir: PathBuf,
        /// How long to wait for the boot document and for the job to be
        /// ready, each.
        #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
        claim_timeout: Duration,
    },
    /// Print this parallel node's share of a list of tests.
    ///
    /// Reads `HATCHRUN_NODE_INDEX`, `HATCHRUN_NODE_TOTAL` and
    /// `HATCHRUN_TIMINGS`, which `exec` sets for a parallel job. Items
    /// come from the arguments, or from stdin one per line. Outside a
    /// parallel job every item is printed. Exits 2 on unusable input.
    Split {
        /// How to split. Only `timings` is supported.
        #[arg(long, value_enum)]
        by: SplitBy,
        /// The items to split. Without any, stdin is read.
        items: Vec<String>,
    },
}

/// How `split` divides the items.
#[derive(Clone, Copy, clap::ValueEnum)]
enum SplitBy {
    /// By the timings recorded for earlier runs.
    Timings,
}

// A second thread keeps the SIGTERM listener and the job timer running
// while a long `hashFiles` call holds the main one.
#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let result = match &args.command {
        Some(Command::Detect { mark }) => detect(&args.imds_endpoint, mark).await,
        Some(Command::Exec {
            job_file,
            work_dir,
            runner_user,
            env_file,
            node,
            store,
            state_dir,
            result,
            minute,
        }) => {
            let opts = exec::Options {
                work_dir: work_dir.clone(),
                runner_user: Some(runner_user.clone()).filter(|user| !user.is_empty()),
                env_file: env_file.clone(),
                node: node.clone(),
                store: store.clone(),
                state_dir: state_dir.clone(),
                tool_cache: PathBuf::from("/opt/hostedtoolcache"),
                minute: *minute,
            };
            let code = exec_command(job_file, &opts, result.as_deref()).await;
            // A blocking task that outlived the job, such as an archive
            // still being read, must not keep the process up once the
            // result is written.
            {
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
            }
            std::process::exit(code);
        }
        Some(Command::Ci {
            user_data,
            work_dir,
            runner_user,
            env_file,
            node,
            state_dir,
            claim_timeout,
        }) => {
            let opts = ci_options(
                work_dir,
                runner_user,
                env_file,
                node.as_ref(),
                state_dir,
                *claim_timeout,
            );
            let user_data = match user_data {
                Some(path) => {
                    std::fs::read(path).with_context(|| format!("read {}", path.display()))
                }
                None => read_user_data(&args.imds_endpoint).await,
            };
            let code = match user_data {
                Ok(user_data) => ci::run(user_data, opts).await,
                Err(err) => Err(err),
            };
            exit_with(code)
        }
        Some(Command::Split {
            by: SplitBy::Timings,
            items,
        }) => return split::run(items.clone()),
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

/// Options for a CI job. The defaults match `exec`.
fn ci_options(
    work_dir: &Path,
    runner_user: &str,
    env_file: &Path,
    node: Option<&PathBuf>,
    state_dir: &Path,
    claim_timeout: Duration,
) -> ci::CiOptions {
    ci::CiOptions {
        exec: exec::Options {
            work_dir: work_dir.to_owned(),
            runner_user: Some(runner_user.to_owned()).filter(|user| !user.is_empty()),
            env_file: env_file.to_owned(),
            node: node.cloned(),
            store: None,
            state_dir: state_dir.to_owned(),
            tool_cache: PathBuf::from("/opt/hostedtoolcache"),
            minute: Duration::from_secs(60),
        },
        claim_timeout,
    }
}

async fn read_user_data(imds_endpoint: &str) -> Result<Vec<u8>> {
    let metadata = Imds::new(imds_endpoint)?;
    metadata
        .user_data()
        .await
        .context("read user data")?
        .context("the instance has no user data")
}

/// Exits the process with the CI job's exit code, or 1 after logging the
/// error. Output is flushed first: a blocking task that outlived the job
/// must not keep the process up.
fn exit_with(code: Result<i32>) -> ! {
    let code = code.unwrap_or_else(|err| {
        error!("{err:#}");
        1
    });
    {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }
    std::process::exit(code);
}

/// Runs the job file and returns the exit code: 0 success, 1 the job
/// did not succeed or the run failed, 2 the document cannot be used.
/// With `result`, every exit leaves a result file behind.
async fn exec_command(job_file: &Path, opts: &exec::Options, result: Option<&Path>) -> i32 {
    let doc = match std::fs::read(job_file)
        .with_context(|| format!("read {}", job_file.display()))
        .and_then(|bytes| {
            serde_json::from_slice::<hatchrun_workflow::JobDocument>(&bytes)
                .with_context(|| format!("parse {}", job_file.display()))
        }) {
        Ok(doc) => doc,
        Err(err) => {
            error!("{err:#}");
            return failed(result, 2, &exec::mask::Masker::default());
        }
    };
    let mut masker = exec::mask::Masker::default();
    for secret in doc.secrets.values() {
        masker.add(secret);
    }
    masker.add(&doc.github.token);
    if matches!(doc.job.kind, hatchrun_workflow::JobKind::Approval) {
        error!("approval jobs do not run on a runner");
        return failed(result, 2, &masker);
    }
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(err) => {
            error!("listen for SIGTERM: {err}");
            return failed(result, 1, &masker);
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
            // The job could not start, but the control plane still gets
            // a result to read. The message can name a secret that a
            // path or a user name happens to hold.
            error!("{}", masker.mask(&format!("{err:#}")));
            return failed(result, 1, &masker);
        }
    };
    if let Some(path) = result
        && let Err(err) = write_result(path, &job)
    {
        error!("{err:#}");
        return 1;
    }
    i32::from(job.conclusion != hatchrun_workflow::Conclusion::Success)
}

/// Writes a failed result to `result`, if there is a path, and returns
/// `code`.
fn failed(result: Option<&Path>, code: i32, masker: &exec::mask::Masker) -> i32 {
    let failure = hatchrun_workflow::JobResult {
        conclusion: hatchrun_workflow::Conclusion::Failure,
        outputs: std::collections::BTreeMap::new(),
        steps: Vec::new(),
        annotations: Vec::new(),
    };
    if let Some(path) = result
        && let Err(err) = write_result(path, &failure)
    {
        error!("{}", masker.mask(&format!("{err:#}")));
    }
    code
}

/// Writes the job result as JSON.
fn write_result(path: &Path, job: &hatchrun_workflow::JobResult) -> Result<()> {
    let bytes = serde_json::to_vec(job).context("serialize the result")?;
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
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
    // A launch by the control plane's CI says so in its user data.
    let user_data = metadata.user_data().await.context("read user data")?;
    if source::ci_ticket(user_data.as_deref().unwrap_or_default())?.is_some() {
        let opts = ci_options(
            Path::new("/home/runner/work"),
            &args.runner_user,
            &args.env_file,
            None,
            Path::new("/var/lib/hatchrun-agent"),
            args.config_timeout,
        );
        exit_with(ci::run(user_data.unwrap_or_default(), opts).await);
    }
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
