// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::policy::{ForkApproval, ForkRuns, HeadCheckout, RepoPolicy};

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
    let run = db.wait(insert_run(db.db(), &new)).unwrap().unwrap();
    // The webhook records the pull request before it makes the runs; a
    // run is approved only while its pull request is open at its commit.
    // Forced, so a later call for the same pull request wins whatever the
    // time says.
    db.wait(upsert_pull_forced(
        db.db(),
        &NewPull {
            repo_id: 7,
            number,
            title: "change".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            author: author.into(),
            author_association: "FIRST_TIME_CONTRIBUTOR".into(),
            head_repo_id: Some(99),
            head_sha: sha.into(),
            state: "open".into(),
            updated_at: 5_000_000,
        },
    ))
    .unwrap();
    run
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

fn audit(now: u64) -> NewAudit<'static> {
    NewAudit {
        tenant: "t1",
        actor: "admin",
        action: Action::ForkPolicyPut,
        scope: "repo",
        repo: Some("acme/app"),
        target: "acme/app",
        now,
    }
}

fn audit_rows(db: &TestDb) -> u64 {
    db.column(
        "SELECT count(*)::bigint AS n FROM ci_audit WHERE action = 'fork_policy.put'",
        &[],
        "n",
    )
    .unwrap()
}

#[test]
fn a_policy_and_its_audit_row_are_saved_together_or_not_at_all() {
    let db = setup();
    let policy = RepoPolicy::default_for(false);
    db.wait(put_repo_policy(db.db(), 7, &policy, 5, &audit(5)))
        .unwrap();
    assert_eq!(audit_rows(&db), 1);
    // A policy the table refuses leaves no audit row behind.
    let bad = RepoPolicy {
        fork_max_running: 11,
        ..policy
    };
    assert!(
        db.wait(put_repo_policy(db.db(), 7, &bad, 6, &audit(6)))
            .is_err()
    );
    assert_eq!(audit_rows(&db), 1);
    assert_eq!(db.wait(repo_policy(db.db(), 7, false)).unwrap(), policy);
    // An audit row the table refuses (an unknown tenant) undoes the
    // policy write too.
    let stranger = NewAudit {
        tenant: "nobody",
        ..audit(7)
    };
    let changed = RepoPolicy {
        fork_max_running: 5,
        ..policy
    };
    assert!(
        db.wait(put_repo_policy(db.db(), 7, &changed, 7, &stranger))
            .is_err()
    );
    assert_eq!(db.wait(repo_policy(db.db(), 7, false)).unwrap(), policy);
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
        pull_request_target_head_checkout: HeadCheckout::Approve,
    };
    db.wait(put_repo_policy(db.db(), 7, &policy, 5, &audit(5)))
        .unwrap();
    // Even for a private repository, the saved row decides.
    assert_eq!(db.wait(repo_policy(db.db(), 7, true)).unwrap(), policy);
    let lower = RepoPolicy {
        fork_approval: ForkApproval::FirstTimers,
        ..policy
    };
    db.wait(put_repo_policy(db.db(), 7, &lower, 6, &audit(6)))
        .unwrap();
    assert_eq!(db.wait(repo_policy(db.db(), 7, false)).unwrap(), lower);
    // The head checkout rule is saved and read back with the rest.
    let warn = RepoPolicy {
        pull_request_target_head_checkout: HeadCheckout::Warn,
        ..lower
    };
    db.wait(put_repo_policy(db.db(), 7, &warn, 7, &audit(7)))
        .unwrap();
    assert_eq!(db.wait(repo_policy(db.db(), 7, false)).unwrap(), warn);
}

#[test]
fn out_of_range_policy_numbers_are_refused_by_the_table() {
    let db = setup();
    let bad = RepoPolicy {
        fork_max_running: 11,
        ..RepoPolicy::default_for(false)
    };
    assert!(
        db.wait(put_repo_policy(db.db(), 7, &bad, 1, &audit(1)))
            .is_err()
    );
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
        .wait(approve_run(db.db(), "t1", 7, run.id, "maintainer", 50))
        .unwrap()
        .unwrap();
    assert_eq!(approved.gate_status, GateStatus::Approved);
    assert_eq!(approved.gate_by.as_deref(), Some("maintainer"));
    assert_eq!(approved.gate_at, Some(50));
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, run.id, "other", 60))
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
        db.wait(approve_run(db.db(), "t1", 7, deferred.id, "m", 5))
            .unwrap()
            .is_some()
    );
    let done = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE id = $1",
        &params![done.id],
    );
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, done.id, "m", 5))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_run_that_never_waited_cannot_be_approved() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::NotGated, 1);
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, run.id, "m", 5))
            .unwrap()
            .is_none()
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
        db.wait(approve_run(db.db(), "t1", 7, run.id, "late", 8))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(end_gate(db.db(), run.id, "expired", None, 9))
            .unwrap()
            .is_none()
    );
    let approved = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.wait(approve_run(db.db(), "t1", 7, approved.id, "m", 3))
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
    db.wait(approve_run(db.db(), "t1", 7, approved.id, "m", 5))
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
    db.wait(approve_run(db.db(), "t1", 7, approved.id, "m", 60))
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
    let listed = db.wait(deferred_runs(db.db(), 10, 5, 5)).unwrap();
    let ids: Vec<u64> = listed.iter().map(|run| run.id).collect();
    assert_eq!(ids, [first.id, second.id]);
    assert_eq!(db.wait(deferred_runs(db.db(), 1, 5, 5)).unwrap().len(), 1);
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
    assert_eq!(db.wait(deferred_runs(db.db(), 10, 5, 5)).unwrap().len(), 0);
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
        head_sha: "abc".into(),
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

