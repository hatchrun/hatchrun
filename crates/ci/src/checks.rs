// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a job looks like as a GitHub check run, and the outbox that
//! coalesces the updates sent for it.
//!
//! [`render`] builds the whole request from the job's stored state, so
//! any update can be sent at any time and the last one wins. The
//! [`Outbox`] keeps the set of jobs whose check run is behind, never a
//! queue of states: a job marked many times while GitHub throttles us
//! sends its latest state once.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hatchrun_workflow::{Annotation, AnnotationLevel, Conclusion, StepSummary};
use serde::{Deserialize, Serialize};

use crate::github::{Capped, CheckRun, GhAnnotation, Output, rfc3339};
use crate::graph::{Node, State};

/// The most annotations with a file in one request (GitHub's limit).
pub const MAX_ANNOTATIONS: usize = 50;

/// The most annotations with a file sent for one job attempt, which
/// bounds the API calls a noisy job can cause.
pub const MAX_TOTAL_ANNOTATIONS: usize = 500;

/// GitHub's limit on `output.summary` and `output.text`, in characters.
pub const MAX_OUTPUT_CHARS: usize = 65_535;

const TRUNCATED: &str = "\n\n(truncated)";

/// The last lines of a failed job shown in the check run.
pub const TAIL_LINES: usize = 50;

const EXTERNAL_ID_PREFIX: &str = "hatchrun-ci:";

/// The check run's name: the workflow and the planned job's id.
#[must_use]
pub fn check_name(workflow: &str, plan_id: &str) -> String {
    format!("{workflow} / {plan_id}")
}

/// The `external_id` that names job `job_id` again when GitHub asks for
/// a re-run.
#[must_use]
pub fn external_id(job_id: u64) -> String {
    format!("{EXTERNAL_ID_PREFIX}{job_id}")
}

/// The job id in an `external_id` made by [`external_id`].
#[must_use]
pub fn parse_external_id(value: &str) -> Option<u64> {
    value.strip_prefix(EXTERNAL_ID_PREFIX)?.parse().ok()
}

/// Where the app shows a job. `repo` is `owner/name`.
#[must_use]
pub fn details_url(app_url: &str, repo: &str, run_id: u64, job_id: u64) -> String {
    format!("{app_url}/ci/{repo}/runs/{run_id}/jobs/{job_id}")
}

/// One step of a job, with its result once it has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepView {
    /// Position in the job, from 0.
    pub index: usize,
    /// The step's name.
    pub name: String,
    /// How it went, once it ended.
    pub summary: Option<StepSummary>,
}

/// Everything [`render`] needs.
#[derive(Debug, Clone)]
pub struct JobView<'a> {
    /// The workflow's name.
    pub workflow: &'a str,
    /// The job.
    pub node: &'a Node,
    /// The commit the check run is on.
    pub head_sha: &'a str,
    /// Where the app shows the job.
    pub details_url: String,
    /// The job's steps.
    pub steps: &'a [StepView],
    /// Every annotation of the attempt, as stored.
    pub annotations: &'a [Annotation],
    /// How many annotations with a file GitHub already has.
    pub file_annotations_sent: usize,
    /// The last lines of the log, for a failed job.
    pub failed_tail: Option<&'a [String]>,
    /// Why the queued job waits for a launch.
    pub held: Option<&'a str>,
    /// Token scopes the App holds less of than the job asked.
    pub capped: &'a [Capped],
    /// Unix milliseconds.
    pub started_at: Option<u64>,
    /// Unix milliseconds.
    pub completed_at: Option<u64>,
}

pub(crate) fn conclusion_word(c: Conclusion) -> &'static str {
    match c {
        Conclusion::Success => "success",
        Conclusion::Failure => "failure",
        Conclusion::Cancelled => "cancelled",
        Conclusion::Skipped => "skipped",
    }
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}m {}s", secs / 60, secs % 60)
}

/// `text` cut to `max` characters, ending in a marker when it was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(TRUNCATED.chars().count());
    let end = text.char_indices().nth(keep).map_or(text.len(), |(i, _)| i);
    format!("{}{TRUNCATED}", &text[..end])
}

fn cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

