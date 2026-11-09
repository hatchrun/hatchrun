// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Containers for a job: the job's `container`, its `services`, and the
//! steps that run in them, as GitHub's runner does it.
//!
//! The agent drives the `docker` CLI of the image (rootful Docker
//! Engine; the runner user is in the `docker` group). A job with a
//! `container` or `services` gets a user-defined network. Services
//! start first, then the job container, which idles on `tail -f
//! /dev/null`; each `run:` step and each JavaScript action is a
//! `docker exec` into it. The work directory is mounted at `/__w`, the
//! image's Node builds at `/__e` and the tool cache at `/__t`.
//! Everything the job started is removed when it ends.
//!
//! Docker actions (`uses: docker://image` and `runs.using: docker`)
//! live in [`action`]; the command lines and their parsing, which need
//! no Docker, in [`args`].

pub mod action;
pub mod args;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hatchrun_workflow::container::split_options;
use hatchrun_workflow::{Container, StepSummary};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use self::args::{Create, Health};
use super::node::EXTERNALS;
use super::sink::Sink;
use super::{Job, Launch, Line, Ran, interp, random_name};
use crate::account::DEFAULT_PATH;
use crate::envfile;
use hatchrun_workflow::Conclusion;

/// How long a pull, a build or any other single Docker command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_mins(30);
/// The most health checks a service gets, as on GitHub: a wait doubles
/// each time, so the last starts after about a minute.
const HEALTH_TRIES: u32 = 6;
/// What a start that was stopped reports, as a step does.
pub(super) const CANCELED: &str = "The operation was canceled.";
/// The most lines of a container's log printed when it fails.
const LOG_TAIL: &str = "40";

/// What the job has started in Docker.
#[derive(Default)]
pub(super) struct DockerState {
    /// Marks every container and network the job makes.
    label: String,
    /// Where `docker login` keeps credentials for this job.
    config: Option<PathBuf>,
    /// The job's network, once it exists.
    network: Option<String>,
    /// The job container, once it runs.
    job: Option<JobContainer>,
    /// Containers to remove at the end, last started last.
    containers: Vec<String>,
    /// Named volumes the job's containers use.
    volumes: Vec<String>,
    /// Images the job built.
    images: Vec<String>,
    /// Where each host directory is inside the job container, longest
    /// host path first.
    maps: Vec<(String, String)>,
    /// What the image's environment file sets: the host's, not the
    /// container's. Empty until [`Job::load_host_env`] runs.
    host_env: BTreeMap<String, String>,
    /// Images already pulled for docker actions.
    pulled: BTreeSet<String>,
    /// The `docker` binary, once found.
    bin: Option<PathBuf>,
    /// Dockerfile actions already built, by action directory and file.
    built: BTreeMap<String, String>,
    /// The job is being cleaned up: commands no longer stop on cancel.
    cleaning: bool,
}

/// The container the job's steps run in.
pub(super) struct JobContainer {
    name: String,
    /// Whether `bash` is on its `PATH`, which picks the default shell.
    pub(super) has_bash: bool,
    /// The `PATH` the image sets, for `GITHUB_PATH` entries to go in
    /// front of.
    path: String,
}

impl DockerState {
    /// A state for a job whose containers carry `label`.
    pub(super) fn new(label: String) -> Self {
        Self {
            label,
            ..Self::default()
        }
    }

    /// Whether the job runs its steps in a container.
    pub(super) fn contained(&self) -> bool {
        self.job.is_some()
    }

    /// Whether the job container has `bash`; `None` without one.
    pub(super) fn has_bash(&self) -> Option<bool> {
        self.job.as_ref().map(|job| job.has_bash)
    }

    /// Whether the job started anything that needs removing.
    fn has_work(&self) -> bool {
        self.network.is_some() || !self.containers.is_empty() || !self.images.is_empty()
    }
}

/// A `container` or service with its expressions evaluated.
struct Resolved {
    image: String,
    credentials: Option<(String, String)>,
    env: BTreeMap<String, String>,
    ports: Vec<String>,
    volumes: Vec<String>,
    options: Vec<String>,
}

/// A service that was started.
struct Service {
    id: String,
    name: String,
    ports: BTreeMap<String, String>,
}

