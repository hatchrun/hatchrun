// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::policy::{ForkApproval, ForkRuns, RepoPolicy};

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
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!((waiting.pending, waiting.deferred), (2, 1));
    assert_eq!(waiting.author_active, 2);
    let eve = db.wait(fork_waiting(db.db(), 7, "eve", None)).unwrap();
    assert_eq!(eve.author_active, 1);
    let nobody = db.wait(fork_waiting(db.db(), 7, "nobody", None)).unwrap();
    assert_eq!(nobody.author_active, 0);
    // A run that ended no longer counts for its author.
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE delivery = 'd'",
        &[],
    );
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 1);
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
    db.wait(set_gate_check(db.db(), 7, 1, "s1", 5150)).unwrap();
    db.wait(set_gate_check(db.db(), 7, 1, "s1", 5150)).unwrap();
    db.wait(set_gate_check(db.db(), 7, 2, "s2", 6000)).unwrap();
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 8, 5150, "m", 9))
            .unwrap()
            .len(),
        0
    );
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 5150, "maintainer", 9))
        .unwrap();
    let mut ids: Vec<u64> = approved.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [a.id, b.id]);
    let read = db.wait(super::run(db.db(), other.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 5150, "again", 10))
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
fn deferred_runs_are_listed_oldest_first_and_admitted_once() {
    let db = setup();
    let first = fork_run(&db, "d1", 100, "s", "eve", GateStatus::Deferred, 10);
    let second = fork_run(&db, "d2", 101, "s", "eve", GateStatus::Deferred, 11);
    fork_run(&db, "p", 102, "s", "eve", GateStatus::Pending, 5);
    let listed = db.wait(deferred_runs(db.db(), 10, 5)).unwrap();
    let ids: Vec<u64> = listed.iter().map(|run| run.id).collect();
    assert_eq!(ids, [first.id, second.id]);
    assert_eq!(db.wait(deferred_runs(db.db(), 1, 5)).unwrap().len(), 1);
    let admitted = db
        .wait(admit_run(db.db(), first.id, GateStatus::Pending, None, 20))
        .unwrap()
        .unwrap();
    assert_eq!(admitted.gate_status, GateStatus::Pending);
    assert!(
        db.wait(admit_run(db.db(), first.id, GateStatus::Pending, None, 21))
            .unwrap()
            .is_none(),
        "a run that left the queue is not admitted again"
    );
    // A run that needs no approval is admitted as approved by the policy.
    let auto = db
        .wait(admit_run(
            db.db(),
            second.id,
            GateStatus::Approved,
            Some("policy"),
            22,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(auto.gate_status, GateStatus::Approved);
    assert_eq!(auto.gate_by.as_deref(), Some("policy"));
    assert_eq!(db.wait(deferred_runs(db.db(), 10, 5)).unwrap().len(), 0);
}

#[test]
fn an_author_over_the_cap_is_deferred_then_admitted_when_runs_end() {
    let db = setup();
    let policy = RepoPolicy::default_for(false);
    // Three runs of the author are active, so the fourth delivery defers.
    for n in 0..3 {
        fork_run(
            &db,
            &format!("a{n}"),
            n + 1,
            "s",
            "mallory",
            GateStatus::NotGated,
            1,
        );
    }
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 3);
    assert_eq!(
        crate::policy::admission(&policy, "CONTRIBUTOR", waiting),
        crate::policy::Admission::Deferred
    );
    let deferred = fork_run(&db, "late", 9, "s", "mallory", GateStatus::Deferred, 5);
    // The deferred run is not one of the author's active runs.
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 3);
    assert_eq!(waiting.deferred, 1);
    // One of the three ends: the author is under the cap, and the
    // reconciler's decision for the deferred run (with its own count
    // left out) lets it run without an approval.
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE delivery = 'a0'",
        &[],
    );
    let mut waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 2);
    waiting.deferred = 0;
    assert_eq!(
        crate::policy::admission(&policy, &deferred.author_association, waiting),
        crate::policy::Admission::Pending,
        "a first-time contributor asks for approval once admitted"
    );
    assert_eq!(
        crate::policy::admission(&policy, "CONTRIBUTOR", waiting),
        crate::policy::Admission::Run
    );
    let admitted = db
        .wait(admit_run(
            db.db(),
            deferred.id,
            GateStatus::Pending,
            None,
            30,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(admitted.gate_status, GateStatus::Pending);
}

#[test]
fn a_waiting_run_with_no_check_run_is_found_for_the_reconciler() {
    let db = setup();
    let lost = fork_run(&db, "lost", 1, "s1", "mallory", GateStatus::Pending, 10);
    let lost_deferred = fork_run(&db, "lost2", 2, "s2", "mallory", GateStatus::Deferred, 20);
    let carded = fork_run(&db, "carded", 3, "s3", "mallory", GateStatus::Pending, 10);
    fork_run(&db, "young", 4, "s4", "mallory", GateStatus::Pending, 900);
    fork_run(&db, "free", 5, "s5", "mallory", GateStatus::NotGated, 10);
    db.wait(set_gate_check(db.db(), 7, 3, "s3", 5150)).unwrap();
    let found = db.wait(runs_without_card(db.db(), 500, 10, 5)).unwrap();
    let ids: Vec<u64> = found.iter().map(|run| run.id).collect();
    assert_eq!(ids, [lost.id, lost_deferred.id]);
    assert!(!ids.contains(&carded.id));
    // Once its id is recorded (adopted or made), it is not found again.
    db.wait(set_gate_check(db.db(), 7, 1, "s1", 6000)).unwrap();
    let found = db.wait(runs_without_card(db.db(), 500, 10, 5)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, lost_deferred.id);
    // And the button, which finds the run by its check run id, works.
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 6000, "maintainer", 40))
        .unwrap();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].id, lost.id);
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

