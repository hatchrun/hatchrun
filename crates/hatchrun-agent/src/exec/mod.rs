// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-agent exec`: runs one job from a [`JobDocument`] on this
//! machine, without GitHub's runner.

pub mod action;
pub mod archive;
pub mod builtin;
pub mod commands;
pub mod composite;
pub mod context;
pub mod files;
pub mod layout;
pub mod lines;
pub mod mask;
pub mod node;
pub mod shell;
pub mod sink;
pub mod step;
pub mod store;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hatchrun_workflow::{
    Annotation, Conclusion, ExprError, JobDocument, JobKind, JobResult, JobStatus, PlannedStep,
    RunStep, StepSummary, interpolate,
};

use self::context::{Contexts, Runtime, condition};
use self::files::StepFiles;
use self::layout::Layout;
use self::mask::Masker;
use self::sink::Sink;
use self::step::{Budget, Exit, Process, StepOutput, spawn};
use crate::envfile;
use crate::runner::{DEFAULT_PATH, home_of};

/// How long a cancelled job may keep running steps that still run,
/// such as `always()` steps and `post` hooks, in `--minute`s.
const GRACE_MINUTES: u32 = 5;

/// How the agent runs a job.
pub struct Options {
    /// The work directory, laid out like GitHub's hosted runners.
    pub work_dir: PathBuf,
    /// The user steps run as. `None` keeps the agent's user.
    pub runner_user: Option<String>,
    /// KEY=VALUE file that becomes the steps' environment.
    pub env_file: PathBuf,
    /// The Node binary that runs JavaScript actions.
    pub node: Option<PathBuf>,
    /// The directory that keeps cache, workspace and test result
    /// archives.
    pub store: Option<PathBuf>,
    /// `RUNNER_TOOL_CACHE`.
    pub tool_cache: PathBuf,
    /// How long one `timeout-minutes` minute lasts. Tests shorten it.
    pub minute: Duration,
}

/// Runs the document's job and reports how it went. `cancel` counts the
/// requests to stop: the first cancels the job, which then has
/// [`GRACE_MINUTES`] minutes left for what still runs, and a second
/// kills the running process at once and skips everything left.
///
/// # Errors
///
/// When the work directory cannot be set up.
pub async fn run_job(
    doc: JobDocument,
    opts: &Options,
    sink: &mut dyn Sink,
    cancel: tokio::sync::watch::Receiver<u32>,
) -> Result<JobResult> {
    if matches!(doc.job.kind, JobKind::Approval) {
        bail!("approval jobs do not run on a runner");
    }
    let layout = Layout::new(&opts.work_dir, &doc.github.repository, &opts.tool_cache);
    layout.create()?;
    let event = write_event(&layout, &doc)?;
    StepFiles::reset(&layout.temp)?;
    let owner = opts.runner_user.as_deref().map(owner_of).transpose()?;
    if let Some(owner) = owner {
        let dirs = [
            &layout.work,
            layout.workspace.parent().unwrap_or(&layout.workspace),
            &layout.workspace,
            &layout.temp,
            &layout.actions,
            &layout.temp.join("_github_workflow"),
            &event,
        ];
        for path in dirs {
            // The link itself, never what it points at.
            nix::unistd::fchownat(
                nix::fcntl::AT_FDCWD,
                path,
                Some(owner.0),
                Some(owner.1),
                nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
            )
            .with_context(|| format!("chown {}", path.display()))?;
        }
    }

    // Held for the whole job: restores place files through it, so a
    // step that swaps a parent directory for a link cannot redirect them.
    let workspace_fd = nix::fcntl::open(
        &layout.workspace,
        nix::fcntl::OFlag::O_RDONLY
            | nix::fcntl::OFlag::O_DIRECTORY
            | nix::fcntl::OFlag::O_NOFOLLOW
            | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .with_context(|| format!("open {}", layout.workspace.display()))?;

    let mut masker = Masker::default();
    for secret in doc.secrets.values() {
        masker.add(secret);
    }
    masker.add(&doc.github.token);

    let mut job = Job {
        ctx: Contexts::new(&doc, &layout),
        doc: &doc,
        opts,
        layout,
        event,
        masker,
        annotations: Vec::new(),
        budget: Budget::default(),
        owner,
        github_env: BTreeMap::new(),
        path_dirs: Vec::new(),
        status: JobStatus::Success,
        cancel_requests: cancel,
        grace_until: None,
        aborted: false,
        declared_env: BTreeMap::new(),
        env_error: None,
        fetched: BTreeMap::new(),
        states: BTreeMap::new(),
        posts: Vec::new(),
        scope_env: BTreeMap::new(),
        depth: 0,
        composite_runs: 0,
        next_slot: 3 * doc.job.steps.len(),
        workspace_fd,
        cache_saves: Vec::new(),
    };
    job.start();

    // GitHub runs every `pre` before the first step, in step order.
    job.pre_phase(sink).await;
    let mut summaries = Vec::new();
    for (index, step) in doc.job.steps.iter().enumerate() {
        if job.aborted {
            summaries.push(job.skip(index, step, sink));
        } else {
            summaries.push(job.step(index, step, sink).await);
        }
    }
    job.post_phase(sink).await;
    job.save_caches(sink).await;

    let conclusion = match job.status {
        JobStatus::Success => Conclusion::Success,
        JobStatus::Failure => Conclusion::Failure,
        JobStatus::Cancelled => Conclusion::Cancelled,
    };
    Ok(JobResult {
        conclusion,
        outputs: BTreeMap::new(),
        steps: summaries,
        annotations: job.annotations,
    })
}

/// The user's uid and gid, for handing step files to it.
fn owner_of(name: &str) -> Result<(nix::unistd::Uid, nix::unistd::Gid)> {
    let user = nix::unistd::User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("user {name} does not exist"))?;
    Ok((user.uid, user.gid))
}

