// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::secrets::{job_naming, put_secret_ok, put_var_ok, save_environment};
use super::usage::subscribe;
use super::*;
use hatchrun_cloud::testdb::Db as TestDb;

#[test]
fn take_pool_takes_ready_only() {
    let db = setup();
    save(
        &db,
        "ubuntu-latest",
        &[],
        &[pool_row("i-warm", "ubuntu-latest", "warming", 1)],
    );
    assert_eq!(
        db.wait(take_pool(db.db(), "ubuntu-latest")).unwrap(),
        None,
        "a warming row is not ready"
    );
    save(
        &db,
        "ubuntu-latest",
        &[],
        &[
            pool_row("i-b", "ubuntu-latest", "ready", 20),
            pool_row("i-a", "ubuntu-latest", "ready", 10),
        ],
    );
    save(
        &db,
        "ubuntu-26.04-8core",
        &[],
        &[pool_row("i-other", "ubuntu-26.04-8core", "ready", 1)],
    );
    let taken = |db: &TestDb| db.wait(take_pool(db.db(), "ubuntu-latest")).unwrap();
    assert_eq!(taken(&db).as_deref(), Some("i-a"));
    assert_eq!(taken(&db).as_deref(), Some("i-b"));
    assert_eq!(taken(&db), None);
    let left = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].instance_id, "i-warm");
}

#[test]
fn trim_skips_a_taken_instance() {
    let db = setup();
    save(
        &db,
        "ubuntu-latest",
        &[],
        &[pool_row("i-1", "ubuntu-latest", "ready", 1)],
    );
    assert_eq!(
        db.wait(take_pool(db.db(), "ubuntu-latest"))
            .unwrap()
            .as_deref(),
        Some("i-1")
    );
    assert!(!db.wait(drop_pool(db.db(), "i-1")).unwrap());
}

#[test]
fn a_warming_row_moves_to_stopping_once() {
    let db = setup();
    save(
        &db,
        "ubuntu-latest",
        &[],
        &[pool_row("i-1", "ubuntu-latest", "warming", 1)],
    );
    let to_stopping = |db: &TestDb| {
        db.wait(move_pool(db.db(), "i-1", "warming", "stopping", 9))
            .unwrap()
    };
    assert!(to_stopping(&db));
    assert!(!to_stopping(&db));
    let rows = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
    assert_eq!((rows[0].state.as_str(), rows[0].since), ("stopping", 9));
}

#[test]
fn save_pool_never_brings_back_a_taken_row() {
    let db = setup();
    save(
        &db,
        "ubuntu-latest",
        &[],
        &[
            pool_row("i-1", "ubuntu-latest", "ready", 1),
            pool_row("i-2", "ubuntu-latest", "warming", 2),
        ],
    );
    let before = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
    assert_eq!(before.len(), 2);
    // A job takes i-1 while the pass works on its copy.
    assert_eq!(
        db.wait(take_pool(db.db(), "ubuntu-latest"))
            .unwrap()
            .as_deref(),
        Some("i-1")
    );
    let mut stopping = pool_row("i-2", "ubuntu-latest", "stopping", 2);
    stopping.since = 50;
    save(
        &db,
        "ubuntu-latest",
        &[pool_row("i-1", "ubuntu-latest", "ready", 1), stopping],
        &[pool_row("i-3", "ubuntu-latest", "warming", 60)],
    );
    let rows = db.wait(pool_rows(db.db(), "ubuntu-latest")).unwrap();
    let ids: Vec<_> = rows.iter().map(|row| row.instance_id.as_str()).collect();
    assert_eq!(ids, ["i-2", "i-3"], "i-1 stays gone");
    assert_eq!((rows[0].state.as_str(), rows[0].since), ("stopping", 50));
}

#[test]
fn claims_by_bucket_groups_by_template() {
    let db = setup();
    let claimed = |delivery: &str, template: &str, started_at: u64| {
        let (_, job) = run_with_job_named(&db, delivery, "queued");
        db.run(
            "UPDATE ci_jobs SET template = $2, started_at = $3 WHERE id = $1",
            &params![job, template, started_at],
        );
    };
    claimed("a", "ubuntu-latest", 600_001);
    claimed("b", "ubuntu-latest", 899_999);
    claimed("c", "ubuntu-latest", 900_000);
    claimed("d", "ubuntu-26.04-8core", 650_000);
    claimed("e", "ubuntu-latest", 100_000);
    let (_, never) = run_with_job_named(&db, "f", "queued");
    db.run(
        "UPDATE ci_jobs SET template = 'ubuntu-latest' WHERE id = $1",
        &params![never],
    );
    let mut found = db.wait(claims_by_bucket(db.db(), 600_000)).unwrap();
    found.sort();
    assert_eq!(
        found,
        [
            ("ubuntu-26.04-8core".to_owned(), 600_000, 1),
            ("ubuntu-latest".to_owned(), 600_000, 2),
            ("ubuntu-latest".to_owned(), 900_000, 1),
        ]
    );
}