fn level_word(level: AnnotationLevel) -> &'static str {
    match level {
        AnnotationLevel::Error => "error",
        AnnotationLevel::Warning => "warning",
        AnnotationLevel::Notice => "notice",
    }
}

/// An annotation as GitHub takes it, `None` when it has no file.
#[must_use]
pub fn annotation(a: &Annotation) -> Option<GhAnnotation> {
    let path = a.file.clone()?;
    let start_line = a.line.unwrap_or(1).max(1);
    let end_line = a.end_line.unwrap_or(start_line).max(start_line);
    let one_line = start_line == end_line;
    Some(GhAnnotation {
        path,
        start_line,
        end_line,
        start_column: a.col.filter(|_| one_line),
        end_column: a.end_column.filter(|_| one_line),
        annotation_level: match a.level {
            AnnotationLevel::Error => "failure",
            AnnotationLevel::Warning => "warning",
            AnnotationLevel::Notice => "notice",
        }
        .to_owned(),
        message: a.message.clone(),
        title: a.title.clone(),
    })
}

/// One paragraph for each scope the App holds less of than the job asked.
fn capped_lines(capped: &[Capped]) -> String {
    let mut out = String::new();
    for c in capped {
        let _ = writeln!(
            out,
            "The token's `{}` permission is {}: the job asked for {} and the hatchrun App holds no more.\n",
            c.scope, c.got, c.asked
        );
    }
    out
}

/// The check run request for a job, and the new count of annotations
/// with a file that GitHub has once the request is sent.
///
/// Each PATCH replaces `output.summary`, so annotations without a file
/// are listed in it every time. GitHub appends the annotations of each
/// request to the ones it has, so only those with a file past
/// `file_annotations_sent` go in `output.annotations`, at most
/// [`MAX_ANNOTATIONS`] per request. The caller sends again while the
/// returned count is below the number of annotations with a file, up to
/// [`MAX_TOTAL_ANNOTATIONS`]; the summary says how many were left out
/// past that. Summary and text are cut to [`MAX_OUTPUT_CHARS`].
#[must_use]
pub fn render(view: &JobView<'_>) -> (CheckRun, usize) {
    let held = view.held.filter(|_| view.node.state == State::Queued);
    let (status, conclusion, title) = match view.node.state {
        State::Queued if held.is_some() => ("queued", None, "Waiting"),
        State::Waiting | State::Queued => ("queued", None, "Queued"),
        State::Running => ("in_progress", None, "Running"),
        State::Done(c) => (
            "completed",
            Some(conclusion_word(c)),
            match c {
                Conclusion::Success => "Succeeded",
                Conclusion::Failure => "Failed",
                Conclusion::Cancelled => "Cancelled",
                Conclusion::Skipped => "Skipped",
            },
        ),
    };
    let title = match (view.started_at, view.completed_at) {
        (Some(start), Some(end)) => {
            format!("{title} in {}", duration(end.saturating_sub(start)))
        }
        _ => title.to_owned(),
    };

    let mut summary = String::new();
    if let Some(message) = held {
        summary.push_str(message);
        summary.push_str("\n\n");
    }
    summary.push_str(&capped_lines(view.capped));
    if let Some(message) = &view.node.message {
        summary.push_str(message);
        summary.push_str("\n\n");
    }
    if !view.steps.is_empty() {
        summary.push_str("| Step | Result | Time |\n| --- | --- | --- |\n");
        for step in view.steps {
            let (result, time) = match &step.summary {
                Some(s) => (conclusion_word(s.conclusion), duration(s.duration_ms)),
                None => ("pending", String::new()),
            };
            let _ = writeln!(summary, "| {} | {result} | {time} |", cell(&step.name));
        }
        summary.push('\n');
    }
    let loose: Vec<&Annotation> = view
        .annotations
        .iter()
        .filter(|a| a.file.is_none())
        .collect();
    let with_file: Vec<GhAnnotation> = view.annotations.iter().filter_map(annotation).collect();
    let left_out = with_file.len().saturating_sub(MAX_TOTAL_ANNOTATIONS);
    if !loose.is_empty() || left_out > 0 {
        for a in loose {
            let _ = writeln!(
                summary,
                "- **{}**: {}",
                level_word(a.level),
                a.message.replace(['\n', '\r'], " ")
            );
        }
        if left_out > 0 {
            let _ = writeln!(
                summary,
                "- {left_out} more annotations with a file were left out (limit {MAX_TOTAL_ANNOTATIONS})."
            );
        }
        summary.push('\n');
    }
    // The annotation list is last, so cutting the end cuts it first.
    let summary = truncate(summary.trim_end(), MAX_OUTPUT_CHARS);

    let total = with_file.len().min(MAX_TOTAL_ANNOTATIONS);
    let sent = view.file_annotations_sent.min(total);
    let end = total.min(sent + MAX_ANNOTATIONS);
    let fresh = with_file[sent..end].to_vec();
    let fresh_len = fresh.len();

    let text = view
        .failed_tail
        .filter(|tail| !tail.is_empty())
        .map(|tail| {
            let body = tail.join("\n");
            let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            let room = MAX_OUTPUT_CHARS - 2 * fence.len() - 2;
            format!("{fence}\n{}\n{fence}", truncate(&body, room))
        });

    let run = CheckRun {
        name: check_name(view.workflow, &view.node.job.id),
        head_sha: view.head_sha.to_owned(),
        external_id: external_id(view.node.id),
        details_url: view.details_url.clone(),
        status: status.to_owned(),
        started_at: view.started_at.map(rfc3339),
        conclusion: conclusion.map(str::to_owned),
        completed_at: view.completed_at.map(rfc3339),
        output: Some(Output {
            title,
            summary,
            text,
            annotations: fresh,
        }),
    };
    (run, sent + fresh_len)
}