fn second_repo(db: &TestDb) {
    let repo = RepoRow {
        id: 8,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/other".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();
}

#[test]
fn one_card_covers_every_waiting_run_of_the_commit_and_one_click_decides_them_all() {
    let db = setup();
    let ci = fork_run(&db, "d1", 4, "head", "mallory", GateStatus::Pending, 1);
    let mut lint = new_run("d1", "lint", 1);
    lint.event = "pull_request".into();
    lint.git_ref = "refs/pull/4/merge".into();
    lint.sha = "head".into();
    lint.pull_number = Some(4);
    lint.trust = Trust::Fork;
    lint.author_login = "mallory".into();
    lint.gate_status = GateStatus::Pending;
    let lint = db.wait(insert_run(db.db(), &lint)).unwrap().unwrap();
    // The same pull request at an older commit is a different card.
    let older = fork_run(&db, "d0", 4, "older", "mallory", GateStatus::Pending, 0);
    let group = db.wait(gate_group(db.db(), 7, 4, "head")).unwrap();
    let ids: Vec<u64> = group.iter().map(|run| run.id).collect();
    assert_eq!(ids, [ci.id, lint.id]);
    db.wait(set_gate_check(db.db(), 7, 4, "head", 5150))
        .unwrap();
    for id in [ci.id, lint.id] {
        let read = db.wait(super::run(db.db(), id)).unwrap().unwrap();
        assert_eq!(read.gate_check_run_id, Some(5150));
    }
    let read = db.wait(super::run(db.db(), older.id)).unwrap().unwrap();
    assert_eq!(
        read.gate_check_run_id, None,
        "another commit has its own card"
    );
    // Approving from the app approves what the card lists, no more.
    let approved = db
        .wait(approve_group(db.db(), ci.id, "maintainer", 10))
        .unwrap();
    let mut got: Vec<u64> = approved.iter().map(|run| run.id).collect();
    got.sort_unstable();
    assert_eq!(got, [ci.id, lint.id]);
    let read = db.wait(super::run(db.db(), older.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
    assert_eq!(
        db.wait(approve_group(db.db(), ci.id, "again", 11))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn the_apps_reject_cancels_what_the_card_lists() {
    let db = setup();
    let a = fork_run(&db, "d1", 4, "head", "mallory", GateStatus::Pending, 1);
    let b = fork_run(&db, "d2", 4, "head", "mallory", GateStatus::Deferred, 2);
    let other = fork_run(&db, "d3", 5, "head", "eve", GateStatus::Pending, 3);
    let ended = db
        .wait(end_group(db.db(), b.id, "cancelled", Some("maintainer"), 9))
        .unwrap();
    let mut ids: Vec<u64> = ended.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [a.id, b.id]);
    let read = db.wait(super::run(db.db(), other.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
}

#[test]
fn the_button_also_approves_a_deferred_run_its_card_lists() {
    let db = setup();
    let pending = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let deferred = fork_run(&db, "b", 1, "s1", "mallory", GateStatus::Deferred, 2);
    db.wait(set_gate_check(db.db(), 7, 1, "s1", 5150)).unwrap();
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 5150, "m", 9))
        .unwrap();
    let mut ids: Vec<u64> = approved.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [pending.id, deferred.id]);
}

#[test]
fn stuck_runs_of_one_repository_do_not_starve_the_others() {
    let db = setup();
    second_repo(&db);
    for n in 0..7 {
        let run = fork_run(
            &db,
            &format!("stuck{n}"),
            n + 1,
            "s",
            "mallory",
            GateStatus::Deferred,
            10 + n,
        );
        db.run(
            "UPDATE ci_runs SET gate_check_run_id = NULL WHERE id = $1",
            &params![run.id],
        );
    }
    let other = fork_run(&db, "other", 50, "s", "eve", GateStatus::Deferred, 900);
    db.run(
        "UPDATE ci_runs SET repo_id = 8 WHERE id = $1",
        &params![other.id],
    );
    // Seven older runs of repository 7 would fill a plain oldest-first
    // page; the window keeps five of each repository.
    let listed = db.wait(deferred_runs(db.db(), 6, 5)).unwrap();
    let repos: Vec<u64> = listed.iter().map(|run| run.repo_id).collect();
    assert_eq!(repos.iter().filter(|&&id| id == 7).count(), 5);
    assert!(listed.iter().any(|run| run.id == other.id), "{repos:?}");
    let cards = db.wait(runs_without_card(db.db(), 10_000, 6, 5)).unwrap();
    assert_eq!(cards.iter().filter(|run| run.repo_id == 7).count(), 5);
    assert!(cards.iter().any(|run| run.id == other.id));
}

#[test]
fn an_authors_cap_counts_pull_requests_not_runs() {
    let db = setup();
    // One pull request with three workflows is three runs and one pull
    // request.
    for (n, workflow) in ["ci", "lint", "docs"].into_iter().enumerate() {
        let mut new = new_run(&format!("d{n}"), workflow, 1);
        new.event = "pull_request".into();
        new.git_ref = "refs/pull/4/merge".into();
        new.pull_number = Some(4);
        new.trust = Trust::Fork;
        new.author_login = "mallory".into();
        new.gate_status = GateStatus::NotGated;
        db.wait(insert_run(db.db(), &new)).unwrap().unwrap();
    }
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 1);
    fork_run(&db, "p5", 5, "s", "mallory", GateStatus::NotGated, 2);
    fork_run(&db, "p6", 6, "s", "mallory", GateStatus::Pending, 3);
    let waiting = db.wait(fork_waiting(db.db(), 7, "mallory", None)).unwrap();
    assert_eq!(waiting.author_active, 3);
    // A new commit on a pull request that is active already is not a
    // fourth pull request.
    let again = db
        .wait(fork_waiting(db.db(), 7, "mallory", Some(4)))
        .unwrap();
    assert_eq!(again.author_active, 2);
}

#[test]
fn a_fork_run_whose_approval_ended_asks_again_when_rerun() {
    let db = setup();
    let expired = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    db.wait(set_gate_check(db.db(), 7, 1, "s1", 5150)).unwrap();
    db.wait(end_gate(db.db(), expired.id, "expired", None, 5))
        .unwrap()
        .unwrap();
    let reopened = db.wait(reopen_gate(db.db(), expired.id)).unwrap().unwrap();
    assert_eq!(reopened.gate_status, GateStatus::Pending);
    assert_eq!((reopened.gate_by, reopened.gate_at), (None, None));
    assert_eq!(reopened.gate_check_run_id, None, "the old card is decided");
    // Pending again: a second rerun changes nothing.
    assert!(db.wait(reopen_gate(db.db(), expired.id)).unwrap().is_none());
    let cancelled = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.wait(end_gate(db.db(), cancelled.id, "cancelled", Some("m"), 6))
        .unwrap()
        .unwrap();
    assert!(
        db.wait(reopen_gate(db.db(), cancelled.id))
            .unwrap()
            .is_some()
    );
    // An approved run keeps its approval, and a trusted run has none.
    let approved = fork_run(&db, "c", 3, "s3", "mallory", GateStatus::Pending, 3);
    db.wait(approve_run(db.db(), approved.id, "m", 7))
        .unwrap()
        .unwrap();
    assert!(
        db.wait(reopen_gate(db.db(), approved.id))
            .unwrap()
            .is_none()
    );
    let own = db
        .wait(insert_run(db.db(), &new_run("push", "ci", 4)))
        .unwrap()
        .unwrap();
    assert!(db.wait(reopen_gate(db.db(), own.id)).unwrap().is_none());
    assert!(!crate::fork::rerun_reopens(&approved));
    assert!(crate::fork::rerun_reopens(&expired_row(&db, expired.id)));
}

fn expired_row(db: &TestDb, id: u64) -> RunRow {
    db.run(
        "UPDATE ci_runs SET gate_status = 'expired' WHERE id = $1",
        &params![id],
    );
    db.wait(super::run(db.db(), id)).unwrap().unwrap()
}
