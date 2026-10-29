// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::jobs::{held_of, woken_ids};
use super::*;
use hatchrun_cloud::testdb::Db as TestDb;

pub(super) fn put_var_ok(db: &TestDb, owner: Owner, name: &str, value: &str) {
    let put = db
        .wait(put_var(db.db(), "t1", owner, None, name, value, 100, 5))
        .unwrap();
    assert_eq!(put, Put::Saved);
}

pub(super) fn put_secret_ok(db: &TestDb, owner: Owner, name: &str, key: &str, sealed: &str) {
    let put = db
        .wait(put_secret(
            db.db(),
            "t1",
            owner,
            None,
            name,
            key,
            sealed,
            100,
            5,
        ))
        .unwrap();
    assert_eq!(put, Put::Saved);
}

#[test]
fn repo_value_overrides_org() {
    let db = setup();
    put_var_ok(&db, Owner::Org, "A", "1");
    put_var_ok(&db, Owner::Org, "B", "2");
    put_var_ok(&db, Owner::Repo(7), "A", "3");
    let vars = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
    assert_eq!(
        vars,
        BTreeMap::from([("A".into(), "3".into()), ("B".into(), "2".into())])
    );
    let org = db.wait(vars_of(db.db(), Owner::Org)).unwrap();
    assert_eq!(org.len(), 2);
    assert!(
        db.wait(delete_var(db.db(), "t1", Owner::Repo(7), "A"))
            .unwrap()
    );
    assert!(
        !db.wait(delete_var(db.db(), "t1", Owner::Repo(7), "A"))
            .unwrap()
    );
    let vars = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
    assert_eq!(vars["A"], "1");
}

async fn vars_of(db: &Db, owner: Owner) -> Result<Vec<VarItem>> {
    vars(db, "t1", owner).await
}

#[test]
fn put_counts_against_the_limit() {
    let db = setup();
    let put = |name: &str, limit| {
        db.wait(put_secret(
            db.db(),
            "t1",
            Owner::Org,
            None,
            name,
            "k",
            "s",
            limit,
            5,
        ))
        .unwrap()
    };
    assert_eq!(put("A", 2), Put::Saved);
    assert_eq!(put("B", 2), Put::Saved);
    assert_eq!(put("C", 2), Put::TooMany);
    // Overwriting a name at the limit is allowed.
    assert_eq!(put("A", 2), Put::Saved);
    let names = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(names.len(), 2);
    // The limit is per scope.
    let repo = db
        .wait(put_secret(
            db.db(),
            "t1",
            Owner::Repo(7),
            None,
            "C",
            "k",
            "s",
            2,
            5,
        ))
        .unwrap();
    assert_eq!(repo, Put::Saved);
    // And the same for vars.
    let var = |name: &str| {
        db.wait(put_var(db.db(), "t1", Owner::Org, None, name, "v", 1, 5))
            .unwrap()
    };
    assert_eq!(var("A"), Put::Saved);
    assert_eq!(var("B"), Put::TooMany);
    assert_eq!(var("A"), Put::Saved);
}

#[test]
fn sealed_secrets_filters_by_name() {
    let db = setup();
    put_secret_ok(&db, Owner::Org, "A", "k", "org-a");
    put_secret_ok(&db, Owner::Repo(7), "A", "k", "repo-a");
    put_secret_ok(&db, Owner::Repo(7), "B", "k", "repo-b");
    put_secret_ok(&db, Owner::Org, "C", "k", "org-c");
    let all = db
        .wait(sealed_secrets(db.db(), "t1", 7, None, None))
        .unwrap();
    assert_eq!(all.len(), 4);
    let names = ["A".to_owned(), "C".to_owned()];
    let some = db
        .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&names)))
        .unwrap();
    let got: Vec<_> = some
        .iter()
        .map(|r| (r.name.as_str(), r.repo_id, r.sealed.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("A", None, "org-a"),
            ("A", Some(7), "repo-a"),
            ("C", None, "org-c")
        ]
    );
    let none = db
        .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&[])))
        .unwrap();
    assert_eq!(none.len(), 0);
    // Another repository sees only the organization's rows.
    let other = db
        .wait(sealed_secrets(db.db(), "t1", 8, None, None))
        .unwrap();
    assert_eq!(other.len(), 2);
    assert!(!format!("{:?}", all[0]).contains("org-a"));
}

