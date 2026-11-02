// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;

/// Twelve runs over three repositories, some at the same time, one
/// of them with a job on an instance and a branch; and a run of
/// another installation that matches every search. The first three
/// are running; the rest took 0 or 2 minutes, by GitHub's `completed_at`
/// where even and by when the job last changed where odd.
fn many() -> Db {
    let db = Db::new();
    let repos = ["acme/web", "acme/API", "acme/infra"];
    for i in 0..12u64 {
        let id = 1000 + i;
        db.run(
            "INSERT INTO runs (id, installation_id, repository, workflow_name, head_branch, \
             head_sha, attempt, queued_at, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, 'ab', 1, $6, $6, $6)",
            &params![
                id,
                INSTALLATION,
                repos[(i % 3) as usize],
                if i % 4 == 0 { "Release" } else { "CI" },
                if i == 7 { "fix_50%" } else { "main" },
                NOW - (i / 2) * 60_000
            ],
        );
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             launch_state, instance_id, queued_at, updated_at, name, completed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9::bigint + $11::bigint, $10, $12)",
            &params![
                id * 10,
                INSTALLATION,
                repos[(i % 3) as usize],
                id,
                if i % 2 == 0 {
                    "hatchrun-2cpu"
                } else {
                    "hatchrun-8cpu-arm64"
                },
                if i < 3 { "in_progress" } else { "completed" },
                "launched",
                (i == 5).then_some("i-0abc5"),
                NOW - (i / 2) * 60_000,
                if i == 9 { "Deploy (prod)" } else { "build" },
                if i % 2 == 0 {
                    1_000
                } else {
                    ((i + 1) % 4) * 60_000
                },
                (i >= 3 && i % 2 == 0).then(|| NOW - (i / 2) * 60_000 + (i % 4) * 60_000)
            ],
        );
    }
    db.run(
        "INSERT INTO runs (id, installation_id, repository, workflow_name, head_branch, \
         head_sha, attempt, queued_at, created_at, updated_at) \
         VALUES (9000, $1, 'acme/web', 'Release', 'fix_50%', 'ab', 1, $2, $2, $2)",
        &params![INSTALLATION + 1, NOW],
    );
    db
}

/// Every run of a list, read two at a time through the cursor.
fn walk(db: &Db, group: Option<Group>, order: (Sort, Direction), q: Option<&str>) -> Vec<u64> {
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let got = sorted(db, group, order, q, before.as_ref(), 2);
        let Some(last) = got.last() else {
            return seen;
        };
        // As the app writes it, read back as the Worker reads it.
        let third = match order.0 {
            Sort::Started => String::new(),
            Sort::Repository => format!(":{}", last.repository),
            Sort::Took => format!(
                ":{}",
                last.ended_at.map_or(String::new(), |at| at.to_string())
            ),
        };
        before = cursor(&format!("{}:{}{third}", last.queued_at, last.id), order.0);
        assert!(before.is_some(), "{order:?} after {}", last.id);
        seen.extend(ids(&got));
    }
}

#[test]
fn every_sort_pages_through_every_run_once() {
    let db = many();
    let all: Vec<WorkflowRun> = db.all(
        &format!("SELECT {RUN_COLUMNS} FROM runs WHERE installation_id = $1"),
        &params![INSTALLATION],
    );
    let key = |r: &WorkflowRun| {
        (
            r.repository.to_lowercase(),
            std::cmp::Reverse((r.queued_at, r.id)),
        )
    };
    let mut newest: Vec<&WorkflowRun> = all.iter().collect();
    newest.sort_by_key(|r| std::cmp::Reverse((r.queued_at, r.id)));
    let mut az: Vec<&WorkflowRun> = all.iter().collect();
    az.sort_by_key(|r| key(r));
    let mut za: Vec<&WorkflowRun> = all.iter().collect();
    za.sort_by_key(|r| {
        (
            std::cmp::Reverse(r.repository.to_lowercase()),
            std::cmp::Reverse((r.queued_at, r.id)),
        )
    });
    let list = |runs: &[&WorkflowRun]| runs.iter().map(|r| r.id).collect::<Vec<_>>();
    let oldest: Vec<u64> = list(&newest).into_iter().rev().collect();

    assert_eq!(
        walk(&db, None, (Sort::Started, Direction::Desc), None),
        list(&newest)
    );
    assert_eq!(
        walk(&db, None, (Sort::Started, Direction::Asc), None),
        oldest
    );
    assert_eq!(
        walk(&db, None, (Sort::Repository, Direction::Asc), None),
        list(&az)
    );
    assert_eq!(
        walk(&db, None, (Sort::Repository, Direction::Desc), None),
        list(&za)
    );
    // The runs still going, oldest first, either way; then the
    // finished ones by how long they took, ties by id. 1000 and 1001
    // were queued at the same time.
    let took = |id: u64| {
        let run = all.iter().find(|r| r.id == id).unwrap();
        run.ended_at.map(|at| at - run.queued_at)
    };
    assert_eq!(
        [1000, 1003, 1004, 1005, 1006].map(took),
        [None, Some(0), Some(0), Some(120_000), Some(120_000)]
    );
    assert_eq!(
        walk(&db, None, (Sort::Took, Direction::Asc), None),
        [
            1002, 1000, 1001, 1003, 1004, 1007, 1008, 1011, 1005, 1006, 1009, 1010
        ]
    );
    assert_eq!(
        walk(&db, None, (Sort::Took, Direction::Desc), None),
        [
            1002, 1000, 1001, 1005, 1006, 1009, 1010, 1003, 1004, 1007, 1008, 1011
        ]
    );
    // API sorts with the lower case names, before infra and web.
    assert_eq!(az[0].repository, "acme/API");
    assert_eq!(za[0].repository, "acme/web");
}