/// The jobs whose check run is behind, and when GitHub lets us send
/// again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    /// Marked jobs and the generation of their latest mark.
    marked: BTreeMap<u64, u64>,
    counter: u64,
    blocked_until_ms: u64,
    attempts: u32,
}

impl Outbox {
    /// The job's check run needs an update. Marking again changes
    /// nothing a sender sees, except the generation: the update sent is
    /// the one for the state at send time.
    pub fn mark(&mut self, job: u64) {
        self.counter += 1;
        self.marked.insert(job, self.counter);
    }

    /// Whether no job waits for an update.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }

    /// The jobs to send now. Empty while GitHub has us blocked.
    #[must_use]
    pub fn due(&self, now_ms: u64) -> Vec<u64> {
        if now_ms < self.blocked_until_ms {
            return Vec::new();
        }
        self.marked.keys().copied().collect()
    }

    /// The generation of the job's latest mark. A sender reads it
    /// before it renders and hands it back to [`Outbox::sent`].
    #[must_use]
    pub fn generation(&self, job: u64) -> Option<u64> {
        self.marked.get(&job).copied()
    }

    /// The job's update for `generation` went through. The mark goes
    /// only when no newer mark arrived while the request was in flight;
    /// the attempt count resets either way.
    pub fn sent(&mut self, job: u64, generation: u64) {
        if self.marked.get(&job) == Some(&generation) {
            self.marked.remove(&job);
        }
        self.attempts = 0;
    }

    /// GitHub throttled a send: block for `wait_secs` (never shortening
    /// a block that is already longer) and count the attempt.
    pub fn throttled(&mut self, now_ms: u64, wait_secs: u64) {
        let until = now_ms.saturating_add(wait_secs.saturating_mul(1000));
        self.blocked_until_ms = self.blocked_until_ms.max(until);
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Throttled sends in a row.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.attempts
    }

