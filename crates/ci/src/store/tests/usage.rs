// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use hatchrun_cloud::testdb::Db as TestDb;

#[test]
fn a_failed_launch_retries_or_fails_the_job() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    claim(&db, job_id, 10);
    db.wait(launch_failed(db.db(), job_id, "no capacity", Some(500), 20))
        .unwrap();
    let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(
        (job_row.status.as_str(), job_row.next_launch_at),
        ("queued", Some(500))
    );
    claim(&db, job_id, 30);
    db.wait(launch_failed(db.db(), job_id, "no capacity", None, 40))
        .unwrap();
    let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(job_row.status, "completed");
    assert_eq!(job_row.conclusion.as_deref(), Some("failure"));
    assert_eq!(job_row.completed_at, Some(40));
}

#[test]
fn late_calls_leave_a_completed_job_alone() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    claim(&db, job_id, 10);
    assert!(db.wait(launched(db.db(), job_id, "i-1", 11)).unwrap());
    assert!(db.wait(start_job(db.db(), job_id, 12)).unwrap());
    let steps = serde_json::json!([{"name": "first"}]);
    assert!(
        db.wait(finish_job(
            db.db(),
            job_id,
            1,
            "success",
            &steps,
            Some("k1"),
            20
        ))
        .unwrap()
    );
    let late = serde_json::json!([{"name": "late"}]);
    assert!(
        !db.wait(finish_job(
            db.db(),
            job_id,
            1,
            "failure",
            &late,
            Some("k2"),
            30
        ))
        .unwrap()
    );
    assert!(!db.wait(start_job(db.db(), job_id, 31)).unwrap());
    assert!(!db.wait(launched(db.db(), job_id, "i-2", 32)).unwrap());
    assert!(
        !db.wait(launch_failed(db.db(), job_id, "late", Some(900), 33))
            .unwrap()
    );
    assert!(
        !db.wait(launch_failed(db.db(), job_id, "late", None, 34))
            .unwrap()
    );
    db.wait(heartbeat(db.db(), job_id, 40)).unwrap();
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert_eq!(row.conclusion.as_deref(), Some("success"));
    assert_eq!(row.steps, steps);
    assert_eq!(row.log_key.as_deref(), Some("k1"));
    assert_eq!(row.instance_id.as_deref(), Some("i-1"));
    assert_eq!(
        (row.completed_at, row.heartbeat_at, row.next_launch_at),
        (Some(20), Some(12), None)
    );
    assert_eq!(db.wait(open_jobs(db.db(), 10)).unwrap().len(), 0);
}

#[test]
fn a_launch_result_for_a_job_that_is_not_launching_changes_nothing() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    assert!(!db.wait(launched(db.db(), job_id, "i-1", 11)).unwrap());
    assert!(
        !db.wait(launch_failed(db.db(), job_id, "e", None, 12))
            .unwrap()
    );
    assert!(!db.wait(start_job(db.db(), job_id, 13)).unwrap());
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(row.status, "queued");
}

#[test]
fn launched_records_the_instance() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    claim(&db, job_id, 10);
    db.wait(launched(db.db(), job_id, "i-123", 11)).unwrap();
    let job_row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(job_row.status, "launched");
    assert_eq!(job_row.instance_id.as_deref(), Some("i-123"));
}

#[test]
fn sweep_deletes_expired_runs_and_old_deliveries() {
    let db = setup();
    let old = db
        .wait(insert_run(db.db(), &new_run("old", "ci", 1_000)))
        .unwrap()
        .unwrap();
    db.wait(insert_jobs(db.db(), old.id, &[new_job("build")], 1_000))
        .unwrap();
    let fresh = db
        .wait(insert_run(
            db.db(),
            &new_run("fresh", "ci", 1_000 + 20 * DAY),
        ))
        .unwrap()
        .unwrap();
    let now = 1_000 + 31 * DAY;
    db.wait(record_delivery(db.db(), "old", "push", 1_000))
        .unwrap();
    db.wait(record_delivery(db.db(), "fresh", "push", now - DAY))
        .unwrap();
    let gone = db.wait(sweep(db.db(), now)).unwrap();
    assert_eq!(gone, vec![old.id]);
    assert!(db.wait(run(db.db(), old.id)).unwrap().is_none());
    assert!(db.wait(run(db.db(), fresh.id)).unwrap().is_some());
    let jobs: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_jobs", &[], "n")
        .unwrap();
    assert_eq!(jobs, 0);
    assert!(!db.wait(delivery_seen(db.db(), "old")).unwrap());
    assert!(db.wait(delivery_seen(db.db(), "fresh")).unwrap());
}