#[test]
fn reseal_skips_a_row_that_changed() {
    let db = setup();
    put_secret_ok(&db, Owner::Repo(7), "A", "old", "s1");
    put_secret_ok(&db, Owner::Org, "B", "new", "s2");
    let stale = db.wait(stale_secrets(db.db(), "new", 10)).unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].name, "A");
    assert!(!format!("{:?}", stale[0]).contains("s1"));
    // The secret is set again between the read and the re-seal.
    put_secret_ok(&db, Owner::Repo(7), "A", "old", "s1-changed");
    assert!(
        !db.wait(reseal(db.db(), &stale[0], "new", "s1-resealed"))
            .unwrap()
    );
    let now = db.wait(stale_secrets(db.db(), "new", 10)).unwrap();
    assert_eq!(now[0].sealed, "s1-changed");
    assert!(
        db.wait(reseal(db.db(), &now[0], "new", "s1-resealed"))
            .unwrap()
    );
    assert_eq!(db.wait(stale_secrets(db.db(), "new", 10)).unwrap().len(), 0);
}

#[test]
fn stale_secrets_is_bounded() {
    let db = setup();
    for name in ["A", "B", "C"] {
        put_secret_ok(&db, Owner::Org, name, "old", "s");
    }
    assert_eq!(db.wait(stale_secrets(db.db(), "new", 2)).unwrap().len(), 2);
}

#[test]
fn deleting_a_repo_or_tenant_removes_its_secrets() {
    let db = setup();
    put_secret_ok(&db, Owner::Repo(7), "A", "k", "s");
    assert!(
        db.wait(delete_secret(db.db(), "t1", Owner::Repo(7), "A"))
            .unwrap()
    );
    assert!(
        !db.wait(delete_secret(db.db(), "t1", Owner::Repo(7), "A"))
            .unwrap()
    );
    put_secret_ok(&db, Owner::Repo(7), "A", "k", "s");
    db.run("DELETE FROM tenants WHERE id = 't1'", &[]);
    let left = db.rows("SELECT 1 FROM ci_secrets", &[]).unwrap();
    assert_eq!(left.len(), 0);
}

pub(super) fn save_environment(
    db: &TestDb,
    name: &str,
    branches: &[&str],
    approval: bool,
) -> EnvironmentRow {
    let branches: Vec<String> = branches.iter().map(|b| (*b).to_owned()).collect();
    match db
        .wait(put_environment(
            db.db(),
            "t1",
            7,
            name,
            &branches,
            approval,
            10,
            5,
        ))
        .unwrap()
    {
        PutEnvironment::Saved(row) => row,
        PutEnvironment::TooMany => panic!("too many"),
    }
}

pub(super) fn job_naming(db: &TestDb, delivery: &str, environment: Option<&str>) -> u64 {
    let run = db
        .wait(insert_run(db.db(), &new_run(delivery, "ci", 1_000)))
        .unwrap()
        .unwrap();
    let job = NewJob {
        environment: environment.map(str::to_owned),
        ..new_job("deploy")
    };
    db.wait(insert_jobs(db.db(), run.id, &[job], 1_000))
        .unwrap()[0]
        .1
}

