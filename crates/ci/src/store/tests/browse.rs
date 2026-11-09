// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;

fn run_on(db: &TestDb, delivery: &str, workflow: &str, git_ref: &str, now: u64) -> RunRow {
    let mut new = new_run(delivery, workflow, now);
    new.git_ref = git_ref.into();
    if let Some(number) = git_ref
        .strip_prefix("refs/pull/")
        .and_then(|rest| rest.strip_suffix("/merge"))
    {
        new.event = "pull_request".into();
        new.base_ref = Some("main".into());
        new.pull_number = number.parse().ok();
        new.head_ref = Some("feature".into());
    }
    db.wait(insert_run(db.db(), &new)).unwrap().unwrap()
}

fn pull(number: u64, state: &str, updated_at: u64) -> NewPull {
    NewPull {
        repo_id: 7,
        number,
        title: format!("change {number}"),
        head_ref: "feature".into(),
        base_ref: "main".into(),
        author: "octocat".into(),
        author_association: "MEMBER".into(),
        head_repo_id: Some(7),
        head_sha: "abc".into(),
        state: state.into(),
        updated_at,
    }
}

#[test]
fn a_pull_request_run_keeps_its_pull_request() {
    let db = setup();
    db.wait(upsert_pull(db.db(), &pull(3, "open", 100)))
        .unwrap();
    let run = run_on(&db, "a", "ci", "refs/pull/3/merge", DAY);
    assert_eq!(run.pull_number, Some(3));
    assert_eq!(run.pull_title.as_deref(), Some("change 3"));
    assert_eq!(run.branch(), Some("feature"));
    // A rename shows on the runs already recorded.
    let mut renamed = pull(3, "open", 200);
    renamed.title = "renamed".into();
    db.wait(upsert_pull(db.db(), &renamed)).unwrap();
    let read = db
        .wait(crate::store::run(db.db(), run.id))
        .unwrap()
        .unwrap();
    assert_eq!(read.pull_title.as_deref(), Some("renamed"));
    assert_eq!(run.head_ref.as_deref(), Some("feature"));
    let push = run_on(&db, "b", "ci", "refs/heads/main", 2 * DAY);
    assert_eq!(push.pull_number, None);
    assert_eq!(push.head_ref, None);
    let again = db
        .wait(run_for_delivery(db.db(), "a", "ci"))
        .unwrap()
        .unwrap();
    assert_eq!(again.pull_number, Some(3));
}

#[test]
fn runs_filter_by_workflow_and_pull_request() {
    let db = setup();
    let ci_old = run_on(&db, "a", "ci", "refs/pull/3/merge", DAY).id;
    let lint = run_on(&db, "b", "lint", "refs/pull/3/merge", 2 * DAY).id;
    let other = run_on(&db, "c", "ci", "refs/pull/4/merge", 3 * DAY).id;
    let main = run_on(&db, "d", "ci", "refs/heads/main", 4 * DAY).id;
    // The run ids of a page, newest first, and the total.
    let found = |f: RunFilter| {
        let (rows, total) = db.wait(runs_page(db.db(), &f, None, 10)).unwrap();
        (rows.iter().map(|r| r.run.id).collect::<Vec<_>>(), total)
    };
    let base = RunFilter {
        repo_ids: vec![7],
        ..RunFilter::default()
    };
    assert_eq!(found(base.clone()), (vec![main, other, lint, ci_old], 4));
    let mut f = base.clone();
    f.workflows = vec!["ci".into()];
    assert_eq!(found(f), (vec![main, other, ci_old], 3));
    let mut f = base.clone();
    f.pull = Some(3);
    assert_eq!(found(f), (vec![lint, ci_old], 2));
    let mut f = base;
    f.pull = Some(3);
    f.workflows = vec!["lint".into()];
    assert_eq!(found(f), (vec![lint], 1));
}