#[test]
fn sweep_deletes_the_oldest_hundred_at_a_time() {
    let db = setup();
    for n in 0..(SWEEP_LIMIT + 5) {
        db.wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", 1_000)))
            .unwrap()
            .unwrap();
    }
    let now = 1_000 + 31 * DAY;
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 100);
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 5);
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 0);
}

#[test]
fn stalled_jobs_match_the_reconcilers_decisions() {
    use crate::reconcile::{Fix, stalled};
    let db = setup();
    let run = db
        .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
        .unwrap()
        .unwrap();
    let names = [
        "fresh-queued",
        "quiet-queued",
        "due-queued",
        "old-launching",
        "old-launched",
        "silent-running",
        "beating-running",
        "old-waiting",
        "old-completed",
    ];
    let jobs: Vec<NewJob> = names.iter().map(|name| new_job(name)).collect();
    let ids = db.wait(insert_jobs(db.db(), run.id, &jobs, 1)).unwrap();
    let now = 10_000_000u64;
    let set = |name: &str, status: &str, updated: u64, heartbeat: Option<u64>, due: Option<u64>| {
        let id = ids.iter().find(|(plan, _)| plan == name).unwrap().1;
        db.run(
            "UPDATE ci_jobs SET status = $2, updated_at = $3, heartbeat_at = $4, \
             started_at = $3, next_launch_at = $5 WHERE id = $1",
            &params![id, status, updated, heartbeat, due],
        );
    };
    set("fresh-queued", "queued", now - 1_000, None, None);
    set("quiet-queued", "queued", now - 61_000, None, None);
    set("due-queued", "queued", now - 1_000, None, Some(now - 1));
    set("old-launching", "launching", now - 601_000, None, None);
    set("old-launched", "launched", now - 601_000, None, None);
    set(
        "silent-running",
        "running",
        now - 1_000,
        Some(now - 121_000),
        None,
    );
    set(
        "beating-running",
        "running",
        now - 1_000,
        Some(now - 5_000),
        None,
    );
    set("old-waiting", "waiting", 0, None, None);
    set("old-completed", "completed", 0, None, None);
    let rows = db.wait(stalled_jobs(db.db(), now, 50)).unwrap();
    let mut from_sql: Vec<String> = rows.iter().map(|row| row.plan_id.clone()).collect();
    from_sql.sort();
    assert_eq!(
        from_sql,
        [
            "due-queued",
            "old-launched",
            "old-launching",
            "quiet-queued",
            "silent-running"
        ]
    );
    let all = db.wait(jobs_for_run(db.db(), run.id)).unwrap();
    let mut from_rust: Vec<u64> = stalled(&all, now)
        .into_iter()
        .map(|fix| match fix {
            Fix::Requeue(id) | Fix::Fail(id, _) | Fix::TerminateAndFail(id, ..) => id,
        })
        .collect();
    from_rust.sort_unstable();
    let mut sql_ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
    sql_ids.sort_unstable();
    assert_eq!(from_rust, sql_ids);
    assert_eq!(db.wait(stalled_jobs(db.db(), now, 2)).unwrap().len(), 2);
}