#[test]
fn environments_are_unique_per_repo_ignoring_case() {
    let db = setup();
    save_environment(&db, "Production", &["release"], false);
    let again = save_environment(&db, "production", &["main"], true);
    assert_eq!(again.name, "Production");
    assert_eq!(again.branches, ["main"]);
    assert!(again.approval);
    let all = db.wait(environments(db.db(), 7)).unwrap();
    assert_eq!(all, [again]);
    // The index refuses a second row that differs only in case.
    let tried = db.try_run(
        "INSERT INTO ci_environments (tenant_id, repo_id, name, branches, approval, updated_at) \
         VALUES ('t1', 7, 'PRODUCTION', '[]', false, 0)",
        &[],
    );
    assert!(tried.is_err());
}

#[test]
fn environment_lookup_ignores_case() {
    let db = setup();
    let made = save_environment(&db, "Production", &["main"], true);
    let found = db.wait(environment(db.db(), 7, "PRODUCTION")).unwrap();
    assert_eq!(found, Some(made.clone()));
    assert_eq!(db.wait(environment(db.db(), 7, "staging")).unwrap(), None);
    assert_eq!(
        db.wait(environment(db.db(), 8, "production")).unwrap(),
        None
    );

    let named = job_naming(&db, "d1", Some("production"));
    let got = db.wait(job_environment(db.db(), named)).unwrap().unwrap();
    assert_eq!(got.environment, Some(made));
    assert_eq!(got.repo_id, 7);
    assert_eq!(got.full_name, "acme/app");
    assert_eq!(got.event, "push");
    assert_eq!(got.git_ref, "refs/heads/main");
    let missing = job_naming(&db, "d2", Some("nope"));
    let got = db.wait(job_environment(db.db(), missing)).unwrap().unwrap();
    assert_eq!(got.environment, None);
    let none = job_naming(&db, "d3", None);
    let got = db.wait(job_environment(db.db(), none)).unwrap().unwrap();
    assert_eq!(got.environment, None);
    assert_eq!(db.wait(job_environment(db.db(), 999_999)).unwrap(), None);
}

#[test]
fn put_environment_counts_against_the_limit() {
    let db = setup();
    let put = |name: &str| {
        db.wait(put_environment(db.db(), "t1", 7, name, &[], false, 2, 5))
            .unwrap()
    };
    assert!(matches!(put("a"), PutEnvironment::Saved(_)));
    assert!(matches!(put("b"), PutEnvironment::Saved(_)));
    assert_eq!(put("c"), PutEnvironment::TooMany);
    assert!(matches!(put("A"), PutEnvironment::Saved(_)));
    assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 2);
}

#[test]
fn deleting_an_environment_deletes_its_secrets_and_vars() {
    let db = setup();
    let env = save_environment(&db, "Production", &[], false);
    let owner = Owner::Environment {
        repo_id: 7,
        environment_id: env.id,
    };
    put_secret_ok(&db, owner, "S", "k", "s");
    put_var_ok(&db, owner, "V", "1");
    put_secret_ok(&db, Owner::Repo(7), "S", "k", "repo");
    assert!(
        db.wait(delete_environment(db.db(), 7, "production"))
            .unwrap()
    );
    assert!(
        !db.wait(delete_environment(db.db(), 7, "production"))
            .unwrap()
    );
    let left = db
        .rows(
            "SELECT 1 FROM ci_secrets WHERE environment_id IS NOT NULL",
            &[],
        )
        .unwrap();
    assert_eq!(left.len(), 0);
    let left = db
        .rows(
            "SELECT 1 FROM ci_vars WHERE environment_id IS NOT NULL",
            &[],
        )
        .unwrap();
    assert_eq!(left.len(), 0);
    assert_eq!(
        db.wait(secret_names(db.db(), "t1", Owner::Repo(7)))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn environment_values_win_over_repo_and_org() {
    let db = setup();
    let env = save_environment(&db, "e", &[], false);
    let owner = Owner::Environment {
        repo_id: 7,
        environment_id: env.id,
    };
    put_var_ok(&db, Owner::Org, "X", "o");
    put_var_ok(&db, Owner::Repo(7), "X", "r");
    put_var_ok(&db, owner, "X", "e");
    let with = db.wait(job_vars(db.db(), "t1", 7, Some(env.id))).unwrap();
    assert_eq!(with, BTreeMap::from([("X".into(), "e".into())]));
    let without = db.wait(job_vars(db.db(), "t1", 7, None)).unwrap();
    assert_eq!(without, BTreeMap::from([("X".into(), "r".into())]));

    put_secret_ok(&db, owner, "S", "k", "env");
    put_secret_ok(&db, Owner::Org, "S", "k", "org");
    put_secret_ok(&db, Owner::Repo(7), "S", "k", "repo");
    let rows = db
        .wait(sealed_secrets(db.db(), "t1", 7, Some(env.id), None))
        .unwrap();
    let got: Vec<_> = rows
        .iter()
        .map(|r| (r.owner(), r.sealed.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (Owner::Org, "org"),
            (Owner::Repo(7), "repo"),
            (owner, "env")
        ]
    );
    let rows = db
        .wait(sealed_secrets(db.db(), "t1", 7, None, None))
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.environment_id.is_none()));
}

