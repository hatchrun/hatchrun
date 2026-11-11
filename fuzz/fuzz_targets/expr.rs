// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fuzzes GitHub-style `${{ }}` expressions and interpolation.

#![no_main]

use std::sync::LazyLock;

use hatchrun_workflow::expr::hatchrun_context;
use hatchrun_workflow::{ExprError, Functions, JobStatus, checks_status, evaluate, interpolate};
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};

struct Fns;

impl Functions for Fns {
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError> {
        Ok(patterns.join(","))
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

/// The context every input evaluates against, built once.
static CONTEXT: LazyLock<Value> = LazyLock::new(|| {
    let github = json!({
        "ref": "refs/heads/main",
        "ref_name": "main",
        "sha": "0123456789abcdef",
        "actor": "fuzzer",
        "event_name": "push",
        "repository": "hatchrun/hatchrun",
        "server_url": "https://github.com",
        "workflow": "ci",
    });
    json!({
        "hatchrun": hatchrun_context(&github),
        "github": github,
        "env": {"FOO": "bar"},
        "matrix": {"os": "ubuntu-latest", "n": 1},
        "needs": {"build": {"result": "success", "outputs": {"x": "y"}}},
        "steps": {"one": {"outputs": {"z": "q"}, "outcome": "success", "conclusion": "success"}},
        "secrets": {"TOKEN": "secret"},
        "parameters": {"p": "v"},
        "strategy": {"fail-fast": true, "job-index": 0, "job-total": 1, "max-parallel": 1},
        "runner": {"os": "Linux", "arch": "X64", "temp": "/tmp", "tool_cache": "/tmp/tools"},
        "job": {"status": "success"},
        "vars": {"V": "x"},
        "inputs": {"i": "x"},
    })
});

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = evaluate(text, &CONTEXT, &Fns);
    let _ = interpolate(text, &CONTEXT, &Fns);
    let _ = checks_status(text);
});