#[test]
fn reset_job_clears_the_attempt() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "queued");
    claim(&db, job_id, 10);
    db.wait(start_job(db.db(), job_id, 20)).unwrap();
    let steps = serde_json::json!([{"name": "build"}]);
    db.wait(finish_job(
        db.db(),
        job_id,
        1,
        "failure",
        &steps,
        Some("logs/x"),
        30,
    ))
    .unwrap();
    let done = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(done.steps, steps);
    assert_eq!(done.log_key.as_deref(), Some("logs/x"));
    db.wait(reset_job(db.db(), job_id, 2, 40)).unwrap();
    let reset = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(reset.status, "waiting");
    assert_eq!(reset.attempt, 2);
    assert_eq!(reset.launch_attempts, 0);
    assert_eq!(reset.conclusion, None);
    assert_eq!(reset.instance_id, None);
    assert_eq!(reset.next_launch_at, None);
    assert_eq!(reset.heartbeat_at, None);
    assert_eq!(reset.started_at, None);
    assert_eq!(reset.completed_at, None);
    assert_eq!(reset.log_key, None);
    assert_eq!(reset.steps, serde_json::json!([]));
    assert_eq!(reset.updated_at, 40);
}

#[test]
fn start_job_sets_the_heartbeat() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launched");
    db.wait(start_job(db.db(), job_id, 77)).unwrap();
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.heartbeat_at, Some(77));
    assert_eq!(row.started_at, Some(77));
    db.wait(heartbeat(db.db(), job_id, 90)).unwrap();
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!((row.heartbeat_at, row.started_at), (Some(90), Some(77)));
}

#[test]
fn late_repeats_of_side_effects_change_nothing() {
    let db = setup();
    let (run_row, job_id) = run_with_job(&db, "queued");
    let steps = serde_json::json!([]);
    db.wait(reset_job(db.db(), job_id, 2, 5)).unwrap();
    db.wait(set_job(db.db(), job_id, "launching", None, 6))
        .unwrap();
    // The same reset again, and a finish of attempt 1, both late.
    db.wait(reset_job(db.db(), job_id, 2, 7)).unwrap();
    assert_eq!(
        db.wait(job(db.db(), job_id)).unwrap().unwrap().status,
        "launching"
    );
    assert!(
        !db.wait(finish_job(db.db(), job_id, 1, "failure", &steps, None, 8))
            .unwrap()
    );
    db.wait(reopen_run_at(db.db(), run_row.id, 2, 9)).unwrap();
    db.wait(finish_run(db.db(), run_row.id, 1, "failure", 10))
        .unwrap();
    let row = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.conclusion), ("in_progress", None));
    assert!(
        db.wait(finish_job(db.db(), job_id, 2, "success", &steps, None, 11))
            .unwrap()
    );
}

#[test]
fn queue_job_only_moves_a_waiting_job_of_that_attempt() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "waiting");
    assert!(!db.wait(queue_job(db.db(), job_id, 2, 5)).unwrap());
    assert!(db.wait(queue_job(db.db(), job_id, 1, 5)).unwrap());
    assert!(db.wait(queue_job(db.db(), job_id, 1, 6)).unwrap());
    db.wait(set_job(db.db(), job_id, "launching", None, 7))
        .unwrap();
    assert!(!db.wait(queue_job(db.db(), job_id, 1, 8)).unwrap());
    let row = db.wait(job(db.db(), job_id)).unwrap().unwrap();
    assert_eq!(row.status, "launching");
}

#[test]
fn reopen_run_at_sets_the_attempt_and_repeats() {
    let db = setup();
    let (run_row, _) = run_with_job(&db, "queued");
    db.wait(finish_run(db.db(), run_row.id, 1, "failure", 6))
        .unwrap();
    for _ in 0..2 {
        db.wait(reopen_run_at(db.db(), run_row.id, 2, 7)).unwrap();
        let row = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
        assert_eq!((row.run_attempt, row.status.as_str()), (2, "in_progress"));
        assert_eq!(row.conclusion, None);
    }
}

#[test]
fn reopening_a_run_sets_the_attempt() {
    let db = setup();
    let (run_row, _) = run_with_job(&db, "queued");
    db.wait(run_started(db.db(), run_row.id, 5)).unwrap();
    let started = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
    assert_eq!(started.status, "in_progress");
    db.wait(finish_run(db.db(), run_row.id, 1, "failure", 6))
        .unwrap();
    let finished = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
    assert_eq!(finished.status, "completed");
    assert_eq!(finished.conclusion.as_deref(), Some("failure"));
    assert_eq!(finished.completed_at, Some(6));
    db.wait(reopen_run_at(db.db(), run_row.id, 2, 7)).unwrap();
    let reopened = db.wait(run(db.db(), run_row.id)).unwrap().unwrap();
    assert_eq!(reopened.status, "in_progress");
    assert_eq!(reopened.conclusion, None);
    assert_eq!(reopened.run_attempt, 2);
}

