// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;

/// A run of the `issue_comment` event on issue `number`, started by the
/// comment `comment` when it is set.
fn comment_run(
    db: &TestDb,
    delivery: &str,
    number: u64,
    comment: Option<(u64, &str, bool)>,
) -> RunRow {
    let mut new = new_run(delivery, "ci", 10);
    new.event = "issue_comment".into();
    new.chat = Some(Chat {
        issue_number: number,
        command: comment.map(|(comment_id, name, rocket)| CommentCommand {
            comment_id,
            name: name.into(),
            rocket,
        }),
    });
    db.wait(insert_run(db.db(), &new)).unwrap().unwrap()
}

fn active(db: &TestDb, number: u64) -> u64 {
    db.wait(active_comment_runs(db.db(), 7, number, "none"))
        .unwrap()
}

#[test]
fn a_command_run_keeps_its_comment_and_reaction_set() {
    let db = setup();
    let plain = comment_run(&db, "d1", 12, Some((9001, "retest", false)));
    let rocket = comment_run(&db, "d2", 12, Some((9002, "deploy", true)));
    let comment = |id: u64| -> Option<i64> {
        db.column(
            "SELECT comment_id AS n FROM ci_runs WHERE id = $1",
            &params![id],
            "n",
        )
    };
    assert_eq!(comment(plain.id), Some(9001));
    assert_eq!(comment(rocket.id), Some(9002));
    let reaction = |id: u64| -> Option<String> {
        db.column(
            "SELECT command_reaction AS n FROM ci_runs WHERE id = $1",
            &params![id],
            "n",
        )
    };
    assert_eq!(reaction(rocket.id).as_deref(), Some("rocket"));
    assert_eq!(reaction(plain.id).as_deref(), Some("default"));
    let name: String = db
        .column(
            "SELECT command_name AS n FROM ci_runs WHERE id = $1",
            &params![rocket.id],
            "n",
        )
        .unwrap();
    assert_eq!(name, "deploy");
}

#[test]
fn a_run_no_command_started_has_no_comment() {
    let db = setup();
    let comment = comment_run(&db, "d1", 12, None);
    let id: Option<Option<i64>> = db.column(
        "SELECT comment_id AS n FROM ci_runs WHERE id = $1",
        &params![comment.id],
        "n",
    );
    assert_eq!(id, Some(None));
}

#[test]
fn the_cap_counts_the_active_comment_runs_of_one_issue() {
    let db = setup();
    assert_eq!(active(&db, 12), 0);
    let first = comment_run(&db, "d1", 12, None);
    comment_run(&db, "d2", 12, Some((1, "retest", false)));
    comment_run(&db, "d3", 13, None);
    assert_eq!(active(&db, 12), 2);
    assert_eq!(active(&db, 13), 1);
    // A run that is over frees its place.
    db.wait(finish_run(db.db(), first.id, 1, "success", 20))
        .unwrap();
    assert_eq!(active(&db, 12), 1);
}

#[test]
fn the_cap_leaves_out_the_runs_of_the_delivery_being_counted() {
    let db = setup();
    comment_run(&db, "d1", 12, None);
    comment_run(&db, "d1:command:retest", 12, Some((1, "retest", false)));
    comment_run(&db, "d10", 12, None);
    let count = |delivery: &str| {
        db.wait(active_comment_runs(db.db(), 7, 12, delivery))
            .unwrap()
    };
    assert_eq!(count("other"), 3);
    // A retry of d1 does not count its own two runs, and the run of
    // d10 is another delivery's.
    assert_eq!(count("d1"), 1);
    assert_eq!(count("d10"), 2);
}

#[test]
fn the_cap_leaves_out_other_events_and_other_repositories() {
    let db = setup();
    // An `issues` run on the same number does not count: the cap is for
    // comment events.
    let mut issues = new_run("i", "ci", 10);
    issues.event = "issues".into();
    issues.chat = Some(Chat {
        issue_number: 12,
        command: None,
    });
    db.wait(insert_run(db.db(), &issues)).unwrap().unwrap();
    // A run with no issue does not either.
    db.wait(insert_run(db.db(), &new_run("p", "ci", 10)))
        .unwrap()
        .unwrap();
    assert_eq!(active(&db, 12), 0);
    // Another repository's issue 12 is another issue.
    let repo = RepoRow {
        id: 8,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/other".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();
    let mut other = new_run("o", "ci", 10);
    other.repo_id = 8;
    other.event = "issue_comment".into();
    other.chat = Some(Chat {
        issue_number: 12,
        command: None,
    });
    db.wait(insert_run(db.db(), &other)).unwrap().unwrap();
    assert_eq!(active(&db, 12), 0);
    assert_eq!(
        db.wait(active_comment_runs(db.db(), 8, 12, "none"))
            .unwrap(),
        1
    );
}

#[test]
fn a_command_comment_needs_all_three_columns() {
    let db = setup();
    let run = comment_run(&db, "d1", 12, None);
    // A comment id with no name is refused by the table.
    let refused = db.try_run(
        "UPDATE ci_runs SET comment_id = 5 WHERE id = $1",
        &params![run.id],
    );
    assert!(refused.is_err());
}
