// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::jobs::{
    ACTIVE, CURRENT, FAILED, RUN_COLUMNS, SEARCH, WAITING, counts_query, jobs_query, list_query,
    run_query,
};
use crate::testdb::{Db, Param, params};

#[test]
fn status_names_a_group() {
    let page: Page = serde_json::from_value(serde_json::json!({"status": "failed"})).unwrap();
    assert_eq!(page.status, Some(Group::Failed));
    let page: Page = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(page.status, None);
    assert!(serde_json::from_value::<Page>(serde_json::json!({"status": "done"})).is_err());
}

#[test]
fn the_list_is_newest_first_unless_asked() {
    let page: Page = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!((page.sort, page.dir), (Sort::Started, Direction::Desc));
    let page: Page =
        serde_json::from_value(serde_json::json!({"sort": "repository", "dir": "asc", "q": "web"}))
            .unwrap();
    assert_eq!((page.sort, page.dir), (Sort::Repository, Direction::Asc));
    assert_eq!(page.q.as_deref(), Some("web"));
    let page: Page = serde_json::from_value(serde_json::json!({"sort": "took"})).unwrap();
    assert_eq!(page.sort, Sort::Took);
    for bad in [
        serde_json::json!({"sort": "duration"}),
        serde_json::json!({"sort": "status"}),
        serde_json::json!({"dir": "up"}),
    ] {
        assert!(serde_json::from_value::<Page>(bad).is_err());
    }
}

#[test]
fn a_search_takes_wildcards_literally() {
    assert_eq!(contains("web"), "%web%");
    assert_eq!(contains("50%_off\\"), "%50\\%\\_off\\\\%");
}

#[test]
fn a_group_lists_what_its_count_counts() {
    let counts = counts_query();
    for group in [Group::Active, Group::Waiting, Group::Failed] {
        let list = list_query(Some(group), Sort::Started, Direction::Desc);
        assert!(list.contains(&format!(" AND {} AND {CURRENT})", group.predicate())));
        assert!(counts.contains(&format!("({}) AND {CURRENT})", group.predicate())));
    }
    let all = list_query(None, Sort::Started, Direction::Desc);
    assert!([ACTIVE, WAITING, FAILED].iter().all(|p| !all.contains(p)));
}

#[test]
fn a_cursor_is_a_time_and_a_run() {
    assert_eq!(
        cursor("1792140000000:18234567890", Sort::Started),
        Some(Cursor {
            queued_at: 1_792_140_000_000,
            id: 18_234_567_890,
            repository: None,
            ended_at: None,
        })
    );
    assert_eq!(
        cursor("1:2:acme/web", Sort::Repository).and_then(|c| c.repository),
        Some("acme/web".to_owned())
    );
    assert_eq!(cursor("1:2", Sort::Repository), None);
    assert_eq!(
        cursor("1:2:3", Sort::Took).and_then(|c| c.ended_at),
        Some(3)
    );
    // A run still going has no end.
    assert_eq!(
        cursor("1:2:", Sort::Took),
        Some(Cursor {
            queued_at: 1,
            id: 2,
            repository: None,
            ended_at: None,
        })
    );
    assert_eq!(cursor("1:2", Sort::Took), None);
    assert_eq!(cursor("1:2:x", Sort::Took), None);
    assert_eq!(cursor("1792140000000", Sort::Started), None);
    assert_eq!(cursor("x:1", Sort::Started), None);
}

#[test]
fn failed_jobs_list_from_their_own_index() {
    // A partial index serves a query only when the query's WHERE
    // implies the index's.
    assert!(FAILED.starts_with("launch_state = 'failed' AND "));
}