#[test]
fn lease_is_taken_once() {
    let db = setup();
    assert!(db.wait(lease(db.db(), 1_000, 56_000)).unwrap());
    assert!(!db.wait(lease(db.db(), 1_001, 56_001)).unwrap());
    assert!(db.wait(lease(db.db(), 57_000, 112_000)).unwrap());
}

#[test]
fn the_last_minute_is_kept() {
    let db = setup();
    assert_eq!(db.wait(last_minute(db.db())).unwrap(), None);
    db.wait(set_last_minute(db.db(), 60_000)).unwrap();
    assert_eq!(db.wait(last_minute(db.db())).unwrap(), Some(60_000));
}

#[test]
fn replace_schedules_replaces_the_repo_rows() {
    let db = setup();
    let both = [
        ("ci".to_owned(), "0 * * * *".to_owned()),
        ("lint".to_owned(), "5 * * * *".to_owned()),
    ];
    db.wait(replace_schedules(db.db(), 7, &both)).unwrap();
    assert_eq!(db.wait(schedules(db.db(), 10)).unwrap().len(), 2);
    db.wait(replace_schedules(db.db(), 7, &both[1..])).unwrap();
    let rows = db.wait(schedules(db.db(), 10)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].repo_id, rows[0].workflow.as_str()), (7, "lint"));
    db.wait(replace_schedules(db.db(), 7, &[])).unwrap();
    assert_eq!(db.wait(schedules(db.db(), 10)).unwrap().len(), 0);
}

#[test]
fn tenants_and_repos_are_found() {
    let db = setup();
    let tenant = db
        .wait(tenant_for_installation(db.db(), 99))
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            tenant.id.as_str(),
            tenant.org_login.as_str(),
            tenant.ci_billing
        ),
        ("t1", "acme", CiBilling::Never)
    );
    assert!(
        db.wait(tenant_for_installation(db.db(), 1))
            .unwrap()
            .is_none()
    );
    let by_name = db.wait(repo_by_name(db.db(), "ACME/App")).unwrap().unwrap();
    assert_eq!(by_name.id, 7);
    let mut renamed = by_name;
    renamed.full_name = "acme/renamed".into();
    renamed.private = true;
    db.wait(upsert_repo(db.db(), &renamed, 2)).unwrap();
    let again = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    assert_eq!(
        (again.full_name.as_str(), again.private),
        ("acme/renamed", true)
    );
}

#[test]
fn tenants_are_found_by_id_and_runs_by_delivery() {
    let db = setup();
    let found = db.wait(tenant(db.db(), "t1")).unwrap().unwrap();
    assert_eq!(found.id, "t1");
    assert!(db.wait(tenant(db.db(), "nope")).unwrap().is_none());
    let run = db
        .wait(insert_run(db.db(), &new_run("d1", "ci", 5)))
        .unwrap()
        .unwrap();
    let found = db
        .wait(run_for_delivery(db.db(), "d1", "ci"))
        .unwrap()
        .unwrap();
    assert_eq!(found.id, run.id);
    assert!(
        db.wait(run_for_delivery(db.db(), "d1", "lint"))
            .unwrap()
            .is_none()
    );
}

pub(super) fn subscribe(db: &TestDb, id: &str, status: &str) {
    db.run(
        "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
         cancel_at_period_end, modified_at, updated_at) \
         VALUES ($1, 't1', $2, 'c', 'p', false, 't', 0)",
        &params![id, status],
    );
}

#[test]
fn has_card_needs_an_active_subscription() {
    let db = setup();
    let card = |db: &TestDb| db.wait(has_card(db.db(), "t1")).unwrap();
    assert!(!card(&db), "no subscription");
    subscribe(&db, "s1", "canceled");
    assert!(!card(&db), "canceled");
    subscribe(&db, "s2", "active");
    assert!(card(&db));
    db.run(
        "UPDATE subscriptions SET status = 'trialing' WHERE id = 's2'",
        &[],
    );
    assert!(card(&db), "trialing");
    assert!(!db.wait(has_card(db.db(), "nope")).unwrap());
}