/// Writes the webhook payload where `GITHUB_EVENT_PATH` points.
fn write_event(layout: &Layout, doc: &JobDocument) -> Result<PathBuf> {
    let dir = layout.temp.join("_github_workflow");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("event.json");
    let text = serde_json::to_vec(&doc.event).context("serialize the event")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// The state of a running job.
struct Job<'a> {
    doc: &'a JobDocument,
    opts: &'a Options,
    layout: Layout,
    event: PathBuf,
    ctx: Contexts,
    masker: Masker,
    annotations: Vec<Annotation>,
    budget: Budget,
    /// The runner user's ids, when steps run as one.
    owner: Option<(nix::unistd::Uid, nix::unistd::Gid)>,
    /// `GITHUB_ENV` values so far.
    github_env: BTreeMap<String, String>,
    /// `GITHUB_PATH` directories so far, oldest first.
    path_dirs: Vec<String>,
    status: JobStatus,
    /// How many times the agent was asked to stop.
    cancel_requests: tokio::sync::watch::Receiver<u32>,
    /// When the grace period after the first cancel ends.
    grace_until: Option<Instant>,
    /// A second cancel arrived: nothing else runs.
    aborted: bool,
    /// The job's `env` with expressions replaced.
    declared_env: BTreeMap<String, String>,
    /// Why `declared_env` could not be built. Every step then fails with it.
    env_error: Option<String>,
    /// What the job-start fetch found for each `uses:` step.
    fetched: BTreeMap<usize, Result<action::Action, String>>,
    /// `GITHUB_STATE` pairs per step, for its `post` as `STATE_*`.
    states: BTreeMap<usize, BTreeMap<String, String>>,
    /// `post` hooks that are due, in the order they were queued.
    posts: Vec<node::Post>,
    /// The `env` of the composite action steps that are running, outer
    /// ones first. Empty outside composite actions.
    scope_env: BTreeMap<String, String>,
    /// How many composite actions the running step is inside.
    depth: usize,
    /// How many composite actions the job has started.
    composite_runs: usize,
    /// The next file command slot for a step inside a composite action.
    /// Real steps and their `pre` and `post` use the slots below it.
    next_slot: usize,
    /// The workspace directory, opened before any step ran.
    workspace_fd: std::os::fd::OwnedFd,
    /// Caches to save when the job ends, in step order.
    cache_saves: Vec<builtin::CacheSave>,
}

