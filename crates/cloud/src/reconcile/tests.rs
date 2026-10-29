// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use serde_json::Value;

use super::*;
use crate::testdb::{Db, params};

const NOW: u64 = 1_800_000_000_000;

fn job(db: &Db, id: u64, installation_id: u64, status: &str, queued_at: u64) {
    db.run(
        "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
         queued_at, updated_at) VALUES ($1, $2, 'acme/app', $3, 'hatchrun', $4, $5, $5)",
        &params![id, installation_id, id * 10, status, queued_at],
    );
}

fn tenant(db: &Db, id: &str, installation_id: Option<u64>, region: Option<&str>) {
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
         launch_template_id, subnet_ids, created_at, updated_at) \
         VALUES ($1, $2, $1, $3, 'arn:aws:iam::1:role/r', $4, 'lt-1', 'subnet-1', $5, $5)",
        &params![
            id,
            installation_id.unwrap_or(0) + 1000,
            installation_id,
            region,
            NOW
        ],
    );
}

fn value(db: &Db, sql: &str, params: &[crate::testdb::Param]) -> Value {
    db.rows(sql, params).unwrap()[0]
        .values()
        .next()
        .unwrap()
        .clone()
}

#[test]
fn a_step_gives_back_what_it_did_not_send() {
    // Two calls reserved three each and sent one each.
    assert_eq!(settled(494, 500, 2), 498);
    // A refused token took all three of one call.
    assert_eq!(settled(494, 500, 4), 496);
    assert_eq!(settled(500, 500, 0), 500);
}