#[test]
fn every_query_is_scoped_to_the_installation() {
    let lists = [Sort::Started, Sort::Repository]
        .into_iter()
        .flat_map(|sort| [Direction::Asc, Direction::Desc].map(|dir| list_query(None, sort, dir)));
    for query in lists.chain([
        list_query(Some(Group::Failed), Sort::Started, Direction::Desc),
        jobs_query(),
        counts_query(),
        jobs::activity_query(BUCKET_MS),
    ]) {
        assert!(query.contains("WHERE installation_id = $1 "), "{query}");
    }
    let failed = list_query(Some(Group::Failed), Sort::Started, Direction::Desc);
    assert!(failed.contains("WHERE j.installation_id = $1 "));
    assert!(SEARCH.contains("s.installation_id = $1 "));
    assert!(run_query().ends_with("WHERE id = $1 AND installation_id = $2"));
    assert!(list_query(None, Sort::Started, Direction::Desc).contains(" LIMIT $4"));
    assert!(jobs::activity_query(BUCKET_MS).contains("updated_at >= $2"));
    assert!(jobs::activity_query(BUCKET_MS).starts_with("SELECT (updated_at - $2) / "));
}

/// A job as the webhook and the launch leave it.
struct Seed {
    id: u64,
    status: &'static str,
    launch_state: Option<&'static str>,
    launch_retry: Option<&'static str>,
    queued_at: u64,
    updated_at: u64,
}

const INSTALLATION: u64 = 7;
const NOW: u64 = 1_792_140_000_000 + 7 * 60 * 1000;

/// Each job in a run of its own, `id * 10`, unless the test adds
/// more jobs to it.
fn seed(db: &Db, jobs: &[Seed]) {
    for job in jobs {
        db.run(
            "INSERT INTO runs (id, installation_id, repository, workflow_name, head_sha, \
             attempt, queued_at, created_at, updated_at) \
             VALUES ($1, $2, 'acme/web', 'CI', 'ab', 1, $3, $3, $3) ON CONFLICT (id) DO NOTHING",
            &params![job.id * 10, INSTALLATION, job.queued_at],
        );
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             launch_state, runner_name, instance_id, launch_error, launch_attempts, \
             launch_retry, next_launch_at, queued_at, updated_at) \
             VALUES ($1, $2, 'acme/web', $3, 'hatchrun', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            &params![
                job.id,
                INSTALLATION,
                job.id * 10,
                job.status,
                job.launch_state,
                job.launch_state.map(|_| format!("hatchrun-{}", job.id)),
                (job.launch_state == Some("launched")).then_some("i-0abc"),
                (job.launch_state == Some("failed")).then_some("no capacity"),
                u32::from(job.launch_state.is_some()),
                job.launch_retry,
                job.launch_retry.map(|_| job.updated_at + 60_000),
                job.queued_at,
                job.updated_at,
            ],
        );
    }
}

/// One job of each kind the dashboard tells apart, plus one of
/// another installation, at times that do not fall on a bucket edge.
fn jobs() -> Db {
    let db = Db::new();
    let ago = |minutes: u64| NOW - minutes * 60 * 1000 - 123;
    seed(
        &db,
        &[
            Seed {
                id: 1,
                status: "queued",
                launch_state: None,
                launch_retry: None,
                queued_at: ago(3),
                updated_at: ago(3),
            },
            Seed {
                id: 2,
                status: "queued",
                launch_state: Some("launching"),
                launch_retry: None,
                queued_at: ago(5),
                updated_at: ago(4),
            },
            Seed {
                id: 3,
                status: "in_progress",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(20),
                updated_at: ago(10),
            },
            Seed {
                id: 4,
                status: "completed",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(90),
                updated_at: ago(61),
            },
            Seed {
                id: 5,
                status: "queued",
                launch_state: Some("failed"),
                launch_retry: Some("backoff"),
                queued_at: ago(30),
                updated_at: ago(29),
            },
            Seed {
                id: 6,
                status: "queued",
                launch_state: Some("failed"),
                launch_retry: Some("setup"),
                queued_at: ago(40),
                updated_at: ago(38),
            },
            Seed {
                id: 7,
                status: "completed",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(3000),
                updated_at: ago(2990),
            },
        ],
    );
    db.run(
        "UPDATE jobs SET installation_id = $1 WHERE id = 7",
        &params![INSTALLATION + 1],
    );
    db.run(
        "UPDATE runs SET installation_id = $1 WHERE id = 70",
        &params![INSTALLATION + 1],
    );
    seed(
        &db,
        &[Seed {
            id: 8,
            status: "completed",
            launch_state: Some("launched"),
            launch_retry: None,
            queued_at: ago(3000),
            updated_at: ago(2990),
        }],
    );
    db
}