/// What a step needs to start.
struct Prepared {
    shell: shell::Shell,
    script: PathBuf,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    timeout: Option<Duration>,
    files: StepFiles,
}

/// The step fields that every kind of step has.
struct Common {
    name: String,
    id: Option<String>,
    if_: Option<String>,
    continue_on_error: bool,
}

fn interp(
    text: &str,
    ctx: &Contexts,
    workspace: &Path,
    status: JobStatus,
) -> Result<String, ExprError> {
    interpolate(text, ctx.value(), &Runtime { workspace, status })
}

impl Job<'_> {
    /// Interpolates the job's `env` and sets the `env` context.
    fn start(&mut self) {
        let mut env = BTreeMap::new();
        for (key, value) in &self.doc.job.env {
            match interp(value, &self.ctx, &self.layout.workspace, self.status) {
                Ok(text) => {
                    env.insert(key.clone(), text);
                }
                Err(err) => {
                    self.env_error = Some(format!("env {key}: {err}"));
                    break;
                }
            }
        }
        self.declared_env = env;
        self.sync_env();
    }

    /// Sets the `env` context to the job's env and `GITHUB_ENV` values.
    fn sync_env(&mut self) {
        self.ctx.set_env(&self.env_context());
    }

    fn env_context(&self) -> BTreeMap<String, String> {
        let mut env = self.declared_env.clone();
        env.extend(self.github_env.clone());
        env.extend(self.scope_env.clone());
        env
    }

    /// A file command slot nothing else uses.
    fn fresh_slot(&mut self) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        slot
    }

    fn set_status(&mut self, status: JobStatus) {
        self.status = status;
        self.ctx.set_status(status);
    }

    async fn step(&mut self, index: usize, step: &PlannedStep, sink: &mut dyn Sink) -> StepSummary {
        let started = Instant::now();
        let (common, outcome, outputs) = match step {
            PlannedStep::Run(run) => self.run_step(index, run, sink).await,
            PlannedStep::Uses(uses) => self.uses_step(index, uses, sink).await,
            PlannedStep::Cache(cache) => self.cache_step(index, cache, sink).await,
            PlannedStep::Workspace(workspace) => self.workspace_step(index, workspace, sink).await,
            PlannedStep::TestResults { path } => self.test_results_step(index, path, sink).await,
        };
        let conclusion = self.settle(outcome, common.continue_on_error);
        if let Some(id) = &common.id {
            self.ctx.record_step(id, outcome, conclusion, &outputs);
        }
        let summary = StepSummary {
            name: self.masker.mask(&common.name).into_owned(),
            outcome,
            conclusion,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        sink.step_finished(index, &summary);
        summary
    }

    /// A step that does not run because the job was stopped for good.
    fn skip(&mut self, index: usize, step: &PlannedStep, sink: &mut dyn Sink) -> StepSummary {
        let name = match step {
            PlannedStep::Run(run) => run
                .name
                .clone()
                .unwrap_or_else(|| format!("Run {}", run.run.lines().next().unwrap_or_default())),
            PlannedStep::Uses(uses) => uses
                .name
                .clone()
                .unwrap_or_else(|| format!("Run {}", uses.uses)),
            PlannedStep::Cache(_) => "Cache".to_owned(),
            PlannedStep::Workspace(_) => "Workspace".to_owned(),
            PlannedStep::TestResults { .. } => "Test results".to_owned(),
        };
        let name = self.masker.mask(&name).into_owned();
        sink.step_started(index, &name);
        let summary = StepSummary {
            name,
            outcome: Conclusion::Skipped,
            conclusion: Conclusion::Skipped,
            duration_ms: 0,
        };
        sink.step_finished(index, &summary);
        summary
    }

    /// The conclusion of a step with `outcome`, and its effect on the
    /// job's status.
    fn settle(&mut self, outcome: Conclusion, continue_on_error: bool) -> Conclusion {
        let conclusion = if outcome == Conclusion::Failure && continue_on_error {
            Conclusion::Success
        } else {
            outcome
        };
        match (outcome, conclusion) {
            (Conclusion::Cancelled, _) => self.set_status(JobStatus::Cancelled),
            (_, Conclusion::Failure) if self.status != JobStatus::Cancelled => {
                self.set_status(JobStatus::Failure);
            }
            _ => {}
        }
        conclusion
    }

    /// Prints a line the agent wrote for the current step.
    fn say(&mut self, sink: &mut dyn Sink, line: Line<'_>) {
        let mut out = StepOutput::new(
            &mut self.masker,
            sink,
            &mut self.annotations,
            &mut self.budget,
        );
        match line {
            Line::Error(text) => out.error(text),
            Line::Warning(text) => out.warning(text),
            Line::Note(text) => out.line(text),
        }
    }

    /// Whether a step's `if:` lets it run.
    fn runs(&self, if_: Option<&str>) -> Result<bool, ExprError> {
        let rt = Runtime {
            workspace: &self.layout.workspace,
            status: self.status,
        };
        condition(if_, &self.ctx, &rt)
    }

    async fn run_step(
        &mut self,
        index: usize,
        step: &RunStep,
        sink: &mut dyn Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let workspace = self.layout.workspace.clone();
        let mut name_error = None;
        let name = match &step.name {
            Some(name) => interp(name, &self.ctx, &workspace, self.status).unwrap_or_else(|err| {
                name_error = Some(format!("name: {err}"));
                name.clone()
            }),
            None => format!("Run {}", step.run.lines().next().unwrap_or_default()),
        };
        let common = Common {
            name,
            id: step.id.clone(),
            if_: step.if_.clone(),
            continue_on_error: step.continue_on_error,
        };
        sink.step_started(index, &self.masker.mask(&common.name));
        if let Some(message) = name_error {
            StepOutput::new(
                &mut self.masker,
                sink,
                &mut self.annotations,
                &mut self.budget,
            )
            .warning(&message);
        }

        match self.runs(common.if_.as_deref()) {
            Ok(true) => {}
            Ok(false) => return (common, Conclusion::Skipped, BTreeMap::new()),
            Err(err) => {
                let mut out = StepOutput::new(
                    &mut self.masker,
                    sink,
                    &mut self.annotations,
                    &mut self.budget,
                );
                out.error(&format!("if: {err}"));
                return (common, Conclusion::Failure, BTreeMap::new());
            }
        }

        let prepared = match &self.env_error {
            Some(message) => Err(message.clone()),
            None => prepare(self, index, step),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                self.say(sink, Line::Error(&message));
                return (common, Conclusion::Failure, BTreeMap::new());
            }
        };
        let args = prepared.shell.args_for(&prepared.script.to_string_lossy());
        let ran = self
            .execute(
                Launch {
                    program: &prepared.shell.program,
                    args: &args,
                    cwd: &prepared.cwd,
                    env: &prepared.env,
                    timeout: prepared.timeout,
                    timeout_minutes: step.timeout_minutes,
                    files: &prepared.files,
                },
                sink,
            )
            .await;
        // The script may hold secrets. Nothing needs it after the run.
        let _ = std::fs::remove_file(&prepared.script);
        (common, ran.outcome, ran.outputs)
    }

    /// Runs a process for a step, then reads the file commands it wrote.
    async fn execute(&mut self, launch: Launch<'_>, sink: &mut dyn Sink) -> Ran {
        let mut out = StepOutput::new(
            &mut self.masker,
            sink,
            &mut self.annotations,
            &mut self.budget,
        );
        // After a cancel, whatever still runs shares the grace period.
        let timeout = match self.grace_until {
            Some(end) => {
                let left = end.saturating_duration_since(Instant::now());
                Some(launch.timeout.map_or(left, |t| t.min(left)))
            }
            None => launch.timeout,
        };
        let process = Process {
            program: launch.program,
            args: launch.args,
            cwd: launch.cwd,
            env: launch.env,
            user: self.opts.runner_user.as_deref(),
            timeout,
        };
        let mut cancel: Pin<Box<dyn Future<Output = ()>>> = if self.grace_until.is_some() {
            Box::pin(std::future::pending())
        } else {
            wait_for(self.cancel_requests.clone(), 1)
        };
        let mut abort = wait_for(self.cancel_requests.clone(), 2);
        let spawned = spawn(process, &mut out, &mut cancel, &mut abort).await;
        let grace_over = self.grace_until.is_some_and(|end| Instant::now() >= end);
        let mut outcome = match spawned {
            // The grace period ran out: the job is cancelled, not slow.
            Ok(Exit::TimedOut) if grace_over && launch.timeout.is_none() => {
                exit_outcome(&mut out, Ok(Exit::Cancelled), None)
            }
            other => exit_outcome(&mut out, other, launch.timeout_minutes),
        };
        if outcome == Conclusion::Cancelled {
            if *self.cancel_requests.borrow() >= 2 {
                self.aborted = true;
            }
            if self.grace_until.is_none() {
                self.grace_until = Some(Instant::now() + self.opts.minute * GRACE_MINUTES);
            }
        }
        if out.refused() && outcome == Conclusion::Success {
            outcome = Conclusion::Failure;
        }

        let mut state = Vec::new();
        match files::read(launch.files) {
            Ok(written) => {
                for warning in &written.warnings {
                    out.warning(warning);
                }
                self.github_env.extend(written.env);
                self.path_dirs.extend(written.path);
                state = written.state;
                out.push_outputs(written.outputs);
            }
            Err(err) => {
                out.error(&err.to_string());
                if outcome == Conclusion::Success {
                    outcome = Conclusion::Failure;
                }
            }
        }
        let outputs: BTreeMap<String, String> = out.take_outputs().into_iter().collect();
        self.sync_env();
        Ran {
            outcome,
            outputs,
            state,
        }
    }

    /// Puts the `GITHUB_PATH` directories, newest first, in front of
    /// `env`'s `PATH`.
    fn with_path(&self, env: &mut BTreeMap<String, String>) {
        if self.path_dirs.is_empty() {
            return;
        }
        let mut dirs: Vec<&str> = self.path_dirs.iter().rev().map(String::as_str).collect();
        let path = env.get("PATH").cloned().unwrap_or_default();
        if !path.is_empty() {
            dirs.push(&path);
        }
        let joined = dirs.join(":");
        env.insert("PATH".to_owned(), joined);
    }
}