#[test]
fn queues_count_groups_not_runs() {
    let db = setup();
    // Three workflows of one pull request at one commit are one waiting
    // group, so they take one of the repository's approval slots.
    for (n, workflow) in ["ci", "lint", "docs"].into_iter().enumerate() {
        let mut new = new_run(&format!("d{n}"), workflow, 1);
        new.event = "pull_request".into();
        new.git_ref = "refs/pull/4/merge".into();
        new.sha = "head".into();
        new.pull_number = Some(4);
        new.trust = Trust::Fork;
        new.author_login = "mallory".into();
        new.gate_status = GateStatus::Pending;
        db.wait(insert_run(db.db(), &new)).unwrap().unwrap();
    }
    fork_run(&db, "e", 5, "head", "eve", GateStatus::Pending, 2);
    fork_run(&db, "f", 6, "head", "eve", GateStatus::Deferred, 3);
    let waiting = db.wait(fork_waiting(db.db(), 7, "nobody", None)).unwrap();
    assert_eq!((waiting.pending, waiting.deferred), (2, 1));
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
fn each_run_has_its_own_card_and_the_button_approves_exactly_that_run() {
    let db = setup();
    // Two workflows of one pull request at one commit: two runs, two
    // cards.
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
    db.wait(set_gate_check(db.db(), ci.id, 5150)).unwrap();
    db.wait(set_gate_check(db.db(), lint.id, 5151)).unwrap();
    // Another repository's and another tenant's click find nothing.
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 8, 5150, "m", 9))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t2", 7, 5150, "m", 9))
            .unwrap()
            .len(),
        0
    );
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 5150, "maintainer", 9))
        .unwrap();
    let ids: Vec<u64> = approved.iter().map(|run| run.id).collect();
    assert_eq!(ids, [ci.id], "the click approves the run its card is for");
    let read = db.wait(super::run(db.db(), lint.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending, "the other run waits");
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 5150, "again", 10))
            .unwrap()
            .len(),
        0
    );
    // A card that waits for a slot has no button, so the click does not
    // approve a deferred run.
    let deferred = fork_run(&db, "d2", 5, "s", "eve", GateStatus::Deferred, 2);
    db.wait(set_gate_check(db.db(), deferred.id, 6000)).unwrap();
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 6000, "m", 11))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn the_apps_decision_is_one_run_scoped_by_repository_and_tenant() {
    let db = setup();
    let a = fork_run(&db, "d1", 4, "head", "mallory", GateStatus::Pending, 1);
    let b = fork_run(&db, "d2", 4, "head", "mallory", GateStatus::Pending, 2);
    // Another repository or tenant cannot decide it.
    assert!(
        db.wait(approve_run(db.db(), "t1", 8, a.id, "m", 5))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(approve_run(db.db(), "t2", 7, a.id, "m", 5))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(reject_run(db.db(), "t2", 7, a.id, "m", 5))
            .unwrap()
            .is_none()
    );
    let approved = db
        .wait(approve_run(db.db(), "t1", 7, a.id, "maintainer", 6))
        .unwrap()
        .unwrap();
    assert_eq!(approved.id, a.id);
    let read = db.wait(super::run(db.db(), b.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending, "only the one run");
    let rejected = db
        .wait(reject_run(db.db(), "t1", 7, b.id, "maintainer", 7))
        .unwrap()
        .unwrap();
    assert_eq!(rejected.gate_status, GateStatus::Cancelled);
    assert_eq!(rejected.gate_by.as_deref(), Some("maintainer"));
    assert!(
        db.wait(reject_run(db.db(), "t1", 7, a.id, "m", 8))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_waiting_run_with_no_check_run_is_found_for_the_reconciler() {
    let db = setup();
    let lost = fork_run(&db, "lost", 1, "s1", "mallory", GateStatus::Pending, 10);
    let lost_deferred = fork_run(&db, "lost2", 2, "s2", "mallory", GateStatus::Deferred, 20);
    let carded = fork_run(&db, "carded", 3, "s3", "mallory", GateStatus::Pending, 10);
    fork_run(&db, "young", 4, "s4", "mallory", GateStatus::Pending, 900);
    fork_run(&db, "free", 5, "s5", "mallory", GateStatus::NotGated, 10);
    db.wait(set_gate_check(db.db(), carded.id, 5150)).unwrap();
    let found = db.wait(runs_needing_card(db.db(), 500, 10, 5)).unwrap();
    let ids: Vec<u64> = found.iter().map(|run| run.id).collect();
    assert_eq!(ids, [lost.id, lost_deferred.id]);
    // Once its id is recorded (adopted or made), it is not found again,
    // and the button, which finds the run by that id, works.
    db.wait(set_gate_check(db.db(), lost.id, 6000)).unwrap();
    let found = db.wait(runs_needing_card(db.db(), 500, 10, 5)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, lost_deferred.id);
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 6000, "maintainer", 40))
        .unwrap();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].id, lost.id);
}

