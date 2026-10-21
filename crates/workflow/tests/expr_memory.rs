// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What evaluating an expression allocates. GitHub payloads reach
//! 25 MB, so reading a context must not copy it: the evaluator walks
//! contexts by reference and copies only the value it returns.
//!
//! The allocator is counted, so this file holds one test: the test
//! harness would otherwise count other tests' threads too.

use std::alloc::System;

use hatchrun_workflow::{ExprError, Functions, JobStatus, evaluate, interpolate};
use serde_json::{Value, json};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

struct Funcs;

impl Functions for Funcs {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Ok(String::new())
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

/// A context whose `github.event` holds `size` bytes of text.
fn context(size: usize) -> Value {
    json!({
        "github": {
            "event_name": "pull_request",
            "event": {
                "n": 0,
                "body": "b".repeat(size),
                "pull_request": {"number": 42, "labels": [{"name": "ci"}, {"name": "Big"}]},
            },
        },
    })
}

/// Bytes allocated while running `f`.
fn allocated<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let region = Region::new(GLOBAL);
    let out = f();
    (out, region.change().bytes_allocated)
}

#[test]
fn reads_walk_the_context_by_reference() {
    const MIB: usize = 1 << 20;
    let small = context(1024);
    let large = context(MIB);
    let huge = context(4 * MIB);

    // `github.event` 300 times in one 3.9 KB expression.
    let reads = format!("format('{{0}}',{})", vec!["github.event"; 300].join(","));
    assert!(reads.len() < 4096);
    let leaves = format!("format('{{0}}',{})", vec!["github.event.n"; 250].join(","));
    let filter =
        "contains(github.event.pull_request.labels.*.name, 'big') && github.event.body != ''";
    let text = "#${{ github.event.pull_request.number }} ${{ startsWith(github.event.body, 'B') }}";

    let mut used = Vec::new();
    for ctx in [&small, &large, &huge] {
        let (v, reads_bytes) = allocated(|| evaluate(&reads, ctx, &Funcs).unwrap());
        assert_eq!(v, json!("Object"));
        let (v, leaves_bytes) = allocated(|| evaluate(&leaves, ctx, &Funcs).unwrap());
        assert_eq!(v, json!("0"));
        let (v, filter_bytes) = allocated(|| evaluate(filter, ctx, &Funcs).unwrap());
        assert_eq!(v, json!(true));
        let (v, text_bytes) = allocated(|| interpolate(text, ctx, &Funcs).unwrap());
        assert_eq!(v, "#42 true");
        used.push([reads_bytes, leaves_bytes, filter_bytes, text_bytes]);
    }
    // What a read allocates does not grow with the payload: it was
    // 300 copies of the event, 1.2 GB for the 4 MiB one. What is left
    // is the expression's tree, less than one copy of the 1 MiB event.
    assert_eq!(used[0], used[1], "{used:?}");
    assert_eq!(used[0], used[2], "{used:?}");
    for bytes in used[0] {
        assert!(bytes < MIB / 2, "{used:?}");
    }

    // A value that is returned is copied, and the copy is charged: a
    // small part of a big event is cheap, the whole event is not.
    let (v, bytes) = allocated(|| evaluate("github.event.pull_request", &huge, &Funcs).unwrap());
    assert_eq!(v["number"], json!(42));
    assert!(bytes < 64 * 1024, "{bytes}");
    let (v, bytes) = allocated(|| evaluate("github.event", &large, &Funcs).unwrap());
    assert_eq!(v["n"], json!(0));
    assert!(bytes < 2 * MIB, "{bytes}");
    let err = evaluate("github.event", &context(9 * MIB), &Funcs).unwrap_err();
    assert!(err.message.contains("memory limit"), "{}", err.message);
}