#[test]
fn insert_run_dedupes_on_delivery_and_workflow() {
    let db = setup();
    let first = db
        .wait(insert_run(db.db(), &new_run("d1", "ci", 5)))
        .unwrap();
    let again = db
        .wait(insert_run(db.db(), &new_run("d1", "ci", 6)))
        .unwrap();
    let other = db
        .wait(insert_run(db.db(), &new_run("d1", "lint", 7)))
        .unwrap();
    let first = first.unwrap();
    assert!(again.is_none());
    assert!(other.is_some());
    assert_eq!(first.status, "queued");
    assert_eq!(first.created_at, 5);
    assert_eq!(first.git_ref, "refs/heads/main");
    let expires: u64 = db
        .column(
            "SELECT expires_at AS n FROM ci_runs WHERE id = $1",
            &params![first.id],
            "n",
        )
        .unwrap();
    assert_eq!(expires, 5 + 30 * DAY);
}

#[test]
fn expiry_follows_the_tenant() {
    let db = setup();
    let expires = |run: &RunRow| -> u64 {
        db.column(
            "SELECT expires_at AS n FROM ci_runs WHERE id = $1",
            &params![run.id],
            "n",
        )
        .unwrap()
    };
    let short = db
        .wait(insert_run(db.db(), &new_run("a", "ci", 5)))
        .unwrap()
        .unwrap();
    assert_eq!((expires(&short), short.retention_days), (5 + 30 * DAY, 30));
    db.run(
        "UPDATE tenants SET retention_days = 90 WHERE id = 't1'",
        &[],
    );
    let long = db
        .wait(insert_run(db.db(), &new_run("b", "ci", 5)))
        .unwrap()
        .unwrap();
    assert_eq!((expires(&long), long.retention_days), (5 + 90 * DAY, 90));
    let found = db.wait(run(db.db(), long.id)).unwrap().unwrap();
    assert_eq!(found.retention_days, 90);
    let tenant = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
    assert_eq!(tenant.retention_days, 90);
}

#[test]
fn retention_days_is_30_or_90() {
    let db = setup();
    // hatchrun.io lets a tenant keep 1 to 90 days; CI has two tiers.
    for (set, tier) in [(1, 30), (30, 30), (31, 90), (60, 90), (90, 90)] {
        db.run(
            "UPDATE tenants SET retention_days = $1 WHERE id = 't1'",
            &params![set],
        );
        let tenant = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
        assert_eq!(tenant.retention_days, tier, "{set} days");
        let made = db
            .wait(insert_run(db.db(), &new_run(&format!("r{set}"), "ci", 5)))
            .unwrap()
            .unwrap();
        assert_eq!(made.retention_days, tier, "{set} days");
    }
}

#[test]
fn run_numbers_count_per_workflow() {
    let db = setup();
    let mut numbers = Vec::new();
    for (delivery, workflow) in [("a", "ci"), ("b", "ci"), ("c", "lint")] {
        let run = db
            .wait(insert_run(db.db(), &new_run(delivery, workflow, 1)))
            .unwrap()
            .unwrap();
        numbers.push(run.run_number);
    }
    assert_eq!(numbers, [1, 2, 1]);
}

#[test]
fn a_duplicate_does_not_use_up_a_run_number() {
    let db = setup();
    db.wait(insert_run(db.db(), &new_run("a", "ci", 1)))
        .unwrap()
        .unwrap();
    assert!(
        db.wait(insert_run(db.db(), &new_run("a", "ci", 2)))
            .unwrap()
            .is_none()
    );
    let third = db
        .wait(insert_run(db.db(), &new_run("b", "ci", 3)))
        .unwrap()
        .unwrap();
    assert_eq!(third.run_number, 2);
}