/// The first of `dirs` that holds an executable file called `name`: a
/// file without execute permission, or a directory, is passed over.
fn first_executable(name: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    dirs.iter().map(|dir| dir.join(name)).find(|candidate| {
        candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

impl Job<'_> {
    /// The Docker command, from `--docker` or the first `docker` on the
    /// agent's `PATH` or the default one.
    pub(super) fn docker_bin(&mut self) -> Result<PathBuf, String> {
        if let Some(bin) = &self.docker.bin {
            return Ok(bin.clone());
        }
        // A path is used as it is; a bare name, `docker` or `podman`, is
        // looked up on the `PATH`.
        let bin = match &self.opts.docker {
            Some(bin) if bin.components().count() > 1 || bin.is_absolute() => bin.clone(),
            configured => {
                let name = configured
                    .as_deref()
                    .map_or_else(|| Path::new("docker"), |bin| bin);
                let path = std::env::var_os("PATH").unwrap_or_default();
                let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
                dirs.extend(std::env::split_paths(DEFAULT_PATH));
                first_executable(name, &dirs).ok_or_else(|| {
                    format!(
                        "`{}` is not installed: this job needs it as its Docker command",
                        name.display()
                    )
                })?
            }
        };
        self.docker.bin = Some(bin.clone());
        Ok(bin)
    }

    /// The directory `docker login` writes to. Only the agent can read
    /// it, so a step cannot take a registry login from it.
    fn docker_config(&mut self) -> Result<PathBuf, String> {
        if let Some(dir) = &self.docker.config {
            return Ok(dir.clone());
        }
        let dir = self
            .opts
            .state_dir
            .join(format!("docker-{}", self.docker.label));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        self.docker.config = Some(dir.clone());
        Ok(dir)
    }

    /// The environment the `docker` command itself runs with.
    fn docker_env(&mut self) -> Result<BTreeMap<String, String>, String> {
        let config = self.docker_config()?;
        Ok(BTreeMap::from([
            ("PATH".to_owned(), DEFAULT_PATH.to_owned()),
            (
                "DOCKER_CONFIG".to_owned(),
                config.to_string_lossy().into_owned(),
            ),
        ]))
    }

    /// Runs `docker args` as the agent and collects what it printed.
    /// `stdin` is written to its standard input. An `Err` is a command
    /// that could not run; a command that failed is `Ok` with its code.
    pub(super) async fn docker_capture(
        &mut self,
        args: &[String],
        stdin: Option<&str>,
    ) -> Result<Captured, String> {
        // The docker client gets only the agent's own variables, never a
        // value from the workflow: `LD_PRELOAD` or `SSL_CERT_FILE` in a
        // step's `env` must not reach a process that runs as the agent.
        let env = self.docker_env()?;
        let bin = self.docker_bin()?;
        let mut command = Command::new(&bin);
        command
            .args(args)
            .env_clear()
            .envs(&env)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("start {}: {e}", bin.display()))?;
        let pipe = child.stdin.take();
        // Feeding standard input is part of the command: a docker that
        // never reads it must still time out and stop on cancel.
        let run = async {
            if let (Some(text), Some(mut pipe)) = (stdin, pipe) {
                pipe.write_all(text.as_bytes())
                    .await
                    .map_err(|e| format!("write to docker: {e}"))?;
                drop(pipe);
            }
            child
                .wait_with_output()
                .await
                .map_err(|e| format!("wait for docker: {e}"))
        };
        let mut cancel = super::wait_for(self.cancel_requests.clone(), 1);
        let cleaning = self.docker.cleaning;
        let output = tokio::select! {
            output = tokio::time::timeout(COMMAND_TIMEOUT, run) => output
                .map_err(|_| "docker did not finish in time".to_owned())??,
            // Dropping the child kills it.
            () = &mut cancel, if !cleaning => return Err(CANCELED.to_owned()),
        };
        Ok(Captured {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Like [`Self::docker_capture`], but a failed command is an `Err`
    /// with what it printed.
    async fn docker_ok(&mut self, args: &[String]) -> Result<String, String> {
        let out = self.docker_capture(args, None).await?;
        if out.code == 0 {
            return Ok(out.stdout);
        }
        let shown = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        Err(format!(
            "docker {shown} failed with exit code {}: {}",
            out.code,
            out.stderr.trim()
        ))
    }

    /// Runs `docker args` as the agent in the step's log, with workflow
    /// commands and masking. `files` are the file command files of the
    /// step, read when it ends.
    pub(super) async fn docker_stream(
        &mut self,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        files: Option<&super::files::StepFiles>,
        limit: Option<Duration>,
        sink: &mut dyn Sink,
    ) -> Ran {
        let bin = match self.docker_bin() {
            Ok(bin) => bin,
            Err(message) => {
                self.say(sink, Line::Error(&message));
                return Ran {
                    outcome: Conclusion::Failure,
                    outputs: BTreeMap::new(),
                    state: Vec::new(),
                };
            }
        };
        let program = bin.to_string_lossy().into_owned();
        let cwd = self.layout.work.clone();
        self.execute(
            Launch {
                program: &program,
                args: &args,
                cwd: &cwd,
                env: &env,
                timeout: limit,
                timeout_minutes: limit.map(|l| u32::try_from(l.as_secs() / 60).unwrap_or(u32::MAX)),
                files,
                as_agent: true,
            },
            sink,
        )
        .await
    }

    /// `docker pull` with the container's registry login, when it has
    /// one. Pull output goes to the log.
    async fn pull(&mut self, c: &Resolved, sink: &mut dyn Sink) -> Result<(), String> {
        // Like the runner, log in only with both values: a fork's pull
        // request gets empty secrets and can still pull a public image.
        if let Some((username, password)) = &c.credentials
            && !username.is_empty()
            && !password.is_empty()
        {
            self.masker.add(password);
            let registry = args::registry_of(&c.image).map(str::to_owned);
            let login = args::login_args(registry.as_deref(), username);
            let out = self.docker_capture(&login, Some(password)).await?;
            if out.code != 0 {
                return Err(format!(
                    "docker login failed for {}: {}",
                    registry.as_deref().unwrap_or("Docker Hub"),
                    out.stderr.trim()
                ));
            }
        }
        self.say(sink, Line::Note(&format!("Pulling {}", c.image)));
        self.docker_ok(&["pull".to_owned(), c.image.clone()])
            .await
            .map(|_| ())
    }

    /// Evaluates a container's strings in the job's contexts.
    fn resolve(&self, what: &str, c: &Container) -> Result<Resolved, String> {
        let ws = &self.layout.workspace;
        let eval = |text: &str, key: &str| {
            interp(text, &self.ctx, ws, self.status).map_err(|e| format!("{what} {key}: {e}"))
        };
        let image = eval(&c.image, "image")?.trim().to_owned();
        if image.is_empty() {
            return Err(format!("{what} has an empty image"));
        }
        let credentials = match &c.credentials {
            Some(c) => Some((
                eval(&c.username, "credentials.username")?,
                eval(&c.password, "credentials.password")?,
            )),
            None => None,
        };
        let mut env = BTreeMap::new();
        for (key, value) in &c.env {
            // The name is an expression too.
            let name = eval(key, "env name")?;
            env.insert(name, eval(value, &format!("env.{key}"))?);
        }
        let ports = c
            .ports
            .iter()
            .map(|p| eval(p, "ports"))
            .collect::<Result<_, _>>()?;
        let volumes = c
            .volumes
            .iter()
            .map(|v| eval(v, "volumes"))
            .collect::<Result<_, _>>()?;
        let options = match &c.options {
            Some(text) => {
                let text = eval(text, "options")?;
                let words = split_options(&text).map_err(|e| format!("{what} options: {e}"))?;
                if let Some(name) = hatchrun_workflow::container::forbidden_options(&words).first()
                {
                    return Err(format!("{what} options set `{name}`, which hatchrun sets"));
                }
                words
            }
            None => Vec::new(),
        };
        Ok(Resolved {
            image,
            credentials,
            env,
            ports,
            volumes,
            options,
        })
    }

    /// Starts what the job declares: its network, services and job
    /// container. Does nothing for a job with none. On an error the job
    /// has failed and the caller runs no step.
    pub(super) async fn start_containers(&mut self, sink: &mut dyn Sink) -> Result<(), ()> {
        let doc = self.doc;
        if doc.job.container.is_none() && doc.job.services.is_empty() {
            return Ok(());
        }
        let index = doc.job.steps.len();
        let started = std::time::Instant::now();
        sink.step_started(index, "Initialize containers");
        let result = self.start_containers_inner(sink).await;
        let cancelled = result.is_err() && *self.cancel_requests.borrow() >= 1;
        let outcome = match (&result, cancelled) {
            (Ok(()), _) => Conclusion::Success,
            (Err(_), true) => Conclusion::Cancelled,
            (Err(_), false) => Conclusion::Failure,
        };
        if let Err(message) = &result {
            self.say(sink, Line::Error(message));
            if cancelled {
                // Starts the grace period and sets the job's status.
                self.note_cancel();
            } else {
                self.set_status(hatchrun_workflow::JobStatus::Failure);
            }
        }
        sink.step_finished(
            index,
            &StepSummary {
                name: "Initialize containers".to_owned(),
                outcome,
                conclusion: outcome,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        );
        result.map_err(|_| ())
    }

    async fn start_containers_inner(&mut self, sink: &mut dyn Sink) -> Result<(), String> {
        let doc = self.doc;
        self.docker_bin()?;
        let mut services = Vec::new();
        for (id, service) in &doc.job.services {
            services.push((
                id.clone(),
                self.resolve(&format!("service `{id}`"), service)?,
            ));
        }
        let job = match &doc.job.container {
            Some(c) => Some(self.resolve("container", c)?),
            None => None,
        };
        if let Some(job) = &job
            && let Some(problem) = hatchrun_workflow::container::aliases_of_services(
                "container",
                &job.options,
                doc.job.services.keys(),
            )
            .into_iter()
            .next()
        {
            return Err(problem);
        }

        let network = format!("hatchrun_network_{}", random_name()?);
        self.say(sink, Line::Note(&format!("Creating network {network}")));
        // Recorded first: a stop during the create still removes it.
        self.docker.network = Some(network.clone());
        self.docker_ok(&[
            "network".to_owned(),
            "create".to_owned(),
            "--label".to_owned(),
            self.docker.label.clone(),
            network.clone(),
        ])
        .await?;

        for (_, service) in &services {
            self.pull(service, sink).await?;
        }
        if let Some(job) = &job {
            self.pull(job, sink).await?;
        }

        let label = self.docker.label.clone();
        let mut started = Vec::new();
        for (id, service) in &services {
            let name = format!("hatchrun_{id}_{}", random_name()?);
            self.say(
                sink,
                Line::Note(&format!("Starting service {id} ({})", service.image)),
            );
            let (flags, env_file) = self.env_args(service.env.clone())?;
            let create = Create {
                name: &name,
                label: &label,
                network: Some(&network),
                alias: Some(id),
                env: flags.clone(),
                ports: service.ports.clone(),
                volumes: service.volumes.clone(),
                options: service.options.clone(),
                image: &service.image,
                ..Create::default()
            };
            let port_map = self
                .create_and_start(&create, &service.volumes, env_file.as_deref())
                .await?;
            started.push(Service {
                id: id.clone(),
                name,
                ports: port_map,
            });
        }
        let job_name = match &job {
            Some(job) => Some(self.start_job_container(job, &network, sink).await?),
            None => None,
        };
        for service in &started {
            self.wait_healthy(service, sink).await?;
        }
        self.publish_contexts(job_name.as_deref(), &started, &network);
        Ok(())
    }

    /// `docker create` and `docker start`, then the published ports.
    async fn create_and_start(
        &mut self,
        create: &Create<'_>,
        volumes: &[String],
        env_file: Option<&Path>,
    ) -> Result<BTreeMap<String, String>, String> {
        // A named volume that does not exist yet is made by `docker
        // create` for this job, and only those are removed afterwards.
        for volume in volumes {
            if let Some(name) = args::named_volume(volume)
                && !self.docker.volumes.iter().any(|v| v == name)
            {
                let args = ["volume".to_owned(), "inspect".to_owned(), name.to_owned()];
                let found = self.docker_capture(&args, None).await?;
                // Only a daemon that says the volume does not exist makes
                // it this job's; any other failure leaves it alone.
                if found.code != 0 && found.stderr.to_lowercase().contains("no such volume") {
                    self.docker.volumes.push(name.to_owned());
                }
            }
        }
        let mut argv = vec!["create".to_owned()];
        argv.extend(create.args());
        self.docker.containers.push(create.name.to_owned());
        let out = self.docker_capture(&argv, None).await;
        // The file held the container's variables, secrets among them.
        remove_env_file(env_file);
        let out = out?;
        if out.code != 0 {
            return Err(format!(
                "docker create failed with exit code {}: {}",
                out.code,
                out.stderr.trim()
            ));
        }
        self.docker_ok(&["start".to_owned(), create.name.to_owned()])
            .await?;
        if create.ports.is_empty() {
            return Ok(BTreeMap::new());
        }
        let out = self
            .docker_ok(&["port".to_owned(), create.name.to_owned()])
            .await?;
        Ok(args::parse_ports(&out))
    }

    /// Starts the job container: idle, with the work directory, the
    /// image's Node builds and the tool cache mounted where GitHub's
    /// runner puts them.
    async fn start_job_container(
        &mut self,
        job: &Resolved,
        network: &str,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        let layout = self.layout.clone();
        let temp = &layout.temp;
        let home = temp.join("_github_home");
        std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;
        if let Some((uid, gid)) = self.owner {
            nix::unistd::chown(&home, Some(uid), Some(gid))
                .map_err(|e| format!("chown {}: {e}", home.display()))?;
        }
        let workflow = temp.join("_github_workflow");
        let mut mounts = vec![
            (layout.work.clone(), "/__w".to_owned(), false),
            (workflow.clone(), "/github/workflow".to_owned(), false),
            (home, "/github/home".to_owned(), false),
        ];
        let mut maps = vec![
            (path_text(&workflow), "/github/workflow".to_owned()),
            (path_text(&layout.work), "/__w".to_owned()),
        ];
        if layout.tool_cache.is_dir() {
            mounts.push((layout.tool_cache.clone(), "/__t".to_owned(), false));
            maps.push((path_text(&layout.tool_cache), "/__t".to_owned()));
        }
        let externals = Path::new(EXTERNALS);
        if externals.is_dir() {
            mounts.push((externals.to_owned(), "/__e".to_owned(), true));
            maps.push((path_text(externals), "/__e".to_owned()));
        }
        // A node given with --node is mounted too, so actions find it.
        if let Some(root) = self.opts.node.as_deref().and_then(|n| n.parent()?.parent()) {
            mounts.push((root.to_owned(), "/__e/node_custom".to_owned(), true));
            maps.push((path_text(root), "/__e/node_custom".to_owned()));
        }
        maps.sort_by_key(|(host, _)| std::cmp::Reverse(host.len()));

        let mut volumes: Vec<String> = mounts
            .iter()
            .map(|(host, inside, ro)| {
                format!(
                    "{}:{inside}{}",
                    host.display(),
                    if *ro { ":ro" } else { "" }
                )
            })
            .collect();
        let sock = Path::new("/var/run/docker.sock");
        if sock.exists() {
            volumes.push("/var/run/docker.sock:/var/run/docker.sock".to_owned());
        }
        volumes.extend(job.volumes.iter().cloned());

        let workdir = args::translate_path(&layout.workspace, &maps);
        let mut env = BTreeMap::from([
            ("GITHUB_ACTIONS".to_owned(), "true".to_owned()),
            ("CI".to_owned(), "true".to_owned()),
        ]);
        env.extend(job.env.clone());
        let (flags, env_file) = self.env_args(env)?;
        let name = format!("hatchrun_job_{}", random_name()?);
        let label = self.docker.label.clone();
        self.say(
            sink,
            Line::Note(&format!("Starting job container ({})", job.image)),
        );
        let create = Create {
            name: &name,
            label: &label,
            network: Some(network),
            workdir: Some(&workdir),
            env: flags,
            ports: job.ports.clone(),
            volumes: volumes.clone(),
            options: job.options.clone(),
            // Last, so it wins over an `--entrypoint` in the options.
            entrypoint: Some("tail"),
            image: &job.image,
            command: vec!["-f".to_owned(), "/dev/null".to_owned()],
            ..Create::default()
        };
        self.create_and_start(&create, &job.volumes, env_file.as_deref())
            .await?;

        // What the image's shell setup is like decides the default shell
        // and where `GITHUB_PATH` entries go.
        let has_bash = self
            .exec_probe(&name, "command -v bash >/dev/null 2>&1 && echo yes")
            .await
            .is_some_and(|out| out.trim() == "yes");
        let path = self
            .exec_probe(&name, "echo \"$PATH\"")
            .await
            .map_or_else(|| DEFAULT_PATH.to_owned(), |out| out.trim().to_owned());
        self.load_host_env()?;
        self.docker.maps = maps;
        self.docker.job = Some(JobContainer {
            name: name.clone(),
            has_bash,
            path,
        });
        Ok(name)
    }

    /// Reads the environment file the agent was given, so variables from
    /// it are left out of what containers get.
    fn load_host_env(&mut self) -> Result<(), String> {
        self.docker.host_env = envfile::read(&self.opts.env_file)
            .map_err(|e| format!("{e:#}"))?
            .into_iter()
            .collect();
        Ok(())
    }

    /// Runs a one-line `sh` script in the container and returns what it
    /// printed, or `None` when it could not run.
    async fn exec_probe(&mut self, name: &str, script: &str) -> Option<String> {
        let out = self
            .docker_capture(
                &[
                    "exec".to_owned(),
                    name.to_owned(),
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                ],
                None,
            )
            .await
            .ok()?;
        (out.code == 0).then_some(out.stdout)
    }

    /// Waits until a service's health check passes. A service without
    /// one is ready at once. A wait doubles from `minute / 30` (2
    /// seconds) up to [`HEALTH_TRIES`] checks.
    async fn wait_healthy(&mut self, service: &Service, sink: &mut dyn Sink) -> Result<(), String> {
        let base = self.opts.minute / 30;
        for attempt in 0..HEALTH_TRIES {
            let out = self
                .docker_ok(&[
                    "inspect".to_owned(),
                    "--format".to_owned(),
                    args::HEALTH_FORMAT.to_owned(),
                    service.name.clone(),
                ])
                .await?;
            match args::parse_health(&out) {
                Health::None => return Ok(()),
                Health::Healthy => {
                    self.say(
                        sink,
                        Line::Note(&format!("Service {} is healthy", service.id)),
                    );
                    return Ok(());
                }
                Health::Unhealthy => break,
                Health::Starting => {
                    if attempt + 1 < HEALTH_TRIES {
                        self.say(
                            sink,
                            Line::Note(&format!(
                                "Waiting for service {} to become healthy",
                                service.id
                            )),
                        );
                        let mut cancel = super::wait_for(self.cancel_requests.clone(), 1);
                        tokio::select! {
                            () = tokio::time::sleep(base * 2u32.pow(attempt)) => {}
                            () = &mut cancel => return Err(CANCELED.to_owned()),
                        }
                    }
                }
            }
        }
        let logs = self
            .docker_capture(
                &[
                    "logs".to_owned(),
                    "--tail".to_owned(),
                    LOG_TAIL.to_owned(),
                    service.name.clone(),
                ],
                None,
            )
            .await
            .map(|out| format!("{}{}", out.stdout, out.stderr))
            .unwrap_or_default();
        for line in logs.lines() {
            self.say(sink, Line::Note(line));
        }
        Err(format!("Service {} did not become healthy", service.id))
    }

    /// Sets the contexts the job's containers add: `job.container`,
    /// `job.services` and, inside a container, the paths the steps see.
    fn publish_contexts(&mut self, job: Option<&str>, services: &[Service], network: &str) {
        let container = job.map(|name| json!({ "id": name, "network": network }));
        let mut service_map = serde_json::Map::new();
        for service in services {
            service_map.insert(
                service.id.clone(),
                json!({
                    "id": service.name,
                    "network": network,
                    "ports": service.ports,
                }),
            );
        }
        self.ctx
            .set_containers(container, Value::Object(service_map));
    }

    /// The contexts a step that runs in the job container expands its
    /// strings with: `github.workspace` and the `runner` paths are the
    /// container's. Steps that run on the machine (cache, workspace,
    /// test results) and `if:` keep the machine's paths.
    pub(super) fn step_ctx(&self) -> super::context::Contexts {
        let mut ctx = self.ctx.clone();
        if self.docker.contained() {
            let maps = self.docker.maps.clone();
            ctx.set_paths(&self.layout, |p| args::translate_path(p, &maps));
        }
        ctx
    }

    /// The command line that runs `program args` in the job container:
    /// `docker exec` with the working directory and the step's
    /// environment translated to the paths the container sees. Without a
    /// job container the process runs on the machine as given.
    pub(super) fn launch_for(
        &mut self,
        program: &str,
        argv: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        step_keys: &[String],
    ) -> Result<Exec, String> {
        let Some(job) = &self.docker.job else {
            return Ok(Exec {
                program: program.to_owned(),
                args: argv.to_vec(),
                cwd: cwd.to_owned(),
                env: env.clone(),
                as_agent: false,
                env_file: None,
            });
        };
        let maps = self.docker.maps.clone();
        let explicit = self.explicit_keys(step_keys);
        let inner = self.container_env(env, &maps, Some(&job.path), &explicit);
        let name = job.name.clone();
        let (mut flags, env_file) = self.env_args(inner)?;
        let docker_env = self.docker_env()?;
        let mut line = vec![
            "exec".to_owned(),
            "--workdir".to_owned(),
            args::translate_path(cwd, &maps),
        ];
        line.append(&mut flags);
        line.push(name);
        line.push(args::translate(program, &maps));
        line.extend(argv.iter().map(|a| args::translate(a, &maps)));
        Ok(Exec {
            program: self.docker_bin()?.to_string_lossy().into_owned(),
            args: line,
            cwd: self.layout.work.clone(),
            env: docker_env,
            as_agent: true,
            env_file,
        })
    }

    /// The variables the job assigned itself: its `env`, `GITHUB_ENV`
    /// and the composite actions' `env`, with `step_keys`, the step's
    /// own.
    pub(super) fn explicit_keys(&self, step_keys: &[String]) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = self.env_context().into_keys().collect();
        keys.extend(step_keys.iter().cloned());
        keys
    }

    /// Whether the job container sees `path`: it is under a directory the
    /// container mounts. A path outside them does not exist there.
    pub(super) fn mounted_in_container(&self, path: &Path) -> bool {
        self.docker
            .maps
            .iter()
            .any(|(host, _)| path.starts_with(host))
    }

    /// What a step's environment becomes inside the job container: the
    /// runner's variables with paths translated, without the host's
    /// `HOME`, user and image environment, and a `PATH` of the
    /// container's own with the `GITHUB_PATH` entries in front.
    ///
    /// A variable that equals the environment file's value is left out
    /// as the host's, unless `explicit` names it: the job, the step or
    /// `GITHUB_ENV` assigned it.
    fn container_env(
        &self,
        env: &BTreeMap<String, String>,
        maps: &[(String, String)],
        container_path: Option<&str>,
        explicit: &BTreeSet<String>,
    ) -> BTreeMap<String, String> {
        let host_path = self
            .docker
            .host_env
            .get("PATH")
            .map_or(DEFAULT_PATH, String::as_str);
        let mut inner = BTreeMap::new();
        for (key, value) in env {
            if matches!(key.as_str(), "HOME" | "USER" | "LOGNAME" | "PATH") {
                continue;
            }
            if self.docker.host_env.get(key) == Some(value) && !explicit.contains(key) {
                continue;
            }
            // A container cannot reach a proxy on the loopback: without
            // the variables its tools see no cache, not a dead one.
            if self.cache_unreachable && self.cache_env.contains_key(key) && !explicit.contains(key)
            {
                continue;
            }
            inner.insert(key.clone(), args::translate(value, maps));
        }
        inner.insert("HOME".to_owned(), "/github/home".to_owned());
        let dirs: Vec<String> = self
            .path_dirs
            .iter()
            .rev()
            .map(|d| args::translate(d, maps))
            .collect();
        match env.get("PATH") {
            // A PATH the job or step set itself.
            Some(path)
                if explicit.contains("PATH")
                    || (path != host_path && !is_host_path(path, host_path, &self.path_dirs)) =>
            {
                inner.insert("PATH".to_owned(), args::translate(path, maps));
            }
            _ if !dirs.is_empty() && container_path.is_some() => {
                let path = container_path.unwrap_or_default();
                inner.insert("PATH".to_owned(), format!("{}:{path}", dirs.join(":")));
            }
            _ => {}
        }
        inner
    }

    /// Removes what the job started: containers, volumes, the network,
    /// built images and the login. Problems are logged, never fatal.
    pub(super) async fn stop_containers(&mut self, sink: &mut dyn Sink) {
        self.docker.cleaning = true;
        if !self.docker.has_work() {
            self.remove_login_dir();
            return;
        }
        let index = self.doc.job.steps.len() + 1;
        sink.step_started(index, "Stop containers");
        let label = format!("label={}", self.docker.label);
        // Containers a killed docker action left behind carry the label too.
        let mut names = self.docker.containers.clone();
        if let Ok(found) = self
            .docker_ok(&[
                "ps".to_owned(),
                "--all".to_owned(),
                "--quiet".to_owned(),
                "--filter".to_owned(),
                label.clone(),
            ])
            .await
        {
            names.extend(found.split_whitespace().map(str::to_owned));
        }
        names.reverse();
        names.dedup();
        if !names.is_empty() {
            let mut argv = vec![
                "rm".to_owned(),
                "--force".to_owned(),
                "--volumes".to_owned(),
            ];
            argv.extend(names);
            self.run_cleanup(&argv, "Stopped the containers", sink)
                .await;
        }
        // Only volumes this job made. Anonymous ones went with
        // `rm --volumes`.
        for volume in self.docker.volumes.clone() {
            let argv = ["volume".to_owned(), "rm".to_owned(), volume];
            self.run_cleanup(&argv, "Removed a volume", sink).await;
        }
        if let Some(network) = self.docker.network.take() {
            let argv = ["network".to_owned(), "rm".to_owned(), network];
            self.run_cleanup(&argv, "Removed the network", sink).await;
        }
        if !self.docker.images.is_empty() {
            let mut argv = vec!["rmi".to_owned(), "--force".to_owned()];
            argv.append(&mut self.docker.images);
            self.run_cleanup(&argv, "Removed the built images", sink)
                .await;
        }
        self.remove_login_dir();
        self.docker.job = None;
        sink.step_finished(
            index,
            &StepSummary {
                name: "Stop containers".to_owned(),
                outcome: Conclusion::Success,
                conclusion: Conclusion::Success,
                duration_ms: 0,
            },
        );
    }

    /// Removes the directory `docker login` wrote to.
    fn remove_login_dir(&mut self) {
        if let Some(dir) = self.docker.config.take()
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!("remove {}: {e}", dir.display());
        }
    }

    async fn run_cleanup(&mut self, argv: &[String], done: &str, sink: &mut dyn Sink) {
        match self.docker_ok(argv).await {
            Ok(_) => self.say(sink, Line::Note(done)),
            Err(message) => self.say(sink, Line::Warning(&message)),
        }
    }
}

/// Whether `path` is what the step environment holds when nobody set
/// `PATH`: the image's, with the `GITHUB_PATH` entries in front.
fn is_host_path(path: &str, host_path: &str, dirs: &[String]) -> bool {
    let mut parts: Vec<&str> = dirs.iter().rev().map(String::as_str).collect();
    parts.push(host_path);
    path == parts.join(":")
}

/// Splits a container environment into the text of a Docker env file
/// and the `-e KEY=VALUE` flags for what a file cannot carry. An env file
/// has one `KEY=VALUE` per line and no escapes, so a value with a line
/// break, or a key Docker would misread, goes on the command line, where
/// the runner puts every value. That is the only place a process list
/// can show a value.
fn split_env(env: BTreeMap<String, String>) -> (String, Vec<String>) {
    let mut file = String::new();
    let mut flags = Vec::new();
    for (key, value) in env {
        let plain_key = !key.is_empty()
            && !key.starts_with('#')
            && !key.contains(|c: char| c == '=' || c.is_whitespace() || c == '\0');
        if plain_key && !value.contains(['\n', '\r', '\0']) {
            file.extend([key.as_str(), "=", value.as_str(), "\n"]);
        } else {
            flags.extend(["-e".to_owned(), format!("{key}={value}")]);
        }
    }
    (file, flags)
}

impl Job<'_> {
    /// The flags that give a container its environment: `--env-file`
    /// with a file only the agent can read (in the job's state
    /// directory, mode 0600), and `-e` for the rest. The caller removes
    /// the file as soon as the docker command ends. Nothing from the
    /// workflow goes into the docker client's own environment.
    pub(super) fn env_args(
        &mut self,
        env: BTreeMap<String, String>,
    ) -> Result<(Vec<String>, Option<PathBuf>), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let (text, mut flags) = split_env(env);
        if text.is_empty() {
            return Ok((flags, None));
        }
        let path = self
            .docker_config()?
            .join(format!("env-{}", random_name()?));
        let fail = |e: std::io::Error| format!("write {}: {e}", path.display());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(fail)?;
        if let Err(e) = file.write_all(text.as_bytes()) {
            let _ = std::fs::remove_file(&path);
            return Err(fail(e));
        }
        let mut with_file = vec!["--env-file".to_owned(), path.to_string_lossy().into_owned()];
        with_file.append(&mut flags);
        Ok((with_file, Some(path)))
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// What `docker` printed.
pub(super) struct Captured {
    /// The exit code, or -1 when a signal ended it.
    pub(super) code: i32,
    /// What it wrote to standard output.
    pub(super) stdout: String,
    /// What it wrote to standard error.
    pub(super) stderr: String,
}

/// A process to start for a step, on the machine or through `docker`.
pub(super) struct Exec {
    /// The program to start.
    pub(super) program: String,
    /// Its arguments.
    pub(super) args: Vec<String>,
    /// The directory it starts in.
    pub(super) cwd: PathBuf,
    /// Its whole environment.
    pub(super) env: BTreeMap<String, String>,
    /// It runs as the agent, not the runner user.
    pub(super) as_agent: bool,
    /// The container's variables, for `--env-file`. Removed when the
    /// process has ended.
    pub(super) env_file: Option<PathBuf>,
}

impl Exec {
    /// Removes the environment file.
    pub(super) fn cleanup(&self) {
        remove_env_file(self.env_file.as_deref());
    }
}

/// Removes a container environment file. A file that is gone is fine.
fn remove_env_file(path: Option<&Path>) {
    if let Some(path) = path
        && let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("remove {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_executable_on_the_path_is_the_command() {
        use std::os::unix::fs::PermissionsExt;
        let make = |dir: &Path, mode: u32| {
            let file = dir.join("podman");
            std::fs::write(&file, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let plain = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let good = tempfile::tempdir().unwrap();
        let later = tempfile::tempdir().unwrap();
        make(plain.path(), 0o644);
        std::fs::create_dir(folder.path().join("podman")).unwrap();
        make(good.path(), 0o755);
        make(later.path(), 0o755);
        let dirs = [
            plain.path().to_owned(),
            folder.path().to_owned(),
            good.path().to_owned(),
            later.path().to_owned(),
        ];
        assert_eq!(
            first_executable(Path::new("podman"), &dirs),
            Some(good.path().join("podman"))
        );
        assert_eq!(first_executable(Path::new("docker"), &dirs), None);
    }

    #[test]
    fn a_login_directory_alone_is_no_work_to_clean_up() {
        // The cache proxy's bridge lookup makes the directory; a job
        // without containers must not get a "Stop containers" step.
        let mut state = DockerState::new("hatchrun-x".to_owned());
        state.config = Some(PathBuf::from("/state/docker-x"));
        assert!(!state.has_work());
        state.network = Some("n".to_owned());
        assert!(state.has_work());
    }

    #[test]
    fn env_goes_in_a_file_unless_a_file_cannot_carry_it() {
        let env = BTreeMap::from([
            ("SECRET".to_owned(), "hunter2".to_owned()),
            ("PATH".to_owned(), "/x".to_owned()),
            ("MULTI".to_owned(), "a\nb".to_owned()),
            ("A B".to_owned(), "v".to_owned()),
        ]);
        let (file, flags) = split_env(env);
        assert_eq!(file, "PATH=/x\nSECRET=hunter2\n");
        assert_eq!(flags, ["-e", "A B=v", "-e", "MULTI=a\nb"]);
    }
}
