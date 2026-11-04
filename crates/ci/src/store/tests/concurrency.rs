// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use hatchrun_cloud::testdb::Db as TestDb;

use super::*;

/// A run of its own with one job, `queued`; returns both ids.
fn queued(db: &TestDb, delivery: &str) -> (u64, u64) {
    let (run, job) = run_with_job_named(db, delivery, "queued");
    (run.id, job)
}

fn note(db: &TestDb, run: u64, job: Option<u64>, group: &str, cancel: bool) -> Slot {
    db.wait(register(
        db.db(),
        &NewSlot {
            repo_id: 7,
            run_id: run,
            job_id: job,
            attempt: 1,
            group: Ok((group.to_owned(), cancel)),
            now: 10,
        },
    ))
    .unwrap();
    let all = db.wait(slots(db.db(), run, job.unwrap_or(0))).unwrap();
    all.into_iter().find(|s| s.job_id == job).unwrap()
}

fn enter_it(db: &TestDb, slot: &Slot) -> Slot {
    db.wait(enter(db.db(), slot)).unwrap().unwrap()
}

#[test]
fn the_first_arrival_runs_and_the_next_waits() {
    let db = setup();
    let (a, _) = queued(&db, "a");
    let (b, _) = queued(&db, "b");
    let first = enter_it(&db, &note(&db, a, None, "Deploy", false));
    assert_eq!(first.state, "running");
    // Groups compare ignoring case.
    let second = enter_it(&db, &note(&db, b, None, "deploy", false));
    assert_eq!(second.state, "pending");
    let who = db
        .wait(blocker(db.db(), 7, "deploy"))
        .unwrap()
        .expect("someone runs");
    assert_eq!(who.workflow, "ci");
    assert_eq!(who.job_name, None);
}

#[test]
fn a_newer_arrival_replaces_the_one_that_waits() {
    let db = setup();
    let (a, _) = queued(&db, "a");
    let (b, _) = queued(&db, "b");
    let (c, _) = queued(&db, "c");
    enter_it(&db, &note(&db, a, None, "g", false));
    let pending = enter_it(&db, &note(&db, b, None, "g", false));
    assert_eq!(pending.state, "pending");
    let newest = enter_it(&db, &note(&db, c, None, "g", false));
    assert_eq!(newest.state, "pending");
    let old = db.wait(slots(db.db(), b, 0)).unwrap().remove(0);
    assert_eq!(old.state, "superseded");
    let victims = db.wait(victims(db.db(), 7, "g")).unwrap();
    assert_eq!(victims.len(), 1);
    assert_eq!(victims[0].run_id, b);
    assert_eq!(victims[0].state, "superseded");
    assert_eq!(victims[0].tenant_id, "t1");
}

#[test]
fn cancel_in_progress_marks_the_running_one() {
    let db = setup();
    let (a, _) = queued(&db, "a");
    let (b, _) = queued(&db, "b");
    enter_it(&db, &note(&db, a, None, "g", false));
    let newer = enter_it(&db, &note(&db, b, None, "g", true));
    assert_eq!(newer.state, "pending");
    let victims = db.wait(victims(db.db(), 7, "g")).unwrap();
    assert_eq!(victims.len(), 1);
    assert_eq!(
        (victims[0].run_id, victims[0].state.as_str()),
        (a, "running")
    );
}

#[test]
fn leaving_lets_the_one_that_waits_run_and_wakes_its_held_jobs() {
    let db = setup();
    let (a, _) = queued(&db, "a");
    let (b, job_b) = queued(&db, "b");
    enter_it(&db, &note(&db, a, None, "g", false));
    enter_it(&db, &note(&db, b, None, "g", false));
    assert!(db.wait(hold(db.db(), job_b, 1, "group", 99, 20)).unwrap());
    assert_eq!(db.wait(wake_targets(db.db(), 7, "g")).unwrap(), []);
    assert_eq!(
        db.wait(leave(db.db(), a, None)).unwrap(),
        Some((7, "g".to_owned()))
    );
    assert_eq!(db.wait(slots(db.db(), b, 0)).unwrap()[0].state, "running");
    let woken = db.wait(wake_targets(db.db(), 7, "g")).unwrap();
    assert_eq!(woken.len(), 1);
    assert_eq!((woken[0].job_id, woken[0].attempt), (job_b, 1));
    // The run that left holds nothing, and leaving twice is harmless.
    assert_eq!(db.wait(leave(db.db(), a, None)).unwrap(), None);
}

