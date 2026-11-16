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

/// October 2026, whose months the partition tests use.
const OCTOBER: u64 = 1_790_812_800_000;
const NOVEMBER: u64 = 1_793_491_200_000;
const DECEMBER: u64 = 1_793_491_200_000 + 30 * DAY;

/// The month partitions of `parent` in this schema, oldest first.
fn partitions(db: &TestDb, parent: &str) -> Vec<String> {
    db.rows(
        "SELECT c.relname AS name FROM pg_class c \
         JOIN pg_inherits i ON i.inhrelid = c.oid \
         JOIN pg_class p ON p.oid = i.inhparent \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE p.relname = $1 AND n.nspname = current_schema() ORDER BY 1",
        &params![parent],
    )
    .unwrap()
    .into_iter()
    .map(|row| row["name"].as_str().unwrap().to_owned())
    .collect()
}

#[test]
fn upkeep_creates_the_next_partitions_and_is_idempotent() {
    let db = setup();
    let now = OCTOBER + 15 * DAY;
    db.wait(ensure_partitions(db.db(), now)).unwrap();
    // Twice is fine.
    db.wait(ensure_partitions(db.db(), now)).unwrap();
    // The migration forward-fills the months around the real today, so
    // only check the months this pass owns.
    for (parent, name) in [
        ("ci_runs_t30", "ci_runs_t30_202610"),
        ("ci_runs_t30", "ci_runs_t30_202611"),
        ("ci_runs_t30", "ci_runs_t30_202612"),
        ("ci_runs_t90", "ci_runs_t90_202610"),
        ("ci_runs_t90", "ci_runs_t90_202611"),
        ("ci_runs_t90", "ci_runs_t90_202612"),
        ("ci_jobs_t30", "ci_jobs_t30_202610"),
        ("ci_jobs_t30", "ci_jobs_t30_202611"),
        ("ci_jobs_t30", "ci_jobs_t30_202612"),
        ("ci_usage", "ci_usage_202610"),
        ("ci_usage", "ci_usage_202611"),
        ("ci_usage", "ci_usage_202612"),
        ("ci_audit", "ci_audit_202610"),
        ("ci_audit", "ci_audit_202611"),
        ("ci_audit", "ci_audit_202612"),
    ] {
        assert!(
            partitions(&db, parent).contains(&name.to_owned()),
            "{parent} has no {name}"
        );
    }
}

/// A run of tenant `tenant_id` in repository `repo_id`, recorded at
/// `now`.
fn tenant_run(tenant_id: &str, repo_id: u64, delivery: &str, now: u64) -> NewRun {
    NewRun {
        tenant_id: tenant_id.into(),
        repo_id,
        delivery: delivery.into(),
        ..new_run("ignored", "ci", now)
    }
}