#[test]
fn concurrent_duplicates_use_up_one_number() {
    let db = setup();
    let config: tokio_postgres::Config = hatchrun_cloud::testdb::url().parse().unwrap();
    let second = db.connect(&config);
    let first = new_run("same", "ci", 5);
    let other = new_run("same", "ci", 5);
    let (a, b) = db.wait(async {
        futures_util::join!(insert_run(db.db(), &first), insert_run(&second, &other))
    });
    let recorded = [a.unwrap(), b.unwrap()];
    assert_eq!(recorded.iter().flatten().count(), 1);
    let next = db
        .wait(insert_run(db.db(), &new_run("next", "ci", 6)))
        .unwrap()
        .unwrap();
    assert_eq!(next.run_number, 2);
}

#[test]
fn a_transferred_repo_keeps_its_history_with_the_old_tenant() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    let before = db
        .wait(insert_run(db.db(), &new_run("old", "ci", 5)))
        .unwrap()
        .unwrap();
    let mut moved = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    moved.tenant_id = "t2".into();
    moved.installation_id = 100;
    db.wait(upsert_repo(db.db(), &moved, 6)).unwrap();
    assert_eq!(
        db.wait(runs_for_repo(db.db(), 7, None, 10)).unwrap().len(),
        0
    );
    let mut mine = new_run("new", "ci", 7);
    mine.tenant_id = "t2".into();
    let after = db.wait(insert_run(db.db(), &mine)).unwrap().unwrap();
    let seen = db.wait(runs_for_repo(db.db(), 7, None, 10)).unwrap();
    assert_eq!(seen.iter().map(|r| r.id).collect::<Vec<_>>(), [after.id]);
    // The old run is still there, under its own tenant.
    let old = db.wait(run(db.db(), before.id)).unwrap().unwrap();
    assert_eq!(old.tenant_id, "t1");
}

#[test]
fn a_transferred_repo_leaves_its_secrets_and_vars_behind() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    put_secret_ok(&db, Owner::Repo(7), "REPO_S", "k", "s1");
    put_secret_ok(&db, Owner::Org, "ORG_S", "k", "s2");
    put_var_ok(&db, Owner::Repo(7), "REPO_V", "1");
    put_var_ok(&db, Owner::Org, "ORG_V", "2");
    // Re-registering the repository under its own tenant keeps them.
    let mut same = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    db.wait(upsert_repo(db.db(), &same, 6)).unwrap();
    assert_eq!(
        db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
            .unwrap()
            .len(),
        1
    );
    same.tenant_id = "t2".into();
    same.installation_id = 100;
    db.wait(upsert_repo(db.db(), &same, 7)).unwrap();
    assert_eq!(
        db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.wait(vars(db.db(), "t1", Owner::Repo(7))).unwrap().len(),
        0
    );
    // The old organization's own rows stay.
    assert_eq!(
        db.wait(secret_names(db.db(), "t1", Owner::Org))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(db.wait(vars(db.db(), "t1", Owner::Org)).unwrap().len(), 1);
}