#[test]
fn the_lease_is_taken_once_and_given_back() {
    let db = Db::new();
    let take = params![NOW, NOW + LEASE];
    assert_eq!(db.run(TAKE_LEASE, &take), 1);
    assert_eq!(db.run(TAKE_LEASE, &params![NOW + 1, NOW + 1 + LEASE]), 0);
    let history = r#"[{"at":1,"errors":0,"more":false}]"#;
    assert_eq!(
        db.run(RECORD, &params![NOW, NOW + 5, r#"{"tenants":1}"#, history]),
        1
    );
    let row = &db
        .rows(
            "SELECT lease_until, started_at, finished_at, summary, history FROM reconciler",
            &[],
        )
        .unwrap()[0];
    assert_eq!(row["lease_until"], 0);
    assert_eq!(row["started_at"], NOW);
    assert_eq!(row["finished_at"], NOW + 5);
    assert_eq!(row["summary"], r#"{"tenants":1}"#);
    assert_eq!(row["history"], history);
    assert_eq!(db.run(TAKE_LEASE, &take), 1);
    // A pass that could not read the history keeps the one there.
    db.run(RECORD, &params![NOW, NOW + 6, "{}", None::<&str>]);
    let kept: Option<String> = db.column(HISTORY, &[], "history");
    assert_eq!(kept.as_deref(), Some(history));
}

#[test]
fn the_delivery_mark_only_moves_forward() {
    let db = Db::new();
    assert_eq!(
        db.column::<Option<u64>>(DELIVERIES_UNTIL, &[], "deliveries_until"),
        Some(None)
    );
    assert_eq!(db.run(MOVE_DELIVERIES_UNTIL, &params![NOW]), 1);
    db.run(MOVE_DELIVERIES_UNTIL, &params![NOW - 10]);
    assert_eq!(
        db.column::<Option<u64>>(DELIVERIES_UNTIL, &[], "deliveries_until"),
        Some(Some(NOW))
    );
}

#[test]
fn a_redelivery_is_claimed_once() {
    let db = Db::new();
    let claim = params!["g-1", 77u64, "workflow_job", NOW];
    assert_eq!(db.run(CLAIM_REDELIVERY, &claim), 1);
    assert_eq!(db.run(CLAIM_REDELIVERY, &claim), 0);
    let guids = serde_json::to_string(&["g-1", "g-2"]).unwrap();
    let remembered: Vec<Guid> = db.all(REMEMBERED, &params![guids]);
    assert_eq!(remembered.len(), 1);
    assert_eq!(remembered[0].guid, "g-1");
    let empty: Vec<Guid> = db.all(REMEMBERED, &params!["[]"]);
    assert!(empty.is_empty());
}

#[test]
fn runs_without_details_are_fetched_after_a_minute() {
    let db = Db::new();
    for (id, created_at) in [
        (1u64, NOW - 2 * DETAILS_AFTER),
        (2, NOW),
        (3, NOW - DETAILS_AFTER - 1),
    ] {
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
             queued_at, created_at, updated_at) VALUES ($1, 9, 'acme/app', 'a', 1, $2, $2, $2)",
            &params![id, created_at],
        );
    }
    let due = |db: &Db| -> Vec<u64> {
        db.all::<RunWithoutDetails>(
            jobs::RUNS_WITHOUT_DETAILS,
            &params![NOW - DETAILS_AFTER, DETAILS_PER_PASS],
        )
        .into_iter()
        .map(|run| run.id)
        .collect()
    };
    assert_eq!(due(&db), [1, 3]);
    db.run(
        jobs::DETAILS,
        &params![
            1u64,
            None::<u64>,
            None::<&str>,
            None::<&str>,
            None::<&str>,
            None::<&str>,
            NOW
        ],
    );
    assert_eq!(due(&db), [3]);
    // Tried and failed: it waits its turn behind the others.
    assert_eq!(db.run(jobs::DETAILS_TRIED, &params![NOW, "[3]"]), 1);
    assert_eq!(due(&db), Vec::<u64>::new());
    let later: Vec<u64> = db
        .all::<RunWithoutDetails>(
            jobs::RUNS_WITHOUT_DETAILS,
            &params![NOW + DETAILS_AFTER + 1, DETAILS_PER_PASS],
        )
        .into_iter()
        .map(|run| run.id)
        .collect();
    assert_eq!(later, [2, 3]);
}

#[test]
fn quiet_open_jobs_are_checked_and_recorded() {
    let db = Db::new();
    job(&db, 1, 9, "queued", NOW - 3000);
    job(&db, 2, 9, "in_progress", NOW - 2000);
    job(&db, 3, 9, "completed", NOW - 4000);
    job(&db, 4, 9, "queued", NOW);
    let open: Vec<OpenJob> = db.all(jobs::OPEN_JOBS, &params![NOW - 1000, SYNC_PER_PASS]);
    let ids: Vec<u64> = open.iter().map(|job| job.id).collect();
    assert_eq!(ids, [1, 2]);
    assert_eq!(open[0].installation_id, 9);
    assert_eq!(open[0].repository, "acme/app");
    assert_eq!(open[0].status, "queued");

    // As `checked`: completed by runner r-1.
    let done = params![
        1u64,
        "completed",
        Some("r-1"),
        NOW,
        "queued",
        Some("success"),
        Some(NOW - 2500),
        Some(NOW - 100)
    ];
    assert_eq!(db.run(jobs::RECORD_CHECK, &done), 1);
    // The row moved on, so the same update matches nothing.
    assert_eq!(db.run(jobs::RECORD_CHECK, &done), 0);
    // Only asked, the status unchanged and no runner.
    let asked = params![
        2u64,
        "in_progress",
        None::<&str>,
        NOW,
        "in_progress",
        None::<&str>,
        None::<u64>,
        None::<u64>
    ];
    assert_eq!(db.run(jobs::RECORD_CHECK, &asked), 1);
    let rows = db
        .rows(
            "SELECT id, status, ran_on, checked_at, updated_at FROM jobs \
             WHERE id IN (1, 2) ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["ran_on"], "r-1");
    let ended = db
        .rows(
            "SELECT conclusion, completed_at FROM jobs WHERE id = 1",
            &[],
        )
        .unwrap();
    assert_eq!(ended[0]["conclusion"], "success");
    assert_eq!(ended[0]["completed_at"], NOW - 100);
    assert_eq!(rows[0]["updated_at"], NOW);
    assert_eq!(rows[1]["ran_on"], Value::Null);
    assert_eq!(rows[1]["checked_at"], NOW);
    assert_eq!(rows[1]["updated_at"], NOW - 2000);
    let open: Vec<OpenJob> = db.all(jobs::OPEN_JOBS, &params![NOW - 1000, SYNC_PER_PASS]);
    assert!(open.is_empty());
}

#[test]
fn verified_tenants_are_reconciled_oldest_first() {
    let db = Db::new();
    tenant(&db, "a", Some(1), Some("eu-west-1"));
    tenant(&db, "b", Some(2), Some("eu-west-1"));
    tenant(&db, "c", Some(3), None);
    db.run(MARK_RECONCILED, &params!["a", NOW]);
    db.run(MARK_SWEPT, &params!["b", NOW - 1]);
    db.run(MARK_STACK_CHECKED, &params!["b", NOW - 2]);
    let tenants: Vec<Tenant> = db.all(&tenants_query(), &params![TENANTS_PER_PASS]);
    let ids: Vec<&str> = tenants.iter().map(|tenant| tenant.id.as_str()).collect();
    assert_eq!(ids, ["b", "a"]);
    let b = &tenants[0];
    assert_eq!(b.installation_id, Some(2));
    assert_eq!(b.role_arn, "arn:aws:iam::1:role/r");
    assert_eq!(b.region, "eu-west-1");
    assert_eq!(b.launch_template_id, "lt-1");
    assert_eq!(b.max_runners, 10);
    assert_eq!(b.versions_swept_at, Some(NOW - 1));
    assert_eq!(b.stack_checked_at, Some(NOW - 2));
    assert_eq!(tenants[1].versions_swept_at, None);
}

#[test]
fn a_tenants_jobs_include_the_finished_ones_its_instances_name() {
    let db = Db::new();
    job(&db, 1, 9, "queued", NOW - 30);
    job(&db, 2, 9, "completed", NOW - 20);
    job(&db, 3, 9, "completed", NOW - 10);
    job(&db, 4, 9, "completed", NOW - 5);
    job(&db, 5, 8, "queued", NOW);
    db.run(
        "UPDATE jobs SET ran_on = 'r-3', launch_state = 'launched', runner_name = 'r-3', \
         launch_retry = 'backoff', next_launch_at = $1 WHERE id = 3",
        &params![NOW + 60_000],
    );
    let ids = serde_json::to_string(&[2u64]).unwrap();
    let runners = serde_json::to_string(&["r-3"]).unwrap();
    let mut rows: Vec<JobRow> = db.all(jobs::TENANT_JOBS, &params![9u64, ids, runners]);
    rows.sort_by_key(|row| row.id);
    let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, [1, 2, 3]);
    let three = &rows[2];
    assert_eq!(three.label, "hatchrun");
    assert_eq!(three.status, "completed");
    assert_eq!(three.launch_state.as_deref(), Some("launched"));
    assert_eq!(three.runner_name.as_deref(), Some("r-3"));
    assert_eq!(three.ran_on.as_deref(), Some("r-3"));
    assert_eq!(three.queued_at, NOW - 10);
    assert_eq!(three.updated_at, NOW - 10);
    assert_eq!(three.launch_retry.as_deref(), Some("backoff"));
    assert_eq!(three.next_launch_at, Some(NOW + 60_000));

    // No instances: empty arrays.
    let rows: Vec<JobRow> = db.all(jobs::TENANT_JOBS, &params![9u64, "[]", "[]"]);
    assert_eq!(rows.len(), 1);
}

#[test]
fn job_updates_apply_only_while_the_row_matches() {
    let db = Db::new();
    for id in 1..=4 {
        job(&db, id, 9, "queued", NOW - 100);
    }
    db.run(
        "UPDATE jobs SET launch_state = 'launching', runner_name = 'r-1' WHERE id = 1",
        &[],
    );
    db.run(
        "UPDATE jobs SET launch_state = 'failed', launch_retry = 'setup', \
         next_launch_at = 5 WHERE id = 2",
        &[],
    );
    db.run(
        "UPDATE jobs SET launch_state = 'launched', runner_name = 'r-3' WHERE id = 3",
        &[],
    );
    db.run(
        "UPDATE jobs SET launch_state = 'launched', runner_name = 'r-4' WHERE id = 4",
        &[],
    );
    let adopt = params![1u64, "i-1", NOW];
    let reset = params![2u64, "failed", NOW, None::<&str>];
    let reset_named = params![4u64, "launched", NOW, Some("r-4")];
    let give_up = params![3u64, NOW, "runner r-3 waited 10 minutes", "r-3"];
    let changed = db
        .batch(&[
            (jobs::ADOPT, adopt.clone()),
            (jobs::RESET, reset.clone()),
            (jobs::RESET, reset_named.clone()),
            (jobs::GIVE_UP, give_up.clone()),
        ])
        .unwrap();
    assert_eq!(changed, [1, 1, 1, 1]);
    let rows = db
        .rows(
            "SELECT launch_state, runner_name, instance_id, launch_error, launch_retry, \
             next_launch_at, updated_at FROM jobs ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0]["launch_state"], "launched");
    assert_eq!(rows[0]["instance_id"], "i-1");
    assert_eq!(rows[1]["launch_state"], Value::Null);
    assert_eq!(rows[1]["launch_retry"], Value::Null);
    assert_eq!(rows[1]["next_launch_at"], Value::Null);
    assert_eq!(rows[2]["launch_state"], "failed");
    assert_eq!(rows[2]["launch_error"], "runner r-3 waited 10 minutes");
    assert_eq!(rows[3]["launch_state"], Value::Null);
    assert_eq!(rows[3]["runner_name"], Value::Null);
    assert!(rows.iter().all(|row| row["updated_at"] == NOW));

    // The rows moved on; the same statements change nothing.
    for (sql, params) in [
        (jobs::ADOPT, adopt),
        (jobs::RESET, reset),
        (jobs::RESET, reset_named),
        (jobs::GIVE_UP, give_up),
    ] {
        assert_eq!(db.run(sql, &params), 0, "{sql}");
    }
}

#[test]
fn queued_jobs_read_as_launch_takes_them() {
    let db = Db::new();
    job(&db, 1, 9, "queued", NOW);
    job(&db, 2, 9, "queued", NOW - 10);
    job(&db, 3, 9, "queued", NOW - 20);
    let ids = serde_json::to_string(&[1u64, 2]).unwrap();
    let rows: Vec<LaunchRow> = db.all(jobs::QUEUED_JOBS, &params![ids]);
    let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, [2, 1]);
    assert_eq!(rows[0].installation_id, 9);
    assert_eq!(rows[0].repository, "acme/app");
    assert_eq!(rows[0].run_id, 20);
    assert_eq!(rows[0].label, "hatchrun");
}

#[test]
fn the_catalog_picks_a_region_that_is_due() {
    let db = Db::new();
    tenant(&db, "a", Some(1), Some("eu-west-1"));
    tenant(&db, "b", Some(2), Some("us-east-1"));
    tenant(&db, "c", Some(3), None);
    let fresh = NOW - instance_types::FRESH_FOR;
    let retry = NOW - instance_types::RETRY_AFTER;
    let due = params![fresh, retry];
    db.run(
        "INSERT INTO instance_type_regions (region, refreshed_at, attempted_at) \
         VALUES ('eu-west-1', $1, $1)",
        &params![NOW],
    );
    let picked: Option<CatalogTenant> = db.first(&catalog_query(), &due);
    let picked = picked.unwrap();
    assert_eq!(picked.id, "b");
    assert_eq!(picked.region, "us-east-1");
    assert_eq!(picked.role_arn, "arn:aws:iam::1:role/r");
    db.run(
        "INSERT INTO instance_type_regions (region, attempted_at) VALUES ('us-east-1', $1)",
        &params![NOW],
    );
    assert!(db.first::<CatalogTenant>(&catalog_query(), &due).is_none());
}

#[test]
fn lost_launches_without_aws_are_found() {
    let db = Db::new();
    tenant(&db, "a", Some(1), Some("eu-west-1"));
    tenant(&db, "c", Some(3), None);
    job(&db, 10, 1, "queued", NOW - 100_000);
    job(&db, 11, 2, "queued", NOW - 100_000);
    job(&db, 12, 3, "queued", NOW - 200_000);
    job(&db, 13, 2, "queued", NOW);
    let rows: Vec<LaunchRow> = db.all(
        &jobs::unconnected_query(),
        &params![NOW - 60_000, UNCONNECTED_PER_PASS],
    );
    let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, [12, 11]);
}