#[test]
fn another_environment_stays_out() {
    let db = setup();
    let e = save_environment(&db, "e", &[], false);
    let f = save_environment(&db, "f", &[], false);
    let of_f = Owner::Environment {
        repo_id: 7,
        environment_id: f.id,
    };
    put_secret_ok(&db, of_f, "S", "k", "f");
    put_var_ok(&db, of_f, "V", "f");
    let rows = db
        .wait(sealed_secrets(db.db(), "t1", 7, Some(e.id), None))
        .unwrap();
    assert_eq!(rows.len(), 0);
    assert!(
        db.wait(job_vars(db.db(), "t1", 7, Some(e.id)))
            .unwrap()
            .is_empty()
    );
    // Scopes count their own rows: the repository's limit ignores them.
    let put = db
        .wait(put_secret(
            db.db(),
            "t1",
            Owner::Repo(7),
            None,
            "R",
            "k",
            "s",
            1,
            5,
        ))
        .unwrap();
    assert_eq!(put, Put::Saved);
    assert_eq!(db.wait(secret_names(db.db(), "t1", of_f)).unwrap().len(), 1);
}

#[test]
fn release_approval_holds_frees_the_jobs() {
    let db = setup();
    let hold = |job: u64, reason: &str| {
        db.run(
            "UPDATE ci_jobs SET status = 'queued', held = $2, updated_at = 1 WHERE id = $1",
            &params![job, reason],
        );
    };
    let a = job_naming(&db, "a", Some("Production"));
    let b = job_naming(&db, "b", Some("production"));
    let card = job_naming(&db, "c", Some("production"));
    let other = job_naming(&db, "d", Some("staging"));
    hold(a, "approval");
    hold(b, "approval");
    hold(card, "card");
    hold(other, "approval");
    let woken = db
        .wait(release_approval_holds(db.db(), 7, "PRODUCTION", 50))
        .unwrap();
    assert_eq!(woken_ids(woken.clone()), [a, b]);
    assert!(woken.iter().all(|w| w.attempt == 1));
    for job in [a, b] {
        assert_eq!(held_of(&db, job), None);
        let at: Option<u64> = db
            .column(
                "SELECT next_launch_at AS n FROM ci_jobs WHERE id = $1",
                &params![job],
                "n",
            )
            .unwrap();
        assert_eq!(at, Some(50));
    }
    assert_eq!(held_of(&db, card).as_deref(), Some("card"));
    assert_eq!(held_of(&db, other).as_deref(), Some("approval"));
}

