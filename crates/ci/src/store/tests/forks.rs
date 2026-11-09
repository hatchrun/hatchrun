// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::policy::{ForkApproval, ForkRuns, HeadCheckout, MAX_PENDING, RepoPolicy};

/// A fork run of pull request `number` at `sha` by `author`, in `gate`.
fn fork_run(
    db: &TestDb,
    delivery: &str,
    number: u64,
    sha: &str,
    author: &str,
    gate: GateStatus,
    now: u64,
) -> RunRow {
    let mut new = new_run(delivery, "ci", now);
    new.event = "pull_request".into();
    new.git_ref = format!("refs/pull/{number}/merge");
    new.sha = sha.into();
    new.base_ref = Some("main".into());
    new.pull_number = Some(number);
    new.head_ref = Some("feature".into());
    new.trust = Trust::Fork;
    new.head_repo_id = Some(99);
    new.head_repo_name = Some("mallory/app".into());
    new.author_login = author.into();
    new.author_association = "FIRST_TIME_CONTRIBUTOR".into();
    new.gate_status = gate;
    db.wait(insert_run(db.db(), &new)).unwrap().unwrap()
}

#[test]
fn a_run_keeps_its_trust_origin_and_gate() {
    let db = setup();
    let run = fork_run(&db, "d", 3, "aaa", "mallory", GateStatus::Pending, 10);
    assert_eq!(run.trust, Trust::Fork);
    assert_eq!(run.head_repo_name.as_deref(), Some("mallory/app"));
    assert_eq!(run.author_login, "mallory");
    assert_eq!(run.author_association, "FIRST_TIME_CONTRIBUTOR");
    assert_eq!(run.gate_status, GateStatus::Pending);
    assert_eq!(
        (run.gate_by, run.gate_at, run.gate_check_run_id),
        (None, None, None)
    );
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert_eq!(read.trust, Trust::Fork);
    let gate: String = db
        .column(
            "SELECT gate AS g FROM ci_runs WHERE id = $1",
            &params![run.id],
            "g",
        )
        .unwrap();
    assert_eq!(gate, "fork_approval");
    let own = db
        .wait(insert_run(db.db(), &new_run("push", "ci", 11)))
        .unwrap()
        .unwrap();
    assert_eq!(
        (own.trust, own.gate_status),
        (Trust::Trusted, GateStatus::NotGated)
    );
    let gate: String = db
        .column(
            "SELECT gate AS g FROM ci_runs WHERE id = $1",
            &params![own.id],
            "g",
        )
        .unwrap();
    assert_eq!(gate, "none");
}

#[test]
fn a_repository_without_a_policy_row_has_the_defaults() {
    let db = setup();
    let public = db.wait(repo_policy(db.db(), 7, false)).unwrap();
    assert_eq!(public, RepoPolicy::default_for(false));
    assert_eq!(public.fork_runs, ForkRuns::On);
    let private = db.wait(repo_policy(db.db(), 7, true)).unwrap();
    assert_eq!(private.fork_runs, ForkRuns::Off);
}

#[test]
fn a_saved_policy_wins_over_the_defaults_and_can_be_changed() {
    let db = setup();
    let policy = RepoPolicy {
        fork_runs: ForkRuns::On,
        fork_approval: ForkApproval::AllOutside,
        fork_max_running: 4,
        fork_timeout_minutes: 15,
        pull_request_target_head_checkout: HeadCheckout::Block,
    };
    db.wait(put_repo_policy(db.db(), 7, &policy, 5)).unwrap();
    // Even for a private repository, the saved row decides.
    assert_eq!(db.wait(repo_policy(db.db(), 7, true)).unwrap(), policy);
    let lower = RepoPolicy {
        fork_approval: ForkApproval::FirstTimers,
        ..policy
    };
    db.wait(put_repo_policy(db.db(), 7, &lower, 6)).unwrap();
    assert_eq!(db.wait(repo_policy(db.db(), 7, false)).unwrap(), lower);
}

#[test]
fn out_of_range_policy_numbers_are_refused_by_the_table() {
    let db = setup();
    let bad = RepoPolicy {
        fork_max_running: 11,
        ..RepoPolicy::default_for(false)
    };
    assert!(db.wait(put_repo_policy(db.db(), 7, &bad, 1)).is_err());
}

