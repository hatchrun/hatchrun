// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs with containers against a real Docker Engine. They run only with
//! `HATCHRUN_DOCKER_TESTS=1`, and need `HATCHRUN_DOCKER_IMAGE` (default
//! `redis:8-alpine`: it has `sh`, `tail` and `redis-cli`) to be pulled
//! or pullable.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use hatchrun_workflow::{Conclusion, Container, JobResult, PlannedStep, UsesStep};

static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn enabled() -> Option<std::sync::MutexGuard<'static, ()>> {
    std::env::var("HATCHRUN_DOCKER_TESTS")
        .is_ok_and(|v| v == "1")
        .then(|| {
            ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
}

fn image() -> String {
    std::env::var("HATCHRUN_DOCKER_IMAGE").unwrap_or_else(|_| "redis:8-alpine".to_owned())
}

fn exec(dir: &Path, doc: &hatchrun_workflow::JobDocument) -> Output {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let job = dir.join("job.json");
    std::fs::write(&job, serde_json::to_string(doc).unwrap()).unwrap();
    let env = dir.join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .arg("--state-dir")
        .arg(dir.canonicalize().unwrap().join("state"))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.join("result.json"))
        .output()
        .unwrap()
}

fn result(dir: &Path) -> JobResult {
    serde_json::from_str(&std::fs::read_to_string(dir.join("result.json")).unwrap()).unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Names of containers and networks the agent made.
fn leftovers() -> Vec<String> {
    let mut found = Vec::new();
    for args in [
        vec!["ps", "-a", "--format", "{{.Names}}"],
        vec!["network", "ls", "--format", "{{.Name}}"],
    ] {
        let out = Command::new("docker").args(&args).output().unwrap();
        found.extend(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|n| n.starts_with("hatchrun_"))
                .map(str::to_owned),
        );
    }
    found
}

#[test]
fn a_container_job_reaches_its_service_by_id() {
    let Some(_guard) = enabled() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let script = "echo ws=$GITHUB_WORKSPACE; pwd; echo home=$HOME; \
                  echo \"temp=$RUNNER_TEMP\"; redis-cli -h cache ping; \
                  echo out=ok >> \"$GITHUB_OUTPUT\"";
    let mut step = make::run(script);
    step.id = Some("probe".to_owned());
    let mut doc = make::document(vec![PlannedStep::Run(step)]);
    doc.job.container = Some(Container {
        image: image(),
        env: BTreeMap::from([("FROM_JOB".to_owned(), "yes".to_owned())]),
        ..Container::default()
    });
    doc.job.services = BTreeMap::from([(
        "cache".to_owned(),
        Container {
            image: image(),
            ports: vec!["6379".to_owned()],
            options: Some(
                "--health-cmd 'redis-cli ping' --health-interval 1s --health-retries 10".to_owned(),
            ),
            ..Container::default()
        },
    )]);
    doc.job.outputs = BTreeMap::from([
        (
            "out".to_owned(),
            "${{ steps.probe.outputs.out }}".to_owned(),
        ),
        (
            "port".to_owned(),
            "${{ job.services.cache.ports['6379'] }}".to_owned(),
        ),
    ]);
    let out = exec(dir.path(), &doc);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    for want in [
        "ws=/__w/repo/repo",
        "/__w/repo/repo",
        "home=/github/home",
        "temp=/__w/_temp",
        "PONG",
    ] {
        assert!(text.lines().any(|l| l == want), "{want} missing:\n{text}");
    }
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Success);
    assert_eq!(result.outputs["out"], "ok");
    assert!(result.outputs["port"].parse::<u16>().is_ok());
    assert_eq!(leftovers(), Vec::<String>::new());
}

#[test]
fn uses_docker_runs_an_image_and_reads_its_outputs() {
    let Some(_guard) = enabled() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let doc = make::document(vec![PlannedStep::Uses(UsesStep {
        id: Some("a".to_owned()),
        uses: format!("docker://{}", image()),
        with: BTreeMap::from([
            ("entrypoint".to_owned(), "/bin/sh".into()),
            (
                "args".to_owned(),
                "-c 'echo ws=$GITHUB_WORKSPACE; echo r=$INPUT_WHO >> $GITHUB_OUTPUT'".into(),
            ),
            ("who".to_owned(), "me".into()),
        ]),
        ..UsesStep::default()
    })]);
    let out = exec(dir.path(), &doc);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.lines().any(|l| l == "ws=/github/workspace"), "{text}");
    assert_eq!(leftovers(), Vec::<String>::new());
}

#[test]
fn a_failing_service_health_check_fails_the_job_and_cleans_up() {
    let Some(_guard) = enabled() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![make::step("echo never")]);
    doc.job.services = BTreeMap::from([(
        "bad".to_owned(),
        Container {
            image: image(),
            options: Some(
                "--health-cmd 'false' --health-interval 1s --health-retries 1 \
                 --health-start-period 0s"
                    .to_owned(),
            ),
            ..Container::default()
        },
    )]);
    let out = exec(dir.path(), &doc);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("did not become healthy"));
    assert_eq!(leftovers(), Vec::<String>::new());
}
