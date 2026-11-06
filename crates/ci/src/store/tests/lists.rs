// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::github::InstalledRepo;

fn run_with(db: &TestDb, delivery: &str, actor: &str, git_ref: &str, now: u64) -> RunRow {
    let mut new = new_run(delivery, "ci", now);
    new.actor = actor.into();
    new.git_ref = git_ref.into();
    if git_ref.starts_with("refs/pull/") {
        new.base_ref = Some("main".into());
    }
    db.wait(insert_run(db.db(), &new)).unwrap().unwrap()
}

fn filter() -> RunFilter {
    RunFilter {
        repo_ids: vec![7],
        ..RunFilter::default()
    }
}

fn ids(page: &(Vec<ListedRun>, u64)) -> Vec<u64> {
    page.0.iter().map(|row| row.run.id).collect()
}

#[test]
fn runs_are_filtered_and_counted() {
    let db = setup();
    let ok = run_with(&db, "a", "octocat", "refs/heads/main", 10 * DAY);
    let bad = run_with(&db, "b", "hubot", "refs/heads/dev", 11 * DAY);
    let pr = run_with(&db, "c", "octocat", "refs/pull/3/merge", 12 * DAY);
    let tag = run_with(&db, "d", "octocat", "refs/tags/v1", 13 * DAY);
    db.wait(finish_run(db.db(), ok.id, 1, "success", 10 * DAY + 5))
        .unwrap();
    db.wait(finish_run(db.db(), bad.id, 1, "failure", 11 * DAY + 7))
        .unwrap();
    db.wait(run_started(db.db(), pr.id, 12 * DAY + 1)).unwrap();
    let page = |f: &RunFilter| db.wait(runs_page(db.db(), f, None, 10)).unwrap();

    let all = page(&filter());
    assert_eq!(all.1, 4);
    // The counts ignore the states of the filter.
    let mut counted = filter();
    counted.states = vec![RunStatus::Failure];
    let counts = db.wait(run_counts(db.db(), &counted)).unwrap();
    assert_eq!(
        counts,
        RunCounts {
            all: 4,
            running: 1,
            waiting: 0,
            failed: 1,
            passed: 1
        }
    );
    counted.actors = vec!["hubot".into()];
    assert_eq!(db.wait(run_counts(db.db(), &counted)).unwrap().all, 1);
    assert_eq!(ids(&all), [tag.id, pr.id, bad.id, ok.id]);

    let mut f = filter();
    f.states = vec![RunStatus::Success, RunStatus::Failure];
    assert_eq!(ids(&page(&f)), [bad.id, ok.id]);
    f.states = vec![RunStatus::Running];
    assert_eq!(ids(&page(&f)), [pr.id]);
    f.states = vec![RunStatus::Queued];
    assert_eq!(ids(&page(&f)), [tag.id]);

    // Branch: a push's branch, a pull request's base, never a tag.
    let mut f = filter();
    f.branches = vec!["main".into()];
    assert_eq!(ids(&page(&f)), [pr.id, ok.id]);
    assert_eq!(all.0[1].run.branch(), Some("main"));
    assert_eq!(all.0[0].run.branch(), None);

    let mut f = filter();
    f.actors = vec!["HUBOT".into()];
    assert_eq!(ids(&page(&f)), [bad.id]);

    let mut f = filter();
    f.text = Some("DEV".into());
    assert_eq!(ids(&page(&f)), [bad.id]);
    f.text = Some("ab".into());
    assert_eq!(page(&f).1, 4, "the start of the commit sha");
    f.text = Some("bc".into());
    assert_eq!(page(&f).1, 0, "not the middle of it");
    f.text = Some("100%".into());
    assert_eq!(page(&f).1, 0, "wildcards are text");

    let mut f = filter();
    f.since = 11 * DAY;
    f.until = Some(13 * DAY);
    assert_eq!(ids(&page(&f)), [pr.id, bad.id]);

    let mut f = filter();
    f.repos = vec!["ACME/app".into()];
    assert_eq!(page(&f).1, 4);
    f.repos = vec!["acme/other".into()];
    assert_eq!(page(&f).1, 0);

    // A repository outside the list is not shown.
    let mut f = filter();
    f.repo_ids = vec![8];
    assert_eq!(page(&f).1, 0);
    f.repo_ids = Vec::new();
    assert_eq!(page(&f).1, 0);
}

