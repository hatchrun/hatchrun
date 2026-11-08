// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Docker actions: `uses: docker://image` steps and actions whose
//! `action.yml` says `runs.using: docker`.
//!
//! The runner's rules: the image is pulled (`docker://` and registry
//! images) or built from the action's directory (`image: Dockerfile`);
//! the action runs once with `--entrypoint` and the arguments, on the
//! job's network when it has one, with the workspace at
//! `/github/workspace` and the runner's directories beside it.
//! `pre-entrypoint` and `post-entrypoint` run the same image with
//! another entrypoint and no arguments.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use hatchrun_workflow::{Conclusion, UsesStep};

use super::{COMMAND_TIMEOUT, args, path_text};
use crate::exec::action::{Action, ActionRef, Input, Metadata, Runs};
use crate::exec::files::StepFiles;
use crate::exec::sink::Sink;
use crate::exec::{Common, Job, Line, Ran, base_env, interp, random_name};

/// Where a Docker action's image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::exec) enum Image {
    /// An image to pull, such as `alpine:3` from `docker://alpine:3`.
    Pull(String),
    /// A Dockerfile to build, in the action's directory.
    Build {
        /// The action's directory, which is the build context.
        dir: PathBuf,
        /// The Dockerfile, relative to the directory.
        file: String,
    },
}

/// What one run of a Docker action starts.
#[derive(Debug, Clone)]
pub(in crate::exec) struct DockerRun {
    /// Where the image comes from.
    pub(in crate::exec) image: Image,
    /// Replaces the image's entrypoint.
    pub(in crate::exec) entrypoint: Option<String>,
    /// The arguments after the image, one each.
    pub(in crate::exec) args: Vec<String>,
    /// The step's `env`, `INPUT_*` and the action's own `env`.
    pub(in crate::exec) env: BTreeMap<String, String>,
}

/// The paths a Docker action sees, for [`args::translate`]: longest
/// host path first.
fn maps(job: &Job<'_>) -> Vec<(String, String)> {
    let layout = &job.layout;
    let mut maps = vec![
        (
            path_text(&layout.temp.join("_runner_file_commands")),
            "/github/file_commands".to_owned(),
        ),
        (
            path_text(&layout.temp.join("_github_workflow")),
            "/github/workflow".to_owned(),
        ),
        (
            path_text(&layout.temp.join("_github_home")),
            "/github/home".to_owned(),
        ),
        (path_text(&layout.temp), "/github/runner_temp".to_owned()),
        (path_text(&layout.workspace), "/github/workspace".to_owned()),
        (
            path_text(&layout.tool_cache),
            "/github/runner_tool_cache".to_owned(),
        ),
    ];
    maps.sort_by_key(|(host, _)| std::cmp::Reverse(host.len()));
    maps
}

/// The relative path of a Dockerfile, when it stays inside the action.
fn dockerfile(file: &str) -> Result<&str, String> {
    let path = Path::new(file);
    let inside = path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if file.is_empty() || !inside {
        return Err(format!(
            "`image: {file}` must name a Dockerfile inside the action"
        ));
    }
    Ok(file)
}

