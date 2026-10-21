// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs one process for a step and feeds what it prints through the line
//! splitter, workflow commands and masker to the sink.

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use hatchrun_workflow::Annotation;
use hatchrun_workflow::AnnotationLevel;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::Command;
use tokio::time::{Instant, sleep_until};

use super::commands::{Command as WorkflowCommand, Parser};
use super::lines::Lines;
use super::mask::Masker;
use super::sink::Sink;
use crate::runner::run_as;

/// How long output is read after the main process ends.
const DRAIN: Duration = Duration::from_secs(1);
/// SIGINT, then this long, then SIGTERM.
const AFTER_INT: Duration = Duration::from_millis(7500);
/// SIGTERM, then this long, then SIGKILL.
const AFTER_TERM: Duration = Duration::from_millis(2500);

const SET_OUTPUT_WARNING: &str = "Warning: The `set-output` command is deprecated and will be disabled soon. Write name=value to the file named by $GITHUB_OUTPUT instead.";

/// A process to run for a step.
pub struct Process<'a> {
    /// The program to start.
    pub program: &'a str,
    /// Its arguments.
    pub args: &'a [String],
    /// The directory it starts in.
    pub cwd: &'a Path,
    /// Its whole environment.
    pub env: &'a BTreeMap<String, String>,
    /// The user to run it as. `None` keeps the agent's user.
    pub user: Option<&'a str>,
    /// How long it may run before it is cancelled.
    pub timeout: Option<Duration>,
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this code. A process killed by a signal reports
    /// 128 plus the signal number.
    Code(i32),
    /// The timeout ran out and the process was stopped.
    TimedOut,
    /// The agent was asked to stop and the process was stopped.
    Cancelled,
}

/// Where a step's text goes: lines are split, commands act, and the
/// [`Masker`] hides secrets before the [`Sink`] sees anything.
pub struct StepOutput<'a> {
    masker: &'a mut Masker,
    sink: &'a mut dyn Sink,
    annotations: &'a mut Vec<Annotation>,
    parser: Parser,
    lines: Lines,
    echo: bool,
    outputs: Vec<(String, String)>,
    refused: bool,
}

impl<'a> StepOutput<'a> {
    /// Output for one step. Annotations the step prints are added to
    /// `annotations`.
    pub fn new(
        masker: &'a mut Masker,
        sink: &'a mut dyn Sink,
        annotations: &'a mut Vec<Annotation>,
    ) -> Self {
        Self {
            masker,
            sink,
            annotations,
            parser: Parser::default(),
            lines: Lines::default(),
            echo: false,
            outputs: Vec::new(),
            refused: false,
        }
    }

    /// Adds bytes the process printed.
    pub fn push(&mut self, bytes: &[u8]) {
        for line in self.lines.push_masked(bytes, self.masker) {
            self.handle(&line);
        }
    }

    /// Handles the unfinished last line.
    pub fn finish(&mut self) {
        let lines = std::mem::take(&mut self.lines);
        for line in lines.finish_masked(self.masker) {
            self.handle(&line);
        }
    }

    /// Prints a line the agent itself wrote. It is masked like any other.
    pub fn line(&mut self, text: &str) {
        let masked = self.masker.mask(text);
        self.sink.line(&masked);
    }

    /// Prints `Error: <message>`.
    pub fn error(&mut self, message: &str) {
        self.line(&format!("Error: {message}"));
    }

    /// Prints `Warning: <message>`.
    pub fn warning(&mut self, message: &str) {
        self.line(&format!("Warning: {message}"));
    }

