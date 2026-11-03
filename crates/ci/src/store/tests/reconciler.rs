// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;

#[test]
fn schedules_are_read_a_page_at_a_time() {
    let db = setup();
    for id in [8, 9] {
        let repo = RepoRow {
            id,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: format!("acme/app{id}"),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();
    }
    for repo_id in [7, 8] {
        let rows: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|workflow| ((*workflow).to_owned(), "* * * * *".to_owned()))
            .collect();
        db.wait(replace_schedules(db.db(), repo_id, &rows)).unwrap();
    }
    db.wait(replace_schedules(
        db.db(),
        9,
        &[("a".to_owned(), "* * * * *".to_owned())],
    ))
    .unwrap();
    let mut seen = Vec::new();
    let mut after: Option<ScheduleRow> = None;
    let mut pages = 0;
    loop {
        let page = db
            .wait(schedules_after(db.db(), after.as_ref(), 2))
            .unwrap();
        pages += 1;
        assert!(page.len() <= 2);
        after = page.last().cloned();
        let last = page.len() < 2;
        seen.extend(page);
        if last {
            break;
        }
    }
    // Seven rows in pages of two end with a short page.
    assert_eq!(pages, 4);
    let keys: Vec<_> = seen
        .iter()
        .map(|row| (row.repo_id, row.workflow.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            (7, "a"),
            (7, "b"),
            (7, "c"),
            (8, "a"),
            (8, "b"),
            (8, "c"),
            (9, "a")
        ]
    );
}

#[test]
fn sweep_returns_expired_runs_and_keeps_them_until_deleted() {
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
    // A run nobody deleted is returned again.
    for _ in 0..2 {
        assert_eq!(db.wait(sweep(db.db(), now)).unwrap(), vec![old.id]);
        assert!(db.wait(run(db.db(), old.id)).unwrap().is_some());
    }
    assert!(!db.wait(delivery_seen(db.db(), "old")).unwrap());
    assert!(db.wait(delivery_seen(db.db(), "fresh")).unwrap());
    db.wait(delete_runs(db.db(), &[old.id])).unwrap();
    assert!(db.wait(run(db.db(), old.id)).unwrap().is_none());
    assert!(db.wait(run(db.db(), fresh.id)).unwrap().is_some());
    let jobs: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_jobs", &[], "n")
        .unwrap();
    assert_eq!(jobs, 0);
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 0);
}

#[test]
fn sweep_returns_the_oldest_hundred_at_a_time() {
    let db = setup();
    for n in 0..(SWEEP_LIMIT + 5) {
        db.wait(insert_run(db.db(), &new_run(&format!("d{n}"), "ci", 1_000)))
            .unwrap()
            .unwrap();
    }
    let now = 1_000 + 31 * DAY;
    let first = db.wait(sweep(db.db(), now)).unwrap();
    assert_eq!(first.len(), 100);
    db.wait(delete_runs(db.db(), &first)).unwrap();
    let rest = db.wait(sweep(db.db(), now)).unwrap();
    assert_eq!(rest.len(), 5);
    db.wait(delete_runs(db.db(), &rest)).unwrap();
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap().len(), 0);
}

#[test]
fn deferred_runs_wait_behind_newer_expired_runs() {
    let db = setup();
    let mut stuck = Vec::new();
    for n in 0..SWEEP_LIMIT {
        let row = db
            .wait(insert_run(db.db(), &new_run(&format!("s{n}"), "ci", 1_000)))
            .unwrap()
            .unwrap();
        stuck.push(row.id);
    }
    let newer = db
        .wait(insert_run(db.db(), &new_run("newer", "ci", 2_000)))
        .unwrap()
        .unwrap();
    let now = 1_000 + 31 * DAY;
    // The oldest hundred fill the batch and hide the newer run.
    let first = db.wait(sweep(db.db(), now)).unwrap();
    assert_eq!(first, stuck);
    // Their purge failed: they are deferred and the newer run comes up.
    db.wait(defer_runs(db.db(), &first, now)).unwrap();
    assert_eq!(db.wait(sweep(db.db(), now)).unwrap(), vec![newer.id]);
    // The deferred runs stay until the hour is up, then return.
    assert!(db.wait(run(db.db(), stuck[0])).unwrap().is_some());
    db.wait(delete_runs(db.db(), &[newer.id])).unwrap();
    assert_eq!(db.wait(sweep(db.db(), now + 3_599_000)).unwrap().len(), 0);
    assert_eq!(db.wait(sweep(db.db(), now + 3_600_001)).unwrap(), stuck);
}

#[test]
fn deferring_no_runs_changes_nothing() {
    let db = setup();
    db.wait(defer_runs(db.db(), &[], 5)).unwrap();
}