#[test]
fn waiting_counts_pending_deferred_and_the_authors_active_runs() {
    let db = setup();
    fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Deferred, 2);
    fork_run(&db, "c", 3, "s3", "eve", GateStatus::Pending, 3);
    fork_run(&db, "d", 4, "s4", "mallory", GateStatus::NotGated, 4);
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory")).unwrap();
    assert_eq!((waiting.pending, waiting.deferred), (2, 1));
    assert_eq!(waiting.author_active, 3);
    let eve = db.wait(fork_waiting(db.db(), 7, "eve")).unwrap();
    assert_eq!(eve.author_active, 1);
    let nobody = db.wait(fork_waiting(db.db(), 7, "nobody")).unwrap();
    assert_eq!(nobody.author_active, 0);
    // A run that ended no longer counts for its author.
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE delivery = 'd'",
        &[],
    );
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory")).unwrap();
    assert_eq!(waiting.author_active, 2);
}

#[test]
fn only_the_first_approval_of_a_waiting_run_applies() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let approved = db
        .wait(approve_run(db.db(), run.id, "maintainer", 50))
        .unwrap()
        .unwrap();
    assert_eq!(approved.gate_status, GateStatus::Approved);
    assert_eq!(approved.gate_by.as_deref(), Some("maintainer"));
    assert_eq!(approved.gate_at, Some(50));
    assert!(
        db.wait(approve_run(db.db(), run.id, "other", 60))
            .unwrap()
            .is_none()
    );
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert_eq!(read.gate_by.as_deref(), Some("maintainer"));
}

#[test]
fn a_deferred_run_can_be_approved_and_an_ended_one_cannot() {
    let db = setup();
    let deferred = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Deferred, 1);
    assert!(
        db.wait(approve_run(db.db(), deferred.id, "m", 5))
            .unwrap()
            .is_some()
    );
    let done = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE id = $1",
        &params![done.id],
    );
    assert!(
        db.wait(approve_run(db.db(), done.id, "m", 5))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_run_that_never_waited_cannot_be_approved() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::NotGated, 1);
    assert!(
        db.wait(approve_run(db.db(), run.id, "m", 5))
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_button_approves_only_the_runs_on_its_check_run_in_its_repository() {
    let db = setup();
    let a = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let b = fork_run(&db, "a2", 1, "s1", "mallory", GateStatus::Pending, 1);
    let other = fork_run(&db, "b", 2, "s2", "eve", GateStatus::Pending, 2);
    db.wait(set_gate_check(db.db(), "a", 5150)).unwrap();
    db.wait(set_gate_check(db.db(), "a2", 5150)).unwrap();
    db.wait(set_gate_check(db.db(), "b", 6000)).unwrap();
    assert_eq!(
        db.wait(approve_by_check(db.db(), 8, 5150, "m", 9))
            .unwrap()
            .len(),
        0
    );
    let approved = db
        .wait(approve_by_check(db.db(), 7, 5150, "maintainer", 9))
        .unwrap();
    let mut ids: Vec<u64> = approved.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [a.id, b.id]);
    let read = db.wait(super::run(db.db(), other.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
    assert_eq!(
        db.wait(approve_by_check(db.db(), 7, 5150, "again", 10))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn a_rejection_and_an_approval_race_to_one_outcome() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let ended = db
        .wait(end_gate(
            db.db(),
            run.id,
            "cancelled",
            Some("maintainer"),
            7,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(ended.gate_status, GateStatus::Cancelled);
    assert!(
        db.wait(approve_run(db.db(), run.id, "late", 8))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(end_gate(db.db(), run.id, "expired", None, 9))
            .unwrap()
            .is_none()
    );
    let approved = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.wait(approve_run(db.db(), approved.id, "m", 3))
        .unwrap()
        .unwrap();
    assert!(
        db.wait(end_gate(db.db(), approved.id, "cancelled", Some("x"), 4))
            .unwrap()
            .is_none(),
        "an approved run is not cancelled by the gate"
    );
}

#[test]
fn a_newer_commit_cancels_the_older_waiting_runs_of_its_pull_request() {
    let db = setup();
    let old = fork_run(&db, "a", 5, "old", "mallory", GateStatus::Pending, 1);
    let deferred = fork_run(&db, "b", 5, "older", "mallory", GateStatus::Deferred, 2);
    let current = fork_run(&db, "c", 5, "new", "mallory", GateStatus::Pending, 3);
    let approved = fork_run(&db, "d", 5, "ok", "mallory", GateStatus::Pending, 4);
    db.wait(approve_run(db.db(), approved.id, "m", 5))
        .unwrap()
        .unwrap();
    let elsewhere = fork_run(&db, "e", 6, "x", "eve", GateStatus::Pending, 5);
    let ended = db
        .wait(end_pull_gates(db.db(), 7, 5, Some("new"), 9))
        .unwrap();
    let mut ids: Vec<u64> = ended.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [old.id, deferred.id]);
    for id in [current.id, elsewhere.id] {
        let read = db.wait(super::run(db.db(), id)).unwrap().unwrap();
        assert_eq!(read.gate_status, GateStatus::Pending);
    }
    // A closed pull request cancels every waiting run.
    let closed = db.wait(end_pull_gates(db.db(), 7, 5, None, 10)).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].id, current.id);
}

#[test]
fn waiting_runs_expire_after_their_time_and_not_before() {
    let db = setup();
    let old = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 100);
    let old_deferred = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Deferred, 200);
    let young = fork_run(&db, "c", 3, "s3", "mallory", GateStatus::Pending, 900);
    let approved = fork_run(&db, "d", 4, "s4", "mallory", GateStatus::Pending, 50);
    db.wait(approve_run(db.db(), approved.id, "m", 60))
        .unwrap()
        .unwrap();
    let expired = db.wait(expire_gates(db.db(), 500, 1_000, 10)).unwrap();
    let mut ids: Vec<u64> = expired.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [old.id, old_deferred.id]);
    assert!(
        expired
            .iter()
            .all(|run| run.gate_status == GateStatus::Expired)
    );
    let read = db.wait(super::run(db.db(), young.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
    // The limit bounds one pass.
    let more = fork_run(&db, "e", 7, "s7", "eve", GateStatus::Pending, 1);
    let more2 = fork_run(&db, "f", 8, "s8", "eve", GateStatus::Pending, 2);
    let first = db.wait(expire_gates(db.db(), 500, 1_000, 1)).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, more.id, "the oldest goes first");
    let rest = db.wait(expire_gates(db.db(), 500, 1_000, 5)).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].id, more2.id);
}