#[test]
fn a_card_is_recorded_only_for_a_run_that_still_waits() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    db.wait(approve_run(db.db(), "t1", 7, run.id, "m", 2))
        .unwrap()
        .unwrap();
    // A card that finishes being made after the run was decided is not
    // recorded as the run's button.
    let kept = db.wait(set_gate_check(db.db(), run.id, 5150)).unwrap();
    assert!(
        !kept,
        "the caller settles the new card to the decided state"
    );
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert_eq!(read.gate_check_run_id, None);
    // A run that waits records it.
    let waiting = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 3);
    assert!(db.wait(set_gate_check(db.db(), waiting.id, 6000)).unwrap());
}

#[test]
fn a_run_has_at_most_one_card_and_the_first_recorded_wins() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 10);
    assert!(db.wait(set_gate_check(db.db(), run.id, 5150)).unwrap());
    // A second card made by a racing attempt is not recorded; the caller
    // supersedes it.
    assert!(!db.wait(set_gate_check(db.db(), run.id, 5151)).unwrap());
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert_eq!(read.gate_check_run_id, Some(5150));
    assert!(!read.gate_card_dirty);
}

#[test]
fn a_card_that_could_not_be_updated_keeps_its_id_and_is_found_for_refresh() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 10);
    db.wait(set_gate_check(db.db(), run.id, 5150)).unwrap();
    assert_eq!(
        db.wait(runs_needing_card(db.db(), 500, 10, 5))
            .unwrap()
            .len(),
        0
    );
    db.wait(set_card_dirty(db.db(), run.id, true)).unwrap();
    let found = db.wait(runs_needing_card(db.db(), 500, 10, 5)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, run.id);
    assert_eq!(found[0].gate_check_run_id, Some(5150), "the id is kept");
    assert!(found[0].gate_card_dirty);
    // Brought up to date, it is not found again, and the button still
    // finds the run.
    db.wait(set_card_dirty(db.db(), run.id, false)).unwrap();
    assert_eq!(
        db.wait(runs_needing_card(db.db(), 500, 10, 5))
            .unwrap()
            .len(),
        0
    );
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 5150, "m", 20))
        .unwrap();
    assert_eq!(approved.len(), 1);
    // The approval marked the card to be settled in its own statement,
    // and a mark is cleared only once the card was.
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert!(read.gate_card_dirty);
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
fn neither_one_repository_nor_one_author_fills_the_deferred_pass() {
    let db = setup();
    second_repo(&db);
    // Seven older deferred groups of one author in repository 7, and
    // one newer group of another author in the same repository.
    for n in 0..7 {
        fork_run(
            &db,
            &format!("stuck{n}"),
            n + 1,
            "s",
            "mallory",
            GateStatus::Deferred,
            10 + n,
        );
    }
    let eve = fork_run(&db, "eve", 50, "s", "eve", GateStatus::Deferred, 900);
    // Plus eleven groups of different authors in repository 7, and a
    // group in repository 8.
    for n in 0..11 {
        fork_run(
            &db,
            &format!("many{n}"),
            100 + n,
            "s",
            &format!("user{n}"),
            GateStatus::Deferred,
            100 + n,
        );
    }
    let other_repo = fork_run(
        &db,
        "elsewhere",
        300,
        "s",
        "zed",
        GateStatus::Deferred,
        5000,
    );
    db.run(
        "UPDATE ci_runs SET repo_id = 8 WHERE id = $1",
        &params![other_repo.id],
    );
    // A second workflow of the first group is the same group.
    let mut again = new_run("stuck0", "lint", 11);
    again.event = "pull_request".into();
    again.git_ref = "refs/pull/1/merge".into();
    again.sha = "s".into();
    again.pull_number = Some(1);
    again.trust = Trust::Fork;
    again.author_login = "mallory".into();
    again.gate_status = GateStatus::Deferred;
    db.wait(insert_run(db.db(), &again)).unwrap().unwrap();
    let listed = db.wait(deferred_runs(db.db(), 100, 5, 2)).unwrap();
    let in_repo = |id: u64| listed.iter().filter(|run| run.repo_id == id).count();
    assert_eq!(in_repo(7), 5, "the repository window");
    let mallory = listed
        .iter()
        .filter(|run| run.author_login == "mallory")
        .count();
    assert_eq!(mallory, 2, "the author window");
    assert_eq!(
        listed
            .iter()
            .filter(|run| run.pull_number == Some(1))
            .count(),
        1,
        "a group is listed once, not once per run"
    );
    assert!(
        listed.iter().any(|run| run.id == other_repo.id),
        "another repository is not starved"
    );
    // The author window runs first, so the blocked author's other five
    // groups take none of the repository's five places: the oldest
    // groups of other authors do. Eve's newer one waits for a later
    // pass.
    assert!(!listed.iter().any(|run| run.id == eve.id));
    assert_eq!(
        listed
            .iter()
            .filter(|run| run.author_login.starts_with("user"))
            .count(),
        3
    );
}

#[test]
fn no_card_runs_are_windowed_per_repository() {
    let db = setup();
    second_repo(&db);
    for n in 0..7 {
        fork_run(
            &db,
            &format!("old{n}"),
            n + 1,
            "s",
            "mallory",
            GateStatus::Pending,
            10 + n,
        );
    }
    let other = fork_run(&db, "other", 50, "s", "eve", GateStatus::Pending, 900);
    db.run(
        "UPDATE ci_runs SET repo_id = 8 WHERE id = $1",
        &params![other.id],
    );
    let cards = db.wait(runs_needing_card(db.db(), 10_000, 6, 5)).unwrap();
    assert_eq!(cards.iter().filter(|run| run.repo_id == 7).count(), 5);
    assert!(cards.iter().any(|run| run.id == other.id));
}