#[test]
fn a_recreated_repository_takes_the_name_and_the_old_runs_stay() {
    let db = setup();
    let old = db
        .wait(insert_run(db.db(), &new_run("d1", "ci", 2)))
        .unwrap()
        .unwrap();
    let fresh = RepoRow {
        id: 8,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "Acme/App".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
    let by_name = db.wait(repo_by_name(db.db(), "acme/app")).unwrap().unwrap();
    assert_eq!(by_name.id, 8);
    let stale = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    assert_eq!(stale.full_name, "acme/app#gone-7");
    assert!(db.wait(run(db.db(), old.id)).unwrap().is_some());
}

#[test]
fn a_rename_swap_keeps_both_repositories() {
    let db = setup();
    let other = RepoRow {
        id: 8,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/lib".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &other, 2)).unwrap();
    let name = |id| db.wait(repo(db.db(), id)).unwrap().unwrap().full_name;
    // app and lib swap names. The event for app arrives first, while
    // lib still holds its new name.
    let mut app = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    app.full_name = "acme/lib".into();
    db.wait(upsert_repo(db.db(), &app, 3)).unwrap();
    assert_eq!(name(7), "acme/lib");
    assert_eq!(name(8), "acme/lib#gone-8");
    // Then the event for lib.
    let mut lib = db.wait(repo(db.db(), 8)).unwrap().unwrap();
    lib.full_name = "acme/app".into();
    db.wait(upsert_repo(db.db(), &lib, 4)).unwrap();
    assert_eq!(name(7), "acme/lib");
    assert_eq!(name(8), "acme/app");
}

#[test]
fn jobs_come_back_in_plan_order() {
    let db = setup();
    let run = db
        .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
        .unwrap()
        .unwrap();
    let ids = db
        .wait(insert_jobs(
            db.db(),
            run.id,
            &[new_job("test"), new_job("build"), new_job("deploy")],
            2,
        ))
        .unwrap();
    let plan: Vec<&str> = ids.iter().map(|(plan, _)| plan.as_str()).collect();
    assert_eq!(plan, ["test", "build", "deploy"]);
    let jobs = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
    let names: Vec<&str> = jobs.iter().map(|job| job.plan_id.as_str()).collect();
    assert_eq!(names, ["test", "build", "deploy"]);
    assert!(
        jobs.iter()
            .all(|job| job.status == "waiting" && job.attempt == 1)
    );
    assert_eq!(jobs[0].steps, serde_json::json!([]));
}

#[test]
fn insert_jobs_keeps_template_permissions_and_refs() {
    let db = setup();
    let run = db
        .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
        .unwrap()
        .unwrap();
    let build = NewJob {
        template: Some("ubuntu-26.04-8core-arm".into()),
        token_permissions: BTreeMap::from([
            ("contents".to_owned(), Level::Write),
            ("metadata".to_owned(), Level::Read),
        ]),
        secret_refs: SecretRefs {
            all: false,
            names: ["NPM_TOKEN".to_owned()].into(),
        },
        ..new_job("build")
    };
    let approve = NewJob {
        template: None,
        ..new_job("approve")
    };
    db.wait(insert_jobs(db.db(), run.id, &[build, approve], 2))
        .unwrap();
    let jobs = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
    assert_eq!(jobs[0].template.as_deref(), Some("ubuntu-26.04-8core-arm"));
    assert_eq!(
        jobs[0].token_permissions,
        serde_json::json!({ "contents": "write", "metadata": "read" })
    );
    assert_eq!(
        jobs[0].secret_refs,
        serde_json::json!({ "all": false, "names": ["NPM_TOKEN"] })
    );
    let one = db.wait(job(db.db(), jobs[0].id)).unwrap().unwrap();
    assert_eq!(one, jobs[0]);
    assert_eq!(jobs[1].template, None);
    assert_eq!(
        jobs[1].secret_refs,
        serde_json::json!({ "all": false, "names": [] })
    );
}

#[test]
fn claim_launch_only_once() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    let first = claim(&db, job_id, 10);
    assert_eq!(first.launch_attempts, 1);
    assert_eq!(
        db.wait(claim_launch(db.db(), job_id, "t1", 5, 11)).unwrap(),
        Claim::Gone
    );
    let job = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(job.status, "launching");
}

pub(super) fn held_of(db: &TestDb, job_id: u64) -> Option<String> {
    db.column(
        "SELECT held AS h FROM ci_jobs WHERE id = $1",
        &params![job_id],
        "h",
    )
    .unwrap()
}

#[test]
fn claim_launch_holds_the_sixth_job() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 98, 0, 0)",
        &[],
    );
    db.run(
        "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
         private, updated_at) VALUES (8, 't2', 98, 'other/app', 'main', false, 1)",
        &[],
    );
    let other = db
        .wait(insert_run(
            db.db(),
            &NewRun {
                tenant_id: "t2".into(),
                repo_id: 8,
                ..new_run("o", "ci", 1_000)
            },
        ))
        .unwrap()
        .unwrap();
    let other_jobs: Vec<NewJob> = (0..5).map(|n| new_job(&format!("o{n}"))).collect();
    for (_, id) in db
        .wait(insert_jobs(db.db(), other.id, &other_jobs, 1_000))
        .unwrap()
    {
        db.wait(set_job(db.db(), id, "running", None, 1_001))
            .unwrap();
    }
    let statuses = ["launching", "launched", "running", "running", "launching"];
    for (n, status) in statuses.into_iter().enumerate() {
        run_with_job_named(&db, &format!("d{n}"), status);
    }
    let (_, sixth) = run_with_job_named(&db, "d9", "queued");
    assert_eq!(
        db.wait(claim_launch(db.db(), sixth, "t1", 5, 10)).unwrap(),
        Claim::Busy
    );
    let row = db.wait(job(db.db(), sixth)).unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.launch_attempts), ("queued", 0));
    // A completed job frees a place.
    let first = db
        .wait(insert_run(db.db(), &new_run("free", "ci", 1_000)))
        .unwrap()
        .unwrap();
    let done = db
        .wait(insert_jobs(db.db(), first.id, &[new_job("done")], 1_000))
        .unwrap()[0]
        .1;
    db.wait(set_job(db.db(), done, "completed", Some("success"), 1_002))
        .unwrap();
    assert_eq!(
        db.wait(claim_launch(db.db(), sixth, "t1", 5, 11)).unwrap(),
        Claim::Busy,
        "completed jobs do not count, but five still run"
    );
    let running: u64 = db
        .column(
            "SELECT id AS n FROM ci_jobs WHERE status = 'launching' ORDER BY id LIMIT 1",
            &[],
            "n",
        )
        .unwrap();
    db.wait(set_job(
        db.db(),
        running,
        "completed",
        Some("success"),
        1_003,
    ))
    .unwrap();
    assert!(matches!(
        db.wait(claim_launch(db.db(), sixth, "t1", 5, 12)).unwrap(),
        Claim::Claimed(_)
    ));
    let gone = db
        .wait(claim_launch(db.db(), 999_999, "t1", 5, 13))
        .unwrap();
    assert_eq!(gone, Claim::Gone);
}