#[test]
fn deferred_runs_are_admitted_oldest_first_while_slots_are_free() {
    let db = setup();
    for n in 0..MAX_PENDING {
        fork_run(
            &db,
            &format!("p{n}"),
            n + 1,
            "s",
            "mallory",
            GateStatus::Pending,
            1,
        );
    }
    let first = fork_run(&db, "d1", 100, "s", "eve", GateStatus::Deferred, 10);
    let second = fork_run(&db, "d2", 101, "s", "eve", GateStatus::Deferred, 11);
    assert_eq!(
        db.wait(admit_deferred(db.db(), 10)).unwrap().len(),
        0,
        "no slot"
    );
    // Two slots free up.
    db.run(
        "UPDATE ci_runs SET gate_status = 'cancelled' WHERE delivery IN ('p0', 'p1')",
        &[],
    );
    let admitted = db.wait(admit_deferred(db.db(), 10)).unwrap();
    let ids: Vec<u64> = admitted.iter().map(|run| run.id).collect();
    assert_eq!(ids, [first.id, second.id]);
    assert!(
        admitted
            .iter()
            .all(|run| run.gate_status == GateStatus::Pending)
    );
    // One slot only: only the oldest is admitted.
    db.wait(end_gate(db.db(), first.id, "cancelled", None, 20))
        .unwrap();
    db.wait(end_gate(db.db(), second.id, "cancelled", None, 20))
        .unwrap();
    let third = fork_run(&db, "d3", 102, "s", "eve", GateStatus::Deferred, 30);
    let fourth = fork_run(&db, "d4", 103, "s", "eve", GateStatus::Deferred, 31);
    db.run(
        "UPDATE ci_runs SET gate_status = 'cancelled' WHERE delivery = 'p2'",
        &[],
    );
    db.run(
        "UPDATE ci_runs SET gate_status = 'cancelled' WHERE delivery = 'p3'",
        &[],
    );
    db.run(
        "UPDATE ci_runs SET gate_status = 'pending' WHERE delivery = 'p0'",
        &[],
    );
    db.run(
        "UPDATE ci_runs SET gate_status = 'pending' WHERE delivery = 'p1'",
        &[],
    );
    let admitted = db.wait(admit_deferred(db.db(), 10)).unwrap();
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].id, third.id);
    assert_eq!(admitted[1].id, fourth.id);
}

