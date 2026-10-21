// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Builds [`JobDocument`]s in code for the exec tests.

#![allow(dead_code)]

use std::collections::BTreeMap;

use hatchrun_workflow::{
    CacheService, Github, JobDocument, JobKind, PlannedJob, PlannedStep, RunStep, template,
};
use serde_json::json;

/// A document for `acme/repo` whose job runs `steps` on `ubuntu-latest`.
pub fn document(steps: Vec<PlannedStep>) -> JobDocument {
    JobDocument {
        run_id: "1".to_owned(),
        run_number: 1,
        run_attempt: 1,
        workflow: "ci".to_owned(),
        job: PlannedJob {
            id: "build".to_owned(),
            name: "build".to_owned(),
            requires: Vec::new(),
            kind: JobKind::Run {
                template: template("ubuntu-latest").expect("ubuntu-latest exists"),
            },
            matrix: BTreeMap::new(),
            node: None,
            env: BTreeMap::new(),
            if_: None,
            timeout_minutes: 360,
            outputs: BTreeMap::new(),
            steps,
        },
        event_name: "push".to_owned(),
        event: json!({}),
        github: Github {
            repository: "acme/repo".to_owned(),
            repository_owner: "acme".to_owned(),
            sha: "0".repeat(40),
            ref_: "refs/heads/main".to_owned(),
            ref_name: "main".to_owned(),
            ref_type: "branch".to_owned(),
            head_ref: String::new(),
            base_ref: String::new(),
            actor: "octocat".to_owned(),
            server_url: "https://github.com".to_owned(),
            api_url: "https://api.github.com".to_owned(),
            graphql_url: "https://api.github.com/graphql".to_owned(),
            token: "token".to_owned(),
        },
        needs: BTreeMap::new(),
        secrets: BTreeMap::new(),
        vars: BTreeMap::new(),
        timings: BTreeMap::new(),
        cache: None::<CacheService>,
    }
}

/// The same document with an approval job.
pub fn approval() -> JobDocument {
    let mut doc = document(Vec::new());
    doc.job.kind = JobKind::Approval;
    doc
}

/// A `run:` step running `script`.
pub fn run(script: &str) -> RunStep {
    RunStep {
        run: script.to_owned(),
        ..RunStep::default()
    }
}

/// `run(script)` as a planned step.
pub fn step(script: &str) -> PlannedStep {
    PlannedStep::Run(run(script))
}

/// A planned step from a customised [`RunStep`].
pub fn planned(step: RunStep) -> PlannedStep {
    PlannedStep::Run(step)
}