#[test]
fn tenant_row_reads_the_billing_state() {
    let db = setup();
    let state = |db: &TestDb| {
        let by_id = db.wait(tenant(db.db(), "t1")).unwrap().unwrap().ci_billing;
        let by_installation = db
            .wait(tenant_for_installation(db.db(), 99))
            .unwrap()
            .unwrap()
            .ci_billing;
        assert_eq!(by_id, by_installation);
        by_id
    };
    assert_eq!(state(&db), CiBilling::Never);
    subscribe(&db, "s1", "canceled");
    assert_eq!(state(&db), CiBilling::Lapsed);
    subscribe(&db, "s2", "active");
    assert_eq!(state(&db), CiBilling::Active);
    db.run(
        "UPDATE subscriptions SET status = 'canceled' WHERE id = 's2'",
        &[],
    );
    assert_eq!(state(&db), CiBilling::Lapsed);
    db.run(
        "UPDATE subscriptions SET status = 'trialing' WHERE id = 's2'",
        &[],
    );
    assert_eq!(state(&db), CiBilling::Active);
}

#[test]
fn deliveries_are_recorded_once() {
    let db = setup();
    assert!(!db.wait(delivery_seen(db.db(), "g")).unwrap());
    db.wait(record_delivery(db.db(), "g", "push", 1)).unwrap();
    db.wait(record_delivery(db.db(), "g", "push", 2)).unwrap();
    assert!(db.wait(delivery_seen(db.db(), "g")).unwrap());
}

#[test]
fn runs_page_newest_first() {
    let db = setup();
    let mut ids = Vec::new();
    for n in 0..3 {
        let run = db
            .wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", n)))
            .unwrap()
            .unwrap();
        ids.push(run.id);
    }
    let page = db.wait(runs_for_repo(db.db(), 7, None, 2)).unwrap();
    assert_eq!(
        page.iter().map(|r| r.id).collect::<Vec<_>>(),
        [ids[2], ids[1]]
    );
    let next = db.wait(runs_for_repo(db.db(), 7, Some(ids[1]), 2)).unwrap();
    assert_eq!(next.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[0]]);
}

#[test]
fn open_jobs_skip_completed_ones_and_oldest_come_first() {
    let db = setup();
    let run = db
        .wait(insert_run(db.db(), &new_run("d", "ci", 1)))
        .unwrap()
        .unwrap();
    let ids = db
        .wait(insert_jobs(
            db.db(),
            run.id,
            &[new_job("a"), new_job("b"), new_job("c")],
            100,
        ))
        .unwrap();
    db.wait(set_job(db.db(), ids[0].1, "queued", None, 300))
        .unwrap();
    db.wait(set_job(db.db(), ids[1].1, "queued", None, 200))
        .unwrap();
    db.wait(set_job(
        db.db(),
        ids[2].1,
        "completed",
        Some("success"),
        400,
    ))
    .unwrap();
    let open = db.wait(open_jobs(db.db(), 10)).unwrap();
    assert_eq!(
        open.iter().map(|j| j.id).collect::<Vec<_>>(),
        [ids[1].1, ids[0].1]
    );
    let some = db
        .wait(jobs_by_ids(db.db(), &[ids[2].1, ids[0].1]))
        .unwrap();
    assert_eq!(some.len(), 2);
    assert_eq!(db.wait(jobs_by_ids(db.db(), &[])).unwrap().len(), 0);
    let done = db.wait(job(db.db(), ids[2].1)).unwrap().unwrap();
    assert_eq!(done.completed_at, Some(400));
}

fn usage_rows(db: &TestDb) -> Vec<(u64, u64, String, u64, u64)> {
    db.rows(
        "SELECT job_id, attempt, template, seconds, ended_at FROM ci_usage \
         ORDER BY job_id, attempt",
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row["job_id"].as_u64().unwrap(),
            row["attempt"].as_u64().unwrap(),
            row["template"].as_str().unwrap().to_owned(),
            row["seconds"].as_u64().unwrap(),
            row["ended_at"].as_u64().unwrap(),
        )
    })
    .collect()
}

