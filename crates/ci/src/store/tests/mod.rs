// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

mod jobs;
mod secrets;
mod usage;

use std::collections::BTreeMap;

use hatchrun_cloud::db::params;
use hatchrun_cloud::testdb::Db as TestDb;
use hatchrun_workflow::{Level, SecretRefs};

use super::*;

pub(super) const DAY: u64 = 86_400_000;

pub(super) fn setup() -> TestDb {
    let db = TestDb::new();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t1', 1, 'acme', 99, 0, 0)",
        &[],
    );
    let repo = RepoRow {
        id: 7,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/app".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();
    db
}

/// Claims job `job_id` of tenant t1, which must succeed.
pub(super) fn claim(db: &TestDb, job_id: u64, now: u64) -> JobRow {
    match db
        .wait(claim_launch(db.db(), job_id, "t1", 5, now))
        .unwrap()
    {
        Claim::Claimed(job) => job,
        other => panic!("not claimed: {other:?}"),
    }
}

pub(super) fn new_run(delivery: &str, workflow: &str, now: u64) -> NewRun {
    NewRun {
        tenant_id: "t1".into(),
        repo_id: 7,
        workflow: workflow.into(),
        delivery: delivery.into(),
        event: "push".into(),
        git_ref: "refs/heads/main".into(),
        sha: "abc".into(),
        actor: "octocat".into(),
        now,
    }
}

pub(super) fn new_job(plan_id: &str) -> NewJob {
    NewJob {
        plan_id: plan_id.into(),
        name: plan_id.into(),
        timeout_minutes: 360,
        template: Some("ubuntu-latest".into()),
        token_permissions: BTreeMap::from([("metadata".to_owned(), Level::Read)]),
        secret_refs: SecretRefs::default(),
        environment: None,
    }
}

/// A run with one job, which the test moves to `status`.
pub(super) fn run_with_job(db: &TestDb, status: &str) -> (RunRow, u64) {
    let run = db
        .wait(insert_run(db.db(), &new_run("d", "ci", 1_000)))
        .unwrap()
        .unwrap();
    let ids = db
        .wait(insert_jobs(db.db(), run.id, &[new_job("build")], 1_000))
        .unwrap();
    let job_id = ids[0].1;
    db.wait(set_job(db.db(), job_id, status, None, 1_001))
        .unwrap();
    (run, job_id)
}

/// As [`run_with_job`], for a run of delivery `delivery`.
pub(super) fn run_with_job_named(db: &TestDb, delivery: &str, status: &str) -> (RunRow, u64) {
    let run = db
        .wait(insert_run(db.db(), &new_run(delivery, "ci", 1_000)))
        .unwrap()
        .unwrap();
    let ids = db
        .wait(insert_jobs(db.db(), run.id, &[new_job("build")], 1_000))
        .unwrap();
    let job_id = ids[0].1;
    db.wait(set_job(db.db(), job_id, status, None, 1_001))
        .unwrap();
    (run, job_id)
}

pub(super) fn pool_row(id: &str, template: &str, state: &str, launched_at: u64) -> PoolRow {
    PoolRow {
        instance_id: id.into(),
        template: template.into(),
        state: state.into(),
        launched_at,
        since: launched_at,
    }
}

pub(super) fn save(db: &TestDb, template: &str, rows: &[PoolRow], launched: &[PoolRow]) {
    db.wait(save_pool(db.db(), template, rows, launched))
        .unwrap();
}