#[test]
fn jobs_of_one_run_hold_groups_on_their_own() {
    let db = setup();
    let (a, job_a) = queued(&db, "a");
    let (b, job_b) = queued(&db, "b");
    let first = enter_it(&db, &note(&db, a, Some(job_a), "lock", false));
    let second = enter_it(&db, &note(&db, b, Some(job_b), "lock", false));
    assert_eq!(
        (first.state.as_str(), second.state.as_str()),
        ("running", "pending")
    );
    let who = db.wait(blocker(db.db(), 7, "lock")).unwrap().unwrap();
    assert_eq!(who.job_name.as_deref(), Some("build"));
    let both = db.wait(slots(db.db(), a, job_a)).unwrap();
    assert_eq!(both.len(), 1);
}

#[test]
fn a_holder_that_ended_without_leaving_is_swept() {
    let db = setup();
    let (a, job_a) = queued(&db, "a");
    let (b, _) = queued(&db, "b");
    enter_it(&db, &note(&db, a, Some(job_a), "g", false));
    let waiting = enter_it(&db, &note(&db, b, None, "g", false));
    assert_eq!(waiting.state, "pending");
    db.wait(set_job(db.db(), job_a, "completed", Some("success"), 30))
        .unwrap();
    // Anyone who looks at the group again finds the place free.
    let (c, _) = queued(&db, "c");
    let third = enter_it(&db, &note(&db, c, None, "g", false));
    assert_eq!(
        db.wait(slots(db.db(), b, 0)).unwrap()[0].state,
        "running",
        "the pending one is promoted"
    );
    assert_eq!(third.state, "pending");
}

#[test]
fn a_group_that_failed_to_evaluate_is_recorded_as_invalid() {
    let db = setup();
    let (a, _) = queued(&db, "a");
    db.wait(register(
        db.db(),
        &NewSlot {
            repo_id: 7,
            run_id: a,
            job_id: None,
            attempt: 1,
            group: Err("concurrency group is empty".into()),
            now: 10,
        },
    ))
    .unwrap();
    let row = db.wait(slots(db.db(), a, 0)).unwrap().remove(0);
    assert_eq!(row.state, "invalid");
    assert_eq!(row.group_name, "concurrency group is empty");
    // Evaluating again, now well, replaces it.
    let fixed = note(&db, a, None, "ok", false);
    assert_eq!(fixed.state, "new");
}

#[test]
fn a_superseded_row_whose_job_is_open_is_found_for_another_cancel() {
    let db = setup();
    let (a, job_a) = queued(&db, "a");
    let (b, job_b) = queued(&db, "b");
    let (c, job_c) = queued(&db, "c");
    enter_it(&db, &note(&db, a, Some(job_a), "g", false));
    enter_it(&db, &note(&db, b, Some(job_b), "g", false));
    enter_it(&db, &note(&db, c, Some(job_c), "g", false));
    // b was superseded by c; its cancel is lost.
    let stuck = db.wait(unfinished_superseded(db.db(), 10)).unwrap();
    assert_eq!(stuck.len(), 1);
    assert_eq!((stuck[0].run_id, stuck[0].job_id), (b, Some(job_b)));
    // The holder leaves and c is promoted: b is still found.
    db.wait(leave(db.db(), a, Some(job_a))).unwrap();
    assert_eq!(
        db.wait(unfinished_superseded(db.db(), 10)).unwrap().len(),
        1
    );
    // Once the job ended it is not sent again.
    db.wait(set_job(db.db(), job_b, "completed", Some("cancelled"), 40))
        .unwrap();
    assert_eq!(db.wait(unfinished_superseded(db.db(), 10)).unwrap(), []);
}