#[test]
fn a_run_with_no_pull_request_is_not_counted_as_a_group() {
    let db = setup();
    // A fork run always has a pull request, but the count must not
    // depend on it: a NULL pull number is one group, not none.
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    db.run(
        "UPDATE ci_runs SET pull_number = NULL WHERE id = $1",
        &params![run.id],
    );
    let waiting = db.wait(fork_waiting(db.db(), 7, "nobody", None)).unwrap();
    assert_eq!(waiting.pending, 1);
}

use crate::policy::APPROVAL_EXPIRES_MS;

#[test]
fn an_approval_after_the_deadline_is_refused_even_before_the_reconciler_expires_the_run() {
    let db = setup();
    // Created at 1: the deadline is 1 + the approval time.
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let other = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 1);
    db.wait(set_gate_check(db.db(), other.id, 5150)).unwrap();
    let late = APPROVAL_EXPIRES_MS + 2;
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, run.id, "m", late))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 5150, "m", late))
            .unwrap()
            .len(),
        0,
        "the check run's button is held to the same deadline"
    );
    // On time, both work.
    let on_time = APPROVAL_EXPIRES_MS + 1;
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, run.id, "m", on_time))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 5150, "m", on_time))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_run_is_approved_only_while_its_pull_request_is_open_at_its_commit() {
    let db = setup();
    let closed = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let moved = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 1);
    let live = fork_run(&db, "c", 3, "s3", "mallory", GateStatus::Pending, 1);
    db.wait(set_gate_check(db.db(), closed.id, 5150)).unwrap();
    db.wait(set_gate_check(db.db(), moved.id, 5151)).unwrap();
    // The pull request closed, and another advanced to a newer commit,
    // before their webhooks cancelled the old runs.
    db.run("UPDATE ci_pulls SET state = 'closed' WHERE number = 1", &[]);
    db.run(
        "UPDATE ci_pulls SET head_sha = 'newer' WHERE number = 2",
        &[],
    );
    for run in [&closed, &moved] {
        assert!(
            db.wait(approve_run(db.db(), "t1", 7, run.id, "m", 10))
                .unwrap()
                .is_none()
        );
    }
    for check in [5150, 5151] {
        assert_eq!(
            db.wait(approve_by_check(db.db(), "t1", 7, check, "m", 10))
                .unwrap()
                .len(),
            0
        );
    }
    assert!(
        db.wait(approve_run(db.db(), "t1", 7, live.id, "m", 10))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_delivery_older_than_the_recorded_pull_request_does_not_apply() {
    let db = setup();
    let mut pull = NewPull {
        repo_id: 7,
        number: 9,
        title: "t".into(),
        head_ref: "f".into(),
        base_ref: "main".into(),
        author: "mallory".into(),
        author_association: "FIRST_TIMER".into(),
        head_repo_id: Some(99),
        head_sha: "B".into(),
        state: "open".into(),
        updated_at: 200,
    };
    assert_eq!(
        db.wait(upsert_pull(db.db(), &pull)).unwrap(),
        PullWrite::Applied
    );
    // The delivery for the older commit A arrives late.
    pull.head_sha = "A".into();
    pull.updated_at = 100;
    assert_eq!(
        db.wait(upsert_pull(db.db(), &pull)).unwrap(),
        PullWrite::Stale
    );
    let head = |db: &TestDb| -> String {
        db.column(
            "SELECT head_sha AS h FROM ci_pulls WHERE number = 9",
            &[],
            "h",
        )
        .unwrap()
    };
    assert_eq!(head(&db), "B", "the newer commit stays the head");
    // The same time (a redelivery) and a newer one apply.
    pull.head_sha = "B".into();
    pull.updated_at = 200;
    assert_eq!(
        db.wait(upsert_pull(db.db(), &pull)).unwrap(),
        PullWrite::Applied
    );
    // Two pushes in one second: the same time with another head is a
    // tie, whichever arrived first, and delivery order does not settle it.
    pull.head_sha = "B2".into();
    assert_eq!(
        db.wait(upsert_pull(db.db(), &pull)).unwrap(),
        PullWrite::Tied
    );
    assert_eq!(head(&db), "B", "a tie changes nothing by itself");
    // GitHub's answer says B2 is the head: the row is made to say so.
    db.wait(upsert_pull_forced(db.db(), &pull)).unwrap();
    assert_eq!(head(&db), "B2");
    pull.head_sha = "C".into();
    pull.updated_at = 300;
    assert_eq!(
        db.wait(upsert_pull(db.db(), &pull)).unwrap(),
        PullWrite::Applied
    );
}

#[test]
fn a_run_that_ends_while_it_waits_ends_its_gate_and_frees_its_slot() {
    let db = setup();
    let pending = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let deferred = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Deferred, 2);
    let approved = fork_run(&db, "c", 3, "s3", "mallory", GateStatus::Pending, 3);
    let bare = fork_run(&db, "d", 4, "s4", "mallory", GateStatus::Pending, 4);
    let own = db
        .wait(insert_run(db.db(), &new_run("push", "ci", 5)))
        .unwrap()
        .unwrap();
    db.wait(set_gate_check(db.db(), pending.id, 5150)).unwrap();
    db.wait(approve_run(db.db(), "t1", 7, approved.id, "m", 6))
        .unwrap()
        .unwrap();
    assert_eq!(
        db.wait(fork_waiting(db.db(), 7, "x", None))
            .unwrap()
            .pending,
        2
    );
    // The run is cancelled: its object finishes it.
    for run in [&pending, &deferred, &approved, &bare, &own] {
        db.wait(finish_run(db.db(), run.id, 1, "cancelled", 20))
            .unwrap();
    }
    let read = |id| db.wait(super::run(db.db(), id)).unwrap().unwrap();
    let gone = read(pending.id);
    assert_eq!(gone.status, "completed");
    assert_eq!(gone.gate_status, GateStatus::Cancelled);
    assert_eq!(gone.gate_at, Some(20));
    assert!(gone.gate_card_dirty, "its card is to be settled");
    assert_eq!(read(deferred.id).gate_status, GateStatus::Cancelled);
    assert!(!read(bare.id).gate_card_dirty, "no card, nothing to settle");
    assert_eq!(read(bare.id).gate_status, GateStatus::Cancelled);
    // An approved run keeps its approval, and the repository's own run
    // has none.
    assert_eq!(read(approved.id).gate_status, GateStatus::Approved);
    assert_eq!(read(own.id).gate_status, GateStatus::NotGated);
    // The slots are free: nothing waits.
    let waiting = db.wait(fork_waiting(db.db(), 7, "x", None)).unwrap();
    assert_eq!((waiting.pending, waiting.deferred), (0, 0));
    // The button of the cancelled run no longer approves it.
    assert_eq!(
        db.wait(approve_by_check(db.db(), "t1", 7, 5150, "m", 30))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn a_decision_marks_its_card_to_be_settled_and_the_reconciler_finds_it() {
    let db = setup();
    let rejected = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    let approved = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 1);
    let plain = fork_run(&db, "c", 3, "s3", "mallory", GateStatus::Pending, 1);
    let expired = fork_run(&db, "d", 4, "s4", "mallory", GateStatus::Pending, 1);
    for (run, card) in [
        (&rejected, 5150),
        (&approved, 5151),
        (&plain, 5152),
        (&expired, 5153),
    ] {
        db.wait(set_gate_check(db.db(), run.id, card)).unwrap();
    }
    db.wait(reject_run(db.db(), "t1", 7, rejected.id, "m", 5))
        .unwrap()
        .unwrap();
    db.wait(approve_run(db.db(), "t1", 7, approved.id, "m", 5))
        .unwrap()
        .unwrap();
    // The oldest waiting run expires.
    let ended = db.wait(expire_gates(db.db(), 10, 6, 1)).unwrap();
    assert_eq!(ended[0].id, plain.id);
    // Nobody settled the cards: all are found, whichever way they were
    // decided; a run that ended is cancelled again until it completes.
    let found = db.wait(unfinished_decisions(db.db(), 10, 10)).unwrap();
    let mut ids: Vec<u64> = found.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    let mut want = vec![rejected.id, approved.id, plain.id];
    want.sort_unstable();
    assert!(
        want.iter().all(|id| ids.contains(id)),
        "{ids:?} should hold {want:?}"
    );
    assert!(
        !ids.contains(&expired.id),
        "a waiting run is not a decision"
    );
    // Settled: the mark is cleared. A rejected run whose object has not
    // completed it is still found, and is not once it completed.
    for run in [&rejected, &approved, &plain] {
        db.wait(clear_decided_mark(db.db(), run.id)).unwrap();
    }
    let found = db.wait(unfinished_decisions(db.db(), 10, 10)).unwrap();
    let ids: Vec<u64> = found.iter().map(|run| run.id).collect();
    assert!(ids.contains(&rejected.id), "its run is not completed");
    assert!(!ids.contains(&approved.id));
    db.wait(finish_run(db.db(), rejected.id, 1, "cancelled", 50))
        .unwrap();
    db.wait(finish_run(db.db(), plain.id, 1, "cancelled", 50))
        .unwrap();
    // The run that still waited is cancelled too, which ends its gate
    // and marks its card: settled in turn.
    db.wait(finish_run(db.db(), expired.id, 1, "cancelled", 50))
        .unwrap();
    db.wait(clear_decided_mark(db.db(), expired.id)).unwrap();
    assert_eq!(
        db.wait(unfinished_decisions(db.db(), 10, 10))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn blocked_groups_rotate_behind_the_ones_not_looked_at_yet() {
    let db = setup();
    // Four deferred groups of one repository; a pass takes two.
    let runs: Vec<RunRow> = (0..4)
        .map(|n| {
            fork_run(
                &db,
                &format!("d{n}"),
                n + 1,
                "s",
                &format!("user{n}"),
                GateStatus::Deferred,
                10 + n,
            )
        })
        .collect();
    let first = db.wait(deferred_runs(db.db(), 10, 2, 5)).unwrap();
    let ids: Vec<u64> = first.iter().map(|run| run.id).collect();
    assert_eq!(ids, [runs[0].id, runs[1].id]);
    db.wait(mark_scanned(db.db(), &ids, 100)).unwrap();
    // The two that stayed blocked rotate behind the two never looked at.
    let second = db.wait(deferred_runs(db.db(), 10, 2, 5)).unwrap();
    let ids: Vec<u64> = second.iter().map(|run| run.id).collect();
    assert_eq!(ids, [runs[2].id, runs[3].id]);
    db.wait(mark_scanned(db.db(), &ids, 200)).unwrap();
    let third = db.wait(deferred_runs(db.db(), 10, 2, 5)).unwrap();
    let ids: Vec<u64> = third.iter().map(|run| run.id).collect();
    assert_eq!(ids, [runs[0].id, runs[1].id], "round again");
    // The same for the cards that are still to be made.
    db.run("UPDATE ci_runs SET gate_scanned_at = 0", &[]);
    let first = db.wait(runs_needing_card(db.db(), 10_000, 10, 2)).unwrap();
    let ids: Vec<u64> = first.iter().map(|run| run.id).collect();
    assert_eq!(ids.len(), 2);
    db.wait(mark_scanned(db.db(), &ids, 300)).unwrap();
    let second = db.wait(runs_needing_card(db.db(), 10_000, 10, 2)).unwrap();
    let again: Vec<u64> = second.iter().map(|run| run.id).collect();
    assert!(
        again.iter().all(|id| !ids.contains(id)),
        "{again:?} vs {ids:?}"
    );
    db.wait(mark_scanned(db.db(), &[], 1)).unwrap();
}

#[test]
fn a_refresh_that_finishes_after_a_decision_cannot_clear_the_decisions_mark() {
    let db = setup();
    let run = fork_run(&db, "a", 1, "s1", "mallory", GateStatus::Pending, 1);
    db.wait(set_gate_check(db.db(), run.id, 5150)).unwrap();
    // The refresh built the approval card, then the run was approved,
    // which marked the (decided) card to be settled.
    db.wait(approve_run(db.db(), "t1", 7, run.id, "m", 5))
        .unwrap()
        .unwrap();
    // The refresh finishes: it would write "pending" over the decided
    // card and clear the mark. The mark stays, so the reconciler settles
    // the card again.
    db.wait(set_card_dirty(db.db(), run.id, false)).unwrap();
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert!(read.gate_card_dirty);
    // Only settling the decided card clears it.
    db.wait(clear_decided_mark(db.db(), run.id)).unwrap();
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert!(!read.gate_card_dirty);
    // And clearing is for decided runs only: a waiting run keeps its mark.
    let waiting = fork_run(&db, "b", 2, "s2", "mallory", GateStatus::Pending, 2);
    db.wait(set_gate_check(db.db(), waiting.id, 5151)).unwrap();
    db.wait(set_card_dirty(db.db(), waiting.id, true)).unwrap();
    db.wait(clear_decided_mark(db.db(), waiting.id)).unwrap();
    let read = db.wait(super::run(db.db(), waiting.id)).unwrap().unwrap();
    assert!(read.gate_card_dirty);
}

/// A `pull_request_target` run of pull request `number`, on the base
/// commit, for the pull request's head `head`, in `gate`.
fn target_run(db: &TestDb, delivery: &str, number: u64, head: &str, gate: GateStatus) -> RunRow {
    let mut new = new_run(delivery, "ci", 10);
    new.event = "pull_request_target".into();
    new.git_ref = "refs/heads/main".into();
    new.sha = "basebase".into();
    new.base_ref = Some("main".into());
    new.pull_number = Some(number);
    new.head_sha = Some(head.into());
    new.lints = Some("one\ntwo".into());
    new.gate_status = gate;
    db.wait(insert_run(db.db(), &new)).unwrap().unwrap()
}

#[test]
fn a_target_run_is_approved_and_superseded_by_the_pull_requests_head() {
    let db = setup();
    // The pull request is open at head `h1`.
    let fork = fork_run(&db, "d-pr", 4, "h1", "mallory", GateStatus::Pending, 10);
    let target = target_run(&db, "d-pr:target", 4, "h1", GateStatus::Pending);
    assert_eq!(target.sha, "basebase");
    assert_eq!(target.pr_sha(), "h1");
    assert_eq!(fork.pr_sha(), "h1");
    assert_eq!(target.lints.as_deref(), Some("one\ntwo"));
    // Both are one group of one pull request at one commit.
    let waiting = db
        .wait(fork_waiting(db.db(), 7, "mallory", Some(4)))
        .unwrap();
    assert_eq!(waiting.pending, 1);
    // A click approves the run it is for, at the head, though the run's
    // own sha is the base's.
    db.wait(set_gate_check(db.db(), target.id, 555)).unwrap();
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 555, "maint", 20))
        .unwrap();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].id, target.id);
    assert_eq!(approved[0].gate_status, GateStatus::Approved);
}