/// Through the webhook's own statements: a run of two jobs, then a
/// re-run of one of them.
#[test]
fn a_run_ends_when_its_current_jobs_do() {
    let db = Db::new();
    let ended = || -> Option<u64> {
        let run: WorkflowRun = db
            .first(&run_query(), &params![100u64, INSTALLATION])
            .unwrap();
        run.ended_at
    };
    let t0 = NOW - 600_000;
    let queue = |id: u64, name: &str, attempt: u64, at: u64| {
        db.run(
            crate::jobs::QUEUED,
            &params![
                id,
                INSTALLATION,
                "acme/web",
                100u64,
                "hatchrun",
                at,
                name,
                attempt
            ],
        );
    };
    let complete = |id: u64, at: u64, completed_at: Option<u64>| {
        db.run(
            crate::jobs::PROGRESS,
            &params![
                "completed",
                at,
                id,
                None::<&str>,
                "success",
                None::<u64>,
                completed_at
            ],
        );
    };
    db.run(
        crate::jobs::RUN,
        &params![
            100u64,
            INSTALLATION,
            "acme/web",
            "CI",
            "main",
            "ab",
            1u64,
            t0
        ],
    );
    queue(1, "build", 1, t0);
    queue(2, "test", 1, t0);
    assert_eq!(ended(), None);
    complete(1, t0 + 5_000, Some(t0 + 4_000));
    assert_eq!(ended(), None);
    // Launch bookkeeping on a live job leaves the run alone.
    db.run(
        "UPDATE jobs SET launch_state = 'launching', updated_at = $1 WHERE id = 2",
        &params![t0 + 6_000],
    );
    assert_eq!(ended(), None);
    // No completed_at from GitHub: when the job last changed.
    complete(2, t0 + 9_000, None);
    assert_eq!(ended(), Some(t0 + 9_000));

    // The re-run of test starts the run again, and its end is the
    // new attempt's, past the build that was not run again.
    let t1 = t0 + 100_000;
    db.run(
        crate::jobs::RUN_ATTEMPT,
        &params![100u64, 2u64, t1, "cd", "main"],
    );
    queue(3, "test", 2, t1);
    assert_eq!(ended(), None);
    complete(3, t1 + 8_000, Some(t1 + 7_000));
    assert_eq!(ended(), Some(t1 + 7_000));
    // A completed job that changes again keeps GitHub's time.
    db.run(
        "UPDATE jobs SET updated_at = $1 WHERE id = 3",
        &params![t1 + 50_000],
    );
    assert_eq!(ended(), Some(t1 + 7_000));

    // With the re-run's job gone, attempt 1's test is current again,
    // and no end falls before the run was queued.
    db.run("DELETE FROM jobs WHERE id = 3", &[]);
    assert_eq!(ended(), Some(t1));
}