#[test]
fn environment_changes_release_holds_through_the_handler_path() {
    use crate::settings_api::release_holds;

    let db = setup();
    let hold = |job: u64| {
        db.run(
            "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 1 \
             WHERE id = $1",
            &params![job],
        );
    };
    let release = |approval: bool, deleted: bool, now: u64| {
        db.wait(release_holds(
            db.db(),
            7,
            "production",
            approval,
            deleted,
            now,
        ))
        .unwrap()
    };
    let job = job_naming(&db, "a", Some("Production"));
    hold(job);

    // A save that keeps approval on frees nothing.
    save_environment(&db, "Production", &["main"], true);
    assert_eq!(woken_ids(release(true, false, 10)), Vec::<u64>::new());
    assert_eq!(held_of(&db, job).as_deref(), Some("approval"));

    // Turning approval off frees the job and schedules its launch.
    let row = save_environment(&db, "Production", &["main"], false);
    let woken = release(row.approval, false, 20);
    assert_eq!(woken_ids(woken), [job]);
    assert_eq!(held_of(&db, job), None);

    // Deleting the environment frees a job held on it.
    let later = job_naming(&db, "b", Some("Production"));
    hold(later);
    save_environment(&db, "Production", &[], true);
    assert!(
        db.wait(delete_environment(db.db(), 7, "production"))
            .unwrap()
    );
    assert_eq!(woken_ids(release(false, true, 30)), [later]);
    assert_eq!(held_of(&db, later), None);
}

#[test]
fn a_transferred_repo_leaves_its_environments() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    save_environment(&db, "Production", &[], false);
    let mut same = db.wait(repo(db.db(), 7)).unwrap().unwrap();
    db.wait(upsert_repo(db.db(), &same, 6)).unwrap();
    assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 1);
    same.tenant_id = "t2".into();
    same.installation_id = 100;
    db.wait(upsert_repo(db.db(), &same, 7)).unwrap();
    assert_eq!(db.wait(environments(db.db(), 7)).unwrap().len(), 0);
}

#[test]
fn job_rows_carry_environment() {
    let db = setup();
    let id = job_naming(&db, "d", Some("production"));
    let row = db.wait(job(db.db(), id)).unwrap().unwrap();
    assert_eq!(row.environment.as_deref(), Some("production"));
    assert_eq!(row.approval_attempt, None);
    assert_eq!(row.approval_decision, None);
    assert_eq!(row.approval_by, None);
    assert_eq!(row.approval_at, None);
    let id = job_naming(&db, "e", None);
    assert_eq!(
        db.wait(job(db.db(), id)).unwrap().unwrap().environment,
        None
    );
}

/// Repositories 8 (acme/web) and 9 (acme/lib) join repo 7 (acme/app).
fn with_repos() -> TestDb {
    let db = setup();
    for (id, name) in [(8, "acme/web"), (9, "acme/lib")] {
        let repo = RepoRow {
            id,
            tenant_id: "t1".into(),
            installation_id: 99,
            full_name: name.into(),
            default_branch: "main".into(),
            private: false,
        };
        db.wait(upsert_repo(db.db(), &repo, 2)).unwrap();
    }
    db
}

fn put_selected(db: &TestDb, kind: &str, name: &str, repos: Option<&[u64]>) {
    let put = if kind == "secret" {
        db.wait(put_secret(
            db.db(),
            "t1",
            Owner::Org,
            repos,
            name,
            "k",
            name,
            100,
            5,
        ))
    } else {
        db.wait(put_var(
            db.db(),
            "t1",
            Owner::Org,
            repos,
            name,
            name,
            100,
            5,
        ))
    };
    assert_eq!(put.unwrap(), Put::Saved);
}

fn secret_values(db: &TestDb, repo_id: u64) -> Vec<String> {
    db.wait(sealed_secrets(db.db(), "t1", repo_id, None, None))
        .unwrap()
        .into_iter()
        .map(|row| row.sealed)
        .collect()
}

fn var_names(db: &TestDb, repo_id: u64) -> Vec<String> {
    db.wait(job_vars(db.db(), "t1", repo_id, None))
        .unwrap()
        .into_keys()
        .collect()
}

