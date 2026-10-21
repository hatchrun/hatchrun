// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-agent exec`: runs one job from a [`JobDocument`] on this
//! machine, without GitHub's runner.

pub mod commands;
pub mod context;
pub mod files;
pub mod layout;
pub mod lines;
pub mod mask;
pub mod shell;
pub mod sink;
pub mod step;

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

/// How the agent runs a job.
// The node binary and the store are read once actions run.
#[allow(dead_code)]
pub struct Options {
    /// The work directory, laid out like GitHub's hosted runners.
    pub work_dir: PathBuf,
    /// The user steps run as. `None` keeps the agent's user.
    pub runner_user: Option<String>,
    /// KEY=VALUE file that becomes the steps' environment.
    pub env_file: PathBuf,
    /// The Node binary that runs JavaScript actions.
    pub node: Option<PathBuf>,
    /// A directory of prefetched actions.
    pub store: Option<PathBuf>,
    /// `RUNNER_TOOL_CACHE`.
    pub tool_cache: PathBuf,
    /// How long one `timeout-minutes` minute lasts. Tests shorten it.
    pub minute: Duration,
}

/// Runs the document's job and reports how it went. `cancel` resolves
/// when the agent is asked to stop.
///
/// # Errors
///
/// When the work directory cannot be set up.
pub async fn run_job(
    doc: JobDocument,
    opts: &Options,
    sink: &mut dyn Sink,
    cancel: impl Future<Output = ()> + 'static,
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
        cancel: Box::pin(cancel),
        declared_env: BTreeMap::new(),
        env_error: None,
    };
    job.start();

    let mut summaries = Vec::new();
    for (index, step) in doc.job.steps.iter().enumerate() {
        summaries.push(job.step(index, step, sink).await);
    }

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
    cancel: Pin<Box<dyn Future<Output = ()>>>,
    /// The job's `env` with expressions replaced.
    declared_env: BTreeMap<String, String>,
    /// Why `declared_env` could not be built. Every step then fails with it.
    env_error: Option<String>,
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
        env
    }

    fn set_status(&mut self, status: JobStatus) {
        self.status = status;
        self.ctx.set_status(status);
    }

    async fn step(&mut self, index: usize, step: &PlannedStep, sink: &mut dyn Sink) -> StepSummary {
        let started = Instant::now();
        let (common, outcome, outputs) = match step {
            PlannedStep::Run(run) => self.run_step(index, run, sink).await,
            PlannedStep::Uses(uses) => {
                let common = Common {
                    name: uses
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Run {}", uses.uses)),
                    id: uses.id.clone(),
                    if_: uses.if_.clone(),
                    continue_on_error: uses.continue_on_error,
                };
                self.unsupported(index, common, "`uses:` steps", sink)
            }
            PlannedStep::Cache(_) => {
                let common = Common {
                    name: "Cache".to_owned(),
                    id: None,
                    if_: None,
                    continue_on_error: false,
                };
                self.unsupported(index, common, "`cache` steps", sink)
            }
            PlannedStep::Workspace(_) => {
                let common = Common {
                    name: "Workspace".to_owned(),
                    id: None,
                    if_: None,
                    continue_on_error: false,
                };
                self.unsupported(index, common, "workspace steps", sink)
            }
            PlannedStep::TestResults { .. } => {
                let common = Common {
                    name: "Test results".to_owned(),
                    id: None,
                    if_: None,
                    continue_on_error: false,
                };
                self.unsupported(index, common, "`test-results` steps", sink)
            }
        };
        let conclusion = if outcome == Conclusion::Failure && common.continue_on_error {
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

    /// Whether a step's `if:` lets it run.
    fn runs(&self, if_: Option<&str>) -> Result<bool, ExprError> {
        let rt = Runtime {
            workspace: &self.layout.workspace,
            status: self.status,
        };
        condition(if_, &self.ctx, &rt)
    }

    /// A step kind a later release adds. It fails, unless its `if`
    /// skips it.
    fn unsupported(
        &mut self,
        index: usize,
        common: Common,
        what: &str,
        sink: &mut dyn Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        sink.step_started(index, &self.masker.mask(&common.name));
        let runs = self.runs(common.if_.as_deref());
        let mut out = StepOutput::new(
            &mut self.masker,
            sink,
            &mut self.annotations,
            &mut self.budget,
        );
        let outcome = match runs {
            Ok(true) => {
                out.error(&format!("{what} are not supported yet"));
                Conclusion::Failure
            }
            Ok(false) => Conclusion::Skipped,
            Err(err) => {
                out.error(&format!("if: {err}"));
                Conclusion::Failure
            }
        };
        (common, outcome, BTreeMap::new())
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
        // `prepare` borrowed the job; the output borrows its masker and
        // annotations from here on.
        let mut out = StepOutput::new(
            &mut self.masker,
            sink,
            &mut self.annotations,
            &mut self.budget,
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                out.error(&message);
                return (common, Conclusion::Failure, BTreeMap::new());
            }
        };
        let args = prepared.shell.args_for(&prepared.script.to_string_lossy());
        let process = Process {
            program: &prepared.shell.program,
            args: &args,
            cwd: &prepared.cwd,
            env: &prepared.env,
            user: self.opts.runner_user.as_deref(),
            timeout: prepared.timeout,
        };
        let spawned = spawn(process, &mut out, &mut self.cancel).await;
        // The script may hold secrets. Nothing needs it after the run.
        let _ = std::fs::remove_file(&prepared.script);
        let mut outcome = exit_outcome(&mut out, spawned, step.timeout_minutes);
        if outcome == Conclusion::Cancelled {
            // The signal future resolved once and must not be polled again.
            self.cancel = Box::pin(std::future::pending());
        }
        if out.refused() && outcome == Conclusion::Success {
            outcome = Conclusion::Failure;
        }

        let read = files::read(&prepared.files);
        match read {
            Ok(written) => {
                for warning in &written.warnings {
                    out.warning(warning);
                }
                self.github_env.extend(written.env);
                self.path_dirs.extend(written.path);
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
        (common, outcome, outputs)
    }
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

    let mut env = base_env(job, &files, step, index)?;
    env.extend(job.github_env.clone());
    env.extend(job.declared_env.clone());
    env.extend(step_env);
    if !job.path_dirs.is_empty() {
        let mut dirs: Vec<&str> = job.path_dirs.iter().rev().map(String::as_str).collect();
        let path = env.get("PATH").cloned().unwrap_or_default();
        if !path.is_empty() {
            dirs.push(&path);
        }
        let joined = dirs.join(":");
        env.insert("PATH".to_owned(), joined);
    }

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

/// The environment file's variables, then the ones the runner sets.
fn base_env(
    job: &Job<'_>,
    files: &StepFiles,
    step: &RunStep,
    index: usize,
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

    let action = match (&step.id, index) {
        (Some(id), _) => id.clone(),
        (None, 0) => "__run".to_owned(),
        (None, n) => format!("__run_{}", n + 1),
    };
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
        ("GITHUB_ACTION", action),
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
