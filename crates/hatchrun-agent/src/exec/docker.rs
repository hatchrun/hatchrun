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
        self.network.is_some()
            || !self.containers.is_empty()
            || !self.images.is_empty()
            || self.config.is_some()
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

impl Job<'_> {
    /// The Docker command, from `--docker` or the first `docker` on the
    /// agent's `PATH` or the default one.
    fn docker_bin(&mut self) -> Result<PathBuf, String> {
        if let Some(bin) = &self.docker.bin {
            return Ok(bin.clone());
        }
        let bin = if let Some(bin) = &self.opts.docker {
            bin.clone()
        } else {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            dirs.extend(std::env::split_paths(DEFAULT_PATH));
            dirs.into_iter()
                .map(|dir| dir.join("docker"))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| {
                    "Docker is not installed: this job needs a `docker` command".to_owned()
                })?
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
        let bin = self.docker_bin()?;
        let env = self.docker_env()?;
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
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())
                .await
                .map_err(|e| format!("write to docker: {e}"))?;
            drop(pipe);
        }
        let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "docker did not finish in time".to_owned())?
            .map_err(|e| format!("wait for docker: {e}"))?;
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
                timeout: None,
                timeout_minutes: None,
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
        if let Some((username, password)) = &c.credentials {
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
            env.insert(key.clone(), eval(value, &format!("env.{key}"))?);
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
        let outcome = if result.is_ok() {
            Conclusion::Success
        } else {
            Conclusion::Failure
        };
        if let Err(message) = &result {
            self.say(sink, Line::Error(message));
            self.set_status(hatchrun_workflow::JobStatus::Failure);
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

        let network = format!("hatchrun_network_{}", random_name()?);
        self.say(sink, Line::Note(&format!("Creating network {network}")));
        self.docker_ok(&[
            "network".to_owned(),
            "create".to_owned(),
            "--label".to_owned(),
            self.docker.label.clone(),
            network.clone(),
        ])
        .await?;
        self.docker.network = Some(network.clone());

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
            let create = Create {
                name: &name,
                label: &label,
                network: Some(&network),
                alias: Some(id),
                env: service
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                ports: service.ports.clone(),
                volumes: service.volumes.clone(),
                options: service.options.clone(),
                image: &service.image,
                ..Create::default()
            };
            let port_map = self.create_and_start(&create, &service.volumes).await?;
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
    ) -> Result<BTreeMap<String, String>, String> {
        for volume in volumes {
            if let Some(name) = args::named_volume(volume)
                && !self.docker.volumes.iter().any(|v| v == name)
            {
                self.docker.volumes.push(name.to_owned());
            }
        }
        let mut argv = vec!["create".to_owned()];
        argv.extend(create.args());
        self.docker.containers.push(create.name.to_owned());
        self.docker_ok(&argv).await?;
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
        let mut env: Vec<(String, String)> = vec![
            ("GITHUB_ACTIONS".to_owned(), "true".to_owned()),
            ("CI".to_owned(), "true".to_owned()),
        ];
        env.extend(job.env.iter().map(|(k, v)| (k.clone(), v.clone())));
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
            env,
            ports: job.ports.clone(),
            volumes: volumes.clone(),
            options: job.options.clone(),
            // Last, so it wins over an `--entrypoint` in the options.
            entrypoint: Some("tail"),
            image: &job.image,
            command: vec!["-f".to_owned(), "/dev/null".to_owned()],
            ..Create::default()
        };
        self.create_and_start(&create, &job.volumes).await?;

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
                        tokio::time::sleep(base * 2u32.pow(attempt)).await;
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
        if self.docker.contained() {
            let maps = self.docker.maps.clone();
            self.ctx
                .set_paths(&self.layout, |p| args::translate_path(p, &maps));
        }
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
    ) -> Result<Exec, String> {
        let Some(job) = &self.docker.job else {
            return Ok(Exec {
                program: program.to_owned(),
                args: argv.to_vec(),
                cwd: cwd.to_owned(),
                env: env.clone(),
                as_agent: false,
            });
        };
        let maps = self.docker.maps.clone();
        let inner = self.container_env(env, &maps, Some(&job.path));
        let (mut flags, cli_env) = env_flags(inner);
        let name = job.name.clone();
        let mut docker_env = self.docker_env()?;
        docker_env.extend(cli_env);
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
        })
    }

    /// What a step's environment becomes inside the job container: the
    /// runner's variables with paths translated, without the host's
    /// `HOME`, user and image environment, and a `PATH` of the
    /// container's own with the `GITHUB_PATH` entries in front.
    fn container_env(
        &self,
        env: &BTreeMap<String, String>,
        maps: &[(String, String)],
        container_path: Option<&str>,
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
            if self.docker.host_env.get(key) == Some(value) {
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
            Some(path) if path != host_path && !is_host_path(path, host_path, &self.path_dirs) => {
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
        if !self.docker.has_work() {
            return;
        }
        let index = self.doc.job.steps.len();
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
        for volume in self.docker.volumes.clone() {
            let argv = [
                "volume".to_owned(),
                "rm".to_owned(),
                "--force".to_owned(),
                volume,
            ];
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
        if let Some(dir) = self.docker.config.take()
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!("remove {}: {e}", dir.display());
        }
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

/// `-e` flags for a container environment. A value goes through the
/// docker command's own environment, so a secret never shows in a
/// process list; `PATH`, `HOME` and `DOCKER_*` would change how the
/// docker command itself runs, so those go on the command line.
fn env_flags(env: BTreeMap<String, String>) -> (Vec<String>, BTreeMap<String, String>) {
    let mut flags = Vec::new();
    let mut cli = BTreeMap::new();
    for (key, value) in env {
        flags.push("-e".to_owned());
        if matches!(key.as_str(), "PATH" | "HOME") || key.starts_with("DOCKER_") {
            flags.push(format!("{key}={value}"));
        } else {
            flags.push(key.clone());
            cli.insert(key, value);
        }
    }
    (flags, cli)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// What `docker` printed.
pub(super) struct Captured {
    pub(super) code: i32,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// A process to start for a step, on the machine or through `docker`.
pub(super) struct Exec {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) env: BTreeMap<String, String>,
    /// It runs as the agent, not the runner user.
    pub(super) as_agent: bool,
}