#[test]
fn run_pages_follow_the_sort_key() {
    let db = setup();
    // Two runs share a creation time: the id breaks the tie.
    let a = run_with(&db, "a", "x", "refs/heads/main", 5);
    let b = run_with(&db, "b", "x", "refs/heads/main", 5);
    let c = run_with(&db, "c", "x", "refs/heads/main", 9);
    let first = db.wait(runs_page(db.db(), &filter(), None, 2)).unwrap();
    assert_eq!((ids(&first), first.1), (vec![c.id, b.id], 3));
    let after = After {
        created_at: first.0[1].run.created_at,
        id: first.0[1].run.id,
    };
    let second = db
        .wait(runs_page(db.db(), &filter(), Some(after), 2))
        .unwrap();
    assert_eq!((ids(&second), second.1), (vec![a.id], 3));
}

#[test]
fn job_counts_and_approvals() {
    let db = setup();
    let run = run_with(&db, "a", "x", "refs/heads/main", 5);
    let mut gated = new_job("deploy");
    gated.environment = Some("prod".into());
    let jobs = [new_job("build"), new_job("test"), new_job("lint"), gated];
    let inserted = db.wait(insert_jobs(db.db(), run.id, &jobs, 5)).unwrap();
    let id = |plan: &str| inserted.iter().find(|(p, _)| p == plan).unwrap().1;
    db.run(
        &format!(
            "UPDATE ci_jobs SET status = 'completed', conclusion = 'success' WHERE id = {}",
            id("build")
        ),
        &[],
    );
    db.run(
        &format!(
            "UPDATE ci_jobs SET status = 'completed', conclusion = 'failure' WHERE id = {}",
            id("test")
        ),
        &[],
    );
    db.run(
        &format!(
            "UPDATE ci_jobs SET status = 'running' WHERE id = {}",
            id("lint")
        ),
        &[],
    );
    db.run(
        &format!(
            "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 99 WHERE id = {}",
            id("deploy")
        ),
        &[],
    );
    let counts = db.wait(job_counts(db.db(), &[run.id, 999])).unwrap();
    assert_eq!(
        counts[&run.id],
        JobCounts {
            total: 4,
            passed: 1,
            failed: 1,
            running: 1,
            waiting: 1,
            cancelled: 0,
            skipped: 0
        }
    );
    assert!(!counts.contains_key(&999));
    let approvals = db.wait(approval_jobs(db.db(), "t1", 10)).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].job_name, "deploy");
    assert_eq!(approvals[0].environment.as_deref(), Some("prod"));
    assert_eq!(approvals[0].since, 99);
    assert_eq!(approvals[0].run.repository, "acme/app");
    assert_eq!(db.wait(approval_jobs(db.db(), "t2", 10)).unwrap().len(), 0);

    // A run with a job waiting for approval is waiting, not running.
    db.wait(run_started(db.db(), run.id, 6)).unwrap();
    let mut f = filter();
    f.states = vec![RunStatus::Waiting];
    assert_eq!(db.wait(runs_page(db.db(), &f, None, 10)).unwrap().1, 1);
    f.states = vec![RunStatus::Running];
    assert_eq!(db.wait(runs_page(db.db(), &f, None, 10)).unwrap().1, 0);
}

#[test]
fn the_latest_default_branch_run_is_the_one_that_counts() {
    let db = setup();
    let old = run_with(&db, "a", "x", "refs/heads/main", 5);
    let new = run_with(&db, "b", "x", "refs/heads/main", 6);
    let other = run_with(&db, "c", "x", "refs/heads/dev", 7);
    let open = run_with(&db, "d", "x", "refs/heads/main", 8);
    db.wait(finish_run(db.db(), old.id, 1, "success", 9))
        .unwrap();
    db.wait(finish_run(db.db(), new.id, 1, "failure", 9))
        .unwrap();
    db.wait(finish_run(db.db(), other.id, 1, "failure", 9))
        .unwrap();
    let _ = open;
    let latest = db.wait(latest_on_default(db.db(), "t1", &[7])).unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].run.id, new.id);
    assert_eq!(latest[0].repository, "acme/app");
    assert_eq!(
        db.wait(latest_on_default(db.db(), "t1", &[]))
            .unwrap()
            .len(),
        0
    );
}