/// Sets the hold of a job directly, the way `hold` leaves it.
fn set_held(db: &TestDb, job_id: u64, reason: &str, updated: u64) {
    db.run(
        "UPDATE ci_jobs SET held = $2, updated_at = $3 WHERE id = $1",
        &params![job_id, reason, updated],
    );
}

pub(super) fn woken_ids(woken: Vec<hatchrun_cloud::ci_wake::Woken>) -> Vec<u64> {
    woken.into_iter().map(|w| w.job_id).collect()
}

#[test]
fn a_new_job_does_not_jump_a_held_one() {
    let db = setup();
    for (n, status) in ["running", "running", "running", "running"]
        .into_iter()
        .enumerate()
    {
        run_with_job_named(&db, &format!("r{n}"), status);
    }
    let (_, older) = run_with_job_named(&db, "older", "queued");
    let (_, newer) = run_with_job_named(&db, "newer", "queued");
    set_held(&db, older, "concurrency", 5);
    // One place is free, and the older job waits for it.
    assert_eq!(
        db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
        Claim::Busy
    );
    assert!(matches!(
        db.wait(claim_launch(db.db(), older, "t1", 5, 11)).unwrap(),
        Claim::Claimed(_)
    ));
    assert_eq!(
        db.wait(claim_launch(db.db(), newer, "t1", 5, 12)).unwrap(),
        Claim::Busy,
        "the tenant is full again"
    );
}

#[test]
fn a_held_job_of_another_reason_or_tenant_blocks_nobody() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 98, 0, 0)",
        &[],
    );
    db.run(
        "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
         private, updated_at) VALUES (8, 't2', 98, 'other/app', 'main', false, 1)",
        &[],
    );
    let other = db
        .wait(insert_run(
            db.db(),
            &NewRun {
                tenant_id: "t2".into(),
                repo_id: 8,
                ..new_run("o", "ci", 1_000)
            },
        ))
        .unwrap()
        .unwrap();
    let held_elsewhere = db
        .wait(insert_jobs(db.db(), other.id, &[new_job("o")], 1_000))
        .unwrap()[0]
        .1;
    db.wait(set_job(db.db(), held_elsewhere, "queued", None, 1_001))
        .unwrap();
    set_held(&db, held_elsewhere, "concurrency", 5);
    let (_, capped) = run_with_job_named(&db, "capped", "queued");
    set_held(&db, capped, "spend_cap", 5);
    let (_, newer) = run_with_job_named(&db, "newer", "queued");
    assert!(matches!(
        db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
        Claim::Claimed(_)
    ));
}