    /// The `::set-output::` pairs printed so far.
    pub fn take_outputs(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.outputs)
    }

    /// Whether the step used a command that is turned off, which fails
    /// the step.
    pub fn refused(&self) -> bool {
        self.refused
    }

    fn handle(&mut self, raw: &str) {
        match self.parser.parse(raw) {
            None => self.line(raw),
            Some(command) => self.command(command, raw),
        }
    }

    /// Prints a command line that changes state, when `::echo::on`.
    fn echoed(&mut self, raw: &str) {
        if self.echo {
            self.line(raw);
        }
    }

    fn command(&mut self, command: WorkflowCommand, raw: &str) {
        match command {
            WorkflowCommand::AddMask(value) => {
                self.masker.add(&value);
                self.echoed(raw);
            }
            WorkflowCommand::Annotation(mut a) => {
                a.message = self.masker.mask(&a.message).into_owned();
                a.title = a.title.map(|t| self.masker.mask(&t).into_owned());
                a.file = a.file.map(|f| self.masker.mask(&f).into_owned());
                let level = match a.level {
                    AnnotationLevel::Error => "Error",
                    AnnotationLevel::Warning => "Warning",
                    AnnotationLevel::Notice => "Notice",
                };
                self.sink.annotation(&a);
                self.sink.line(&format!("{level}: {}", a.message));
                self.annotations.push(a);
            }
            WorkflowCommand::Debug(text) => self.line(&format!("##[debug]{text}")),
            WorkflowCommand::Group(title) => self.line(&format!("##[group]{title}")),
            WorkflowCommand::EndGroup => self.line("##[endgroup]"),
            WorkflowCommand::StopCommands(_) | WorkflowCommand::Resume(_) => self.echoed(raw),
            WorkflowCommand::Echo(on) => self.echo = on,
            WorkflowCommand::SetOutput { name, value } => {
                self.line(SET_OUTPUT_WARNING);
                self.outputs.push((name, value));
            }
            WorkflowCommand::Refused(message) => {
                self.refused = true;
                self.error(message);
            }
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_hours(24 * 365 * 30)
}

fn signal_group(pid: Pid, signal: Signal) {
    // The group may already be gone.
    let _ = killpg(pid, signal);
}

/// Runs `p` in its own process group and feeds its output, stdout and
/// stderr together, to `out`.
///
/// When the process ends, its group is killed and output is read until
/// end of file or one second, whichever comes first. A timeout or
/// `cancel` stops the group the way GitHub does: SIGINT, 7.5 seconds,
/// SIGTERM, 2.5 seconds, SIGKILL.
///
/// # Errors
///
/// When the process cannot be started or waited for.
pub async fn spawn(
    p: Process<'_>,
    out: &mut StepOutput<'_>,
    cancel: &mut (impl Future<Output = ()> + Unpin),
) -> Result<Exit> {
    let (reader, writer) = std::io::pipe().context("create output pipe")?;
    let stderr = writer.try_clone().context("create output pipe")?;
    let mut command = Command::new(p.program);
    command
        .args(p.args)
        .current_dir(p.cwd)
        .env_clear()
        .envs(p.env)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(stderr)
        .process_group(0)
        .kill_on_drop(true);
    if let Some(user) = p.user {
        run_as(&mut command, user)?;
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", p.program))?;
    // The child holds the only write ends now. Keeping ours would stop
    // the reader from ever seeing end of file.
    drop(command);
    let pid = Pid::from_raw(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .context("read process id")?,
    );

    let fd: std::os::fd::OwnedFd = reader.into();
    fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).context("set output pipe non-blocking")?;
    let mut reader = Receiver::from_owned_fd(fd).context("read output pipe")?;

    let mut buf = vec![0u8; 16 << 10];
    let timeout_at = p.timeout.map(|t| Instant::now() + t);
    let mut stopping: Option<Exit> = None;
    let mut stage = 0u8;
    let mut escalate_at: Option<Instant> = None;
    let mut drain_at: Option<Instant> = None;
    let mut status = None;
    let mut eof = false;

    loop {
        if status.is_some() && eof {
            break;
        }
        tokio::select! {
            read = reader.read(&mut buf), if !eof => match read {
                Ok(n) if n > 0 => out.push(&buf[..n]),
                _ => eof = true,
            },
            waited = child.wait(), if status.is_none() => {
                status = Some(waited.context("wait for process")?);
                signal_group(pid, Signal::SIGKILL);
                escalate_at = None;
                drain_at = Some(Instant::now() + DRAIN);
            }
            () = sleep_until(timeout_at.unwrap_or_else(far_future)),
                if timeout_at.is_some() && stopping.is_none() && status.is_none() =>
            {
                stopping = Some(Exit::TimedOut);
                signal_group(pid, Signal::SIGINT);
                stage = 1;
                escalate_at = Some(Instant::now() + AFTER_INT);
            }
            () = &mut *cancel, if stopping.is_none() && status.is_none() => {
                stopping = Some(Exit::Cancelled);
                signal_group(pid, Signal::SIGINT);
                stage = 1;
                escalate_at = Some(Instant::now() + AFTER_INT);
            }
            () = sleep_until(escalate_at.unwrap_or_else(far_future)), if escalate_at.is_some() => {
                if stage == 1 {
                    signal_group(pid, Signal::SIGTERM);
                    stage = 2;
                    escalate_at = Some(Instant::now() + AFTER_TERM);
                } else {
                    signal_group(pid, Signal::SIGKILL);
                    escalate_at = None;
                }
            }
            () = sleep_until(drain_at.unwrap_or_else(far_future)), if drain_at.is_some() => break,
        }
    }
    out.finish();

    if let Some(exit) = stopping {
        return Ok(exit);
    }
    let status = status.context("process status")?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(Exit::Code(code))
}
