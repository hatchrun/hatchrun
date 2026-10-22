// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The job document the control plane hands to the agent.

use std::collections::BTreeMap;

use hatchrun_workflow::{
    CacheService, Conclusion, Github, JobDocument, JobKind, Need, PlannedJob, compile, parse,
    template,
};
use serde_json::json;

const SPEC: &str = include_str!("../../cli/tests/fixtures/spec_example.yml");

fn github() -> Github {
    Github {
        repository: "acme/app".into(),
        repository_owner: "acme".into(),
        sha: "0123456789abcdef".into(),
        ref_: "refs/heads/main".into(),
        ref_name: "main".into(),
        ref_type: "branch".into(),
        head_ref: String::new(),
        base_ref: String::new(),
        actor: "octo".into(),
        server_url: "https://github.com".into(),
        api_url: "https://api.github.com".into(),
        graphql_url: "https://api.github.com/graphql".into(),
        token: "ghs_x".into(),
    }
}

fn document(job: PlannedJob) -> JobDocument {
    let mut needs = BTreeMap::new();
    needs.insert(
        "test".to_string(),
        Need {
            result: Conclusion::Success,
            outputs: BTreeMap::from([("version".to_string(), "1.2.3".to_string())]),
        },
    );
    JobDocument {
        run_id: "run_1".into(),
        run_number: 7,
        run_attempt: 1,
        workflow: "ci".into(),
        job,
        event_name: "push".into(),
        event: json!({"ref": "refs/heads/main", "commits": [{"id": "abc"}]}),
        github: github(),
        needs,
        secrets: BTreeMap::new(),
        vars: BTreeMap::from([("REGION".to_string(), "ap-south-1".to_string())]),
        timings: BTreeMap::from([("suite::a".to_string(), 1.5)]),
        cache: Some(CacheService {
            url: "https://cache.example/".into(),
            token: "c_tok".into(),
        }),
    }
}

fn first_job() -> PlannedJob {
    let plan = compile(&parse(SPEC).unwrap(), "ci", "refs/heads/main").unwrap();
    plan.jobs.into_iter().next().unwrap()
}

#[test]
fn job_document_round_trips() {
    let plan = compile(&parse(SPEC).unwrap(), "ci", "refs/heads/main").unwrap();
    assert!(plan.jobs.len() > 1);
    for job in plan.jobs {
        let doc = document(job);
        let text = serde_json::to_string(&doc).unwrap();
        let back: JobDocument = serde_json::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }
}

#[test]
fn job_kind_serializes_as_the_label() {
    let kind = JobKind::Run {
        template: template("ubuntu-26.04-arm").unwrap(),
    };
    assert_eq!(
        serde_json::to_value(kind).unwrap(),
        json!({"run": {"template": "ubuntu-26.04-arm"}})
    );
}

#[test]
fn unknown_template_is_an_error() {
    let err = serde_json::from_str::<JobKind>(r#"{"run":{"template":"ubuntu-24.04"}}"#)
        .unwrap_err()
        .to_string();
    assert!(err.contains("ubuntu-24.04"), "{err}");
}

#[test]
fn debug_hides_secrets() {
    let mut doc = document(first_job());
    doc.secrets.insert("K".into(), "s3cr3t".into());
    let text = format!("{doc:?}");
    for hidden in ["s3cr3t", "ghs_x", "c_tok", "commits"] {
        assert!(!text.contains(hidden), "{hidden} leaked: {text}");
    }
    assert!(text.contains("[redacted]"));
}

#[test]
fn every_planned_step_round_trips() {
    use hatchrun_workflow::{CacheStep, PlannedStep, RunStep, UsesStep, WorkspaceStep};
    let mut job = first_job();
    job.steps = vec![
        PlannedStep::Run(RunStep {
            run: "make".into(),
            continue_on_error: true,
            env: BTreeMap::from([("A".to_string(), "1".to_string())]),
            ..RunStep::default()
        }),
        PlannedStep::Uses(UsesStep {
            uses: "actions/checkout@v4".into(),
            with: BTreeMap::from([("depth".to_string(), json!(1))]),
            ..UsesStep::default()
        }),
        PlannedStep::Cache(CacheStep {
            key: "k".into(),
            restore_keys: vec!["k-".into()],
            paths: vec!["target".into()],
        }),
        PlannedStep::Workspace(WorkspaceStep::Persist {
            root: ".".into(),
            paths: vec!["out".into()],
        }),
        PlannedStep::Workspace(WorkspaceStep::Attach { at: ".".into() }),
        PlannedStep::TestResults {
            path: "results".into(),
        },
    ];
    let doc = document(job);
    let back: JobDocument = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
    assert_eq!(back, doc);
}
