// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs `hatchrun-agent exec` as a binary.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::path::Path;
use std::process::{Command, Output};

use hatchrun_workflow::JobDocument;

fn exec(dir: &Path, doc: &str) -> Output {
    let job = dir.join("job.json");
    std::fs::write(&job, doc).unwrap();
    let env = dir.join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.join("result.json"))
        .output()
        .unwrap()
}

fn json(doc: &JobDocument) -> String {
    serde_json::to_string(doc).unwrap()
}

#[test]
fn empty_job_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), &json(&make::document(Vec::new())));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let result = std::fs::read_to_string(dir.path().join("result.json")).unwrap();
    assert_eq!(
        result,
        r#"{"conclusion":"success","outputs":{},"steps":[],"annotations":[]}"#
    );
}

#[test]
fn layout_is_created() {
    let dir = tempfile::tempdir().unwrap();
    exec(dir.path(), &json(&make::document(Vec::new())));
    let work = dir.path().join("work");
    assert!(work.join("repo/repo").is_dir());
    assert!(work.join("_temp").is_dir());
    assert!(work.join("_actions").is_dir());
}

#[test]
fn bad_document_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), "{}");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing field"), "{stderr}");
}

#[test]
fn approval_job_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), &json(&make::approval()));
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("approval jobs do not run on a runner"),
        "{stderr}"
    );
}