fn finish(db: &TestDb, job_id: u64, attempt: u32, now: u64) -> bool {
    db.wait(finish_job(
        db.db(),
        job_id,
        attempt,
        "success",
        &serde_json::json!([]),
        None,
        now,
    ))
    .unwrap()
}

#[test]
fn finish_job_records_seconds_from_claim() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launching");
    assert!(db.wait(start_job(db.db(), job_id, 10_000)).unwrap());
    assert!(finish(&db, job_id, 1, 71_000));
    assert_eq!(
        usage_rows(&db),
        [(job_id, 1, "ubuntu-latest".to_owned(), 61, 71_000)]
    );
}

#[test]
fn a_job_never_claimed_is_not_metered() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launched");
    assert!(finish(&db, job_id, 1, 71_000));
    assert_eq!(usage_rows(&db).len(), 0);
}

#[test]
fn finishing_twice_records_once() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launching");
    db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
    assert!(finish(&db, job_id, 1, 71_000));
    assert!(!finish(&db, job_id, 1, 71_000), "same time");
    assert!(!finish(&db, job_id, 1, 500_000), "later");
    assert_eq!(usage_rows(&db).len(), 1);
    assert_eq!(usage_rows(&db)[0].4, 71_000);
}

#[test]
fn each_attempt_is_metered() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launching");
    db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
    assert!(finish(&db, job_id, 1, 20_000));
    db.wait(reset_job(db.db(), job_id, 2, 30_000)).unwrap();
    db.wait(set_job(db.db(), job_id, "launching", None, 30_001))
        .unwrap();
    db.wait(start_job(db.db(), job_id, 31_000)).unwrap();
    assert!(finish(&db, job_id, 2, 100_000));
    let rows = usage_rows(&db);
    assert_eq!(
        rows.iter().map(|r| (r.1, r.3)).collect::<Vec<_>>(),
        [(1, 60), (2, 69)]
    );
}

#[test]
fn stale_result_is_not_metered() {
    let db = setup();
    let (_, job_id) = run_with_job(&db, "launching");
    db.wait(start_job(db.db(), job_id, 10_000)).unwrap();
    db.wait(reset_job(db.db(), job_id, 2, 20_000)).unwrap();
    db.wait(set_job(db.db(), job_id, "launching", None, 20_001))
        .unwrap();
    db.wait(start_job(db.db(), job_id, 21_000)).unwrap();
    assert!(!finish(&db, job_id, 1, 90_000));
    assert_eq!(usage_rows(&db).len(), 0);
}

#[test]
fn seconds_round_up_with_a_minute_minimum() {
    let db = setup();
    let units_of = |job_id: u64| -> u64 {
        db.rows(
            "SELECT units FROM ci_usage WHERE job_id = $1",
            &params![job_id],
        )
        .unwrap()[0]["units"]
            .as_u64()
            .unwrap()
    };
    let mut got = Vec::new();
    let finished = |n: usize, ms: u64, template: Option<&str>| {
        let run = db
            .wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", 1)))
            .unwrap()
            .unwrap();
        let ids = db
            .wait(insert_jobs(db.db(), run.id, &[new_job("build")], 1))
            .unwrap();
        let job_id = ids[0].1;
        if let Some(template) = template {
            db.run(
                "UPDATE ci_jobs SET template = $2 WHERE id = $1",
                &params![job_id, template],
            );
        }
        db.wait(set_job(db.db(), job_id, "launching", None, 2))
            .unwrap();
        db.wait(start_job(db.db(), job_id, 1_000)).unwrap();
        assert!(finish(&db, job_id, 1, 1_000 + ms));
        (job_id, usage_rows(&db).last().unwrap().3)
    };
    for (n, ms) in [0_u64, 1, 60_000, 60_001, 61_500].into_iter().enumerate() {
        let (job_id, seconds) = finished(n, ms, None);
        assert_eq!(
            seconds,
            hatchrun_cloud::usage::job_seconds(1_000, 1_000 + ms)
        );
        // 2 vCPU: one unit a second.
        assert_eq!(units_of(job_id), seconds);
        got.push(seconds);
    }
    assert_eq!(got, [60, 60, 60, 61, 62]);
    let (job_id, seconds) = finished(9, 90_500, Some("ubuntu-26.04-8core"));
    assert_eq!((seconds, units_of(job_id)), (91, 364));
}