#[test]
fn a_thirty_day_month_drops_on_time_while_ninety_day_stays() {
    let db = setup();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at, \
         retention_days) VALUES ('t2', 2, 'initech', 100, 0, 0, 90)",
        &[],
    );
    let repo = RepoRow {
        id: 8,
        tenant_id: "t2".into(),
        installation_id: 100,
        full_name: "initech/app".into(),
        default_branch: "main".into(),
        private: false,
    };
    db.wait(upsert_repo(db.db(), &repo, 1)).unwrap();

    let october = OCTOBER + 5 * DAY;
    let old = db
        .wait(make_run(db.db(), &tenant_run("t1", 7, "old", october)))
        .unwrap()
        .unwrap();
    assert_eq!(old.retention_days, 30);
    db.wait(insert_jobs(db.db(), old.id, &[new_job("build")], october))
        .unwrap();
    let kept = db
        .wait(make_run(db.db(), &tenant_run("t2", 8, "kept", october)))
        .unwrap()
        .unwrap();
    assert_eq!(kept.retention_days, 90);
    db.wait(insert_jobs(db.db(), kept.id, &[new_job("build")], october))
        .unwrap();
    db.wait(record_delivery(db.db(), "old", "push", october))
        .unwrap();
    db.wait(record_delivery(db.db(), "fresh", "push", DECEMBER - DAY))
        .unwrap();

    // November keeps both: October ended less than 30 days ago. Older
    // empty months from the fixtures may drop.
    let november = NOVEMBER + 15 * DAY;
    db.wait(ensure_partitions(db.db(), november)).unwrap();
    let dropped = db
        .wait(drop_expired_partitions(db.db(), november, &[]))
        .unwrap();
    assert!(
        !dropped.iter().any(|name| name.contains("202610")),
        "{dropped:?}"
    );
    assert!(db.wait(run(db.db(), old.id)).unwrap().is_some());
    assert!(db.wait(run(db.db(), kept.id)).unwrap().is_some());

    // December drops the 30 day October while the 90 day one stays.
    // Audit keeps 400 days, so its October stays too.
    db.wait(ensure_partitions(db.db(), DECEMBER)).unwrap();
    let dropped = db
        .wait(drop_expired_partitions(
            db.db(),
            DECEMBER,
            &["ci_runs_t30_202610".to_owned()],
        ))
        .unwrap();
    for partition in ["ci_runs_t30_202610", "ci_jobs_t30_202610"] {
        assert!(dropped.contains(&partition.to_owned()), "{dropped:?}");
    }
    assert!(
        !dropped.iter().any(|name| name.contains("202610")
            && (name.contains("t90")
                || name.starts_with("ci_audit")
                || name.starts_with("ci_usage"))),
        "{dropped:?}"
    );
    assert!(db.wait(run(db.db(), old.id)).unwrap().is_none());
    assert!(db.wait(run(db.db(), kept.id)).unwrap().is_some());
    assert_eq!(db.wait(jobs_for_run(db.db(), old.id)).unwrap(), []);
    assert_eq!(db.wait(jobs_for_run(db.db(), kept.id)).unwrap().len(), 1);
    let jobs: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_jobs", &[], "n")
        .unwrap();
    assert_eq!(jobs, 1);
    assert!(!db.wait(delivery_seen(db.db(), "old")).unwrap());
    assert!(db.wait(delivery_seen(db.db(), "fresh")).unwrap());
    // Dropping again changes nothing.
    assert_eq!(
        db.wait(drop_expired_partitions(
            db.db(),
            DECEMBER,
            &["ci_runs_t30_202610".to_owned()]
        ))
        .unwrap(),
        Vec::<String>::new()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_run_and_its_jobs_drop_together_while_usage_and_audit_stay() {
    let db = setup();
    let october = OCTOBER + 5 * DAY;
    let row = db
        .wait(make_run(db.db(), &new_run("d", "ci", october)))
        .unwrap()
        .unwrap();
    let ids = db
        .wait(insert_jobs(db.db(), row.id, &[new_job("build")], october))
        .unwrap();
    let job_id = ids[0].1;
    db.wait(queue_job(db.db(), job_id, 1, None, october))
        .unwrap();
    let claimed = claim(&db, job_id, october);
    db.wait(start_job(db.db(), job_id, october)).unwrap();
    assert_eq!(claimed.attempt, 1);
    db.wait(finish_job(
        db.db(),
        job_id,
        1,
        "success",
        &serde_json::json!([]),
        None,
        None,
        october + 120_000,
    ))
    .unwrap();
    // An artifact and a concurrency slot that name the run.
    db.run(
        "INSERT INTO ci_artifacts (run_id, job_id, attempt, name, object_key, created_at, \
         finalized_at, size) VALUES ($1, $2, 1, 'a', 'k', $3, $3, 10)",
        &params![row.id, job_id, october],
    );
    db.run(
        "INSERT INTO ci_concurrency (repo_id, run_id, attempt, group_key, group_name, state, \
         created_at) VALUES (7, $1, 1, 'g', 'g', 'running', $2)",
        &params![row.id, october],
    );
    db.wait(record_audit(
        db.db(),
        &NewAudit {
            tenant: "t1",
            actor: "octocat",
            action: Action::SecretPut,
            scope: "org",
            repo: None,
            target: "TOKEN",
            now: october,
        },
    ))
    .unwrap();

    // The rows sit in the October partitions.
    let part: String = db
        .column(
            "SELECT 'ci_runs_t30_202610' AS n WHERE EXISTS \
             (SELECT 1 FROM ci_runs_t30_202610 WHERE id = $1)",
            &params![row.id],
            "n",
        )
        .unwrap();
    assert_eq!(part, "ci_runs_t30_202610");
    let jobs: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_jobs_t30_202610 WHERE run_id = $1",
            &params![row.id],
            "n",
        )
        .unwrap();
    assert_eq!(jobs, 1);
    let usage: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_usage_202610 WHERE job_id = $1",
            &params![job_id],
            "n",
        )
        .unwrap();
    assert_eq!(usage, 1);
    let audits: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_audit_202610 WHERE tenant_id = 't1'",
            &[],
            "n",
        )
        .unwrap();
    assert_eq!(audits, 1);

    // The 30 day October drops in December; usage keeps its own 90 days
    // and audit keeps 400.
    db.wait(ensure_partitions(db.db(), DECEMBER)).unwrap();
    let dropped = db
        .wait(drop_expired_partitions(
            db.db(),
            DECEMBER,
            &["ci_runs_t30_202610".to_owned()],
        ))
        .unwrap();
    assert!(dropped.contains(&"ci_runs_t30_202610".to_owned()));
    assert!(dropped.contains(&"ci_jobs_t30_202610".to_owned()));
    assert!(!dropped.contains(&"ci_usage_202610".to_owned()));
    assert!(!dropped.contains(&"ci_audit_202610".to_owned()));
    assert!(db.wait(run(db.db(), row.id)).unwrap().is_none());
    let artifacts: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_artifacts", &[], "n")
        .unwrap();
    assert_eq!(artifacts, 0);
    let slots: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_concurrency", &[], "n")
        .unwrap();
    assert_eq!(slots, 0);
    let usage: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_usage_202610 WHERE job_id = $1",
            &params![job_id],
            "n",
        )
        .unwrap();
    assert_eq!(usage, 1);
    let audits: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_audit_202610 WHERE tenant_id = 't1'",
            &[],
            "n",
        )
        .unwrap();
    assert_eq!(audits, 1);

    // Usage drops once October is 90 days out: mid-February 2027 is
    // past November plus 90 days. Audit stays past that.
    let february = 1_798_761_600_000 + 45 * DAY;
    db.wait(ensure_partitions(db.db(), february)).unwrap();
    let dropped = db
        .wait(drop_expired_partitions(db.db(), february, &[]))
        .unwrap();
    assert!(dropped.contains(&"ci_usage_202610".to_owned()));
    assert!(!dropped.contains(&"ci_audit_202610".to_owned()));
    let usage: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_usage", &[], "n")
        .unwrap();
    assert_eq!(usage, 0);
}