#[test]
fn the_commit_of_an_approval_is_an_indexed_expression_not_a_generated_column() {
    #[derive(serde::Deserialize)]
    struct Row {
        found: String,
    }
    let db = setup();
    let generated: Row = db
        .wait(db.db().first(
            "SELECT is_generated AS found FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'ci_runs' \
             AND column_name = 'head_sha'",
            &params![],
        ))
        .unwrap()
        .unwrap();
    // A plain column: adding it did not rewrite the table.
    assert_eq!(generated.found, "NEVER");
    let index: Row = db
        .wait(db.db().first(
            "SELECT indexdef AS found FROM pg_indexes \
             WHERE schemaname = current_schema() AND indexname = 'ci_runs_gate_commit'",
            &params![],
        ))
        .unwrap()
        .unwrap();
    assert!(
        index.found.contains("COALESCE(head_sha, sha)"),
        "{}",
        index.found
    );
    let pr_sha: Option<Row> = db
        .wait(db.db().first(
            "SELECT column_name AS found FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'ci_runs' \
             AND column_name = 'pr_sha'",
            &params![],
        ))
        .unwrap();
    assert!(pr_sha.is_none());
}

#[test]
fn the_runs_of_one_delivery_do_not_count_against_each_other() {
    let db = setup();
    // The target run is asked for first; the fork run of the same
    // delivery is then counted without it.
    let target = target_run(&db, "d-pr:target", 4, "h1", GateStatus::Pending);
    let fork = fork_run(&db, "d-pr", 4, "h1", "mallory", GateStatus::Pending, 10);
    assert_eq!(target.pr_sha(), fork.pr_sha());
    let counted = db
        .wait(fork_waiting_beside(db.db(), 7, "mallory", Some(4), None))
        .unwrap();
    assert_eq!(counted.pending, 1, "one group, one card's room");
    let beside = db
        .wait(fork_waiting_beside(
            db.db(),
            7,
            "mallory",
            Some(4),
            Some("h1"),
        ))
        .unwrap();
    assert_eq!(beside.pending, 0, "its own group is not counted against it");
    // Another pull request's group, or another commit of this one, is.
    fork_run(&db, "d-other", 5, "o1", "eve", GateStatus::Pending, 11);
    let other = db
        .wait(fork_waiting_beside(
            db.db(),
            7,
            "mallory",
            Some(4),
            Some("h1"),
        ))
        .unwrap();
    assert_eq!(other.pending, 1);
}