#[test]
fn standing_counts_this_month_and_running_jobs() {
    let db = setup();
    let month = 1_790_812_800_000_u64;
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 98, 0, 0)",
        &[],
    );
    // Units are what was recorded at completion: job 2 was priced before
    // a price change, job 5 has a template the catalog no longer has.
    for (job, tenant, template, seconds, ended, units) in [
        (1, "t1", "ubuntu-latest", 420, month - 1, 420),
        (2, "t1", "ubuntu-latest", 300, month + DAY, 1_000),
        (3, "t1", "ubuntu-26.04-4core", 180, month + 2 * DAY, 180),
        (4, "t2", "ubuntu-latest", 6_000, month + DAY, 6_000),
        (5, "t1", "removed-label", 60, month + 3 * DAY, 77),
    ] {
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
             ended_at) VALUES ($1, 1, $2, $3, $4, $6, $5)",
            &params![job, tenant, template, seconds, ended, units],
        );
    }
    let (_, first) = run_with_job(&db, "running");
    let (_, second) = run_with_job_named(&db, "d2", "launching");
    let (_, _done) = run_with_job_named(&db, "d3", "completed");
    let (_, _queued) = run_with_job_named(&db, "d4", "queued");
    let default_cap = 50_000_000;
    let mut standing_t1 = db
        .wait(standing(db.db(), first, month, default_cap))
        .unwrap()
        .unwrap();
    standing_t1.seconds_by_template.sort();
    assert_eq!(
        standing_t1,
        Standing {
            tenant_id: "t1".into(),
            org_login: "acme".into(),
            card: false,
            cap_micros: default_cap,
            seconds_by_template: vec![
                ("removed-label".into(), 60),
                ("ubuntu-26.04-4core".into(), 180),
                ("ubuntu-latest".into(), 300)
            ],
            units: 1_257,
            running: 2,
        }
    );
    let by_tenant = db
        .wait(tenant_standing(db.db(), "t1", month, default_cap))
        .unwrap()
        .unwrap();
    let by_second_job = db
        .wait(standing(db.db(), second, month, default_cap))
        .unwrap()
        .unwrap();
    assert_eq!(by_tenant.running, 2);
    assert_eq!(by_second_job.tenant_id, "t1");
    // Another tenant: its own rows, no jobs, and a cap row wins.
    db.run(
        "INSERT INTO ci_spend_caps (tenant_id, cap_micros, updated_at) \
         VALUES ('t2', 70000000, 0)",
        &[],
    );
    subscribe(&db, "s1", "active");
    let other = db
        .wait(tenant_standing(db.db(), "t2", month, default_cap))
        .unwrap()
        .unwrap();
    assert_eq!(
        other.seconds_by_template,
        [("ubuntu-latest".to_owned(), 6_000)]
    );
    assert_eq!(
        (other.running, other.cap_micros, other.card),
        (0, 70_000_000, false)
    );
    let first_again = db
        .wait(tenant_standing(db.db(), "t1", month, default_cap))
        .unwrap()
        .unwrap();
    assert!(first_again.card);
    assert!(
        db.wait(tenant_standing(db.db(), "nope", month, default_cap))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(standing(db.db(), 999_999, month, default_cap))
            .unwrap()
            .is_none()
    );
    let empty = db
        .wait(tenant_standing(
            db.db(),
            "t1",
            month + 40 * DAY,
            default_cap,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(empty.seconds_by_template.len(), 0);
    assert_eq!(empty.units, 0);
}

#[test]
fn sweep_deletes_usage_sent_long_ago() {
    let db = setup();
    let now = 200 * DAY;
    for (job, sent) in [
        (1, Some(now - 91 * DAY)),
        (2, Some(now - 89 * DAY)),
        (3, None),
    ] {
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
             ended_at, sent_at) VALUES ($1, 1, 't1', 'ubuntu-latest', 60, 60, 0, $2)",
            &params![job, sent],
        );
    }
    db.run(
        "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
         ended_at, rejected_at) VALUES (4, 1, 't1', 'ubuntu-latest', 60, 60, 0, $1)",
        &params![now - 91 * DAY],
    );
    db.wait(sweep(db.db(), now)).unwrap();
    assert_eq!(
        usage_rows(&db).iter().map(|r| r.0).collect::<Vec<_>>(),
        [2, 3]
    );
}