#[test]
fn an_empty_database_drops_nothing() {
    let db = setup();
    assert_eq!(
        db.wait(drop_expired_partitions(db.db(), 5, &[])).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn only_unconcluded_runs_need_purging() {
    let db = setup();
    let run = db
        .wait(make_run(db.db(), &new_run("d", "ci", 1_000)))
        .unwrap()
        .unwrap();
    let needing = db
        .wait(runs_needing_purge(db.db(), "ci_runs_t30_197001", 100))
        .unwrap();
    assert_eq!(needing, [run.id]);
    db.wait(finish_run(db.db(), run.id, 1, "success", 2_000))
        .unwrap();
    let needing = db
        .wait(runs_needing_purge(db.db(), "ci_runs_t30_197001", 100))
        .unwrap();
    assert_eq!(needing, Vec::<u64>::new());
}

#[test]
fn a_purged_run_is_not_listed_again() {
    let db = setup();
    let ids: Vec<u64> = (0..3)
        .map(|n| {
            db.wait(make_run(db.db(), &new_run(&format!("p{n}"), "ci", 1_000)))
                .unwrap()
                .unwrap()
                .id
        })
        .collect();
    let first = db
        .wait(runs_needing_purge(db.db(), "ci_runs_t30_197001", 2))
        .unwrap();
    assert_eq!(first, ids[..2]);
    db.wait(mark_purged(db.db(), "ci_runs_t30_197001", &first, 5_000))
        .unwrap();
    let rest = db
        .wait(runs_needing_purge(db.db(), "ci_runs_t30_197001", 2))
        .unwrap();
    assert_eq!(rest, ids[2..]);
}

#[test]
fn an_audit_month_drops_once_it_is_older_than_four_hundred_days() {
    let db = setup();
    db.wait(record_audit(
        db.db(),
        &NewAudit {
            tenant: "t1",
            actor: "octocat",
            action: Action::SecretPut,
            scope: "org",
            repo: None,
            target: "TOKEN",
            now: 1_000,
        },
    ))
    .unwrap();
    // February 1971 is 400 days out from January 1970, so the month
    // stays; March 1971 drops it.
    let kept = db
        .wait(drop_expired_partitions(db.db(), 1_000 + 400 * DAY, &[]))
        .unwrap();
    assert!(
        !kept.iter().any(|name| name.starts_with("ci_audit")),
        "{kept:?}"
    );
    let dropped = db
        .wait(drop_expired_partitions(db.db(), 1_000 + 432 * DAY, &[]))
        .unwrap();
    assert!(
        dropped.contains(&"ci_audit_197001".to_owned()),
        "{dropped:?}"
    );
    let audits: i64 = db
        .column("SELECT COUNT(*) AS n FROM ci_audit", &[], "n")
        .unwrap();
    assert_eq!(audits, 0);
}
