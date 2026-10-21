// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs `hatchrun-agent exec` as a binary.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use hatchrun_workflow::{Conclusion, JobDocument, JobResult};

fn exec(dir: &Path, doc: &str) -> Output {
    exec_with(dir, doc, &[])
}

fn exec_with(dir: &Path, doc: &str, extra: &[&str]) -> Output {
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
        .args(extra)
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

fn result(dir: &Path) -> JobResult {
    let text = std::fs::read_to_string(dir.join("result.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_steps(dir: &Path, steps: Vec<hatchrun_workflow::PlannedStep>) -> Output {
    exec(dir, &json(&make::document(steps)))
}

#[test]
fn run_step_output_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo hi; exit 3")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "hi"), "{}", stdout(&out));
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Failure);
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
}

#[test]
fn env_layers() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(Vec::new());
    doc.job.env.insert("A".to_owned(), "job".to_owned());
    let mut step = make::run("echo $A $B");
    step.env.insert("A".to_owned(), "step".to_owned());
    step.env.insert("B".to_owned(), "${{ env.A }}".to_owned());
    doc.job.steps = vec![make::planned(step)];
    let out = exec(dir.path(), &json(&doc));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).lines().any(|l| l == "step job"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn github_env_and_path_reach_later_steps() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(
        dir.path(),
        vec![
            make::step("echo X=1 >> \"$GITHUB_ENV\"; echo /opt/x >> \"$GITHUB_PATH\""),
            make::step("echo \"x=$X\"; echo \"path=$PATH\""),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "x=1"), "{text}");
    assert!(
        text.lines().any(|l| l.starts_with("path=/opt/x:")),
        "{text}"
    );
}

#[test]
fn step_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = make::run("echo v=7 >> \"$GITHUB_OUTPUT\"");
    first.id = Some("a".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::planned(first),
            make::step("echo got ${{ steps.a.outputs.v }}"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).lines().any(|l| l == "got 7"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn continue_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("exit 1");
    step.continue_on_error = true;
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Success);
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
    assert_eq!(result.steps[0].conclusion, Conclusion::Success);
}

#[test]
fn if_always_runs_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut always = make::run("echo cleanup");
    always.if_ = Some("always()".to_owned());
    let mut skipped = make::run("echo skipme");
    skipped.name = Some("skipped".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::step("exit 1"),
            make::planned(skipped),
            make::planned(always),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "cleanup"), "{text}");
    assert!(!text.contains("skipme"), "{text}");
    let result = result(dir.path());
    assert_eq!(result.steps[1].outcome, Conclusion::Skipped);
    assert_eq!(result.steps[2].outcome, Conclusion::Success);
}

#[test]
fn timeout_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut slow = make::run("trap '' TERM; sleep 600");
    slow.timeout_minutes = Some(1);
    let mut always = make::run("echo after");
    always.if_ = Some("always()".to_owned());
    let doc = make::document(vec![make::planned(slow), make::planned(always)]);
    let began = Instant::now();
    let out = exec_with(dir.path(), &json(&doc), &["--minute", "2s"]);
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "after"));
    let result = result(dir.path());
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
}

#[test]
fn background_child_does_not_hold_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let began = Instant::now();
    let out = run_steps(
        dir.path(),
        vec![make::step(
            "sleep 600 & echo $! > \"$RUNNER_TEMP/bg.pid\"; echo done",
        )],
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "done"));
    let pid = std::fs::read_to_string(dir.path().join("work/_temp/bg.pid")).unwrap();
    let gone = (0..20).any(|_| {
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if alive {
            std::thread::sleep(Duration::from_millis(100));
        }
        !alive
    });
    assert!(gone, "background process {} is still running", pid.trim());
}

#[test]
fn working_directory_and_custom_shell() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("import os; print(os.getcwd())");
    step.working_directory = Some("sub".to_owned());
    step.shell = Some("python3 {0}".to_owned());
    let out = run_steps(
        dir.path(),
        vec![make::step("mkdir sub"), make::planned(step)],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let workspace = std::fs::canonicalize(dir.path().join("work/repo/repo")).unwrap();
    let want = workspace.join("sub").to_string_lossy().into_owned();
    assert!(stdout(&out).lines().any(|l| l == want), "{}", stdout(&out));
}

#[test]
fn add_mask_hides_later_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("echo \"::add-mask::v4lue\"; echo v4lue");
    step.name = Some("mask".to_owned());
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    let text = stdout(&out);
    assert_eq!(text.lines().filter(|l| *l == "***").count(), 1, "{text}");
    assert!(
        !text.contains("add-mask") && !text.contains("v4lue"),
        "{text}"
    );
}

#[test]
fn bad_expression_fails_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo ${{ nope( }}")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).contains("at byte"), "{}", stdout(&out));
    assert_eq!(result(dir.path()).steps[0].outcome, Conclusion::Failure);
}

#[test]
fn secrets_are_masked_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(Vec::new());
    doc.secrets.insert("K".to_owned(), "s3cr3t".to_owned());
    let mut step = make::run("echo ${{ secrets.K }}; echo \"::error title=s3cr3t::s3cr3t\"");
    step.name = Some("show".to_owned());
    let mut bad = make::run("echo ${{ secrets.K.nope( }}");
    bad.name = Some("bad".to_owned());
    doc.job.steps = vec![make::planned(step), make::planned(bad)];
    let out = exec(dir.path(), &json(&doc));
    let text = stdout(&out);
    assert!(!text.contains("s3cr3t"), "{text}");
    assert!(text.lines().any(|l| l == "***"), "{text}");
    assert!(text.lines().any(|l| l == "Error: ***"), "{text}");
    let result = result(dir.path());
    assert!(!format!("{result:?}").contains("s3cr3t"));
}

#[test]
fn sigterm_cancels_the_running_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut always = make::run("echo after");
    always.if_ = Some("always()".to_owned());
    let doc = make::document(vec![make::step("sleep 600"), make::planned(always)]);
    let job = dir.path().join("job.json");
    std::fs::write(&job, json(&doc)).unwrap();
    let env = dir.path().join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.path().join("work"))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.path().join("result.json"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let began = Instant::now();
    let out = child.wait_with_output().unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).lines().any(|l| l == "after"));
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Cancelled);
    assert_eq!(result.steps[0].outcome, Conclusion::Cancelled);
    assert_eq!(result.steps[1].outcome, Conclusion::Success);
}

#[test]
fn set_output_works_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = make::run("echo \"::set-output name=o::5\"");
    first.id = Some("a".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::planned(first),
            make::step("echo got ${{ steps.a.outputs.o }}"),
        ],
    );
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "got 5"), "{text}");
    assert!(
        text.contains("`set-output` command is deprecated"),
        "{text}"
    );
}