impl Job<'_> {
    /// Turns an image into a name `docker run` takes: pulls it or builds
    /// the Dockerfile. A repeat of either in one job is skipped.
    async fn resolve_image(
        &mut self,
        image: &Image,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        match image {
            Image::Pull(name) => {
                if !self.docker.pulled.contains(name) {
                    self.say(sink, Line::Note(&format!("Pulling {name}")));
                    let env = self.docker_env()?;
                    let ran = self
                        .docker_stream(
                            vec!["pull".to_owned(), name.clone()],
                            env,
                            None,
                            Some(COMMAND_TIMEOUT),
                            sink,
                        )
                        .await;
                    if ran.outcome != Conclusion::Success {
                        return Err(format!("Failed to pull {name}"));
                    }
                    self.docker.pulled.insert(name.clone());
                }
                Ok(name.clone())
            }
            Image::Build { dir, file } => {
                let key = format!("{}\0{file}", dir.display());
                if let Some(tag) = self.docker.built.get(&key) {
                    return Ok(tag.clone());
                }
                let tag = format!("hatchrun_action_{}:latest", random_name()?);
                let dockerfile = dir.join(dockerfile(file)?);
                self.say(
                    sink,
                    Line::Note(&format!("Building {}", dockerfile.display())),
                );
                let argv = vec![
                    "build".to_owned(),
                    "--label".to_owned(),
                    self.docker.label.clone(),
                    "--tag".to_owned(),
                    tag.clone(),
                    "--file".to_owned(),
                    path_text(&dockerfile),
                    path_text(dir),
                ];
                self.docker.images.push(tag.clone());
                let env = self.docker_env()?;
                let ran = self
                    .docker_stream(argv, env, None, Some(COMMAND_TIMEOUT), sink)
                    .await;
                if ran.outcome != Conclusion::Success {
                    return Err(format!("Failed to build {}", dockerfile.display()));
                }
                self.docker.built.insert(key, tag.clone());
                Ok(tag)
            }
        }
    }

    /// Runs a Docker action's container once, with the file command
    /// files of `slot`, and reads them when it ends. `state_index`
    /// is the step whose `STATE_*` it sees and whose state it adds to.
    #[allow(clippy::too_many_lines)]
    pub(in crate::exec) async fn run_docker(
        &mut self,
        slot: usize,
        state_index: usize,
        action_name: &str,
        run: &DockerRun,
        sink: &mut dyn Sink,
    ) -> Ran {
        let failed = |job: &mut Self, sink: &mut dyn Sink, message: &str| {
            job.say(sink, Line::Error(message));
            Ran {
                outcome: Conclusion::Failure,
                outputs: BTreeMap::new(),
                state: Vec::new(),
            }
        };
        let image = match self.resolve_image(&run.image, sink).await {
            Ok(image) => image,
            Err(message) => return failed(self, sink, &message),
        };
        let files = match StepFiles::create(&self.layout.temp, slot, self.owner) {
            Ok(files) => files,
            Err(err) => return failed(self, sink, &format!("{err:#}")),
        };
        let home = self.layout.temp.join("_github_home");
        if let Err(err) = std::fs::create_dir_all(&home) {
            return failed(self, sink, &format!("create {}: {err}", home.display()));
        }
        if let Some((uid, gid)) = self.owner {
            let _ = nix::unistd::chown(&home, Some(uid), Some(gid));
        }

        // Only the runner's variables and the step's go in, never the
        // host's environment file or its `PATH`.
        if let Err(message) = self.load_host_env() {
            return failed(self, sink, &message);
        }
        let mut env = match base_env(self, &files, action_name) {
            Ok(env) => env,
            Err(message) => return failed(self, sink, &message),
        };
        env.extend(self.env_context());
        env.extend(run.env.clone());
        if let Some(state) = self.states.get(&state_index) {
            for (key, value) in state {
                env.insert(format!("STATE_{key}"), value.clone());
            }
        }
        env.remove("GITHUB_ACTION_PATH");
        env.remove("RUNNER_WORKSPACE");
        let maps = maps(self);
        let explicit = self.explicit_keys(&run.env.keys().cloned().collect::<Vec<_>>());
        let inner = self.container_env(&env, &maps, None, &explicit);
        let (flags, env_file) = match self.env_args(inner) {
            Ok(args) => args,
            Err(message) => return failed(self, sink, &message),
        };
        let docker_env = match self.docker_env() {
            Ok(env) => env,
            Err(message) => {
                super::remove_env_file(env_file.as_deref());
                return failed(self, sink, &message);
            }
        };

        let layout = self.layout.clone();
        let name = match random_name() {
            Ok(name) => format!("hatchrun_action_{name}"),
            Err(message) => {
                super::remove_env_file(env_file.as_deref());
                return failed(self, sink, &message);
            }
        };
        let mut argv = vec![
            "run".to_owned(),
            "--rm".to_owned(),
            "--name".to_owned(),
            name,
            "--label".to_owned(),
            self.docker.label.clone(),
            "--workdir".to_owned(),
            "/github/workspace".to_owned(),
        ];
        if let Some(network) = &self.docker.network {
            argv.extend(["--network".to_owned(), network.clone()]);
        }
        argv.extend(flags);
        let mut mounts = vec![
            (
                layout.temp.join("_runner_file_commands"),
                "/github/file_commands",
            ),
            (layout.temp.join("_github_workflow"), "/github/workflow"),
            (home, "/github/home"),
            (layout.workspace.clone(), "/github/workspace"),
            (layout.temp.clone(), "/github/runner_temp"),
        ];
        if layout.tool_cache.is_dir() {
            mounts.push((layout.tool_cache.clone(), "/github/runner_tool_cache"));
        }
        for (host, inside) in mounts {
            argv.extend([
                "--volume".to_owned(),
                format!("{}:{inside}", host.display()),
            ]);
        }
        if Path::new("/var/run/docker.sock").exists() {
            argv.extend([
                "--volume".to_owned(),
                "/var/run/docker.sock:/var/run/docker.sock".to_owned(),
            ]);
        }
        if let Some(entrypoint) = &run.entrypoint {
            argv.extend([
                "--entrypoint".to_owned(),
                args::translate(entrypoint, &maps),
            ]);
        }
        argv.push(image);
        // The workspace is `/github/workspace` in the container.
        argv.extend(run.args.iter().map(|a| args::translate(a, &maps)));

        let ran = self
            .docker_stream(argv, docker_env, Some(&files), None, sink)
            .await;
        super::remove_env_file(env_file.as_deref());
        self.states
            .entry(state_index)
            .or_default()
            .extend(ran.state.iter().cloned());
        ran
    }

    /// Evaluates a Docker action's `env` and `args` in the action's
    /// scope, which sees `inputs` and no secrets.
    pub(in crate::exec) fn docker_run(
        &self,
        action: &Action,
        evaluated: &crate::exec::node::Evaluated,
    ) -> Result<DockerRun, String> {
        let Runs::Docker {
            image,
            entrypoint,
            args,
            env,
            ..
        } = &action.meta.runs
        else {
            return Err("not a Docker action".to_owned());
        };
        let mut scope = self.ctx.with_action(evaluated.scope.clone());
        // The action's own `env` and `args` read the step's `env` too.
        let mut visible = self.env_context();
        visible.extend(evaluated.step_env.clone());
        scope.set_env(&visible);
        let ws = &self.layout.workspace;
        let eval = |text: &str, what: &str| {
            interp(text, &scope, ws, self.status).map_err(|e| format!("{what}: {e}"))
        };
        let image = match image.strip_prefix("docker://") {
            Some(name) => Image::Pull(name.to_owned()),
            None => Image::Build {
                dir: action.dir.clone(),
                file: eval(image, "image")?,
            },
        };
        let mut merged = BTreeMap::new();
        for (key, value) in env {
            merged.insert(key.clone(), eval(value, &format!("env {key}"))?);
        }
        // The step's own `env` and the inputs win over the action's.
        merged.extend(evaluated.env.clone());
        let args = args
            .iter()
            .map(|a| eval(a, "args"))
            .collect::<Result<Vec<_>, _>>()?;
        let entrypoint = entrypoint
            .as_ref()
            .map(|e| eval(e, "entrypoint"))
            .transpose()?;
        Ok(DockerRun {
            image,
            entrypoint,
            args,
            env: merged,
        })
    }

    /// The pieces of a `uses: docker://image` step: an action with no
    /// `action.yml`, whose `with:` entries are all inputs, and the
    /// `entrypoint` and `args` among them say what runs.
    pub(in crate::exec) fn docker_image_step(
        &self,
        step: &UsesStep,
        image: &str,
        name: &str,
    ) -> Result<(DockerRun, crate::exec::node::Evaluated), String> {
        let action = Action {
            dir: self.layout.workspace.clone(),
            meta: Metadata {
                name: String::new(),
                inputs: step
                    .with
                    .keys()
                    .map(|key| {
                        (
                            key.clone(),
                            Input {
                                description: String::new(),
                                required: false,
                                default: None,
                            },
                        )
                    })
                    .collect(),
                outputs: BTreeMap::new(),
                runs: Runs::Docker {
                    image: format!("docker://{image}"),
                    entrypoint: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    pre_entrypoint: None,
                    pre_if: None,
                    post_entrypoint: None,
                    post_if: None,
                },
            },
        };
        let r = ActionRef::Docker {
            image: image.to_owned(),
        };
        let mut evaluated = self.evaluate(step, &action, &r, name)?;
        evaluated.env.remove("GITHUB_ACTION_PATH");
        let input = |key: &str| {
            evaluated
                .scope
                .inputs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
        };
        let args = match input("args") {
            Some(text) => hatchrun_workflow::container::split_options(&text)
                .map_err(|e| format!("with args: {e}"))?,
            None => Vec::new(),
        };
        let run = DockerRun {
            image: Image::Pull(image.to_owned()),
            entrypoint: input("entrypoint").filter(|e| !e.is_empty()),
            args,
            env: evaluated.env.clone(),
        };
        Ok((run, evaluated))
    }

    /// Runs a Docker action's `uses:` step: the main container, with its
    /// `post-entrypoint` queued. Returns what the step reports.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::exec) async fn docker_step(
        &mut self,
        index: usize,
        step: &UsesStep,
        name: &str,
        prepared: (DockerRun, crate::exec::node::Evaluated),
        post: Option<(String, Option<String>)>,
        common: Common,
        sink: &mut dyn Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let (run, evaluated) = prepared;
        for warning in &evaluated.warnings {
            self.say(sink, Line::Warning(warning));
        }
        if let Some((entrypoint, condition)) = post {
            let mut post_run = run.clone();
            post_run.entrypoint = Some(entrypoint);
            post_run.args = Vec::new();
            self.queue_post(crate::exec::node::Post::docker(
                index,
                step.uses.clone(),
                name.to_owned(),
                post_run,
                condition,
                step.continue_on_error,
            ));
        }
        let ran = self.run_docker(index, index, name, &run, sink).await;
        (common, ran.outcome, ran.outputs)
    }
}