#[test]
fn a_newer_head_clears_the_older_ones_waiting_runs_before_anything_is_counted() {
    let db = setup();
    target_run(&db, "d1:target", 4, "h1", GateStatus::Pending);
    fork_run(&db, "d1", 4, "h1", "mallory", GateStatus::Pending, 10);
    let before = db
        .wait(fork_waiting_beside(
            db.db(),
            7,
            "mallory",
            Some(4),
            Some("h2"),
        ))
        .unwrap();
    assert_eq!(before.pending, 1, "the old head still holds a slot");
    let ended = db
        .wait(end_pull_gates(db.db(), 7, 4, Some("h2"), 30))
        .unwrap();
    assert_eq!(ended.len(), 2);
    let after = db
        .wait(fork_waiting_beside(
            db.db(),
            7,
            "mallory",
            Some(4),
            Some("h2"),
        ))
        .unwrap();
    assert_eq!(after.pending, 0);
}

#[test]
fn a_target_run_is_approved_or_rejected_from_the_app_like_a_fork_run() {
    let db = setup();
    fork_run(&db, "d-pr", 4, "h1", "mallory", GateStatus::Pending, 10);
    let one = target_run(&db, "d1:target", 4, "h1", GateStatus::Pending);
    let approved = db
        .wait(approve_run(db.db(), "t1", 7, one.id, "maint", 20))
        .unwrap()
        .unwrap();
    assert_eq!(approved.gate_status, GateStatus::Approved);
    assert_eq!(approved.gate_by.as_deref(), Some("maint"));
    // A second decision changes nothing.
    assert!(
        db.wait(reject_run(db.db(), "t1", 7, one.id, "other", 21))
            .unwrap()
            .is_none()
    );
    let two = target_run(&db, "d2:target", 4, "h1", GateStatus::Pending);
    let rejected = db
        .wait(reject_run(db.db(), "t1", 7, two.id, "maint", 22))
        .unwrap()
        .unwrap();
    assert_eq!(rejected.gate_status, GateStatus::Cancelled);
}