#[test]
fn the_app_lists_the_pending_runs_of_the_tenants_readable_repositories() {
    let db = setup();
    let a = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 5);
    fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Deferred, 6);
    fork_run(&db, "c", 3, "s3", "mallory", GateStatus::NotGated, 7);
    let c = fork_run(&db, "d", 4, "s4", "eve", GateStatus::Pending, 3);
    let listed = db.wait(pending_gate_runs(db.db(), "t1", &[7], 10)).unwrap();
    let ids: Vec<u64> = listed.iter().map(|row| row.run.id).collect();
    assert_eq!(ids, [c.id, a.id], "the longest waiting first");
    assert_eq!(listed[0].repository, "acme/app");
    assert_eq!(listed[0].run.trust, Trust::Fork);
    assert_eq!(
        db.wait(pending_gate_runs(db.db(), "t1", &[8], 10))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.wait(pending_gate_runs(db.db(), "t2", &[7], 10))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.wait(pending_gate_runs(db.db(), "t1", &[7], 1))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_fork_job_waits_for_a_place_under_the_repositorys_fork_cap() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::NotGated, 1);
    let jobs: Vec<NewJob> = ["one", "two", "three"].map(new_job).into();
    let ids = db.wait(insert_jobs(db.db(), run.id, &jobs, 1)).unwrap();
    for (_, id) in &ids {
        db.wait(set_job(db.db(), *id, "queued", None, 2)).unwrap();
    }
    let claim = |id| {
        db.wait(claim_launch(db.db(), id, "t1", 5, true, Some(2), 3))
            .unwrap()
    };
    assert!(matches!(claim(ids[0].1), Claim::Claimed(_)));
    assert!(matches!(claim(ids[1].1), Claim::Claimed(_)));
    assert_eq!(claim(ids[2].1), Claim::ForkLimit);
    assert_eq!(db.wait(fork_running(db.db(), ids[2].1)).unwrap(), 2);
    // A job of a trusted run is not held by the fork cap.
    let own = db
        .wait(insert_run(db.db(), &new_run("push", "ci", 4)))
        .unwrap()
        .unwrap();
    let own_job = db
        .wait(insert_jobs(db.db(), own.id, &[new_job("own")], 4))
        .unwrap()[0]
        .1;
    db.wait(set_job(db.db(), own_job, "queued", None, 5))
        .unwrap();
    assert!(matches!(
        db.wait(claim_launch(db.db(), own_job, "t1", 5, true, Some(2), 6))
            .unwrap(),
        Claim::Claimed(_)
    ));
    // A job that ends makes room, and the held one is the one to wake.
    db.wait(set_job(db.db(), ids[0].1, "completed", Some("success"), 7))
        .unwrap();
    db.wait(hold(db.db(), ids[2].1, 1, "fork_limit", 100, 8))
        .unwrap();
    let woken = db.wait(held_fork_jobs(db.db(), ids[1].1)).unwrap();
    assert_eq!(woken.len(), 1);
    assert_eq!(woken[0].job_id, ids[2].1);
    assert!(matches!(claim(ids[2].1), Claim::Claimed(_)));
}

#[test]
fn jobs_held_for_a_runs_approval_are_found_by_run() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let other = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 1);
    let ids = db
        .wait(insert_jobs(
            db.db(),
            run.id,
            &[new_job("x"), new_job("y")],
            1,
        ))
        .unwrap();
    let theirs = db
        .wait(insert_jobs(db.db(), other.id, &[new_job("z")], 1))
        .unwrap();
    for (_, id) in ids.iter().chain(&theirs) {
        db.wait(set_job(db.db(), *id, "queued", None, 2)).unwrap();
        db.wait(hold(db.db(), *id, 1, "fork_approval", 99, 3))
            .unwrap();
    }
    let woken = db.wait(fork_approval_held(db.db(), run.id)).unwrap();
    let got: Vec<u64> = woken.iter().map(|w| w.job_id).collect();
    assert_eq!(got, [ids[0].1, ids[1].1]);
}

#[test]
fn a_pull_request_row_keeps_its_authors_association_and_head_repository() {
    let db = setup();
    let mut pull = NewPull {
        repo_id: 7,
        number: 3,
        title: "t".into(),
        head_ref: "f".into(),
        base_ref: "main".into(),
        author: "mallory".into(),
        author_association: "FIRST_TIMER".into(),
        head_repo_id: Some(99),
        state: "open".into(),
        updated_at: 10,
    };
    db.wait(upsert_pull(db.db(), &pull)).unwrap();
    pull.author_association = "CONTRIBUTOR".into();
    pull.head_repo_id = None;
    pull.updated_at = 20;
    db.wait(upsert_pull(db.db(), &pull)).unwrap();
    let assoc: String = db
        .column(
            "SELECT author_association AS a FROM ci_pulls WHERE number = 3",
            &[],
            "a",
        )
        .unwrap();
    assert_eq!(assoc, "CONTRIBUTOR");
    let head: Option<u64> = db
        .column(
            "SELECT head_repo_id AS h FROM ci_pulls WHERE number = 3",
            &[],
            "h",
        )
        .unwrap();
    assert_eq!(head, None);
}