#[test]
fn caps_are_set_and_replaced() {
    let db = setup();
    let month = 1_790_812_800_000_u64;
    let default_cap = 50_000_000;
    db.wait(set_spend_cap(db.db(), "t1", 75_000_000, 5))
        .unwrap();
    let s = db
        .wait(tenant_standing(db.db(), "t1", month, default_cap))
        .unwrap()
        .unwrap();
    assert_eq!(s.cap_micros, 75_000_000);
    db.wait(set_spend_cap(db.db(), "t1", 60_000_000, 6))
        .unwrap();
    let s = db
        .wait(tenant_standing(db.db(), "t1", month, default_cap))
        .unwrap()
        .unwrap();
    assert_eq!(s.cap_micros, 60_000_000);
    let t = db.wait(tenant_by_org_id(db.db(), 1)).unwrap().unwrap();
    assert_eq!(t.id, "t1");
    assert!(db.wait(tenant_by_org_id(db.db(), 2)).unwrap().is_none());
}

#[test]
fn usage_goes_to_polar_oldest_first_and_once() {
    let db = setup();
    for (job, attempt, ended) in [(3, 1, 30), (1, 1, 10), (1, 2, 20), (2, 1, 20)] {
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
             ended_at) VALUES ($1, $2, 't1', 'ubuntu-latest', 240, 240, $3)",
            &params![job, attempt, ended],
        );
    }
    let keys = |rows: &[UnsentUsage]| -> Vec<(u64, u32)> {
        rows.iter().map(|r| (r.job_id, r.attempt)).collect()
    };
    let all = db.wait(unsent_usage(db.db(), 10)).unwrap();
    assert_eq!(keys(&all), [(1, 1), (1, 2), (2, 1), (3, 1)]);
    assert_eq!(all[0].tenant_id, "t1");
    assert_eq!(all[0].seconds, 240);
    assert_eq!(all[0].units, 240);
    let two = db.wait(unsent_usage(db.db(), 2)).unwrap();
    assert_eq!(keys(&two), [(1, 1), (1, 2)]);
    db.wait(usage_sent(db.db(), &keys(&two), 99)).unwrap();
    db.wait(usage_sent(db.db(), &[], 99)).unwrap();
    let rest = db.wait(unsent_usage(db.db(), 10)).unwrap();
    assert_eq!(keys(&rest), [(2, 1), (3, 1)]);
    // A quarantined row is never listed again, nor marked sent.
    db.wait(usage_rejected(db.db(), &[(2, 1)], "no such customer", 150))
        .unwrap();
    db.wait(usage_sent(db.db(), &[(2, 1)], 160)).unwrap();
    let rest = db.wait(unsent_usage(db.db(), 10)).unwrap();
    assert_eq!(keys(&rest), [(3, 1)]);
    let row = db
        .rows(
            "SELECT sent_at, rejected_at, reject_reason FROM ci_usage \
             WHERE job_id = 2",
            &[],
        )
        .unwrap();
    assert!(row[0]["sent_at"].is_null());
    assert_eq!(row[0]["rejected_at"].as_u64(), Some(150));
    assert_eq!(row[0]["reject_reason"].as_str(), Some("no such customer"));
    // A second mark does not move the time.
    db.wait(usage_sent(db.db(), &[(1, 1)], 200)).unwrap();
    let sent = db
        .rows(
            "SELECT sent_at FROM ci_usage WHERE job_id = 1 AND attempt = 1",
            &[],
        )
        .unwrap();
    assert_eq!(sent[0]["sent_at"].as_u64(), Some(99));
}