#[test]
fn retention_deletes_old_rows() {
    let db = Db::new();
    db.run(
        "INSERT INTO users (id, login, avatar_url, created_at, updated_at) \
         VALUES (1, 'u', 'a', 0, 0)",
        &[],
    );
    for (key, expires_at) in [("old", NOW - 1), ("new", NOW + 1)] {
        db.run(
            "INSERT INTO sessions (key, user_id, github_token, created_at, expires_at) \
             VALUES ($1, 1, 't', 0, $2)",
            &params![key, expires_at],
        );
    }
    job(&db, 1, 9, "completed", NOW - 100);
    job(&db, 2, 9, "completed", NOW);
    db.run(
        "UPDATE jobs SET expires_at = CASE id WHEN 1 THEN $1::bigint ELSE $2::bigint END WHERE id IN (1, 2)",
        &params![NOW - 1, NOW + 1],
    );
    // Run 10 loses its only job; run 20 expired too but keeps one.
    for (id, expires_at) in [(10u64, NOW - 1), (20, NOW - 1), (30, NOW + 1)] {
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
             queued_at, created_at, updated_at, expires_at) \
             VALUES ($1, 9, 'acme/app', 'a', 1, 0, 0, 0, $2)",
            &params![id, expires_at],
        );
    }
    for (guid, at) in [("old", NOW - REDELIVERY_MEMORY - 1), ("new", NOW)] {
        db.run(CLAIM_REDELIVERY, &params![guid, 1u64, "e", at]);
    }
    for (subnet, until) in [("s-old", NOW - 1), ("s-new", NOW + 1)] {
        db.run(
            "INSERT INTO unavailable_offerings \
             (tenant_id, instance_type, subnet_id, spot, code, until) \
             VALUES ('a', 'm8g.large', $1, false, 'c', $2)",
            &params![subnet, until],
        );
    }
    for (id, at) in [("old", NOW - CACHE_UPLOAD_MEMORY - 1), ("new", NOW)] {
        db.run(
            "INSERT INTO cache_uploads (id, job_id, object_key, upload_id, created_at) \
             VALUES ($1, 1, 'k', 'u', $2)",
            &params![id, at],
        );
    }
    // As `retention` binds them.
    let deleted = db
        .batch(&[
            (DELETE_SESSIONS, params![NOW, DELETE_BATCH]),
            (jobs::DELETE_JOBS, params![NOW, DELETE_BATCH]),
            (
                DELETE_REDELIVERIES,
                params![NOW - REDELIVERY_MEMORY, DELETE_BATCH],
            ),
            (DELETE_OFFERINGS, params![NOW]),
            (
                jobs::DELETE_UPLOADS,
                params![NOW - CACHE_UPLOAD_MEMORY, DELETE_BATCH],
            ),
            (jobs::DELETE_RUNS, params![NOW, DELETE_BATCH]),
        ])
        .unwrap();
    assert_eq!(deleted, [1, 1, 1, 1, 1, 1]);
    assert_eq!(
        value(
            &db,
            "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM runs",
            &[]
        ),
        "20,30"
    );
    for (table, key) in [
        ("sessions", "key"),
        ("redeliveries", "guid"),
        ("unavailable_offerings", "subnet_id"),
        ("cache_uploads", "id"),
    ] {
        let left = value(
            &db,
            &format!("SELECT string_agg({key}::text, ',') AS kept FROM {table}"),
            &[],
        );
        assert!(left == "new" || left == "s-new", "{table} kept {left}");
    }
    assert_eq!(
        value(
            &db,
            "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
            &[]
        ),
        "2"
    );
}