#[test]
fn a_newer_head_ends_the_waiting_target_run_of_the_older_one() {
    let db = setup();
    let old = target_run(&db, "d1:target", 4, "h1", GateStatus::Pending);
    let new = target_run(&db, "d2:target", 4, "h2", GateStatus::Pending);
    let ended = db
        .wait(end_pull_gates(db.db(), 7, 4, Some("h2"), 30))
        .unwrap();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].id, old.id);
    let kept = db.wait(super::run(db.db(), new.id)).unwrap().unwrap();
    assert_eq!(kept.gate_status, GateStatus::Pending);
}

#[test]
fn a_target_run_is_not_approved_once_the_head_moved() {
    let db = setup();
    // The pull request is at `h2` now; the run is for `h1`.
    let old = target_run(&db, "d1:target", 4, "h1", GateStatus::Pending);
    db.wait(set_gate_check(db.db(), old.id, 556)).unwrap();
    fork_run(&db, "d-pr", 4, "h2", "mallory", GateStatus::Pending, 12);
    let approved = db
        .wait(approve_by_check(db.db(), "t1", 7, 556, "maint", 20))
        .unwrap();
    assert_eq!(approved.len(), 0);
}

#[test]
fn a_pull_request_and_a_pull_request_target_run_of_one_workflow_both_exist() {
    let db = setup();
    // One delivery starts both for a workflow that listens to both events.
    let delivery = "guid-1";
    let mut pull = new_run(delivery, "ci", 10);
    pull.event = "pull_request".into();
    pull.git_ref = "refs/pull/4/merge".into();
    pull.pull_number = Some(4);
    let mut target = new_run(&crate::webhook::target_delivery(delivery), "ci", 10);
    target.event = "pull_request_target".into();
    target.git_ref = "refs/heads/main".into();
    target.pull_number = Some(4);
    let a = db.wait(insert_run(db.db(), &pull)).unwrap();
    let b = db.wait(insert_run(db.db(), &target)).unwrap();
    assert!(a.is_some() && b.is_some(), "neither run may be dropped");
    assert_ne!(a.unwrap().id, b.unwrap().id);
    // The same key again finds the run it made instead of a second one.
    assert!(db.wait(insert_run(db.db(), &target)).unwrap().is_none());
}