fn ids(runs: &[WorkflowRun]) -> Vec<u64> {
    runs.iter().map(|run| run.id).collect()
}

fn page(db: &Db, group: Option<Group>, before: Option<(u64, u64)>, limit: u32) -> Vec<WorkflowRun> {
    let before = before.map(|(queued_at, id)| Cursor {
        queued_at,
        id,
        repository: None,
        ended_at: None,
    });
    sorted(
        db,
        group,
        (Sort::Started, Direction::Desc),
        None,
        before.as_ref(),
        limit,
    )
}

/// A page in any order, of the runs matching `q` when set, after
/// the run `before`, bound as `runs` binds it.
fn sorted(
    db: &Db,
    group: Option<Group>,
    (sort, dir): (Sort, Direction),
    q: Option<&str>,
    before: Option<&Cursor>,
    limit: u32,
) -> Vec<WorkflowRun> {
    let fifth = match (before, sort) {
        (Some(c), Sort::Repository) => Param::from(c.repository.as_deref()),
        (Some(c), Sort::Took) => Param::from(c.ended_at),
        _ => Param::from(None::<u64>),
    };
    let mut values = params![
        INSTALLATION,
        before.map(|c| c.queued_at),
        before.map(|c| c.id),
        limit
    ];
    values.push(fifth);
    values.push(Param::from(q.map(contains)));
    db.all(&list_query(group, sort, dir), &values)
}

fn jobs_of(db: &Db, runs: &[u64]) -> Vec<Job> {
    db.all(
        &jobs_query(),
        &params![INSTALLATION, serde_json::to_string(runs).unwrap()],
    )
}

#[test]
fn the_list_reads_into_runs() {
    let db = jobs();
    let all = page(&db, None, None, PAGE);
    assert_eq!(ids(&all), [10, 20, 30, 50, 60, 40, 80]);
    assert_eq!(all[0].workflow_name.as_deref(), Some("CI"));
    let jobs = jobs_of(&db, &ids(&all));
    assert_eq!(jobs.len(), 7);
    let failed = jobs.iter().find(|job| job.id == 5).unwrap();
    assert_eq!(failed.launch_error.as_deref(), Some("no capacity"));
    assert_eq!(failed.launch_attempts, 1);
    assert!(failed.next_launch_at.is_some());

    let older = page(&db, None, Some((all[2].queued_at, all[2].id)), 2);
    assert_eq!(ids(&older), [50, 60]);

    let group = |g| ids(&page(&db, Some(g), None, PAGE));
    assert_eq!(group(Group::Active), [20, 30]);
    assert_eq!(group(Group::Waiting), [10, 50]);
    assert_eq!(group(Group::Failed), [60]);
}

/// Run 100: two jobs in attempt 1, one of which never launched; the
/// re-run of that one in attempt 2 is running.
fn rerun(db: &Db) {
    let ago = |minutes: u64| NOW - minutes * 60 * 1000;
    seed(
        db,
        &[
            Seed {
                id: 11,
                status: "completed",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(50),
                updated_at: ago(45),
            },
            Seed {
                id: 12,
                status: "completed",
                launch_state: Some("failed"),
                launch_retry: None,
                queued_at: ago(50),
                updated_at: ago(44),
            },
            Seed {
                id: 13,
                status: "in_progress",
                launch_state: Some("launched"),
                launch_retry: None,
                queued_at: ago(2),
                updated_at: ago(1),
            },
        ],
    );
    db.run(
        "UPDATE jobs SET run_id = 100, name = CASE id WHEN 11 THEN 'Save' ELSE 'Restore' END, \
         run_attempt = CASE id WHEN 13 THEN 2 ELSE 1 END WHERE id IN (11, 12, 13)",
        &[],
    );
    db.run(
        "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, queued_at, \
         created_at, updated_at) VALUES (100, $1, 'acme/web', 'ab', 2, $2, $2, $2)",
        &params![INSTALLATION, ago(2)],
    );
    db.run("DELETE FROM runs WHERE id IN (110, 120, 130)", &[]);
}