#[test]
fn retention_deletes_a_batch_at_most() {
    let db = Db::new();
    for id in 0..3u64 {
        job(&db, id + 1, 9, "completed", id);
    }
    db.run(
        "UPDATE jobs SET expires_at = queued_at WHERE expires_at IS NULL",
        &[],
    );
    assert_eq!(db.run(jobs::DELETE_JOBS, &params![NOW, 2u32]), 2);
    assert_eq!(
        value(
            &db,
            "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
            &[]
        ),
        "3"
    );
}

#[test]
fn jobs_expire_after_their_tenants_retention() {
    const DAY: u64 = 24 * 60 * 60 * 1000;
    let db = Db::new();
    // Installation 1 keeps 90 days, 2 the default 30, 3 has no
    // tenant.
    tenant(&db, "a", Some(1), Some("eu-west-1"));
    tenant(&db, "b", Some(2), Some("eu-west-1"));
    db.run("UPDATE tenants SET retention_days = 90 WHERE id = 'a'", &[]);
    for installation in 1..=3u64 {
        // As the webhook records a job of run `installation`.
        db.batch(&[
            (
                crate::jobs::RUN,
                params![
                    installation,
                    installation,
                    "acme/app",
                    None::<&str>,
                    None::<&str>,
                    "a",
                    1u64,
                    NOW
                ],
            ),
            (
                crate::jobs::QUEUED,
                params![
                    installation,
                    installation,
                    "acme/app",
                    installation,
                    "hatchrun",
                    NOW,
                    "build",
                    1u64
                ],
            ),
        ])
        .unwrap();
    }
    let expiry = |table: &str| {
        value(
            &db,
            &format!("SELECT string_agg(expires_at::text, ',' ORDER BY id) AS expiry FROM {table}"),
            &[],
        )
    };
    let want = format!("{},{},{}", NOW + 90 * DAY, NOW + 30 * DAY, NOW + 30 * DAY);
    assert_eq!(expiry("jobs"), want);
    assert_eq!(expiry("runs"), want);

    let retention = |now: u64| {
        db.batch(&[
            (jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
            (jobs::DELETE_RUNS, params![now, DELETE_BATCH]),
        ])
        .unwrap()
    };
    assert_eq!(retention(NOW + 30 * DAY), [0, 0]);
    assert_eq!(retention(NOW + 30 * DAY + 1), [2, 2]);
    assert_eq!(
        value(
            &db,
            "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM jobs",
            &[]
        ),
        "1"
    );
    assert_eq!(
        value(
            &db,
            "SELECT string_agg(id::text, ',' ORDER BY id) AS ids FROM runs",
            &[]
        ),
        "1"
    );
    assert_eq!(retention(NOW + 90 * DAY), [0, 0]);
    assert_eq!(retention(NOW + 90 * DAY + 1), [1, 1]);
}

#[test]
fn rows_without_an_expiry_get_the_default() {
    let db = Db::new();
    // As the Worker from before migration 0019 recorded them: an
    // old job and run, and a recent job.
    job(&db, 1, 9, "completed", NOW - UNSET_RETENTION - 1);
    job(&db, 2, 9, "queued", NOW);
    db.run(
        "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, \
         queued_at, created_at, updated_at) VALUES (10, 9, 'acme/app', 'a', 1, $1, $1, $1)",
        &params![NOW - UNSET_RETENTION - 1],
    );
    // As `retention` binds them.
    let pass = |db: &Db| {
        db.batch(&[
            (jobs::BACKFILL_JOBS, params![UNSET_RETENTION, DELETE_BATCH]),
            (jobs::BACKFILL_RUNS, params![UNSET_RETENTION, DELETE_BATCH]),
            (jobs::DELETE_JOBS, params![NOW, DELETE_BATCH]),
            (jobs::DELETE_RUNS, params![NOW, DELETE_BATCH]),
        ])
        .unwrap()
    };
    assert_eq!(pass(&db), [2, 1, 1, 1]);
    let left = db.rows("SELECT id, expires_at FROM jobs", &[]).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["id"], 2);
    assert_eq!(left[0]["expires_at"], NOW + UNSET_RETENTION);
    assert_eq!(value(&db, "SELECT COUNT(*) AS n FROM runs", &[]), 0);
    // Filled in: the next pass has nothing to do.
    assert_eq!(pass(&db), [0, 0, 0, 0]);
}

