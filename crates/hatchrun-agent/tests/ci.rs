// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs `hatchrun-agent ci` as a binary against a local control plane.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::path::Path;
use std::process::Command;

use hatchrun_workflow::agent::{Batch, CLAIM_PATH, EVENTS_PATH, RESULT_PATH};
use hatchrun_workflow::{Conclusion, JobDocument, JobResult};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A broker and control plane in one server: the ticket `t` claims a
/// boot document that points back at the server.
async fn control_plane(claim: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    let boot = json!({"ci": {"url": server.uri(), "token": "job-token"}});
    Mock::given(method("POST"))
        .and(path("/v1/tickets/t/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"config": boot.to_string()})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CLAIM_PATH))
        .and(header("authorization", "Bearer job-token"))
        .respond_with(claim)
        .mount(&server)
        .await;
    for route in [EVENTS_PATH, RESULT_PATH] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"cancel": 0})))
            .mount(&server)
            .await;
    }
    server
}

async fn agent(server: &MockServer, dir: &Path) -> Option<i32> {
    let user_data = dir.join("user-data.json");
    let doc = json!({"mode": "ci", "broker": server.uri(), "ticket": "t"});
    std::fs::write(&user_data, doc.to_string()).unwrap();
    let env = dir.join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    let work = dir.join("work");
    let state = dir.canonicalize().unwrap().join("state");
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
            .arg("ci")
            .arg("--user-data")
            .arg(user_data)
            .arg("--work-dir")
            .arg(work)
            .arg("--state-dir")
            .arg(state)
            .args(["--runner-user", "", "--claim-timeout", "20s"])
            .arg("--env-file")
            .arg(env)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    out.status.code()
}

fn job(doc: &JobDocument) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(doc)
}

async fn posted<T: serde::de::DeserializeOwned>(server: &MockServer, route: &str) -> Vec<T> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == route)
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

fn texts(batches: &[Batch]) -> Vec<String> {
    batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|event| match event {
            hatchrun_workflow::agent::AgentEvent::Line { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_the_job_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![make::step("echo hi-from-ci; echo hunter2xyz")]);
    doc.secrets.insert("S".into(), "hunter2xyz".into());
    let server = control_plane(job(&doc)).await;

    assert_eq!(agent(&server, dir.path()).await, Some(0));

    let batches: Vec<Batch> = posted(&server, EVENTS_PATH).await;
    let seqs: Vec<u64> = batches.iter().map(|batch| batch.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    let lines = texts(&batches);
    assert!(lines.iter().any(|line| line == "hi-from-ci"), "{lines:?}");
    let everything = String::from_utf8_lossy(
        &server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .flat_map(|request| request.body.clone())
            .collect::<Vec<u8>>(),
    )
    .into_owned();
    assert!(
        !everything.contains("hunter2xyz"),
        "a secret left the machine"
    );
    let results: Vec<JobResult> = posted(&server, RESULT_PATH).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].conclusion, Conclusion::Success);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_job_exits_1_with_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let doc = make::document(vec![make::step("exit 3")]);
    let server = control_plane(job(&doc)).await;

    assert_eq!(agent(&server, dir.path()).await, Some(1));

    let results: Vec<JobResult> = posted(&server, RESULT_PATH).await;
    assert_eq!(results[0].conclusion, Conclusion::Failure);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ended_job_exits_0_without_running() {
    let dir = tempfile::tempdir().unwrap();
    let server = control_plane(ResponseTemplate::new(409)).await;

    assert_eq!(agent(&server, dir.path()).await, Some(0));

    assert_eq!(posted::<Batch>(&server, EVENTS_PATH).await.len(), 0);
    assert_eq!(posted::<JobResult>(&server, RESULT_PATH).await.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_leaves_the_job_level_if_to_the_control_plane() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![make::step("echo still-ran")]);
    doc.job.if_ = Some("false".into());
    let server = control_plane(job(&doc)).await;

    assert_eq!(agent(&server, dir.path()).await, Some(0));

    let lines = texts(&posted(&server, EVENTS_PATH).await);
    assert!(lines.iter().any(|line| line == "still-ran"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_cancels_the_job_and_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    let doc = make::document(vec![make::step("echo ready; sleep 30")]);
    let server = control_plane(job(&doc)).await;
    let user_data = dir.path().join("user-data.json");
    let ticket = json!({"mode": "ci", "broker": server.uri(), "ticket": "t"});
    std::fs::write(&user_data, ticket.to_string()).unwrap();
    let env = dir.path().join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("ci")
        .arg("--user-data")
        .arg(&user_data)
        .arg("--work-dir")
        .arg(dir.path().join("work"))
        .arg("--state-dir")
        .arg(dir.path().canonicalize().unwrap().join("state"))
        .args(["--runner-user", "", "--claim-timeout", "20s"])
        .arg("--env-file")
        .arg(&env)
        .spawn()
        .unwrap();
    let mut ready = false;
    for _ in 0..300 {
        ready = texts(&posted(&server, EVENTS_PATH).await)
            .iter()
            .any(|line| line == "ready");
        if ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready, "the step never started");
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let started = std::time::Instant::now();
    let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(status.code(), Some(1));
    let results: Vec<JobResult> = posted(&server, RESULT_PATH).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].conclusion, Conclusion::Cancelled);
}