/// Resolves once `requests` reaches `count`.
fn wait_for(
    mut requests: tokio::sync::watch::Receiver<u32>,
    count: u32,
) -> Pin<Box<dyn Future<Output = ()>>> {
    Box::pin(async move {
        // A closed channel means no more requests will come.
        if requests.wait_for(|n| *n >= count).await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

/// A line the agent prints about a step.
#[derive(Clone, Copy)]
enum Line<'a> {
    Error(&'a str),
    Warning(&'a str),
    Note(&'a str),
}

/// A process to run for a step, with the files it writes to.
struct Launch<'a> {
    program: &'a str,
    args: &'a [String],
    cwd: &'a Path,
    env: &'a BTreeMap<String, String>,
    timeout: Option<Duration>,
    timeout_minutes: Option<u32>,
    files: &'a StepFiles,
}

/// How a launched process went.
struct Ran {
    outcome: Conclusion,
    outputs: BTreeMap<String, String>,
    /// `GITHUB_STATE` pairs it wrote.
    state: Vec<(String, String)>,
}

/// Prints how the process ended and maps it to the step's outcome.
fn exit_outcome(
    out: &mut StepOutput<'_>,
    spawned: Result<Exit>,
    timeout_minutes: Option<u32>,
) -> Conclusion {
    match spawned {
        Ok(Exit::Code(0)) => Conclusion::Success,
        Ok(Exit::Code(code)) => {
            out.error(&format!("Process completed with exit code {code}."));
            Conclusion::Failure
        }
        Ok(Exit::TimedOut) => {
            let minutes = timeout_minutes.unwrap_or_default();
            out.error(&format!("The step timed out after {minutes} minutes."));
            Conclusion::Failure
        }
        Ok(Exit::Cancelled) => {
            out.error("The operation was canceled.");
            Conclusion::Cancelled
        }
        Err(err) => {
            out.error(&format!("{err:#}"));
            Conclusion::Failure
        }
    }
}

/// Resolves everything a `run:` step needs before it starts: files,
/// environment, the script text and the shell.
fn prepare(job: &Job<'_>, index: usize, step: &RunStep) -> Result<Prepared, String> {
    let layout = &job.layout;
    let workspace = &layout.workspace;
    let files = StepFiles::create(&layout.temp, index, job.owner).map_err(|e| format!("{e:#}"))?;

    let mut step_env = BTreeMap::new();
    for (key, value) in &step.env {
        let text = interp(value, &job.ctx, workspace, job.status)
            .map_err(|e| format!("env {key}: {e}"))?;
        step_env.insert(key.clone(), text);
    }
    // The step's own expressions see its env too.
    let mut ctx = job.ctx.clone();
    let mut visible = job.env_context();
    visible.extend(step_env.clone());
    ctx.set_env(&visible);
    let expand = |text: &str, what: &str| {
        interp(text, &ctx, workspace, job.status).map_err(|e| format!("{what}: {e}"))
    };
    let script_text = expand(&step.run, "run")?;
    let cwd = match &step.working_directory {
        Some(dir) => workspace.join(expand(dir, "working-directory")?),
        None => workspace.clone(),
    };
    let shell_name = match &step.shell {
        Some(shell) => Some(expand(shell, "shell")?),
        None => None,
    };

    let mut env = base_env(job, &files, &step_action(step, index))?;
    env.extend(job.github_env.clone());
    env.extend(job.declared_env.clone());
    env.extend(job.scope_env.clone());
    env.extend(step_env);
    job.with_path(&mut env);

    let has_bash = env.get("PATH").is_some_and(|path| {
        path.split(':')
            .any(|dir| Path::new(dir).join("bash").is_file())
    });
    let shell = shell::resolve(shell_name.as_deref(), has_bash)?;
    let script = layout
        .temp
        .join(format!("{}.{}", random_name()?, shell.extension));
    write_script(&script, &script_text, job.owner)?;

    Ok(Prepared {
        shell,
        script,
        cwd,
        env,
        timeout: step
            .timeout_minutes
            .map(|minutes| job.opts.minute * minutes),
        files,
    })
}

/// Writes the script readable by its owner only. A script the step
/// cannot read is removed again.
fn write_script(
    path: &Path,
    text: &str,
    owner: Option<(nix::unistd::Uid, nix::unistd::Gid)>,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let fail = |e: &dyn std::fmt::Display| format!("write {}: {e}", path.display());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| fail(&e))?;
    let written = file
        .write_all(text.as_bytes())
        .map_err(|e| fail(&e))
        .and_then(|()| match owner {
            Some((uid, gid)) => {
                nix::unistd::chown(path, Some(uid), Some(gid)).map_err(|e| fail(&e))
            }
            None => Ok(()),
        });
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// `GITHUB_ACTION` of a `run:` step: its id, else a generated name.
fn step_action(step: &RunStep, index: usize) -> String {
    match (&step.id, index) {
        (Some(id), _) => id.clone(),
        (None, 0) => "__run".to_owned(),
        (None, n) => format!("__run_{}", n + 1),
    }
}

/// The environment file's variables, then the ones the runner sets.
fn base_env(
    job: &Job<'_>,
    files: &StepFiles,
    action: &str,
) -> Result<BTreeMap<String, String>, String> {
    let doc = job.doc;
    let g = &doc.github;
    let layout = &job.layout;
    let mut env: BTreeMap<String, String> = envfile::read(&job.opts.env_file)
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .collect();
    env.entry("PATH".to_owned())
        .or_insert_with(|| DEFAULT_PATH.to_owned());
    let (user, home) = match &job.opts.runner_user {
        Some(name) => (name.clone(), home_of(name).map_err(|e| format!("{e:#}"))?),
        None => (
            std::env::var("USER").unwrap_or_default(),
            std::env::var("HOME").unwrap_or_default(),
        ),
    };
    env.insert("HOME".to_owned(), home);
    env.insert("LOGNAME".to_owned(), user.clone());
    env.insert("USER".to_owned(), user);

    let arch = job.ctx.value()["runner"]["arch"]
        .as_str()
        .unwrap_or("X64")
        .to_owned();
    let runner_workspace = layout
        .workspace
        .parent()
        .unwrap_or(&layout.workspace)
        .to_string_lossy()
        .into_owned();
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let vars = [
        ("CI", "true".to_owned()),
        ("GITHUB_ACTIONS", "true".to_owned()),
        ("GITHUB_ACTION", action.to_owned()),
        ("GITHUB_ACTOR", g.actor.clone()),
        ("GITHUB_API_URL", g.api_url.clone()),
        ("GITHUB_BASE_REF", g.base_ref.clone()),
        ("GITHUB_ENV", path(&files.env)),
        ("GITHUB_EVENT_NAME", doc.event_name.clone()),
        ("GITHUB_EVENT_PATH", path(&job.event)),
        ("GITHUB_GRAPHQL_URL", g.graphql_url.clone()),
        ("GITHUB_HEAD_REF", g.head_ref.clone()),
        ("GITHUB_JOB", doc.job.name.clone()),
        ("GITHUB_OUTPUT", path(&files.output)),
        ("GITHUB_PATH", path(&files.path)),
        ("GITHUB_REF", g.ref_.clone()),
        ("GITHUB_REF_NAME", g.ref_name.clone()),
        ("GITHUB_REF_TYPE", g.ref_type.clone()),
        ("GITHUB_REPOSITORY", g.repository.clone()),
        ("GITHUB_REPOSITORY_OWNER", g.repository_owner.clone()),
        ("GITHUB_RUN_ATTEMPT", doc.run_attempt.to_string()),
        ("GITHUB_RUN_ID", doc.run_id.clone()),
        ("GITHUB_RUN_NUMBER", doc.run_number.to_string()),
        ("GITHUB_SERVER_URL", g.server_url.clone()),
        ("GITHUB_SHA", g.sha.clone()),
        ("GITHUB_STATE", path(&files.state)),
        ("GITHUB_STEP_SUMMARY", path(&files.summary)),
        ("GITHUB_WORKFLOW", doc.workflow.clone()),
        ("GITHUB_WORKSPACE", path(&layout.workspace)),
        ("RUNNER_ARCH", arch),
        ("RUNNER_NAME", "hatchrun".to_owned()),
        ("RUNNER_OS", "Linux".to_owned()),
        ("RUNNER_TEMP", path(&layout.temp)),
        ("RUNNER_TOOL_CACHE", path(&layout.tool_cache)),
        ("RUNNER_WORKSPACE", runner_workspace),
    ];
    env.extend(vars.into_iter().map(|(k, v)| (k.to_owned(), v)));
    Ok(env)
}

/// 32 random hex digits, for file names.
fn random_name() -> Result<String, String> {
    use std::fmt::Write;

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("random name: {e}"))?;
    let mut name = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(name, "{byte:02x}");
    }
    Ok(name)
}
