// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Where a running job's output goes.

// Steps start emitting once `run_job` walks them.
#![allow(dead_code)]

use std::io::Write;

use hatchrun_workflow::{Annotation, AnnotationLevel, StepSummary};

/// Receives a job's progress. Lines are already masked.
pub trait Sink {
    /// A step begins.
    fn step_started(&mut self, index: usize, name: &str);
    /// One line of step output.
    fn line(&mut self, text: &str);
    /// A step attached an annotation.
    fn annotation(&mut self, annotation: &Annotation);
    /// A step ended.
    fn step_finished(&mut self, index: usize, summary: &StepSummary);
}

/// Writes a plain-text log: a `== <name>` header per step, lines as they
/// come.
pub struct TextSink<W: Write> {
    out: W,
}

impl<W: Write> TextSink<W> {
    /// A sink writing to `out`.
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> Sink for TextSink<W> {
    fn step_started(&mut self, _index: usize, name: &str) {
        let _ = writeln!(self.out, "== {name}");
    }

    fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }

    fn annotation(&mut self, annotation: &Annotation) {
        let level = match annotation.level {
            AnnotationLevel::Error => "error",
            AnnotationLevel::Warning => "warning",
            AnnotationLevel::Notice => "notice",
        };
        let _ = writeln!(self.out, "::{level}:: {}", annotation.message);
    }

    fn step_finished(&mut self, _index: usize, _summary: &StepSummary) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_headers_and_lines() {
        let mut buf = Vec::new();
        let mut sink = TextSink::new(&mut buf);
        sink.step_started(0, "build");
        sink.line("hello");
        assert_eq!(String::from_utf8(buf).unwrap(), "== build\nhello\n");
    }
}