#[test]
fn pull_requests_follow_the_webhook_and_ignore_stale_deliveries() {
    let db = setup();
    db.wait(upsert_pull(db.db(), &pull(1, "open", 100)))
        .unwrap();
    db.wait(upsert_pull(db.db(), &pull(2, "open", 200)))
        .unwrap();
    let mut merged = pull(1, "merged", 300);
    merged.title = "renamed".into();
    db.wait(upsert_pull(db.db(), &merged)).unwrap();
    // A delivery that arrives late changes nothing.
    db.wait(upsert_pull(db.db(), &pull(1, "open", 150)))
        .unwrap();
    let page = |state: Option<&str>, q: Option<&str>, after| {
        db.wait(pulls_page(db.db(), 7, "t1", state, q, after, 10))
            .unwrap()
    };
    let (all, total) = page(None, None, None);
    assert_eq!(total, 2);
    assert_eq!(all.iter().map(|p| p.number).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(all[0].state, "merged");
    assert_eq!(all[0].title, "renamed");
    let (open, total) = page(Some("open"), None, None);
    assert_eq!((open.len(), total, open[0].number), (1, 1, 2));
    assert_eq!(page(None, Some("renam"), None).1, 1);
    assert_eq!(page(None, Some("2"), None).1, 1);
    let first = db
        .wait(pulls_page(db.db(), 7, "t1", None, None, None, 1))
        .unwrap();
    let cursor = PullAfter {
        updated_at: first.0[0].updated_at,
        number: first.0[0].number,
    };
    let (rest, _) = page(None, None, Some(cursor));
    assert_eq!(rest.iter().map(|p| p.number).collect::<Vec<_>>(), [2]);
}

#[test]
fn a_pull_request_shows_its_newest_run() {
    let db = setup();
    db.wait(upsert_pull(db.db(), &pull(3, "open", 100)))
        .unwrap();
    db.wait(upsert_pull(db.db(), &pull(4, "open", 90))).unwrap();
    run_on(&db, "a", "ci", "refs/pull/3/merge", DAY);
    let newest = run_on(&db, "b", "ci", "refs/pull/3/merge", 2 * DAY);
    let (rows, _) = db
        .wait(pulls_page(db.db(), 7, "t1", None, None, None, 10))
        .unwrap();
    assert_eq!(rows[0].last_run_id, Some(newest.id));
    assert_eq!(rows[1].last_run_id, None);
    let listed = db.wait(listed_runs(db.db(), &[newest.id])).unwrap();
    assert_eq!(listed[0].repository, "acme/app");
}

#[test]
fn workflows_and_branches_are_grouped_from_runs() {
    let db = setup();
    let now = 20 * DAY;
    run_on(&db, "a", "ci", "refs/heads/main", now - 10 * DAY);
    let ci = run_on(&db, "b", "ci", "refs/heads/dev", now - DAY);
    let lint = run_on(&db, "c", "lint", "refs/heads/dev", now - 2 * DAY);
    let pr = run_on(&db, "d", "ci", "refs/pull/9/merge", now - 3 * DAY);
    run_on(&db, "e", "ci", "refs/tags/v1", now - 4 * DAY);
    db.wait(finish_run(db.db(), ci.id, 1, "failure", now))
        .unwrap();
    db.wait(finish_run(db.db(), pr.id, 1, "success", now))
        .unwrap();

    let counts = db.wait(repo_counts(db.db(), 7, "t1")).unwrap();
    assert_eq!(
        counts,
        RepoCounts {
            workflows: 2,
            branches: 3,
            open_pulls: 0
        }
    );

    let since = now - 7 * DAY;
    let (rows, total) = db
        .wait(workflows_page(db.db(), 7, "t1", since, None, 10))
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(rows[0].name, "ci");
    assert_eq!(rows[0].last_run_id, ci.id);
    // Three of the four ci runs in the window: a is older, the tag is in.
    assert_eq!(
        (rows[0].runs_7d, rows[0].passed_7d, rows[0].failed_7d),
        (3, 1, 1)
    );
    assert_eq!(
        (rows[1].name.as_str(), rows[1].last_run_id),
        ("lint", lint.id)
    );
    let after = After {
        created_at: rows[0].last_at,
        id: rows[0].last_run_id,
    };
    let (rest, _) = db
        .wait(workflows_page(db.db(), 7, "t1", since, Some(after), 10))
        .unwrap();
    assert_eq!(rest.len(), 1);

    // The default branch comes first though dev ran later; the pull
    // request counts for its head branch, and the tag for none.
    let branches = |q: Option<&str>, after| {
        db.wait(branches_page(db.db(), 7, "t1", "main", q, after, 10))
            .unwrap()
    };
    let (rows, total) = branches(None, None);
    assert_eq!(total, 3);
    assert_eq!(
        rows.iter()
            .map(|b| (b.name.as_str(), b.default, b.runs))
            .collect::<Vec<_>>(),
        [("main", true, 1), ("dev", false, 2), ("feature", false, 1)]
    );
    assert_eq!(branches(Some("de"), None).1, 1);
    let after = BranchAfter {
        rank: 0,
        run: After {
            created_at: rows[0].last_at,
            id: rows[0].last_run_id,
        },
    };
    let (rest, _) = branches(None, Some(after));
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].name, "dev");
}
