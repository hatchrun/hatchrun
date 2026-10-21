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

/// Annotations of one level one step may attach.
const MAX_PER_LEVEL: usize = 10;
/// Annotations one job may attach.
pub const MAX_PER_JOB: usize = 50;
/// The longest annotation message kept, in bytes.
const MAX_MESSAGE: usize = 4 << 10;
/// `::set-output::` pairs one step may print.
const MAX_OUTPUTS: usize = 1000;
/// The most bytes of `::set-output::` names and values one step may print.
const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// Values one job may hide with `::add-mask::`.
pub const MAX_MASKS: usize = 1000;
/// The longest `::add-mask::` value, in bytes.
const MAX_MASK_LEN: usize = 64 << 10;

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

/// What a job has used up of its limits. One per job, shared by its steps.
#[derive(Debug, Default)]
pub struct Budget {
    /// Values hidden with `::add-mask::` so far.
    pub masks: usize,
}

/// Cuts `text` to at most `max` bytes without splitting a character.
fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// Where a step's text goes: lines are split, commands act, and the
/// [`Masker`] hides secrets before the [`Sink`] sees anything.
pub struct StepOutput<'a> {
    masker: &'a mut Masker,
    sink: &'a mut dyn Sink,
    annotations: &'a mut Vec<Annotation>,
    budget: &'a mut Budget,
    per_level: [usize; 3],
    output_bytes: usize,
    /// Whether the limit warning and the mask warning were printed.
    warned: [bool; 2],
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
        budget: &'a mut Budget,
    ) -> Self {
        Self {
            masker,
            sink,
            annotations,
            budget,
            per_level: [0; 3],
            output_bytes: 0,
            warned: [false; 2],
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

    /// Adds pairs from the `GITHUB_OUTPUT` file, under the same limits as
    /// `::set-output::`.
    pub fn push_outputs(&mut self, pairs: Vec<(String, String)>) {
        for (name, value) in pairs {
            self.push_output(name, value);
        }
    }

    fn push_output(&mut self, name: String, value: String) {
        self.output_bytes += name.len() + value.len();
        if self.outputs.len() >= MAX_OUTPUTS || self.output_bytes > MAX_OUTPUT_BYTES {
            self.warn_limit(
                "output limit reached: 1000 values and 1 MiB per step; the rest are dropped",
            );
        } else {
            self.outputs.push((name, value));
        }
    }

    fn warn_limit(&mut self, message: &str) {
        if !self.warned[0] {
            self.warned[0] = true;
            self.warning(message);
        }
    }

    fn warn_mask(&mut self, message: &str) {
        if !self.warned[1] {
            self.warned[1] = true;
            self.warning(message);
        }
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
                if value.len() > MAX_MASK_LEN {
                    self.warn_mask("an `add-mask` value over 64 KiB was ignored");
                } else if self.budget.masks >= MAX_MASKS {
                    self.warn_mask("the limit of 1000 `add-mask` values was reached");
                } else {
                    self.budget.masks += 1;
                    self.masker.add(&value);
                }
                self.echoed(raw);
            }
            WorkflowCommand::Annotation(mut a) => {
                a.message = truncate(self.masker.mask(&a.message).into_owned(), MAX_MESSAGE);
                a.title = a
                    .title
                    .map(|t| truncate(self.masker.mask(&t).into_owned(), MAX_MESSAGE));
                a.file = a
                    .file
                    .map(|f| truncate(self.masker.mask(&f).into_owned(), MAX_MESSAGE));
                let level = match a.level {
                    AnnotationLevel::Error => "Error",
                    AnnotationLevel::Warning => "Warning",
                    AnnotationLevel::Notice => "Notice",
                };
                self.sink.line(&format!("{level}: {}", a.message));
                let slot = match a.level {
                    AnnotationLevel::Error => 0,
                    AnnotationLevel::Warning => 1,
                    AnnotationLevel::Notice => 2,
                };
                if self.per_level[slot] >= MAX_PER_LEVEL || self.annotations.len() >= MAX_PER_JOB {
                    self.warn_limit(
                        "annotation limit reached: 10 per level per step and 50 per job; the rest are dropped",
                    );
                } else {
                    self.per_level[slot] += 1;
                    self.sink.annotation(&a);
                    self.annotations.push(a);
                }
            }
            WorkflowCommand::Debug(text) => self.line(&format!("##[debug]{text}")),
            WorkflowCommand::Group(title) => self.line(&format!("##[group]{title}")),
            WorkflowCommand::EndGroup => self.line("##[endgroup]"),
            WorkflowCommand::StopCommands(_) | WorkflowCommand::Resume(_) => self.echoed(raw),
            WorkflowCommand::Echo(on) => self.echo = on,
            WorkflowCommand::SetOutput { name, value } => {
                self.line(SET_OUTPUT_WARNING);
                self.push_output(name, value);
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

/// Kills the process group when dropped, so no exit path of [`spawn`],
/// including a dropped future, leaves it running.
struct GroupGuard {
    pid: Pid,
    armed: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_group(self.pid, Signal::SIGKILL);
        }
    }
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
/// SIGTERM, 2.5 seconds, SIGKILL. `abort` kills the group at once.
///
/// # Errors
///
/// When the process cannot be started or waited for.
pub async fn spawn(
    p: Process<'_>,
    out: &mut StepOutput<'_>,
    cancel: &mut (impl Future<Output = ()> + Unpin),
    abort: &mut (impl Future<Output = ()> + Unpin),
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
    let Some(pid) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw)
    else {
        let _ = child.start_kill();
        anyhow::bail!("read process id");
    };
    let mut guard = GroupGuard { pid, armed: true };

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
    let mut aborted = false;

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
            () = &mut *abort, if !aborted && status.is_none() => {
                aborted = true;
                stopping = Some(Exit::Cancelled);
                signal_group(pid, Signal::SIGKILL);
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
    guard.armed = false;

    if let Some(exit) = stopping {
        return Ok(exit);
    }
    let status = status.context("process status")?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(Exit::Code(code))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use hatchrun_workflow::StepSummary;

    use super::*;

    #[derive(Default)]
    struct Rec {
        lines: Vec<String>,
        annotations: usize,
    }

    impl Sink for Rec {
        fn step_started(&mut self, _: usize, _: &str) {}
        fn line(&mut self, text: &str) {
            self.lines.push(text.to_owned());
        }
        fn annotation(&mut self, _: &Annotation) {
            self.annotations += 1;
        }
        fn step_finished(&mut self, _: usize, _: &StepSummary) {}
    }

    fn feed(text: &str, budget: &mut Budget) -> (Rec, Vec<Annotation>, Vec<(String, String)>) {
        let mut masker = Masker::default();
        let mut sink = Rec::default();
        let mut annotations = Vec::new();
        let outputs = {
            let mut out = StepOutput::new(&mut masker, &mut sink, &mut annotations, budget);
            out.push(text.as_bytes());
            out.finish();
            out.take_outputs()
        };
        (sink, annotations, outputs)
    }

    #[test]
    fn annotations_are_capped_per_level_and_truncated() {
        let mut text = String::new();
        for _ in 0..15 {
            text.push_str("::error::e\n");
        }
        let _ = writeln!(text, "::warning::{}", "x".repeat(10_000));
        let (sink, kept, _) = feed(&text, &mut Budget::default());
        assert_eq!(kept.len(), 11);
        assert_eq!(sink.annotations, 11);
        assert_eq!(kept[10].message.len(), 4096);
        let warnings = sink
            .lines
            .iter()
            .filter(|l| l.contains("limit reached"))
            .count();
        assert_eq!(warnings, 1);
        assert_eq!(sink.lines.iter().filter(|l| *l == "Error: e").count(), 15);
    }

    #[test]
    fn annotations_are_capped_per_job() {
        let mut masker = Masker::default();
        let mut sink = Rec::default();
        let mut kept = Vec::new();
        let mut budget = Budget::default();
        for _ in 0..8 {
            let mut out = StepOutput::new(&mut masker, &mut sink, &mut kept, &mut budget);
            for level in ["error", "warning", "notice"] {
                for _ in 0..3 {
                    out.push(format!("::{level}::m\n").as_bytes());
                }
            }
            out.finish();
        }
        assert_eq!(kept.len(), MAX_PER_JOB);
    }

    #[test]
    fn set_output_pairs_are_capped() {
        let mut text = String::new();
        for i in 0..1100 {
            let _ = writeln!(text, "::set-output name=n{i}::v");
        }
        let (sink, _, outputs) = feed(&text, &mut Budget::default());
        assert_eq!(outputs.len(), 1000);
        assert_eq!(
            sink.lines
                .iter()
                .filter(|l| l.contains("output limit"))
                .count(),
            1
        );
        let line = format!("::set-output name=a::{}\n", "v".repeat(60_000));
        let (_, _, outputs) = feed(&line.repeat(25), &mut Budget::default());
        assert!(
            !outputs.is_empty() && outputs.len() < 25,
            "{}",
            outputs.len()
        );
    }

    #[test]
    fn add_mask_is_capped_and_never_printed() {
        let mut budget = Budget::default();
        let mut text = String::new();
        for i in 0..1005 {
            let _ = writeln!(text, "::add-mask::value{i}");
        }
        let (sink, _, _) = feed(&text, &mut budget);
        assert_eq!(budget.masks, MAX_MASKS);
        assert!(
            sink.lines
                .iter()
                .all(|l| !(0..1005).any(|i| l.contains(&format!("value{i}"))))
        );
        assert_eq!(
            sink.lines.iter().filter(|l| l.contains("add-mask")).count(),
            1
        );
    }

    #[test]
    fn an_oversized_mask_value_is_ignored() {
        let mut masker = Masker::default();
        let mut sink = Rec::default();
        let mut kept = Vec::new();
        let mut budget = Budget::default();
        let mut out = StepOutput::new(&mut masker, &mut sink, &mut kept, &mut budget);
        out.command(WorkflowCommand::AddMask("L".repeat(70_000)), "raw");
        drop(out);
        assert_eq!(budget.masks, 0);
        assert_eq!(sink.lines.len(), 1);
        assert!(sink.lines[0].contains("64 KiB") && !sink.lines[0].contains("LLL"));
        assert_eq!(masker.mask("LLLL"), "LLLL");
    }

    #[test]
    fn file_outputs_share_the_cap() {
        let mut masker = Masker::default();
        let mut sink = Rec::default();
        let mut kept = Vec::new();
        let mut budget = Budget::default();
        let mut out = StepOutput::new(&mut masker, &mut sink, &mut kept, &mut budget);
        out.push(b"::set-output name=a::1\n");
        out.push_outputs(
            (0..1500)
                .map(|i| (format!("n{i}"), "v".to_owned()))
                .collect(),
        );
        assert_eq!(out.take_outputs().len(), MAX_OUTPUTS);
    }

    #[test]
    fn annotation_title_and_file_are_truncated() {
        let long = "t".repeat(10_000);
        let (_, kept, _) = feed(
            &format!("::error title={long},file={long}::m\n"),
            &mut Budget::default(),
        );
        assert_eq!(kept[0].title.as_ref().unwrap().len(), 4096);
        assert_eq!(kept[0].file.as_ref().unwrap().len(), 4096);
    }
}