#[test]
fn the_oldest_held_job_wakes_first() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    let mut ids = Vec::new();
    for n in 0..3 {
        let (_, id) = run_with_job_named(&db, &format!("d{n}"), "queued");
        // The newest was held longest ago: age is the job id.
        set_held(&db, id, "concurrency", 100 - n);
        ids.push(id);
    }
    let (_, running) = run_with_job_named(&db, "run", "running");
    set_held(&db, running, "concurrency", 1);
    let (_, card) = run_with_job_named(&db, "card", "queued");
    set_held(&db, card, "card", 1);
    let one = db
        .wait(ci_wake::held(db.db(), "t1", "concurrency", 1))
        .unwrap();
    assert_eq!(woken_ids(one), [ids[0]]);
    let all = db
        .wait(ci_wake::held(db.db(), "t1", "concurrency", 5))
        .unwrap();
    assert_eq!(woken_ids(all), ids);
    let other = db
        .wait(ci_wake::held(db.db(), "t2", "concurrency", 5))
        .unwrap();
    assert_eq!(woken_ids(other), Vec::<u64>::new());
}

#[test]
fn a_returned_card_turns_the_holds_into_concurrency_holds_and_wakes_five() {
    use hatchrun_cloud::ci_wake;
    assert_eq!(ci_wake::WAKE_LIMIT, hatchrun_workflow::MAX_RUNNING_JOBS);
    let db = setup();
    let mut card = Vec::new();
    for n in 0..7 {
        let (_, id) = run_with_job_named(&db, &format!("c{n}"), "queued");
        set_held(&db, id, "card", 5);
        card.push(id);
    }
    let (_, capped) = run_with_job_named(&db, "cap", "queued");
    set_held(&db, capped, "spend_cap", 5);
    let (_, other) = run_with_job_named(&db, "plain", "queued");
    subscribe(&db, "s1", "active");
    let woken = db.wait(ci_wake::card_returned(db.db(), "t1")).unwrap();
    assert_eq!(woken_ids(woken), card[..5]);
    for id in card.iter().chain([&capped]) {
        assert_eq!(held_of(&db, *id).as_deref(), Some("concurrency"));
    }
    assert_eq!(held_of(&db, other), None, "an unheld job stays unheld");
    // A finishing job wakes the next one in order.
    let next = db
        .wait(ci_wake::held(db.db(), "t1", "concurrency", 1))
        .unwrap();
    assert_eq!(woken_ids(next), card[..1]);
}

#[test]
fn a_new_month_turns_old_spend_cap_holds_into_concurrency_holds() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    let (_, old) = run_with_job_named(&db, "old", "queued");
    set_held(&db, old, "spend_cap", 100);
    let (_, recent) = run_with_job_named(&db, "recent", "queued");
    set_held(&db, recent, "spend_cap", 10_000);
    let (_, card) = run_with_job_named(&db, "card", "queued");
    set_held(&db, card, "card", 100);
    db.wait(ci_wake::release_spend_caps(db.db(), 5_000))
        .unwrap();
    assert_eq!(held_of(&db, old).as_deref(), Some("concurrency"));
    assert_eq!(held_of(&db, recent).as_deref(), Some("spend_cap"));
    assert_eq!(held_of(&db, card).as_deref(), Some("card"));
}

#[test]
fn the_backstop_picks_quiet_held_jobs_per_tenant() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    let mut ids = Vec::new();
    for n in 0..8 {
        let (_, id) = run_with_job_named(&db, &format!("h{n}"), "queued");
        set_held(&db, id, if n % 2 == 0 { "card" } else { "spend_cap" }, 100);
        ids.push(id);
    }
    let (_, recent) = run_with_job_named(&db, "recent", "queued");
    set_held(&db, recent, "card", 10_000);
    let (_, plain) = run_with_job_named(&db, "plain", "queued");
    let _ = plain;
    let picked = db.wait(ci_wake::stale(db.db(), 5_000, 5, 40)).unwrap();
    assert_eq!(woken_ids(picked), ids[..5], "5 per tenant, oldest first");
    let capped = db.wait(ci_wake::stale(db.db(), 5_000, 5, 2)).unwrap();
    assert_eq!(woken_ids(capped), ids[..2], "at most the limit");
    let none = db.wait(ci_wake::stale(db.db(), 50, 5, 40)).unwrap();
    assert!(none.is_empty(), "nothing is quiet that long");
}

#[test]
fn the_backstop_skips_approval_holds() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    let (_, approval) = run_with_job_named(&db, "a", "queued");
    set_held(&db, approval, "approval", 100);
    let (_, card) = run_with_job_named(&db, "c", "queued");
    set_held(&db, card, "card", 100);
    let picked = db.wait(ci_wake::stale(db.db(), 5_000, 5, 40)).unwrap();
    assert_eq!(woken_ids(picked), [card]);
}