#[test]
fn a_comment_approves_every_waiting_run_at_the_head_and_no_other() {
    let db = setup();
    let ci = fork_run(&db, "d1", 4, "h1", "mallory", GateStatus::Pending, 10);
    let target = target_run(&db, "d1:target", 4, "h1", GateStatus::Pending);
    let mut lint = new_run("d1", "lint", 10);
    lint.event = "pull_request".into();
    lint.git_ref = "refs/pull/4/merge".into();
    lint.sha = "h1".into();
    lint.pull_number = Some(4);
    lint.trust = Trust::Fork;
    lint.gate_status = GateStatus::Deferred;
    let lint = db.wait(insert_run(db.db(), &lint)).unwrap().unwrap();
    // A run of an older commit of the same pull request, and a command
    // run of the comment event on the same head, are not touched.
    let old = fork_run(&db, "d0", 4, "h0", "mallory", GateStatus::Pending, 9);
    db.wait(upsert_pull_forced(
        db.db(),
        &NewPull {
            repo_id: 7,
            number: 4,
            title: "change".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            author: "mallory".into(),
            author_association: "NONE".into(),
            head_repo_id: Some(99),
            head_sha: "h1".into(),
            state: "open".into(),
            updated_at: 6_000_000,
        },
    ))
    .unwrap();
    let seen = db.wait(pull_runs_at(db.db(), 7, 4, "h1")).unwrap();
    let ids: Vec<u64> = seen.iter().map(|run| run.id).collect();
    assert_eq!(ids, [ci.id, target.id, lint.id]);
    // Another repository, tenant or pull request finds nothing.
    for (tenant, repo, number) in [("t1", 8, 4), ("t2", 7, 4), ("t1", 7, 5)] {
        assert_eq!(
            db.wait(approve_at_head(
                db.db(),
                tenant,
                repo,
                number,
                "h1",
                "m",
                20
            ))
            .unwrap()
            .len(),
            0
        );
    }
    let approved = db
        .wait(approve_at_head(db.db(), "t1", 7, 4, "h1", "maintainer", 20))
        .unwrap();
    let mut ids: Vec<u64> = approved.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [ci.id, target.id, lint.id]);
    assert!(approved.iter().all(|run| {
        run.gate_status == GateStatus::Approved
            && run.gate_by.as_deref() == Some("maintainer")
            && run.gate_at == Some(20)
    }));
    let read = db.wait(super::run(db.db(), old.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
    // Whoever decides first wins; the second comment finds nothing.
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 4, "h1", "again", 21))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn a_comment_never_revives_an_ended_run_or_approves_after_a_push_or_close() {
    let db = setup();
    let cancelled = fork_run(&db, "d1", 4, "h1", "mallory", GateStatus::Cancelled, 10);
    let expired = fork_run(&db, "d2", 4, "h1", "mallory", GateStatus::Expired, 10);
    let done = fork_run(&db, "d3", 4, "h1", "mallory", GateStatus::Pending, 10);
    db.run(
        "UPDATE ci_runs SET status = 'completed' WHERE id = $1",
        &params![done.id],
    );
    let none = fork_run(&db, "d4", 4, "h1", "mallory", GateStatus::NotGated, 10);
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 4, "h1", "m", 20))
            .unwrap()
            .len(),
        0
    );
    for id in [cancelled.id, expired.id, done.id, none.id] {
        let read = db.wait(super::run(db.db(), id)).unwrap().unwrap();
        assert_ne!(read.gate_by.as_deref(), Some("m"), "run {id}");
    }
    // A waiting run past its deadline, though not yet expired by the
    // reconciler, stays waiting: the comment does not revive it.
    let late = fork_run(&db, "d5", 5, "h5", "mallory", GateStatus::Pending, 10);
    let after = 10 + APPROVAL_EXPIRES_MS + 1;
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 5, "h5", "m", after))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 5, "h5", "m", after - 1))
            .unwrap()
            .len(),
        1,
        "run {} on time",
        late.id
    );
    // A push moved the pull request to a new head: the old one is not
    // approved. A closed pull request approves nothing either.
    let old = fork_run(&db, "d6", 6, "h6", "mallory", GateStatus::Pending, 30);
    db.run(
        "UPDATE ci_pulls SET head_sha = 'h7' WHERE repo_id = 7 AND number = 6",
        &[],
    );
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 6, "h6", "m", 31))
            .unwrap()
            .len(),
        0
    );
    db.run(
        "UPDATE ci_pulls SET head_sha = 'h6', state = 'closed' WHERE repo_id = 7 AND number = 6",
        &[],
    );
    assert_eq!(
        db.wait(approve_at_head(db.db(), "t1", 7, 6, "h6", "m", 31))
            .unwrap()
            .len(),
        0
    );
    let read = db.wait(super::run(db.db(), old.id)).unwrap().unwrap();
    assert_eq!(read.gate_status, GateStatus::Pending);
}

#[test]
fn the_runs_of_a_head_leave_out_what_other_events_made() {
    let db = setup();
    let fork = fork_run(&db, "d1", 4, "h1", "mallory", GateStatus::Pending, 10);
    let mut comment = new_run("d2", "retest", 10);
    comment.event = "issue_comment".into();
    comment.pull_number = Some(4);
    comment.head_sha = Some("h1".into());
    db.wait(insert_run(db.db(), &comment)).unwrap().unwrap();
    let seen = db.wait(pull_runs_at(db.db(), 7, 4, "h1")).unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, fork.id);
    assert_eq!(db.wait(pull_runs_at(db.db(), 7, 4, "h2")).unwrap().len(), 0);
}