#[test]
fn a_search_combines_with_a_tab_and_every_sort() {
    let db = many();
    let orders = [
        (Sort::Started, Direction::Desc),
        (Sort::Started, Direction::Asc),
        (Sort::Repository, Direction::Asc),
        (Sort::Repository, Direction::Desc),
        (Sort::Took, Direction::Asc),
        (Sort::Took, Direction::Desc),
    ];
    let set = |mut ids: Vec<u64>| {
        ids.sort_unstable();
        ids
    };
    for order in orders {
        let find = |q: &str| set(walk(&db, None, order, Some(q)));
        // Repository, case aside; workflow; branch; job name, label
        // and instance.
        assert_eq!(find("API"), [1001, 1004, 1007, 1010]);
        assert_eq!(find("api"), [1001, 1004, 1007, 1010]);
        assert_eq!(find("release"), [1000, 1004, 1008]);
        assert_eq!(find("fix_50"), [1007]);
        assert_eq!(find("prod"), [1009]);
        assert_eq!(find("8cpu"), set((1001..1012).step_by(2).collect()));
        assert_eq!(find("i-0abc"), [1005]);
        // The wildcards match only themselves.
        assert_eq!(find("50%"), [1007]);
        assert_eq!(find("fix%50"), Vec::<u64>::new());
        assert_eq!(find("a_i"), Vec::<u64>::new());
        assert_eq!(find("%"), [1007]);
        assert_eq!(find("_"), [1007]);
        // With a tab: running is the first three runs.
        assert_eq!(
            set(walk(&db, Some(Group::Active), order, Some("acme/web"))),
            [1000]
        );
        assert_eq!(
            set(walk(&db, Some(Group::Active), order, Some("8cpu"))),
            [1001]
        );
    }
    // The searched list keeps the order it was asked for.
    assert_eq!(
        walk(&db, None, (Sort::Started, Direction::Asc), Some("API")),
        [1010, 1007, 1004, 1001]
    );
    assert_eq!(
        walk(
            &db,
            None,
            (Sort::Repository, Direction::Desc),
            Some("release")
        ),
        [1000, 1008, 1004]
    );
}

#[test]
fn the_counts_read_into_counts() {
    let db = jobs();
    let counts: Counts = db
        .first(&counts_query(), &params![INSTALLATION, NOW - DAY_MS])
        .unwrap();
    assert_eq!(
        (
            counts.waiting,
            counts.active,
            counts.running,
            counts.completed_today,
            counts.failed_today
        ),
        (2, 2, 1, 1, 1)
    );
    assert_eq!(
        (
            counts.waiting_runs,
            counts.active_runs,
            counts.failed_runs_today
        ),
        (2, 2, 1)
    );
    let none: Counts = db
        .first(&counts_query(), &params![99u64, NOW - DAY_MS])
        .unwrap();
    assert_eq!(none.active, 0);
}

#[test]
fn the_activity_reads_into_buckets() {
    let db = jobs();
    let since = activity_since(NOW);
    let rows: Vec<Grouped> = db.all(
        &jobs::activity_query(BUCKET_MS),
        &params![INSTALLATION, since],
    );
    let got = activity(since, &rows);
    assert_eq!(got.finished.iter().sum::<u32>(), 1);
    assert_eq!(got.failed.iter().sum::<u32>(), 1);
    // 61 minutes back from 7 minutes into the last bucket is four
    // buckets back, 38 minutes back is three.
    assert_eq!(got.finished[BUCKETS - 5], 1);
    assert_eq!(got.failed[BUCKETS - 4], 1);
}

#[test]
fn the_settings_change_a_tenant() {
    let db = Db::new();
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
         VALUES ('t1', 1, 'acme', 0, 0)",
        &[],
    );
    let changed = db.run(
        SET_SETTINGS,
        &params!["t1", 3u32, Some("eu-west-1,us-east-1"), NOW],
    );
    assert_eq!(changed, 1);
    let rows = db
        .rows(
            "SELECT max_runners, allowed_regions, updated_at FROM tenants",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0]["max_runners"], 3);
    assert_eq!(rows[0]["allowed_regions"], "eu-west-1,us-east-1");
    db.run(SET_SETTINGS, &params!["t1", 3u32, None::<&str>, NOW]);
}

#[test]
fn the_last_bucket_holds_now() {
    let now = 1_792_140_000_000 + 7 * 60 * 1000;
    let since = activity_since(now);
    assert_eq!(since % BUCKET_MS, 0);
    let last = since + (BUCKETS as u64 - 1) * BUCKET_MS;
    assert!((last..last + BUCKET_MS).contains(&now));
    assert_eq!(activity_since(0), 0);
}

#[test]
fn empty_buckets_are_zero() {
    let rows = [
        Grouped {
            bucket: 0,
            finished: 3,
            failed: 1,
        },
        Grouped {
            bucket: 95,
            finished: 2,
            failed: 0,
        },
        Grouped {
            bucket: 96,
            finished: 9,
            failed: 9,
        },
        Grouped {
            bucket: -1,
            finished: 9,
            failed: 9,
        },
    ];
    let got = activity(1000, &rows);
    assert_eq!(got.since, 1000);
    assert_eq!(got.bucket_ms, BUCKET_MS);
    assert_eq!(got.finished.len(), BUCKETS);
    assert_eq!(got.failed.len(), BUCKETS);
    assert_eq!((got.finished[0], got.failed[0]), (3, 1));
    assert_eq!(got.finished[95], 2);
    assert_eq!(got.finished.iter().sum::<u32>(), 5);
    assert_eq!(got.failed.iter().sum::<u32>(), 1);
}