fn installed(id: u64, name: &str, private: bool) -> InstalledRepo {
    InstalledRepo {
        id,
        full_name: name.into(),
        default_branch: "main".into(),
        private,
    }
}

fn names(page: &(Vec<OrgRepo>, u64)) -> Vec<&str> {
    page.0.iter().map(|repo| repo.name.as_str()).collect()
}

fn readers_see_public_and_reachable_repos(db: &TestDb) {
    // A reader who is not an admin sees public repositories and the
    // private ones they reach.
    let reach_none = RepoFilter {
        reach: Some(&[]),
        ..RepoFilter::default()
    };
    let seen = db
        .wait(repos_page(db.db(), "t1", reach_none, 0, 10))
        .unwrap();
    assert_eq!(names(&seen), ["acme/app-renamed", "acme/new"]);
    let reach_api = RepoFilter {
        reach: Some(&[8]),
        ..RepoFilter::default()
    };
    let seen = db
        .wait(repos_page(db.db(), "t1", reach_api, 0, 10))
        .unwrap();
    assert_eq!(
        (names(&seen), seen.1),
        (vec!["acme/api", "acme/app-renamed", "acme/new"], 3)
    );
}

#[test]
fn installation_repositories_sync_and_keep_their_choice() {
    let db = setup();
    let patch = |all: Option<bool>, add: &[String], remove: &[String]| {
        let audit = super::secrets::audit(Action::ReposEnabled, "x");
        db.wait(patch_enabled(db.db(), "t1", all, add, remove, &audit))
            .unwrap();
    };
    let repos = [
        installed(7, "acme/app", false),
        installed(8, "acme/api", true),
        installed(9, "acme/web", false),
    ];
    assert_eq!(
        db.wait(installation_synced(db.db(), "t1")).unwrap(),
        (None, 0)
    );
    db.wait(sync_installation(db.db(), "t1", &repos, 50))
        .unwrap();
    assert_eq!(
        db.wait(installation_synced(db.db(), "t1")).unwrap(),
        (Some(50), 3)
    );
    // The tenant runs everything: all are enabled.
    assert!(db.wait(repo_enabled(db.db(), "t1", 8)).unwrap());

    // Leave all mode with everything off, then choose api again.
    patch(
        Some(false),
        &[],
        &[
            "acme/api".to_owned(),
            "acme/app".to_owned(),
            "acme/web".to_owned(),
        ],
    );
    assert!(!db.wait(repo_enabled(db.db(), "t1", 8)).unwrap());
    patch(None, &["ACME/api".to_owned()], &[]);
    assert!(db.wait(repo_enabled(db.db(), "t1", 8)).unwrap());
    assert!(!db.wait(repo_enabled(db.db(), "t1", 7)).unwrap());
    let next = [
        installed(7, "acme/app-renamed", false),
        installed(8, "acme/api", true),
        installed(10, "acme/new", false),
    ];
    db.wait(sync_installation(db.db(), "t1", &next, 60))
        .unwrap();
    let all = db
        .wait(repos_page(db.db(), "t1", RepoFilter::default(), 0, 10))
        .unwrap();
    assert_eq!(all.1, 3);
    assert_eq!(names(&all), ["acme/api", "acme/app-renamed", "acme/new"]);
    let enabled: Vec<bool> = all.0.iter().map(|r| r.enabled).collect();
    assert_eq!(
        enabled,
        [true, false, false],
        "new repos follow the tenant's flag"
    );
    assert!(
        !db.wait(repo_enabled(db.db(), "t1", 9)).unwrap(),
        "9 is gone"
    );

    let only = |enabled| RepoFilter {
        text: None,
        enabled: Some(enabled),
        reach: None,
    };
    let on = db
        .wait(repos_page(db.db(), "t1", only(true), 0, 10))
        .unwrap();
    assert_eq!(names(&on), ["acme/api"]);
    let off = db
        .wait(repos_page(db.db(), "t1", only(false), 1, 1))
        .unwrap();
    assert_eq!((names(&off), off.1), (vec!["acme/new"], 2));
    let text = RepoFilter {
        text: Some("APP"),
        enabled: None,
        reach: None,
    };
    let found = db.wait(repos_page(db.db(), "t1", text, 0, 10)).unwrap();
    assert_eq!(names(&found), ["acme/app-renamed"]);

    readers_see_public_and_reachable_repos(&db);

    // All again.
    patch(Some(true), &[], &[]);
    assert!(db.wait(repo_enabled(db.db(), "t1", 10)).unwrap());
    // A repository without a row follows the flag.
    assert!(db.wait(repo_enabled(db.db(), "t1", 4242)).unwrap());
    patch(Some(false), &[], &[]);
    assert!(!db.wait(repo_enabled(db.db(), "t1", 4242)).unwrap());
    // Leaving the mode keeps each repository's choice.
    assert!(db.wait(repo_enabled(db.db(), "t1", 10)).unwrap());
    assert!(!db.wait(repo_enabled(db.db(), "nobody", 7)).unwrap());
}