#[test]
fn an_approval_hold_blocks_nobody() {
    let db = setup();
    let (_, older) = run_with_job_named(&db, "older", "queued");
    set_held(&db, older, "approval", 5);
    let (_, newer) = run_with_job_named(&db, "newer", "queued");
    assert!(matches!(
        db.wait(claim_launch(db.db(), newer, "t1", 5, 10)).unwrap(),
        Claim::Claimed(_)
    ));
    let month = 1_790_812_800_000_u64;
    let standing = db
        .wait(standing(db.db(), older, month, 1_000_000))
        .unwrap()
        .unwrap();
    assert_eq!(standing.running, 1, "the launching job, not the held one");
}

fn decide(db: &TestDb, job_id: u64, attempt_of_decision: u32) {
    db.run(
        "UPDATE ci_jobs SET approval_attempt = $2, approval_decision = 'approved', \
         approval_by = 'octo', approval_at = 1 WHERE id = $1",
        &params![job_id, attempt_of_decision],
    );
}

#[test]
fn a_decision_applies_once() {
    let db = setup();
    let (_, job) = run_with_job_named(&db, "decide", "queued");
    set_held(&db, job, "approval", 5);
    assert!(
        !db.wait(decide_approval(db.db(), job, 2, true, "octo", 70))
            .unwrap(),
        "another attempt"
    );
    assert!(
        db.wait(decide_approval(db.db(), job, 1, true, "octo", 70))
            .unwrap()
    );
    assert_eq!(held_of(&db, job), None);
    let row = db.wait(super::job(db.db(), job)).unwrap().unwrap();
    assert_eq!(row.approval_attempt, Some(1));
    assert_eq!(row.approval_decision.as_deref(), Some("approved"));
    assert_eq!(row.approval_by.as_deref(), Some("octo"));
    assert_eq!(row.approval_at, Some(70));
    assert_eq!(row.next_launch_at, Some(70));
    assert_eq!(row.updated_at, 70);
    assert!(
        !db.wait(decide_approval(db.db(), job, 1, false, "mona", 80))
            .unwrap(),
        "a second decision"
    );
    let row = db.wait(super::job(db.db(), job)).unwrap().unwrap();
    assert_eq!(row.approval_by.as_deref(), Some("octo"));
    let (_, card) = run_with_job_named(&db, "card", "queued");
    set_held(&db, card, "card", 5);
    assert!(
        !db.wait(decide_approval(db.db(), card, 1, true, "octo", 70))
            .unwrap()
    );
}

#[test]
fn an_approval_hold_does_not_land_on_a_decided_attempt() {
    let db = setup();
    let (_, job) = run_with_job_named(&db, "raced", "queued");
    let attempt: u32 = 1;
    decide(&db, job, attempt);
    assert!(
        !db.wait(hold(db.db(), job, attempt, "approval", 9_000, 2_000))
            .unwrap()
    );
    assert_eq!(held_of(&db, job), None);
    // A billing hold still lands on an approved attempt.
    assert!(
        db.wait(hold(db.db(), job, attempt, "card", 9_000, 2_000))
            .unwrap()
    );
    assert_eq!(held_of(&db, job).as_deref(), Some("card"));
    // A decision of an earlier attempt does not count.
    let (_, other) = run_with_job_named(&db, "rerun", "queued");
    decide(&db, other, 0);
    assert!(
        db.wait(hold(db.db(), other, attempt, "approval", 9_000, 2_000))
            .unwrap()
    );
}

#[test]
fn the_backstop_frees_decided_approval_holds_up_to_the_limit() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    save_environment(&db, "Guarded", &[], true);
    let mut decided = Vec::new();
    for n in 0..3 {
        let id = job_naming(&db, &format!("d{n}"), Some("guarded"));
        db.run(
            "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 100 \
             WHERE id = $1",
            &params![id],
        );
        decided.push(id);
    }
    decide(&db, decided[0], 1);
    decide(&db, decided[1], 1);
    let freed = woken_ids(
        db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 1))
            .unwrap(),
    );
    assert_eq!(freed, [decided[0]], "limit of one, oldest first");
    let freed = woken_ids(
        db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 10))
            .unwrap(),
    );
    assert_eq!(freed, [decided[1]]);
    assert_eq!(held_of(&db, decided[2]).as_deref(), Some("approval"));
}