    /// When to wake up to send: now when unblocked, the unblock time
    /// otherwise, `None` with nothing marked.
    #[must_use]
    pub fn next_alarm(&self, now_ms: u64) -> Option<u64> {
        if self.marked.is_empty() {
            return None;
        }
        Some(self.blocked_until_ms.max(now_ms))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hatchrun_workflow::{JobKind, PlannedJob};

    use super::*;

    fn node(state: State, message: Option<&str>) -> Node {
        Node {
            id: 42,
            job: PlannedJob {
                id: "build".into(),
                name: "build".into(),
                requires: vec![],
                statuses: std::collections::BTreeMap::new(),
                kind: JobKind::Approval,
                matrix: BTreeMap::new(),
                node: None,
                env: BTreeMap::new(),
                if_: None,
                timeout_minutes: 30,
                outputs: BTreeMap::new(),
                token_permissions: BTreeMap::new(),
                environment: None,
                steps: vec![],
            },
            state,
            attempt: 1,
            result: None,
            message: message.map(str::to_owned),
            cancel: 0,
        }
    }

    fn view<'a>(node: &'a Node, steps: &'a [StepView], notes: &'a [Annotation]) -> JobView<'a> {
        JobView {
            workflow: "ci",
            node,
            head_sha: "abc",
            details_url: "https://ci.hatchrun.io/ci/o/r/runs/1/jobs/42".into(),
            steps,
            annotations: notes,
            file_annotations_sent: 0,
            failed_tail: None,
            held: None,
            capped: &[],
            started_at: None,
            completed_at: None,
        }
    }

    fn note(level: AnnotationLevel, message: &str, file: Option<&str>) -> Annotation {
        Annotation {
            level,
            message: message.into(),
            title: None,
            file: file.map(str::to_owned),
            line: None,
            end_line: None,
            col: None,
            end_column: None,
        }
    }

    fn step(index: usize, name: &str, done: Option<(Conclusion, u64)>) -> StepView {
        StepView {
            index,
            name: name.into(),
            summary: done.map(|(c, ms)| StepSummary {
                name: name.into(),
                outcome: c,
                conclusion: c,
                duration_ms: ms,
            }),
        }
    }

    #[test]
    fn names_and_external_ids_round_trip() {
        assert_eq!(check_name("ci", "test (linux)"), "ci / test (linux)");
        assert_eq!(external_id(42), "hatchrun-ci:42");
        assert_eq!(parse_external_id("hatchrun-ci:42"), Some(42));
        assert_eq!(parse_external_id("hatchrun-ci:x"), None);
        assert_eq!(parse_external_id("other:42"), None);
        assert_eq!(parse_external_id("hatchrun-ci:"), None);
        assert_eq!(
            details_url("https://ci.hatchrun.io", "octo/widgets", 7, 42),
            "https://ci.hatchrun.io/ci/octo/widgets/runs/7/jobs/42"
        );
    }

    #[test]
    fn waiting_and_queued_jobs_are_queued() {
        for state in [State::Waiting, State::Queued] {
            let n = node(state, None);
            let (run, _) = render(&view(&n, &[], &[]));
            assert_eq!(run.status, "queued");
            assert_eq!(run.output.unwrap().title, "Queued");
            assert_eq!(run.conclusion, None);
        }
    }

    #[test]
    fn capped_scopes_are_named() {
        let n = node(State::Running, None);
        let capped = [Capped {
            scope: "contents".into(),
            asked: "write".into(),
            got: "read".into(),
        }];
        let mut v = view(&n, &[], &[]);
        v.capped = &capped;
        let (run, _) = render(&v);
        assert_eq!(
            run.output.unwrap().summary,
            "The token's `contents` permission is read: the job asked for write and the hatchrun App holds no more."
        );
    }

    #[test]
    fn held_job_says_why() {
        let n = node(State::Queued, None);
        let mut v = view(&n, &[], &[]);
        v.held = Some("Waiting for a card. Add one for acme at https://app/o/acme/ci.");
        let (run, _) = render(&v);
        assert_eq!(run.status, "queued");
        assert_eq!(run.conclusion, None);
        let out = run.output.unwrap();
        assert_eq!(out.title, "Waiting");
        assert!(
            out.summary
                .starts_with("Waiting for a card. Add one for acme at https://app/o/acme/ci.")
        );
    }

    #[test]
    fn running_job_drops_the_hold() {
        let n = node(State::Running, None);
        let mut v = view(&n, &[], &[]);
        v.held = Some("Waiting for a card.");
        let (run, _) = render(&v);
        let out = run.output.unwrap();
        assert_eq!(out.title, "Running");
        assert_eq!(out.summary, "");
    }

    #[test]
    fn running_job_is_in_progress() {
        let n = node(State::Running, None);
        let steps = [
            step(0, "checkout", Some((Conclusion::Success, 3_000))),
            step(1, "test", None),
        ];
        let mut v = view(&n, &steps, &[]);
        v.started_at = Some(1_792_676_700_000);
        let (run, sent) = render(&v);
        assert_eq!(sent, 0);
        assert_eq!(run.name, "ci / build");
        assert_eq!(run.head_sha, "abc");
        assert_eq!(run.external_id, "hatchrun-ci:42");
        assert_eq!(run.status, "in_progress");
        assert_eq!(run.started_at.as_deref(), Some("2026-10-22T13:45:00Z"));
        assert_eq!(run.conclusion, None);
        assert_eq!(run.completed_at, None);
        let out = run.output.unwrap();
        assert_eq!(out.title, "Running");
        assert_eq!(
            out.summary,
            "| Step | Result | Time |\n| --- | --- | --- |\n\
             | checkout | success | 0m 3s |\n| test | pending |  |"
        );
        assert_eq!(out.text, None);
    }

    #[test]
    fn finished_jobs_carry_conclusion_and_duration() {
        let cases = [
            (Conclusion::Success, "success", "Succeeded"),
            (Conclusion::Failure, "failure", "Failed"),
            (Conclusion::Cancelled, "cancelled", "Cancelled"),
            (Conclusion::Skipped, "skipped", "Skipped"),
        ];
        for (c, word, title) in cases {
            let n = node(State::Done(c), None);
            let mut v = view(&n, &[], &[]);
            v.started_at = Some(1_000_000);
            v.completed_at = Some(1_000_000 + 125_000);
            let (run, _) = render(&v);
            assert_eq!(run.status, "completed");
            assert_eq!(run.conclusion.as_deref(), Some(word));
            assert_eq!(run.output.unwrap().title, format!("{title} in 2m 5s"));
            assert!(run.completed_at.is_some());
        }
        let n = node(State::Done(Conclusion::Skipped), Some("Needs failed."));
        let (run, _) = render(&view(&n, &[], &[]));
        let out = run.output.unwrap();
        assert_eq!(out.title, "Skipped");
        assert_eq!(out.summary, "Needs failed.");
    }

    #[test]
    fn failed_job_shows_the_tail() {
        let n = node(State::Done(Conclusion::Failure), Some("Step 2 failed."));
        let tail = vec!["error: boom".to_owned(), "exit 1".to_owned()];
        let mut v = view(&n, &[], &[]);
        v.failed_tail = Some(&tail);
        let out = render(&v).0.output.unwrap();
        assert!(out.summary.starts_with("Step 2 failed."));
        assert_eq!(out.text.as_deref(), Some("```\nerror: boom\nexit 1\n```"));
    }

    #[test]
    fn error_with_a_file_becomes_an_annotation() {
        let mut a = note(AnnotationLevel::Error, "bad", Some("src/a.rs"));
        a.line = Some(3);
        let g = annotation(&a).unwrap();
        assert_eq!(g.path, "src/a.rs");
        assert_eq!((g.start_line, g.end_line), (3, 3));
        assert_eq!(g.annotation_level, "failure");
        assert_eq!(g.message, "bad");

        let n = node(State::Running, None);
        let notes = [a];
        let (run, sent) = render(&view(&n, &[], &notes));
        assert_eq!(run.output.unwrap().annotations, vec![g]);
        assert_eq!(sent, 1);
    }

    #[test]
    fn annotation_defaults_and_columns() {
        let mut a = note(AnnotationLevel::Warning, "w", Some("f"));
        let g = annotation(&a).unwrap();
        assert_eq!((g.start_line, g.end_line), (1, 1));
        assert_eq!(g.annotation_level, "warning");

        a.line = Some(2);
        a.col = Some(4);
        a.end_column = Some(9);
        let g = annotation(&a).unwrap();
        assert_eq!((g.start_column, g.end_column), (Some(4), Some(9)));

        a.end_line = Some(5);
        let g = annotation(&a).unwrap();
        assert_eq!((g.start_line, g.end_line), (2, 5));
        assert_eq!((g.start_column, g.end_column), (None, None));

        let n = note(AnnotationLevel::Notice, "n", Some("f"));
        assert_eq!(annotation(&n).unwrap().annotation_level, "notice");
    }

    #[test]
    fn annotation_without_a_file_goes_in_the_summary() {
        let n = node(State::Running, None);
        let notes = [note(AnnotationLevel::Error, "no tests ran", None)];
        let (run, sent) = render(&view(&n, &[], &notes));
        assert_eq!(sent, 0);
        let out = run.output.unwrap();
        assert_eq!(out.annotations, Vec::new());
        assert_eq!(out.summary, "- **error**: no tests ran");
        assert!(annotation(&notes[0]).is_none());
    }

    fn many(n: usize) -> Vec<Annotation> {
        (0..n)
            .map(|i| note(AnnotationLevel::Notice, &format!("n{i}"), Some("f")))
            .collect()
    }

    #[test]
    fn pages_annotations_fifty_to_a_request() {
        let n = node(State::Running, None);
        let notes = many(120);
        let mut sent = 0;
        let mut pages = vec![];
        loop {
            let mut v = view(&n, &[], &notes);
            v.file_annotations_sent = sent;
            let (run, next) = render(&v);
            let page = run.output.unwrap().annotations;
            if page.is_empty() {
                assert_eq!(next, sent);
                break;
            }
            assert_eq!(page[0].message, format!("n{sent}"));
            assert_eq!(next, sent + page.len());
            pages.push(page.len());
            sent = next;
        }
        assert_eq!(pages, vec![50, 50, 20]);
        assert_eq!(sent, 120);
    }

    #[test]
    fn stops_at_five_hundred_and_says_how_many_were_left_out() {
        let n = node(State::Running, None);
        let notes = many(520);
        let mut v = view(&n, &[], &notes);
        v.file_annotations_sent = 480;
        let (run, sent) = render(&v);
        let out = run.output.unwrap();
        assert_eq!(out.annotations.len(), 20);
        assert_eq!(sent, MAX_TOTAL_ANNOTATIONS);
        assert!(out.summary.contains("20 more annotations"));

        v.file_annotations_sent = MAX_TOTAL_ANNOTATIONS;
        let (run, sent) = render(&v);
        assert_eq!(run.output.unwrap().annotations, Vec::new());
        assert_eq!(sent, MAX_TOTAL_ANNOTATIONS);
    }

    #[test]
    fn clamps_lines() {
        let mut a = note(AnnotationLevel::Error, "x", Some("f"));
        a.line = Some(0);
        a.end_line = Some(0);
        let g = annotation(&a).unwrap();
        assert_eq!((g.start_line, g.end_line), (1, 1));
        a.line = Some(9);
        a.end_line = Some(4);
        let g = annotation(&a).unwrap();
        assert_eq!((g.start_line, g.end_line), (9, 9));
    }

    #[test]
    fn long_summary_is_cut_on_a_char_boundary_annotations_first() {
        let n = node(State::Running, Some("msg"));
        let notes: Vec<Annotation> = (0..3)
            .map(|_| note(AnnotationLevel::Error, &"\u{e9}".repeat(30_000), None))
            .collect();
        let out = render(&view(&n, &[], &notes)).0.output.unwrap();
        assert_eq!(out.summary.chars().count(), MAX_OUTPUT_CHARS);
        assert!(out.summary.starts_with("msg\n\n- **error**"));
        assert!(out.summary.ends_with("(truncated)"));
    }

    #[test]
    fn long_tail_is_cut_and_the_fence_closes() {
        let n = node(State::Done(Conclusion::Failure), None);
        let tail = vec!["`".repeat(5), "\u{e9}".repeat(70_000)];
        let mut v = view(&n, &[], &[]);
        v.failed_tail = Some(&tail);
        let text = render(&v).0.output.unwrap().text.unwrap();
        assert!(text.chars().count() <= MAX_OUTPUT_CHARS);
        assert!(text.starts_with("``````\n"));
        assert!(text.ends_with("(truncated)\n``````"));
    }

    #[test]
    fn a_fence_in_the_tail_does_not_close_the_block() {
        let n = node(State::Done(Conclusion::Failure), None);
        let tail = vec!["```".to_owned(), "after".to_owned()];
        let mut v = view(&n, &[], &[]);
        v.failed_tail = Some(&tail);
        let text = render(&v).0.output.unwrap().text.unwrap();
        assert_eq!(text, "````\n```\nafter\n````");
    }

    #[test]
    fn backslashes_in_cells_are_escaped() {
        let n = node(State::Running, None);
        let steps = [step(0, "a\\|b", None)];
        let out = render(&view(&n, &steps, &[])).0.output.unwrap();
        assert!(out.summary.contains("| a\\\\\\|b | pending |"));
    }

    #[test]
    fn sends_only_annotations_past_the_sent_count() {
        let n = node(State::Running, None);
        let notes: Vec<Annotation> = (0..4)
            .map(|i| note(AnnotationLevel::Notice, &format!("n{i}"), Some("f")))
            .collect();
        let mut v = view(&n, &[], &notes);
        v.file_annotations_sent = 3;
        let (run, sent) = render(&v);
        let out = run.output.unwrap().annotations;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "n3");
        assert_eq!(sent, 4);
    }

    #[test]
    fn later_updates_keep_every_summary_annotation() {
        let n = node(State::Running, None);
        let notes = [
            note(AnnotationLevel::Error, "first", None),
            note(AnnotationLevel::Warning, "second", None),
            note(AnnotationLevel::Error, "third", Some("a.rs")),
        ];
        let mut v = view(&n, &[], &notes);
        v.file_annotations_sent = 1;
        let (run, sent) = render(&v);
        let out = run.output.unwrap();
        assert_eq!(out.summary, "- **error**: first\n- **warning**: second");
        assert_eq!(out.annotations, Vec::new());
        assert_eq!(sent, 1);
    }

    #[test]
    fn table_cells_are_escaped() {
        let n = node(State::Running, None);
        let steps = [step(0, "a | b\nc", None)];
        let out = render(&view(&n, &steps, &[])).0.output.unwrap();
        assert!(out.summary.contains("| a \\| b c | pending |  |"));
    }

    #[test]
    fn outbox_keeps_only_the_latest_state_while_blocked() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        o.mark(1);
        o.throttled(now, 60);
        o.mark(1);
        o.mark(1);
        assert_eq!(o.due(now + 59_000), Vec::<u64>::new());
        assert_eq!(o.due(now + 60_000), vec![1]);
        assert_eq!(o.attempt(), 1);
        let g = o.generation(1).unwrap();
        o.sent(1, g);
        assert_eq!(o.due(now + 60_000), Vec::<u64>::new());
        assert_eq!(o.attempt(), 0);
    }

    #[test]
    fn a_mark_during_a_send_survives_it() {
        let mut o = Outbox::default();
        o.mark(1);
        let g = o.generation(1).unwrap();
        o.mark(1);
        o.sent(1, g);
        assert_eq!(o.due(0), vec![1]);
        let g = o.generation(1).unwrap();
        o.sent(1, g);
        assert_eq!(o.due(0), Vec::<u64>::new());
    }

    #[test]
    fn a_shorter_throttle_does_not_shorten_a_block() {
        let mut o = Outbox::default();
        o.mark(1);
        o.throttled(0, 100);
        o.throttled(10_000, 5);
        assert_eq!(o.next_alarm(0), Some(100_000));
        assert_eq!(o.attempt(), 2);
    }

    #[test]
    fn next_alarm_is_the_unblock_time() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        assert_eq!(o.next_alarm(now), None);
        o.mark(7);
        assert_eq!(o.next_alarm(now), Some(now));
        o.throttled(now, 60);
        assert_eq!(o.next_alarm(now), Some(now + 60_000));
        assert_eq!(o.next_alarm(now + 90_000), Some(now + 90_000));
        let g = o.generation(7).unwrap();
        o.sent(7, g);
        assert_eq!(o.next_alarm(now), None);
    }

    #[test]
    fn outbox_round_trips_through_serde() {
        let mut o = Outbox::default();
        o.mark(3);
        o.throttled(10, 5);
        let back: Outbox = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(back, o);
    }
}