#[test]
fn repositories_list_newest_run_first_with_the_last_run() {
    let db = setup();
    let repos = [
        installed(7, "acme/app", false),
        installed(8, "acme/zeta", false),
        installed(9, "acme/alpha", false),
    ];
    db.wait(sync_installation(db.db(), "t1", &repos, 1))
        .unwrap();
    let older = run_with(&db, "a", "x", "refs/heads/main", 5);
    let newer = run_with(&db, "b", "x", "refs/heads/main", 9);
    db.wait(finish_run(db.db(), newer.id, 1, "success", 10))
        .unwrap();
    let _ = older;
    let page = db
        .wait(repos_page(db.db(), "t1", RepoFilter::default(), 0, 10))
        .unwrap();
    // acme/app has runs; the others follow by name.
    assert_eq!(names(&page), ["acme/app", "acme/alpha", "acme/zeta"]);
    let last = page.0[0].last_run.as_ref().unwrap();
    assert_eq!(last.id, newer.id);
    assert_eq!(last.status, "completed");
    assert_eq!(last.conclusion.as_deref(), Some("success"));
    assert_eq!(last.created_at, 9);
    assert!(page.0[1].last_run.is_none());
}

#[test]
fn unknown_names_and_the_setup_counts() {
    let db = setup();
    let first = run_with(&db, "a", "x", "refs/heads/main", 5);
    let repos = [
        installed(7, "acme/app", false),
        installed(8, "acme/api", true),
    ];
    db.wait(sync_installation(db.db(), "t1", &repos, 1))
        .unwrap();
    let asked = ["acme/APP".to_owned(), "acme/gone".to_owned()];
    assert_eq!(
        db.wait(unknown_repos(db.db(), "t1", &asked)).unwrap(),
        ["acme/gone"]
    );
    let row = db.wait(tenant_setup(db.db(), "t1")).unwrap().unwrap();
    assert_eq!(row.installation_id, Some(99));
    assert!(row.all_repos);
    assert_eq!((row.repos, row.enabled), (2, 2));
    assert_eq!(row.first_run_id, Some(first.id));
    assert_eq!(row.first_run_repository.as_deref(), Some("acme/app"));
    db.wait(patch_enabled(
        db.db(),
        "t1",
        Some(false),
        &[],
        &["acme/api".to_owned()],
        &super::secrets::audit(Action::ReposEnabled, "x"),
    ))
    .unwrap();
    let row = db.wait(tenant_setup(db.db(), "t1")).unwrap().unwrap();
    assert!(!row.all_repos);
    assert_eq!((row.repos, row.enabled), (2, 1));
    assert_eq!(
        db.wait(tenant_installation(db.db(), "t1")).unwrap(),
        Some(99)
    );
    let seen = db.wait(tenant_repo_visibility(db.db(), "t1")).unwrap();
    assert_eq!(
        seen,
        [RepoVisibility {
            id: 7,
            private: false,
            installation_id: 99
        }]
    );
}

#[test]
fn a_renamed_repository_updates_its_listing() {
    let db = setup();
    db.wait(sync_installation(
        db.db(),
        "t1",
        &[installed(7, "acme/app", false)],
        1,
    ))
    .unwrap();
    db.wait(touch_installation_repo(
        db.db(),
        "t1",
        7,
        "acme/renamed",
        "trunk",
        true,
        2,
    ))
    .unwrap();
    let page = db
        .wait(repos_page(db.db(), "t1", RepoFilter::default(), 0, 10))
        .unwrap();
    assert_eq!(names(&page), ["acme/renamed"]);
    assert_eq!(page.0[0].default_branch, "trunk");
}