#[test]
fn selected_org_rows_reach_only_their_repos() {
    let db = with_repos();
    for kind in ["secret", "var"] {
        put_selected(&db, kind, "A", Some(&[8]));
        put_selected(&db, kind, "B", None);
    }
    assert_eq!(secret_values(&db, 7), ["B"]);
    assert_eq!(secret_values(&db, 8), ["A", "B"]);
    assert_eq!(var_names(&db, 7), ["B"]);
    assert_eq!(var_names(&db, 8), ["A", "B"]);
    // The names filter goes through the same rule.
    let names = vec!["A".to_owned()];
    let rows = db
        .wait(sealed_secrets(db.db(), "t1", 7, None, Some(&names)))
        .unwrap();
    assert_eq!(rows, []);
}

#[test]
fn a_new_owners_environment_does_not_apply_to_the_old_owners_jobs() {
    let db = setup();
    let job = job_naming(&db, "a", Some("production"));
    db.run(
        "UPDATE ci_jobs SET status = 'queued', held = 'approval', updated_at = 1 \
         WHERE id = $1",
        &params![job],
    );
    // The repository moves to t2, which makes its own production
    // without approval.
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    db.run("UPDATE ci_repos SET tenant_id = 't2' WHERE id = 7", &[]);
    db.run(
        "INSERT INTO ci_environments (tenant_id, repo_id, name, branches, approval, updated_at) \
         VALUES ('t2', 7, 'production', '[]', false, 5)",
        &[],
    );
    let got = db.wait(job_environment(db.db(), job)).unwrap().unwrap();
    assert_eq!(got.environment, None);
    let woken = db
        .wait(release_approval_holds(db.db(), 7, "production", 9))
        .unwrap();
    assert_eq!(woken_ids(woken), Vec::<u64>::new());
    assert_eq!(held_of(&db, job).as_deref(), Some("approval"));
}

#[test]
fn a_transfer_drops_the_repo_from_the_old_orgs_selections() {
    let db = with_repos();
    put_selected(&db, "secret", "A", Some(&[8, 9]));
    put_selected(&db, "var", "A", Some(&[8, 9]));
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    let mut moved = db.wait(repo(db.db(), 8)).unwrap().unwrap();
    moved.tenant_id = "t2".into();
    moved.installation_id = 100;
    db.wait(upsert_repo(db.db(), &moved, 3)).unwrap();
    // It transfers back: the old selection no longer names it.
    moved.tenant_id = "t1".into();
    moved.installation_id = 99;
    db.wait(upsert_repo(db.db(), &moved, 4)).unwrap();
    assert_eq!(secret_values(&db, 8), Vec::<String>::new());
    assert_eq!(var_names(&db, 8), Vec::<String>::new());
    assert_eq!(secret_values(&db, 9), ["A"]);
    assert_eq!(var_names(&db, 9), ["A"]);
}

#[test]
fn an_empty_selection_reaches_no_repo() {
    let db = with_repos();
    put_selected(&db, "secret", "A", Some(&[]));
    put_selected(&db, "var", "A", Some(&[]));
    for repo_id in [7, 8] {
        assert_eq!(secret_values(&db, repo_id), Vec::<String>::new());
        assert_eq!(var_names(&db, repo_id), Vec::<String>::new());
    }
    let listed = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(listed[0].repositories, Some(Vec::new()));
}

#[test]
fn selection_never_crosses_tenants() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t2', 2, 'other', 100, 0, 0)",
        &[],
    );
    let foreign = RepoRow {
        id: 9,
        tenant_id: "t2".into(),
        installation_id: 100,
        full_name: "other/x".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &foreign, 2)).unwrap();
    put_selected(&db, "secret", "A", Some(&[9]));
    put_selected(&db, "var", "A", Some(&[9]));
    let rows = db
        .wait(sealed_secrets(db.db(), "t2", 9, None, None))
        .unwrap();
    assert_eq!(rows, []);
    assert_eq!(
        db.wait(job_vars(db.db(), "t2", 9, None)).unwrap(),
        BTreeMap::new()
    );
    let listed = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(listed[0].repositories, Some(Vec::new()));
    let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(listed[0].repositories, Some(Vec::new()));
}