#[test]
fn a_queued_job_keeps_the_reason_it_was_ended_for() {
    let db = setup();
    let (_, job) = run_with_job_named(&db, "q", "queued");
    assert!(
        db.wait(note_queued_error(db.db(), job, "no such environment", 5))
            .unwrap()
    );
    let error: Option<String> = db
        .column(
            "SELECT launch_error AS e FROM ci_jobs WHERE id = $1",
            &params![job],
            "e",
        )
        .unwrap();
    assert_eq!(error.as_deref(), Some("no such environment"));
    let (_, running) = run_with_job_named(&db, "r", "running");
    assert!(
        !db.wait(note_queued_error(db.db(), running, "x", 5))
            .unwrap()
    );
}

#[test]
fn the_backstop_frees_approval_holds_without_a_rule() {
    use hatchrun_cloud::ci_wake;
    let db = setup();
    let held = |name: &str, environment: &str, updated: u64| {
        let id = job_naming(&db, name, Some(environment));
        db.run(
            "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = $2 \
             WHERE id = $1",
            &params![id, updated],
        );
        id
    };
    save_environment(&db, "Guarded", &[], true);
    save_environment(&db, "Open", &[], false);
    let gone = held("gone", "deleted", 100);
    let off = held("off", "open", 100);
    let guarded = held("guarded", "guarded", 100);
    let recent = held("recent", "deleted", 9_000);
    let mut freed = woken_ids(
        db.wait(ci_wake::release_orphan_approvals(db.db(), 5_000, 6_000, 10))
            .unwrap(),
    );
    freed.sort_unstable();
    assert_eq!(freed, [gone, off]);
    for job in [gone, off] {
        assert_eq!(held_of(&db, job), None);
    }
    assert_eq!(held_of(&db, guarded).as_deref(), Some("approval"));
    assert_eq!(held_of(&db, recent).as_deref(), Some("approval"));
}

#[test]
fn held_jobs_are_not_stalled() {
    let db = setup();
    let (_, held) = run_with_job_named(&db, "held", "queued");
    db.run(
        "UPDATE ci_jobs SET held = 'card', next_launch_at = 1, updated_at = 1 WHERE id = $1",
        &params![held],
    );
    let (_, plain) = run_with_job_named(&db, "plain", "queued");
    db.run(
        "UPDATE ci_jobs SET next_launch_at = 1, updated_at = 1 WHERE id = $1",
        &params![plain],
    );
    let rows = db.wait(stalled_jobs(db.db(), 10_000_000, 50)).unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [plain]);
}

#[test]
fn claim_clears_the_hold() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    assert!(db.wait(hold(db.db(), job_id, 1, "card", 50, 5)).unwrap());
    assert_eq!(held_of(&db, job_id).as_deref(), Some("card"));
    claim(&db, job_id, 10);
    assert_eq!(held_of(&db, job_id), None);
}

#[test]
fn hold_reports_a_change_once() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    assert!(db.wait(hold(db.db(), job_id, 1, "card", 500, 5)).unwrap());
    assert!(!db.wait(hold(db.db(), job_id, 1, "card", 530, 6)).unwrap());
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!((row.next_launch_at, row.updated_at), (Some(530), 6));
    assert!(
        db.wait(hold(db.db(), job_id, 1, "concurrency", 560, 7))
            .unwrap()
    );
    assert_eq!(held_of(&db, job_id).as_deref(), Some("concurrency"));
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(row.next_launch_at, Some(560));
}

#[test]
fn hold_leaves_another_attempt_alone() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    assert!(!db.wait(hold(db.db(), job_id, 2, "card", 500, 5)).unwrap());
    assert_eq!(held_of(&db, job_id), None);
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!((row.next_launch_at, row.updated_at), (None, 1_001));
    let (_, launching) = run_with_job_named(&db, "d2", "launching");
    assert!(
        !db.wait(hold(db.db(), launching, 1, "card", 500, 5))
            .unwrap()
    );
    assert_eq!(held_of(&db, launching), None);
}

#[test]
fn a_rerun_clears_the_hold() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    db.wait(hold(db.db(), job_id, 1, "spend_cap", 50, 5))
        .unwrap();
    db.wait(reset_job(db.db(), job_id, 2, 40)).unwrap();
    assert_eq!(held_of(&db, job_id), None);
}