#[test]
fn environments_of_the_organization_count_their_secrets_and_vars() {
    let db = setup();
    let other = RepoRow {
        id: 8,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/api".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &other, 1)).unwrap();
    for (repo, name) in [(7, "staging"), (7, "Production"), (8, "prod")] {
        let put = db
            .wait(put_environment(
                db.db(),
                "t1",
                repo,
                name,
                &["main".to_owned()],
                name == "prod",
                10,
                5,
                &super::secrets::audit(Action::EnvironmentPut, "x"),
            ))
            .unwrap();
        assert!(matches!(put, PutEnvironment::Saved(_)));
    }
    let production = db
        .wait(environment(db.db(), 7, "production"))
        .unwrap()
        .unwrap();
    let owner = Owner::Environment {
        repo_id: 7,
        environment_id: production.id,
    };
    super::secrets::put_secret_ok(&db, owner, "TOKEN", "k", "sealed");
    super::secrets::put_var_ok(&db, owner, "REGION", "eu");
    let page = db
        .wait(org_environments(db.db(), "t1", None, None, 0, 10))
        .unwrap();
    assert_eq!(page.1, 3);
    let listed: Vec<(&str, &str)> = page
        .0
        .iter()
        .map(|e| (e.repository.as_str(), e.environment.name.as_str()))
        .collect();
    assert_eq!(
        listed,
        [
            ("acme/api", "prod"),
            ("acme/app", "Production"),
            ("acme/app", "staging")
        ]
    );
    assert_eq!((page.0[1].secrets, page.0[1].vars), (1, 1));
    assert_eq!((page.0[0].secrets, page.0[0].vars), (0, 0));
    let by_text = db
        .wait(org_environments(db.db(), "t1", Some("PROD"), None, 0, 10))
        .unwrap();
    assert_eq!(by_text.1, 2);
    let by_repo = db
        .wait(org_environments(
            db.db(),
            "t1",
            None,
            Some("ACME/app"),
            1,
            1,
        ))
        .unwrap();
    assert_eq!((by_repo.1, by_repo.0.len()), (2, 1));
    assert_eq!(by_repo.0[0].environment.name, "staging");
    let none = db
        .wait(org_environments(db.db(), "t2", None, None, 0, 10))
        .unwrap();
    assert_eq!(none.1, 0);
}

#[test]
fn audit_rows_page_newest_first_and_filter_by_repository() {
    let db = setup();
    let record = |action, scope, repo: Option<&str>, target: &str, now| {
        db.wait(record_audit(
            db.db(),
            &NewAudit {
                tenant: "t1",
                actor: "octocat",
                action,
                scope,
                repo,
                target,
                now,
            },
        ))
        .unwrap();
    };
    record(Action::SecretPut, "org", None, "TOKEN", 1);
    record(Action::VarPut, "repo", Some("acme/app"), "MODE", 2);
    record(
        Action::SecretDelete,
        "env",
        Some("acme/app"),
        "prod/TOKEN",
        3,
    );
    record(Action::ReposEnabled, "org", None, "all", 4);

    let (rows, total) = db.wait(audit_page(db.db(), "t1", None, None, 3)).unwrap();
    assert_eq!(total, 4);
    let targets: Vec<&str> = rows.iter().map(|row| row.target.as_str()).collect();
    assert_eq!(targets, ["all", "prod/TOKEN", "MODE"]);
    assert_eq!(rows[1].action, "secret.delete");
    let (rest, _) = db
        .wait(audit_page(db.db(), "t1", None, Some(rows[2].id), 3))
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].target, "TOKEN");

    let (app, total) = db
        .wait(audit_page(db.db(), "t1", Some("ACME/app"), None, 10))
        .unwrap();
    assert_eq!((app.len(), total), (2, 2));
    let (none, total) = db
        .wait(audit_page(db.db(), "other", None, None, 10))
        .unwrap();
    assert_eq!((none.len(), total), (0, 0));
}