#[test]
fn listing_names_the_selected_repos() {
    let db = with_repos();
    put_selected(&db, "secret", "A", Some(&[8, 7]));
    put_selected(&db, "secret", "B", None);
    put_selected(&db, "var", "A", Some(&[8, 7]));
    put_selected(&db, "var", "B", None);
    let both = Some(vec!["acme/app".to_owned(), "acme/web".to_owned()]);
    let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(secrets[0].repositories, both);
    assert_eq!(secrets[1].repositories, None);
    let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(listed[0].repositories, both);
    assert_eq!(listed[1].repositories, None);
    // A replacing put can widen the selection again.
    put_selected(&db, "secret", "A", None);
    let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(secrets[0].repositories, None);
}

#[test]
fn listing_leaves_out_gone_repos() {
    let db = with_repos();
    put_selected(&db, "secret", "A", Some(&[7, 8]));
    put_selected(&db, "var", "A", Some(&[7, 8]));
    // Repo 7 is renamed away when a new repository takes its name.
    let fresh = RepoRow {
        id: 10,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/app".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
    let only_web = Some(vec!["acme/web".to_owned()]);
    let secrets = db.wait(secret_names(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(secrets[0].repositories, only_web);
    let listed = db.wait(vars(db.db(), "t1", Owner::Org)).unwrap();
    assert_eq!(listed[0].repositories, only_web);
    let names = db.wait(tenant_repos(db.db(), "t1")).unwrap();
    assert!(names.iter().all(|name| !name.contains("#gone-")));
}

#[test]
fn a_repo_row_cannot_select_repos() {
    let db = with_repos();
    for table in ["ci_secrets", "ci_vars"] {
        let sql = if table == "ci_secrets" {
            "INSERT INTO ci_secrets (tenant_id, repo_id, name, key_id, sealed, updated_at, \
             repositories) VALUES ('t1', 7, 'A', 'k', 's', 1, '{7}')"
        } else {
            "INSERT INTO ci_vars (tenant_id, repo_id, name, value, updated_at, \
             repositories) VALUES ('t1', 7, 'A', 'v', 1, '{7}')"
        };
        assert!(db.try_run(sql, &[]).is_err(), "{table}");
    }
    // Through the store too: an environment row is a repo_id row.
    let put = db.wait(put_var(
        db.db(),
        "t1",
        Owner::Repo(7),
        Some(&[8]),
        "A",
        "v",
        100,
        5,
    ));
    assert!(put.is_err());
}

#[test]
fn tenant_repos_leaves_out_gone_names() {
    let db = with_repos();
    // Repo 7 takes the name of a new repository 10, and 7 is renamed away.
    let fresh = RepoRow {
        id: 10,
        tenant_id: "t1".into(),
        installation_id: 99,
        full_name: "acme/app".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &fresh, 3)).unwrap();
    let names = db.wait(tenant_repos(db.db(), "t1")).unwrap();
    assert_eq!(names, ["acme/app", "acme/lib", "acme/web"]);
    assert_eq!(
        db.wait(tenant_repos(db.db(), "t2")).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn repo_ids_ignore_case() {
    let db = with_repos();
    let names = vec!["ACME/APP".to_owned(), "acme/nope".to_owned()];
    let found = db.wait(repo_ids(db.db(), "t1", &names)).unwrap();
    assert_eq!(found, [("acme/app".to_owned(), 7)]);
    assert_eq!(db.wait(repo_ids(db.db(), "t1", &[])).unwrap(), []);
    assert_eq!(db.wait(repo_ids(db.db(), "t2", &names)).unwrap(), []);
}