#[test]
fn retention_days_stay_between_one_and_ninety() {
    let db = Db::new();
    tenant(&db, "a", Some(1), Some("eu-west-1"));
    for days in [0u32, 91] {
        assert!(
            db.try_run(
                "UPDATE tenants SET retention_days = $1 WHERE id = 'a'",
                &params![days]
            )
            .is_err(),
            "{days}"
        );
    }
    assert_eq!(
        db.run(
            "UPDATE tenants SET retention_days = $1 WHERE id = 'a'",
            &params![1u32]
        ),
        1
    );
}

#[test]
fn job_queries_read_through_indexes() {
    let db = Db::new();
    // On these empty tables the planner would read a whole index
    // in no useful order and sort; a large table reads the indexes
    // below in order.
    db.run("SET enable_bitmapscan = off", &[]);
    db.run("SET enable_mergejoin = off", &[]);
    db.run("SET enable_hashjoin = off", &[]);
    let plans = [
        (
            "OPEN_JOBS",
            db.plan(jobs::OPEN_JOBS, &params![NOW, SYNC_PER_PASS]),
            &["jobs_unchecked"][..],
        ),
        (
            "TENANT_JOBS",
            db.plan(jobs::TENANT_JOBS, &params![9u64, "[1]", r#"["r-1"]"#]),
            &["jobs_open", "jobs_pkey", "jobs_ran_on"][..],
        ),
        (
            "DELETE_JOBS",
            db.plan(jobs::DELETE_JOBS, &params![NOW, DELETE_BATCH]),
            &["jobs_expiry"][..],
        ),
        (
            "DELETE_RUNS",
            db.plan(jobs::DELETE_RUNS, &params![NOW, DELETE_BATCH]),
            &["runs_expiry", "jobs_run"][..],
        ),
        (
            "BACKFILL_JOBS",
            db.plan(jobs::BACKFILL_JOBS, &params![UNSET_RETENTION, DELETE_BATCH]),
            &["jobs_expiry", "(expires_at IS NULL)"][..],
        ),
        (
            "BACKFILL_RUNS",
            db.plan(jobs::BACKFILL_RUNS, &params![UNSET_RETENTION, DELETE_BATCH]),
            &["runs_expiry", "(expires_at IS NULL)"][..],
        ),
        (
            "DELETE_USAGE",
            db.plan(DELETE_USAGE, &params![NOW, DELETE_BATCH]),
            &["usage_events_done"][..],
        ),
    ];
    for (name, plan, indexes) in plans {
        assert!(!plan.contains("Seq Scan"), "{name}\n{plan}");
        assert!(!plan.contains("Sort"), "{name}\n{plan}");
        for index in indexes {
            assert!(plan.contains(index), "{name} without {index}\n{plan}");
        }
    }
}