#[test]
fn a_rerun_replaces_only_the_jobs_it_runs_again() {
    let db = Db::new();
    rerun(&db);
    let jobs = jobs_of(&db, &[100]);
    assert_eq!(
        jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
        [11, 12, 13]
    );
    assert_eq!(current(&jobs), [true, false, true]);

    let runs = page(&db, None, None, PAGE);
    let listed = list("https://github.com", runs, &jobs);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
        [11, 13]
    );
    assert_eq!(listed[0].older_jobs, 1);
    assert_eq!(
        listed[0].github_url,
        "https://github.com/acme/web/actions/runs/100"
    );

    // The failed launch was replaced, so the run is active, not failed.
    assert_eq!(ids(&page(&db, Some(Group::Active), None, PAGE)), [100]);
    assert!(page(&db, Some(Group::Failed), None, PAGE).is_empty());
    let counts: Counts = db
        .first(&counts_query(), &params![INSTALLATION, NOW - DAY_MS])
        .unwrap();
    assert_eq!((counts.active_runs, counts.failed_runs_today), (1, 0));
    assert_eq!(counts.failed_today, 1);
}

#[test]
fn a_listed_run_reads_as_the_app_expects() {
    let db = Db::new();
    rerun(&db);
    let listed = list(
        "https://github.com",
        page(&db, None, None, PAGE),
        &jobs_of(&db, &[100]),
    );
    let value = serde_json::to_value(&listed[0]).unwrap();
    assert_eq!(value["id"], 100);
    assert_eq!(value["attempt"], 2);
    assert_eq!(value["olderJobs"], 1);
    assert_eq!(value["runNumber"], serde_json::Value::Null);
    assert_eq!(value["jobs"][1]["runAttempt"], 2);
    assert_eq!(value["jobs"][1]["name"], "Restore");
}

#[test]
fn tabs_start_from_the_job_indexes() {
    let db = jobs();
    let plan = |sql: &str| -> String {
        db.plan(
            sql,
            &params![
                INSTALLATION,
                None::<u64>,
                None::<u64>,
                PAGE,
                None::<&str>,
                None::<&str>
            ],
        )
    };
    let failed = plan(&list_query(
        Some(Group::Failed),
        Sort::Started,
        Direction::Desc,
    ));
    // Through jobs_failed, never every job, and the later attempts
    // through jobs_run.
    assert!(failed.contains("jobs_failed"), "{failed}");
    assert!(!failed.contains("Seq Scan"), "{failed}");
    assert!(failed.contains("jobs_run"), "{failed}");
    let all = plan(&list_query(None, Sort::Started, Direction::Desc));
    assert!(all.contains("runs_recent"), "{all}");
}

#[test]
fn every_sort_walks_an_index() {
    let db = jobs();
    let plan = |sql: &str, fifth: Param| -> String {
        let mut values = params![INSTALLATION, 1u64, 1u64, PAGE];
        values.push(fifth);
        values.push(Param::from("%web%"));
        db.plan(sql, &values)
    };
    for (sort, dir, index) in [
        (Sort::Started, Direction::Desc, "runs_recent"),
        (Sort::Started, Direction::Asc, "runs_recent"),
        (Sort::Repository, Direction::Asc, "runs_repository_az"),
        (Sort::Repository, Direction::Desc, "runs_repository "),
        (Sort::Took, Direction::Asc, "runs_took "),
        (Sort::Took, Direction::Desc, "runs_took_desc"),
    ] {
        let fifth = match sort {
            Sort::Started => Param::Null,
            Sort::Repository => Param::from("acme/web"),
            Sort::Took => Param::from(1u64),
        };
        let got = plan(&list_query(None, sort, dir), fifth);
        assert!(got.contains(index), "{sort:?} {dir:?}: {got}");
        assert!(!got.contains("Seq Scan"), "{sort:?} {dir:?}: {got}");
        assert!(!got.contains("Sort"), "{sort:?} {dir:?}: {got}");
        // The search reads jobs through an index too.
        assert!(got.contains("on jobs s"), "{sort:?} {dir:?}: {got}");
    }
}

mod paging;
