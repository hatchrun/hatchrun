// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Times the Worker's Postgres statements at production scale and
//! counts the round trips of its operations.
//!
//! ```text
//! HATCHRUN_BENCH=1 DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres \
//!   cargo test -p hatchrun-cloud --release bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The test seeds a schema of its own: `BENCH_RUNS` runs (200000) of
//! the main tenant with three jobs each, and a tenth of that spread
//! over 49 other tenants. Each statement then runs `BENCH_WARMUP` (3)
//! times untimed and `BENCH_ITERS` (20) times timed, and once more
//! under `EXPLAIN (ANALYZE, BUFFERS)`. Statements that change rows run
//! in a transaction that is rolled back, so every iteration sees the
//! same rows. Last, whole operations run through a proxy that holds
//! every chunk for half of `BENCH_RTT_MS` (20) each way, and on the
//! direct link, `BENCH_RTT_RUNS` (10) times each. The time the proxy
//! adds over the time `SELECT 1` takes through it is the count of round
//! trips.
//!
//! The tables go to stdout and `target/bench/report.md`, the plans to
//! `target/bench/plans/`. Without `HATCHRUN_BENCH=1` the test returns
//! at once, since CI runs the ignored tests with no Postgres.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use futures_util::stream;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::db::{Db, Param, params};
use crate::jobs::{self, Direction, Group, RunsPage, Sort};
use crate::webhook::{Outcome, Placement, QueuedJob};
use crate::{alerts, app, labels, launch, profiles, reconcile, testdb, usage};

/// The main tenant's installation, id, organization and owner.
const MAIN: u64 = 42;
const TENANT: &str = "acmemain00000042";
const ORG: &str = "acme";
const USER: u64 = 1;
/// The owner's session, by key.
const SESSION_KEY: &str = "benchsessionkey";
/// A profile of the main tenant's own, so the webhook looks it up.
const PROFILE: &str = "hatchrun-gpu-build";
/// Run ids of the main tenant and of the others. A job's id is its
/// run's times four plus its place in the run.
const RUN_BASE: u64 = 10_000_000;
const OTHER_RUN_BASE: u64 = 50_000_000;
/// Jobs of installations without a verified tenant.
const UNCONNECTED_BASE: u64 = 700_000_000;
/// Rows the operations create, one range of 100000 per operation.
const FRESH_BASE: u64 = 800_000_000;
/// The other tenants' installations are this plus 1 to 49.
const OTHER_BASE: u64 = 1000;
const OTHERS: u64 = 49;
/// The newest runs of the main tenant, whose jobs are still open.
const OPEN_RUNS: u64 = 200;
const SECOND: u64 = 1000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const RETENTION: u64 = 30 * DAY;
/// As the dashboard, the reconciler and the alerts use them.
const PAGE: u32 = 25;
const BUCKET_MS: u64 = 15 * MINUTE;
const DELETE_BATCH: u32 = 1000;
const LISTED: u32 = 10;
const SYNC_PER_PASS: u32 = 30;
const DETAILS_PER_PASS: u32 = 20;
const TENANTS_PER_PASS: u32 = 50;
const TENANTS_AT_ONCE: usize = 4;
const UNCONNECTED_PER_PASS: u32 = 50;
/// How deep the deep cursor of a list is, in runs.
const DEEP: u32 = 2000;

/// What the environment asks for.
struct Settings {
    runs: u64,
    iters: usize,
    warmup: usize,
    rtt: Duration,
    rtt_runs: usize,
    search: String,
}

fn var<T: FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

impl Settings {
    fn read() -> Self {
        Settings {
            runs: var("BENCH_RUNS", 200_000u64).max(OPEN_RUNS * 2),
            iters: var("BENCH_ITERS", 20usize).max(1),
            warmup: var("BENCH_WARMUP", 3usize),
            rtt: Duration::from_millis(var("BENCH_RTT_MS", 20u64)),
            rtt_runs: var("BENCH_RTT_RUNS", 10usize).max(1),
            search: var("BENCH_SEARCH", "web".to_owned()),
        }
    }
}

#[test]
#[ignore = "seeds a large schema; runs with HATCHRUN_BENCH=1"]
fn bench() {
    if std::env::var("HATCHRUN_BENCH").as_deref() != Ok("1") {
        eprintln!("bench: set HATCHRUN_BENCH=1 and DATABASE_URL to run it");
        return;
    }
    let settings = Settings::read();
    let td = testdb::Db::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |at| u64::try_from(at.as_millis()).unwrap_or(u64::MAX));
    let started = Instant::now();
    seed(&td, settings.runs, now);
    let seeded = started.elapsed();
    let fixture = Fixture::read(&td, settings.runs, now);

    let dir = out_dir();
    std::fs::create_dir_all(dir.join("plans")).expect("create target/bench/plans");
    let mut report = String::new();
    let _ = writeln!(
        report,
        "# Postgres bench\n\nSeeded in {:.1} s: {}\n\n{} iterations after {} warm-up, \
         round trip {} ms.\n",
        seeded.as_secs_f64(),
        table_sizes(&td),
        settings.iters,
        settings.warmup,
        settings.rtt.as_millis()
    );
    let measured = statements(&td, &settings, &fixture, &dir.join("plans"));
    report.push_str(&statement_table(&measured));
    let trips = round_trips(&td, &settings, &fixture);
    report.push('\n');
    report.push_str(&trip_table(&trips, settings.rtt));
    println!("\n{report}");
    std::fs::write(dir.join("report.md"), &report).expect("write target/bench/report.md");
    println!("bench: wrote {}", dir.join("report.md").display());
}

/// `target/bench`, or under `CARGO_TARGET_DIR` when set.
fn out_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
            PathBuf::from,
        )
        .join("bench")
}

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// Seeding.

/// Fills the schema at `runs` runs for the main tenant, as of `now`.
fn seed(td: &testdb::Db, runs: u64, now: u64) {
    let mut statements = vec![
        "SET synchronous_commit = off".to_owned(),
        "ALTER TABLE jobs DISABLE TRIGGER jobs_end".to_owned(),
    ];
    statements.extend(seed_people(now));
    statements.push(runs_sql(
        RUN_BASE,
        runs,
        &MAIN.to_string(),
        OPEN_RUNS,
        20,
        now,
    ));
    statements.push(runs_sql(
        OTHER_RUN_BASE,
        (runs / 10).max(OTHERS),
        &format!("{OTHER_BASE} + 1 + i % {OTHERS}"),
        0,
        0,
        now,
    ));
    statements.push(jobs_sql(RUN_BASE + runs - 1, now));
    statements.extend(seed_jobs_after(now));
    statements.extend(seed_rest(now));
    statements.push("ALTER TABLE jobs ENABLE TRIGGER jobs_end".to_owned());
    // The timed statements commit as the Worker's do, waiting for the
    // WAL flush.
    statements.push("RESET synchronous_commit".to_owned());
    statements.push(
        "VACUUM (ANALYZE) jobs, runs, usage_events, tenants, subscriptions, users, members, \
         sessions, runner_profiles, images, instance_type_regions, cache_uploads, \
         failed_launches, rate_limits, redeliveries, unavailable_offerings, alerts, reconciler"
            .to_owned(),
    );
    for sql in &statements {
        let started = Instant::now();
        td.run(sql, &[]);
        let first = sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
        println!("seed: {:>7.0} ms  {first}", ms(started.elapsed()));
    }
}

/// Tenants, their owners and sessions, and the small tables.
fn seed_people(now: u64) -> Vec<String> {
    let since = now - 400 * DAY;
    vec![
        format!(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             launch_template_id, subnet_ids, max_runners, created_at, updated_at, beta, \
             reconciled_at, fleet, template_version, stack_checked_at, versions_swept_at, \
             cache_bucket, retention_days) VALUES ('{TENANT}', 8000042, 'Acme', {MAIN}, \
             'arn:aws:iam::123456789012:role/hatchrun', 'eu-west-1', 'lt-0123456789abcdef0', \
             'subnet-0a,subnet-0b,subnet-0c', 50, {since}, {now}, false, {now} - 60000, true, \
             5, {now} - 3600000, {now} - 3600000, 'hatchrun-cache-acme', 30)"
        ),
        // 1 to 39 are verified, 1 to 10 in the beta; 11 to 20 pay.
        format!(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             launch_template_id, subnet_ids, max_runners, created_at, updated_at, beta, \
             reconciled_at, retention_days) \
             SELECT 'tenant' || lpad(n::text, 10, '0'), 9000000 + n, 'Org-' || n, \
             {OTHER_BASE} + n, \
             CASE WHEN n <= 39 THEN 'arn:aws:iam::1000000000' || lpad(n::text, 2, '0') \
               || ':role/hatchrun' END, \
             CASE WHEN n <= 39 THEN (ARRAY['us-east-1', 'eu-west-1', 'ap-south-1'])[1 + n % 3] END, \
             CASE WHEN n <= 39 THEN 'lt-' || substr(md5(n::text), 1, 17) END, \
             CASE WHEN n <= 39 THEN 'subnet-' || n END, \
             10, {since}, {now}, n <= 10, {now} - n * 60000, 30 \
             FROM generate_series(1, {OTHERS}) AS n"
        ),
        format!(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             product, current_period_end, cancel_at_period_end, modified_at, updated_at) \
             SELECT 'sub_' || id, id, 'active', 'cus_' || id, 'prod_hatchrun', 'runners', \
             '2026-12-01T00:00:00Z', false, '2026-10-01T00:00:00Z', {now} FROM tenants \
             WHERE id = '{TENANT}' OR org_id - 9000000 BETWEEN 11 AND 20"
        ),
        format!(
            "INSERT INTO users (id, login, name, avatar_url, created_at, updated_at) \
             SELECT n, 'user' || n, 'User ' || n, \
             'https://avatars.githubusercontent.com/u/' || n, {since}, {now} \
             FROM generate_series(1, 1000) AS n"
        ),
        format!(
            "INSERT INTO members (tenant_id, user_id, created_at) \
             SELECT id, CASE WHEN id = '{TENANT}' THEN {USER} ELSE org_id - 9000000 + 100 END, \
             {since} FROM tenants"
        ),
        format!(
            "INSERT INTO sessions (key, user_id, github_token, created_at, expires_at) \
             SELECT md5('session' || n), 1 + n % 1000, 'sealed', \
             {now} - n % 14 * {DAY} - n, {now} - n % 14 * {DAY} - n + 7 * {DAY} \
             FROM generate_series(1, 5000) AS n \
             UNION ALL SELECT '{SESSION_KEY}', {USER}, 'sealed', {now} - {HOUR}, {now} + 6 * {DAY}"
        ),
        format!(
            "INSERT INTO runner_profiles (tenant_id, name, arch, families, cpu_min, memory_min, \
             spot, image, created_at, updated_at) VALUES ('{TENANT}', '{PROFILE}', 'amd64', \
             'g6,g5', 8, 32, false, 'ubuntu-24.04', {since}, {since})"
        ),
        format!(
            "INSERT INTO images (region, arch, image_id, root_device, updated_at, version) \
             SELECT region, arch, 'ami-' || substr(md5(region || arch), 1, 17), '/dev/sda1', \
             {now}, '20261001.1200' FROM unnest(ARRAY['us-east-1', 'eu-west-1', 'ap-south-1']) \
             AS region CROSS JOIN unnest(ARRAY['amd64', 'arm64']) AS arch"
        ),
        format!(
            "INSERT INTO instance_type_regions (region, refreshed_at, attempted_at, types) \
             VALUES ('eu-west-1', {now} - {HOUR}, {now} - {HOUR}, 812), \
             ('us-east-1', {now} - 2 * {DAY}, {now} - 2 * {DAY}, 901)"
        ),
    ]
}

/// `count` runs from id `base` over the last 30 days and an hour, so
/// the oldest hour is past its retention. The newest `open` are still
/// running; the newest `fresh` and every 997th have no details yet.
/// Every 40th older one is on its second attempt.
fn runs_sql(base: u64, count: u64, installation: &str, open: u64, fresh: u64, now: u64) -> String {
    let span = RETENTION + HOUR;
    format!(
        "INSERT INTO runs (id, installation_id, repository, workflow_name, head_branch, \
         head_sha, attempt, queued_at, run_number, event, display_title, actor, path, \
         details_at, details_tried_at, created_at, updated_at, expires_at, cache_write, \
         cache_reads) \
         SELECT {base} + i, {installation}, repository, workflow, branch, \
         md5(i::text) || substr(md5((i + 1)::text), 1, 8), \
         CASE WHEN i % 40 = 7 AND age >= {open} THEN 2 ELSE 1 END, queued, \
         CASE WHEN detailed THEN i + 1 END, \
         CASE WHEN detailed THEN CASE WHEN branch = 'main' THEN 'push' ELSE 'pull_request' END END, \
         CASE WHEN detailed THEN 'Change ' || i END, \
         CASE WHEN detailed THEN 'dev-' || i % 60 END, \
         CASE WHEN detailed THEN '.github/workflows/' || lower(workflow) || '.yml' END, \
         CASE WHEN detailed THEN queued + 4000 END, \
         CASE WHEN NOT detailed AND age >= {fresh} THEN queued + 60000 END, \
         queued, queued, queued + {RETENTION}, \
         CASE WHEN i % 2 = 0 THEN 'acme/b/' || branch END, \
         CASE WHEN i % 2 = 0 THEN '[\"acme/b/' || branch || '\",\"acme/b/main\"]' END \
         FROM (SELECT *, NOT (age < {fresh} OR i % 997 = 0) AS detailed, \
           CASE r % 4 WHEN 0 THEN 'acme/Api-' WHEN 1 THEN 'acme/web-' \
             WHEN 2 THEN 'Acme/Infra_' ELSE 'acme/TOOLS-' END || r AS repository, \
           CASE WHEN i % 3 = 0 THEN 'main' ELSE 'feature/change-' || i % 997 END AS branch \
           FROM (SELECT i, {count} - 1 - i AS age, \
             {now} - ({count} - i) * {span} / {count} AS queued, \
             floor(40 * power((i * 2654435761 % 1000) / 1000.0, 2))::int AS r, \
             (ARRAY['CI', 'CI', 'CI', 'Release', 'Nightly', 'Deploy', 'Lint'])[1 + i * 31 % 7] \
               AS workflow \
             FROM generate_series(0, {count} - 1) AS i) AS numbered) AS described"
    )
}

/// Three jobs per run and a fourth for a run on its second attempt,
/// which re-runs the first. `last` is the main tenant's newest run.
/// Jobs of runs older than [`OPEN_RUNS`] completed; 1 in 100 of them
/// failed to launch. The open jobs count from the newest: the oldest
/// 35 run, 10 are launching or launched, 15 failed to launch and the
/// rest wait.
fn jobs_sql(last: u64, now: u64) -> String {
    format!(
        "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, queued_at, \
         updated_at, launch_state, runner_name, instance_id, launch_error, ran_on, \
         launch_attempts, launch_retry, next_launch_at, name, run_attempt, conclusion, \
         started_at, completed_at, expires_at) \
         SELECT id, installation_id, repository, run_id, label, status, queued_at, \
         CASE WHEN status = 'completed' THEN completed_at WHEN status = 'in_progress' THEN \
           started_at WHEN launch_state = 'failed' THEN queued_at + 60000 ELSE queued_at END, \
         launch_state, CASE WHEN launch_state IS NOT NULL THEN runner END, \
         CASE WHEN launch_state = 'launched' THEN 'i-0' || substr(md5(id || 'i'), 1, 16) END, \
         CASE WHEN launch_state = 'failed' THEN \
           'InsufficientInstanceCapacity: no m7i.xlarge capacity in eu-west-1a' END, \
         CASE WHEN started_at IS NOT NULL THEN runner END, \
         CASE WHEN launch_state IS NULL THEN 0 WHEN id % 25 = 0 THEN 2 ELSE 1 END, \
         retry, CASE WHEN retry = 'backoff' THEN {now} + (place - 540) * 10000 END, \
         name, run_attempt, \
         CASE WHEN status <> 'completed' THEN NULL WHEN failed THEN 'cancelled' \
           WHEN id % 100 < 88 THEN 'success' WHEN id % 100 < 96 THEN 'failure' \
           WHEN id % 100 < 98 THEN 'cancelled' WHEN id % 100 < 99 THEN 'skipped' \
           ELSE 'timed_out' END, \
         started_at, completed_at, queued_at + {RETENTION} \
         FROM (SELECT *, \
           CASE WHEN status <> 'completed' THEN NULL WHEN failed THEN queued_at + 600000 \
             ELSE started_at + 60000 + id * 13 % 1140000 END AS completed_at \
         FROM (SELECT *, \
           CASE WHEN age >= {OPEN_RUNS} THEN CASE WHEN failed THEN 'failed' ELSE 'launched' END \
             WHEN place >= 560 THEN 'launched' WHEN place >= 555 THEN 'launching' \
             WHEN place >= 540 THEN 'failed' END AS launch_state, \
           CASE WHEN status = 'in_progress' THEN queued_at + 30000 \
             WHEN status = 'completed' AND NOT failed THEN queued_at + 20000 + id % 40000 \
             END AS started_at \
         FROM (SELECT *, \
           CASE WHEN age >= {OPEN_RUNS} THEN 'completed' WHEN place >= 565 THEN 'in_progress' \
             ELSE 'queued' END AS status, \
           age >= {OPEN_RUNS} AND id % 100 = 37 AS failed, \
           CASE WHEN age < {OPEN_RUNS} AND place BETWEEN 540 AND 548 THEN 'backoff' \
             WHEN age < {OPEN_RUNS} AND place BETWEEN 549 AND 554 THEN 'setup' END AS retry \
         FROM (SELECT runs.id * 4 + k AS id, runs.installation_id, runs.repository, \
           runs.id AS run_id, age, age * 3 + k AS place, \
           (ARRAY['hatchrun-4cpu', 'hatchrun-8cpu-arm64', 'hatchrun-16cpu', \
             '{PROFILE}/disk=200'])[1 + (runs.id + k) % 4] AS label, \
           CASE k WHEN 1 THEN 'test (' || (ARRAY['ubuntu', 'arm64', 'windows'])[1 + runs.id % 3] \
             || ')' WHEN 2 THEN 'lint' ELSE 'build' END AS name, \
           CASE WHEN k = 3 THEN 2 ELSE 1 END AS run_attempt, \
           CASE WHEN runs.attempt = 2 AND k < 3 THEN runs.queued_at - 1200000 \
             ELSE runs.queued_at + k * 2000 END AS queued_at, \
           'hatchrun-' || (runs.id * 4 + k) || '-' || substr(md5((runs.id * 4 + k)::text), 1, 8) \
             AS runner \
         FROM (SELECT *, CASE WHEN installation_id = {MAIN} THEN {last} - id ELSE 1000000 END \
           AS age FROM runs) AS runs \
         CROSS JOIN generate_series(0, 3) AS k WHERE k < 3 OR runs.attempt = 2) AS placed) \
           AS settled) AS launched) AS ended"
    )
}

/// Lost launches, each run's end, and the minutes of the jobs that ran.
fn seed_jobs_after(now: u64) -> Vec<String> {
    vec![
        // Installation 1045's tenant is not verified; 9999 has none.
        format!(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, name, expires_at) \
             SELECT {UNCONNECTED_BASE} + n, \
             CASE WHEN n <= 20 THEN {OTHER_BASE} + 45 ELSE 9999 END, 'org/app', \
             {UNCONNECTED_BASE} + n, 'hatchrun-4cpu', 'queued', {now} - n * 60000, \
             {now} - n * 60000, 'build', {now} + {RETENTION} FROM generate_series(1, 25) AS n"
        ),
        // As runs_end works it out, for every run at once.
        "UPDATE runs SET ended_at = GREATEST(runs.queued_at, ends.ended) FROM \
         (SELECT run_id, MAX(COALESCE(completed_at, updated_at)) AS ended FROM \
           (SELECT run_id, status, completed_at, updated_at, \
             run_attempt = MAX(run_attempt) OVER (PARTITION BY run_id, name) AS current \
             FROM jobs) AS jobs \
           WHERE current GROUP BY run_id HAVING bool_and(status = 'completed')) AS ends \
         WHERE runs.id = ends.run_id"
            .to_owned(),
        // One in a thousand jobs has no minutes, and a few more in the last
        // day, for the sweep.
        format!(
            "INSERT INTO usage_events (job_id, tenant_id, minutes, ended_at, billed, sent_at) \
             SELECT jobs.id, tenants.id, \
             GREATEST(1, (jobs.completed_at - jobs.started_at + 59999) / 60000), \
             jobs.completed_at, billed, \
             CASE WHEN billed AND jobs.completed_at < {now} - 300000 \
               THEN jobs.completed_at + 120000 END \
             FROM jobs JOIN (SELECT id, installation_id, NOT beta AND EXISTS \
               (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id \
               AND subscriptions.product = 'runners') \
               AS billed FROM tenants) AS tenants \
             ON tenants.installation_id = jobs.installation_id \
             WHERE jobs.status = 'completed' AND jobs.ran_on IS NOT NULL \
             AND jobs.started_at IS NOT NULL AND jobs.id % 1000 <> 3 \
             AND NOT (jobs.completed_at >= {now} - {DAY} AND jobs.id % 199 = 5)"
        ),
        format!(
            "INSERT INTO failed_launches (message_id, job_id, installation_id, error, attempts, \
             failed_at) SELECT 'msg-' || id, id, installation_id, launch_error, launch_attempts, \
             LEAST(queued_at + 600000, {now}) FROM jobs WHERE launch_state = 'failed'"
        ),
    ]
}

/// Uploads, rate limits, redeliveries, offerings and alerts.
fn seed_rest(now: u64) -> Vec<String> {
    let first_job = RUN_BASE * 4;
    vec![
        format!(
            "INSERT INTO cache_uploads (id, job_id, object_key, upload_id, created_at) \
             SELECT 'upload-' || n, {first_job} + n * 4, 'acme/b/main/v1/key-' || n, \
             md5('upload' || n), {now} - n * 86400 FROM generate_series(0, 1999) AS n"
        ),
        format!(
            "INSERT INTO rate_limits (installation_id, remaining, limited, seen_at) \
             SELECT {OTHER_BASE} + n, n * 10, n % 7 = 0, {now} - n * 1800000 \
             FROM generate_series(1, {OTHERS}) AS n \
             UNION ALL SELECT {MAIN}, 50, false, {now} - 300000"
        ),
        format!(
            "INSERT INTO redeliveries (guid, delivery_id, event, redelivered_at) \
             SELECT md5('guid' || n), n, 'workflow_job', {now} - n * 144000 \
             FROM generate_series(1, 3000) AS n"
        ),
        format!(
            "INSERT INTO unavailable_offerings (tenant_id, instance_type, subnet_id, spot, code, \
             until) SELECT '{TENANT}', 'm7i.' || n || 'xlarge', 'subnet-0a', n % 2 = 0, \
             'InsufficientInstanceCapacity', {now} + (n - 50) * 60000 \
             FROM generate_series(1, 100) AS n"
        ),
        format!(
            "INSERT INTO alerts (key, state, detail, fired_at, resolved_at, sent, updated_at) \
             VALUES ('jobs_waiting', 'resolved', '', {now} - {DAY}, {now} - {HOUR}, 'resolved', \
             {now} - {HOUR}), ('failed_launches', 'firing', '3 failed launches', \
             {now} - 1800000, NULL, 'firing', {now} - 1800000)"
        ),
        format!(
            "UPDATE reconciler SET started_at = {now} - 62000, finished_at = {now} - 60000, \
             summary = '{{}}' WHERE id = 1"
        ),
    ]
}

fn table_sizes(td: &testdb::Db) -> String {
    let tables = [
        "runs",
        "jobs",
        "usage_events",
        "failed_launches",
        "cache_uploads",
        "sessions",
        "tenants",
    ];
    let counts = tables
        .iter()
        .map(|table| format!("(SELECT COUNT(*) FROM {table}) AS {table}"))
        .collect::<Vec<_>>()
        .join(", ");
    let row = td
        .rows(&format!("SELECT {counts}"), &[])
        .expect("count the rows")
        .remove(0);
    tables
        .iter()
        .map(|table| format!("{} {table}", row[*table]))
        .collect::<Vec<_>>()
        .join(", ")
}

// The rows the statements are run on.

/// Ids picked from the seeded rows.
struct Fixture {
    now: u64,
    /// Queued jobs no launch took, oldest first.
    waiting: Vec<u64>,
    /// A queued job whose instance is up.
    launched: u64,
    /// Running jobs: id, runner and start.
    running: Vec<(u64, String, u64)>,
    /// The newest completed jobs that ran: id and runner.
    finished: Vec<(u64, String)>,
    /// A completed job half way back.
    old_job: u64,
    /// A job that ran and completed in the last day without minutes:
    /// id, start and end.
    unrecorded: (u64, u64, u64),
    /// A run still going, and its attempt.
    open_run: (u64, u64),
    /// The newest 50 runs.
    page: Vec<u64>,
    /// The runs the reconciler fetches the details of next.
    no_details: Vec<u64>,
}

fn number(row: &Map<String, Value>, key: &str) -> u64 {
    row[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} in {row:?}"))
}

fn string(row: &Map<String, Value>, key: &str) -> String {
    row[key].as_str().unwrap_or_default().to_owned()
}

fn select(td: &testdb::Db, sql: &str) -> Vec<Map<String, Value>> {
    td.rows(sql, &[])
        .unwrap_or_else(|err| panic!("{err}\n{sql}"))
}

fn ids(td: &testdb::Db, sql: &str) -> Vec<u64> {
    select(td, sql)
        .iter()
        .map(|row| number(row, "id"))
        .collect()
}

impl Fixture {
    fn read(td: &testdb::Db, runs: u64, now: u64) -> Self {
        let main = format!("installation_id = {MAIN}");
        let running = select(
            td,
            &format!("SELECT id, runner_name, started_at FROM jobs WHERE {main} AND status = 'in_progress' ORDER BY id"),
        )
        .iter()
        .map(|row| (number(row, "id"), string(row, "runner_name"), number(row, "started_at")))
        .collect();
        let finished = select(
            td,
            &format!(
                "SELECT id, ran_on FROM jobs WHERE {main} AND status = 'completed' \
                 AND ran_on IS NOT NULL ORDER BY completed_at DESC LIMIT 5"
            ),
        )
        .iter()
        .map(|row| (number(row, "id"), string(row, "ran_on")))
        .collect();
        let unrecorded = select(
            td,
            &format!(
                "SELECT id, started_at, completed_at FROM jobs WHERE {main} \
                 AND status = 'completed' AND ran_on IS NOT NULL AND started_at IS NOT NULL \
                 AND completed_at >= {now} - {DAY} AND NOT EXISTS \
                 (SELECT 1 FROM usage_events WHERE job_id = jobs.id) \
                 ORDER BY completed_at DESC LIMIT 1"
            ),
        );
        let unrecorded = unrecorded.first().expect("a job without minutes");
        let open_run = select(
            td,
            &format!("SELECT id, attempt FROM runs WHERE {main} AND ended_at IS NULL LIMIT 1"),
        );
        Fixture {
            now,
            waiting: ids(
                td,
                &format!(
                    "SELECT id FROM jobs WHERE {main} AND status = 'queued' \
                     AND launch_state IS NULL ORDER BY queued_at LIMIT 10"
                ),
            ),
            launched: ids(
                td,
                &format!(
                    "SELECT id FROM jobs WHERE {main} AND status = 'queued' \
                     AND launch_state = 'launched' LIMIT 1"
                ),
            )[0],
            running,
            finished,
            old_job: (RUN_BASE + runs / 2) * 4 + 1,
            unrecorded: (
                number(unrecorded, "id"),
                number(unrecorded, "started_at"),
                number(unrecorded, "completed_at"),
            ),
            open_run: (number(&open_run[0], "id"), number(&open_run[0], "attempt")),
            page: ids(
                td,
                &format!(
                    "SELECT id FROM runs WHERE {main} ORDER BY queued_at DESC, id DESC LIMIT 50"
                ),
            ),
            no_details: ids(
                td,
                "SELECT id FROM runs WHERE details_at IS NULL \
                 ORDER BY COALESCE(details_tried_at, created_at) LIMIT 20",
            ),
        }
    }

    /// The job ids and runners of the main tenant's instances, as the
    /// reconciler passes them to `TENANT_JOBS`.
    fn instances(&self) -> (String, String) {
        let ids: Vec<u64> = self
            .running
            .iter()
            .map(|(id, _, _)| *id)
            .chain(self.finished.iter().map(|(id, _)| *id))
            .collect();
        let runners: Vec<&str> = self
            .running
            .iter()
            .map(|(_, runner, _)| runner.as_str())
            .chain(self.finished.iter().map(|(_, runner)| runner.as_str()))
            .collect();
        (json(&ids), json(&runners))
    }
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("JSON")
}

// Statements.

/// A statement with its values. One that writes runs in a transaction
/// that is rolled back, after `before`.
struct Case {
    name: String,
    sql: String,
    params: Vec<Param>,
    write: bool,
    before: Vec<(String, Vec<Param>)>,
}

fn read(name: &str, sql: impl Into<String>, params: Vec<Param>) -> Case {
    Case {
        name: name.to_owned(),
        sql: sql.into(),
        params,
        write: false,
        before: Vec::new(),
    }
}

fn write(name: &str, sql: impl Into<String>, params: Vec<Param>) -> Case {
    Case {
        write: true,
        ..read(name, sql, params)
    }
}

impl Case {
    fn after(mut self, sql: impl Into<String>, params: Vec<Param>) -> Self {
        self.before.push((sql.into(), params));
        self
    }
}

/// What a statement or operation measured.
struct Measured {
    name: String,
    p50: f64,
    p95: f64,
    rows: String,
    plan: Option<Plan>,
    error: Option<String>,
}

/// What a plan shows.
#[derive(Default)]
struct Plan {
    top: String,
    seq_scans: Vec<String>,
    sorts: Vec<String>,
    hit: u64,
    read: u64,
    server_ms: f64,
    /// Time spent compiling the plan with JIT, part of `server_ms`.
    jit_ms: f64,
}

fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort();
    let at = |percent: usize| samples[(samples.len() * percent).div_ceil(100).max(1) - 1];
    (ms(at(50)), ms(at(95)))
}

/// Every statement, timed and explained, in report order.
fn statements(td: &testdb::Db, s: &Settings, fx: &Fixture, plans: &Path) -> Vec<Measured> {
    let mut cases = webhook_cases(fx);
    cases.extend(launch_cases(fx));
    cases.extend(app_cases(fx));
    cases.extend(list_cases(td, &s.search));
    cases.extend(cache_cases(fx));
    cases.extend(reconcile_cases(fx));
    cases.extend(retention_cases(fx.now));
    cases.extend(usage_cases(fx));
    cases.extend(alert_cases(fx.now));

    let mut measured = vec![record_queued_op(td, s, fx.now), claim_op(td, s, fx.now)];
    for (i, case) in cases.iter().enumerate() {
        let started = Instant::now();
        let result = td.wait(time(td.db(), case, s.warmup, s.iters));
        let plan = td.wait(explain(td.db(), case));
        let (summary, plan_error) = match &plan {
            Ok(plan) => (Some(summarize(plan)), None),
            Err(err) => (None, Some(err.clone())),
        };
        let file = plans.join(format!("{:03}-{}.txt", i + 1, slug(&case.name)));
        let body = format!(
            "{}\n\n{}\n\nvalues: {:?}\n{}\n\n{}\n",
            case.name,
            case.sql,
            case.params,
            case.before
                .iter()
                .map(|(sql, params)| format!("after: {sql} {params:?}"))
                .collect::<Vec<_>>()
                .join("\n"),
            plan.as_deref().unwrap_or_else(|err| err.as_str())
        );
        std::fs::write(&file, body).expect("write a plan");
        let entry = match result {
            Ok((samples, rows)) => {
                let (p50, p95) = percentiles(samples);
                Measured {
                    name: case.name.clone(),
                    p50,
                    p95,
                    rows: rows.to_string(),
                    plan: summary,
                    error: plan_error,
                }
            }
            Err(err) => Measured {
                name: case.name.clone(),
                p50: 0.0,
                p95: 0.0,
                rows: "-".to_owned(),
                plan: summary,
                error: Some(err),
            },
        };
        println!(
            "bench: {:>4}/{} {:<48} p50 {:>9.2} ms  ({:.1} s)",
            i + 1,
            cases.len(),
            entry.name,
            entry.p50,
            started.elapsed().as_secs_f64()
        );
        measured.push(entry);
    }
    measured
}

fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Runs `case` `warmup` times, then `iters` times timed. Returns the
/// samples and the rows the last one read or changed.
async fn time(
    db: &Db,
    case: &Case,
    warmup: usize,
    iters: usize,
) -> Result<(Vec<Duration>, usize), String> {
    let mut samples = Vec::with_capacity(iters);
    let mut rows = 0;
    for i in 0..warmup + iters {
        let (elapsed, n) = once(db, case).await?;
        if i >= warmup {
            samples.push(elapsed);
        }
        rows = n;
    }
    Ok((samples, rows))
}

async fn once(db: &Db, case: &Case) -> Result<(Duration, usize), String> {
    if !case.write {
        let started = Instant::now();
        let rows = db.rows(&case.sql, &case.params).await.map_err(text)?;
        return Ok((started.elapsed(), rows.len()));
    }
    db.run("BEGIN", &[]).await.map_err(text)?;
    let result = async {
        for (sql, params) in &case.before {
            db.run(sql, params).await?;
        }
        let started = Instant::now();
        let changed = db.run(&case.sql, &case.params).await?;
        Ok::<_, worker::Error>((started.elapsed(), changed))
    }
    .await;
    let rollback = db.run("ROLLBACK", &[]).await;
    let (elapsed, changed) = result.map_err(text)?;
    rollback.map_err(text)?;
    Ok((elapsed, usize::try_from(changed).unwrap_or(usize::MAX)))
}

/// `case` under `EXPLAIN (ANALYZE, BUFFERS)`, rolled back.
async fn explain(db: &Db, case: &Case) -> Result<String, String> {
    let sql = format!("EXPLAIN (ANALYZE, BUFFERS) {}", case.sql);
    db.run("BEGIN", &[]).await.map_err(text)?;
    let result = async {
        for (sql, params) in &case.before {
            db.run(sql, params).await?;
        }
        db.rows(&sql, &case.params).await
    }
    .await;
    let rollback = db.run("ROLLBACK", &[]).await;
    let rows = result.map_err(text)?;
    rollback.map_err(text)?;
    Ok(rows
        .iter()
        .map(|row| row["QUERY PLAN"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// One node of a text plan.
struct Node {
    indent: usize,
    label: String,
    /// Actual rows per loop, and loops; no loops when it never ran.
    rows: f64,
    loops: f64,
    /// Rows its filter dropped, per loop.
    removed: f64,
}

impl Node {
    fn parse(indent: usize, body: &str) -> Self {
        let label = body.find("  (").map_or(body, |end| &body[..end]).to_owned();
        let actual = body.find("(actual ").map_or("", |at| &body[at..]);
        let value = |key: &str| {
            actual
                .split_once(key)
                .and_then(|(_, rest)| rest.split([' ', ')']).next()?.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        Node {
            indent,
            label,
            rows: value("rows="),
            loops: value("loops="),
            removed: 0.0,
        }
    }

    /// Rows it returned over every loop.
    fn output(&self) -> f64 {
        self.rows * self.loops
    }

    /// Rows it read over every loop, the ones its filter dropped too.
    fn read(&self) -> f64 {
        (self.rows + self.removed) * self.loops
    }
}

fn summarize(plan: &str) -> Plan {
    let mut summary = Plan::default();
    let mut nodes: Vec<Node> = Vec::new();
    let mut buffers = None;
    for (i, line) in plan.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Planning") {
            buffers = buffers.or(Some((0, 0)));
        }
        if trimmed.starts_with("Timing: Generation")
            && let Some((_, total)) = trimmed.rsplit_once("Total ")
        {
            summary.jit_ms = total.trim_end_matches(" ms").parse().unwrap_or(0.0);
        }
        if let Some(rest) = trimmed.strip_prefix("Execution Time: ") {
            summary.server_ms = rest.trim_end_matches(" ms").parse().unwrap_or(0.0);
        }
        if i == 0 || trimmed.starts_with("->") {
            let body = trimmed.trim_start_matches("->").trim_start();
            nodes.push(Node::parse(line.len() - trimmed.len(), body));
        } else if let Some(removed) = trimmed.strip_prefix("Rows Removed by Filter: ") {
            if let Some(node) = nodes.last_mut() {
                node.removed += removed.trim().parse::<f64>().unwrap_or(0.0);
            }
        } else if buffers.is_none() && trimmed.starts_with("Buffers: shared") {
            let count = |key: &str| {
                trimmed
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix(key)?.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            buffers = Some((count("hit="), count("read=")));
        }
    }
    (summary.hit, summary.read) = buffers.unwrap_or_default();
    summary.top = nodes
        .iter()
        .take(2)
        .map(|node| node.label.as_str())
        .collect::<Vec<_>>()
        .join(" > ");
    for (i, node) in nodes.iter().enumerate() {
        let scanned = node
            .label
            .strip_prefix("Seq Scan on ")
            .or_else(|| node.label.strip_prefix("Parallel Seq Scan on "))
            .and_then(|rest| rest.split_whitespace().next());
        if let Some(table @ ("jobs" | "runs" | "usage_events")) = scanned
            && node.loops > 0.0
        {
            summary
                .seq_scans
                .push(format!("{table} {:.0}", node.read()));
        }
        if node.label.starts_with("Sort") || node.label.starts_with("Incremental Sort") {
            let input = nodes
                .get(i + 1)
                .filter(|child| child.indent > node.indent)
                .map_or(node.output(), Node::output);
            if input > 1000.0 {
                summary.sorts.push(format!("{input:.0}"));
            }
        }
    }
    summary
}

fn webhook_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    let run = FRESH_BASE + 99_999;
    let (open_run, attempt) = fx.open_run;
    let (running, runner, started) = fx.running[0].clone();
    vec![
        read(
            "webhook profile UNKNOWN",
            profiles::store::UNKNOWN,
            params![MAIN, PROFILE],
        ),
        write(
            "webhook RUN (new run)",
            jobs::RUN,
            params![run, MAIN, "acme/web-1", "CI", "main", "abc", 1u64, now],
        ),
        write(
            "webhook RUN_ATTEMPT (open run)",
            jobs::RUN_ATTEMPT,
            params![open_run, attempt + 1, now, "def", "main"],
        ),
        write(
            "webhook QUEUED (new job)",
            jobs::QUEUED,
            params![
                run * 4,
                MAIN,
                "acme/web-1",
                open_run,
                PROFILE,
                now,
                "build",
                attempt
            ],
        ),
        write(
            "webhook PROGRESS in_progress",
            jobs::PROGRESS,
            params![
                "in_progress",
                now,
                fx.launched,
                "hatchrun-x",
                None::<&str>,
                Some(now - 5000),
                None::<u64>
            ],
        ),
        write(
            "webhook PROGRESS completed",
            jobs::PROGRESS,
            params![
                "completed",
                now,
                running,
                runner.as_str(),
                "success",
                started,
                now
            ],
        ),
    ]
}

fn launch_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    vec![
        read("launch tenant_query", launch::tenant_query(), params![MAIN]),
        write(
            "launch CLAIM",
            jobs::CLAIM,
            params![fx.waiting[0], "hatchrun-x-1", now, MAIN, 50u32],
        ),
        read("launch IMAGE", launch::IMAGE, params!["eu-west-1", "amd64"]),
        write(
            "launch RECORD_LAUNCH",
            jobs::RECORD_LAUNCH,
            params![
                fx.waiting[0],
                "launched",
                "i-0bench",
                None::<&str>,
                now,
                None::<&str>,
                None::<u64>
            ],
        ),
    ]
}

fn app_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    let page = json(&fx.page);
    vec![
        read(
            "app FIND_SESSION",
            app::FIND_SESSION,
            params![SESSION_KEY, now],
        ),
        read(
            "app authorize_query",
            app::tenants::authorize_query(),
            params![ORG, USER],
        ),
        read(
            "dashboard jobs_of_runs (50 runs)",
            jobs::jobs_query(),
            params![MAIN, page],
        ),
        read(
            "dashboard counts",
            jobs::counts_query(),
            params![MAIN, now - DAY],
        ),
        read(
            "dashboard activity",
            jobs::activity_query(BUCKET_MS),
            params![MAIN, (now / BUCKET_MS + 1 - DAY / BUCKET_MS) * BUCKET_MS],
        ),
        read(
            "run page run_query",
            jobs::run_query(),
            params![fx.page[10], MAIN],
        ),
        read(
            "job page JOB_WITH_RUN",
            jobs::JOB_WITH_RUN,
            params![fx.old_job, MAIN],
        ),
    ]
}

/// Every list: tab, sort and direction, first page and deep, without
/// and with `search`.
fn list_cases(td: &testdb::Db, search: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let pattern = format!("%{search}%");
    for group in [
        None,
        Some(Group::Active),
        Some(Group::Waiting),
        Some(Group::Failed),
    ] {
        for sort in [Sort::Started, Sort::Repository, Sort::Took] {
            for dir in [Direction::Desc, Direction::Asc] {
                for q in [None, Some(pattern.as_str())] {
                    let sql = jobs::list_query(group, sort, dir);
                    let tab = group.map_or("all".to_owned(), |g| format!("{g:?}").to_lowercase());
                    let name = format!(
                        "list {tab} {} {}{}",
                        format!("{sort:?}").to_lowercase(),
                        format!("{dir:?}").to_lowercase(),
                        if q.is_some() { " q" } else { "" }
                    );
                    let first = params![MAIN, Param::Null, Param::Null, PAGE, Param::Null, q];
                    cases.push(read(&format!("{name} p1"), sql.clone(), first));
                    if let Some((at, values)) = deep(td, &sql, sort, q) {
                        cases.push(read(&format!("{name} @{at}"), sql, values));
                    }
                }
            }
        }
    }
    cases
}

/// The values of the page after the run [`DEEP`] runs into the list,
/// or half way into a shorter one, and how far that is.
fn deep(td: &testdb::Db, sql: &str, sort: Sort, q: Option<&str>) -> Option<(usize, Vec<Param>)> {
    let rows = td
        .rows(
            sql,
            &params![MAIN, Param::Null, Param::Null, DEEP + 1, Param::Null, q],
        )
        .unwrap_or_else(|err| panic!("{err}\n{sql}"));
    if rows.len() < 2 {
        return None;
    }
    let at = if rows.len() > DEEP as usize {
        DEEP as usize
    } else {
        rows.len() / 2
    };
    let row = &rows[at - 1];
    let fifth = match sort {
        Sort::Started => Param::Null,
        Sort::Repository => Param::from(string(row, "repository")),
        Sort::Took => row["ended_at"].as_u64().map_or(Param::Null, Param::from),
    };
    Some((
        at,
        params![
            MAIN,
            number(row, "queued_at"),
            number(row, "id"),
            PAGE,
            fifth,
            q
        ],
    ))
}

fn cache_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    let (job, _, _) = fx.running[0].clone();
    let (run, attempt) = fx.open_run;
    vec![
        read(
            "cache CACHE_JOB",
            jobs::CACHE_JOB,
            params![job, MAIN, TENANT],
        ),
        read(
            "cache UPLOAD",
            jobs::UPLOAD,
            params!["upload-0", RUN_BASE * 4],
        ),
        write(
            "cache START_UPLOAD",
            jobs::START_UPLOAD,
            params!["upload-new", job, "acme/b/main/v1/new", "s3-upload-id", now],
        ),
        write(
            "cache FORGET_UPLOAD",
            jobs::FORGET_UPLOAD,
            params!["upload-1"],
        ),
        write(
            "cache RUN_SCOPES",
            jobs::RUN_SCOPES,
            params![run, "acme/b/main", "[\"acme/b/main\"]", attempt],
        ),
        write(
            "cache JOB_SCOPES",
            jobs::JOB_SCOPES,
            params![job, "acme/b/main", "[\"acme/b/main\"]"],
        ),
    ]
}

fn reconcile_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    let limits = reconcile::plan::Limits::default();
    let (ids, runners) = fx.instances();
    vec![
        read(
            "reconcile OPEN_JOBS",
            jobs::OPEN_JOBS,
            params![now - limits.sync_after, SYNC_PER_PASS],
        ),
        read(
            "reconcile RUNS_WITHOUT_DETAILS",
            jobs::RUNS_WITHOUT_DETAILS,
            params![now - MINUTE, DETAILS_PER_PASS],
        ),
        write(
            "reconcile DETAILS_TRIED",
            jobs::DETAILS_TRIED,
            params![now, json(&fx.no_details)],
        ),
        read(
            "reconcile tenants_query",
            reconcile::tenants_query(),
            params![TENANTS_PER_PASS],
        ),
        write(
            "reconcile MARK_RECONCILED",
            reconcile::MARK_RECONCILED,
            params![TENANT, now],
        ),
        read(
            "reconcile TENANT_JOBS",
            jobs::TENANT_JOBS,
            params![MAIN, ids, runners],
        ),
        read(
            "reconcile QUEUED_JOBS",
            jobs::QUEUED_JOBS,
            params![json(&fx.waiting)],
        ),
        read(
            "reconcile unconnected_query",
            jobs::unconnected_query(),
            params![now - limits.queued_grace, UNCONNECTED_PER_PASS],
        ),
        read(
            "reconcile catalog_query",
            reconcile::catalog_query(),
            params![now - DAY, now - HOUR],
        ),
    ]
}

/// The retention batch, statement by statement, each after the ones
/// before it that it depends on.
fn retention_cases(now: u64) -> Vec<Case> {
    vec![
        write(
            "retention BACKFILL_JOBS",
            jobs::BACKFILL_JOBS,
            params![RETENTION, DELETE_BATCH],
        ),
        write(
            "retention BACKFILL_RUNS",
            jobs::BACKFILL_RUNS,
            params![RETENTION, DELETE_BATCH],
        ),
        write(
            "retention DELETE_SESSIONS",
            reconcile::DELETE_SESSIONS,
            params![now, DELETE_BATCH],
        ),
        write(
            "retention DELETE_JOBS",
            jobs::DELETE_JOBS,
            params![now, DELETE_BATCH],
        ),
        write(
            "retention DELETE_REDELIVERIES",
            reconcile::DELETE_REDELIVERIES,
            params![now - 4 * DAY, DELETE_BATCH],
        ),
        write(
            "retention DELETE_OFFERINGS",
            reconcile::DELETE_OFFERINGS,
            params![now],
        ),
        write(
            "retention DELETE_UPLOADS",
            jobs::DELETE_UPLOADS,
            params![now - DAY, DELETE_BATCH],
        ),
        write(
            "retention DELETE_FAILED",
            alerts::DELETE_FAILED,
            params![now - RETENTION, DELETE_BATCH],
        ),
        write(
            "retention DELETE_RATE_LIMITS",
            alerts::DELETE_RATE_LIMITS,
            params![now - DAY],
        ),
        write(
            "retention DELETE_RUNS (jobs kept)",
            jobs::DELETE_RUNS,
            params![now, DELETE_BATCH],
        ),
        write(
            "retention DELETE_RUNS (after DELETE_JOBS)",
            jobs::DELETE_RUNS,
            params![now, DELETE_BATCH],
        )
        .after(jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
        write(
            "retention DELETE_USAGE",
            reconcile::DELETE_USAGE,
            params![now - 400 * DAY, DELETE_BATCH],
        ),
    ]
}

fn usage_cases(fx: &Fixture) -> Vec<Case> {
    let now = fx.now;
    let (job, started, completed) = fx.unrecorded;
    let minutes = usage::minutes(started, completed);
    vec![
        read("usage month_sql", usage::month_sql(), params![TENANT]),
        write(
            "usage insert_sql",
            usage::insert_sql(),
            params![job, minutes, completed],
        ),
        write(
            "usage used_up_sql",
            usage::used_up_sql(),
            params![job, now, usage::FREE_MINUTES],
        )
        .after(usage::insert_sql(), params![job, minutes, completed]),
        write(
            "usage sweep_sql",
            usage::sweep_sql(),
            params![now - DAY, DELETE_BATCH],
        ),
        read("usage UNSENT", usage::UNSENT, params![500u32]),
        write(
            "usage SENT",
            usage::SENT,
            params![
                json(&fx.page.iter().map(|id| id * 4).collect::<Vec<_>>()),
                now
            ],
        ),
    ]
}

fn alert_cases(now: u64) -> Vec<Case> {
    vec![
        read(
            "alerts waiting_query",
            alerts::waiting_query(),
            params![now - 5 * MINUTE, LISTED],
        ),
        read(
            "alerts FAILED_SINCE",
            alerts::FAILED_SINCE,
            params![now - HOUR, LISTED],
        ),
        read(
            "alerts LOW_SINCE",
            alerts::LOW_SINCE,
            params![now - 15 * MINUTE, LISTED],
        ),
        read("alerts DATABASE", alerts::DATABASE, params![]),
    ]
}

/// Times `op` `warmup` times untimed, then `iters` times; `op` returns
/// the time of the part it measures.
async fn sample(
    warmup: usize,
    iters: usize,
    mut op: impl AsyncFnMut(usize) -> Result<Duration, String>,
) -> Result<Vec<Duration>, String> {
    let mut samples = Vec::with_capacity(iters);
    for i in 0..warmup + iters {
        let elapsed = op(i).await?;
        if i >= warmup {
            samples.push(elapsed);
        }
    }
    Ok(samples)
}

fn measured_op(name: &str, result: Result<Vec<Duration>, String>) -> Measured {
    let (p50, p95, error) = match result {
        Ok(samples) => {
            let (p50, p95) = percentiles(samples);
            (p50, p95, None)
        }
        Err(err) => (0.0, 0.0, Some(err)),
    };
    Measured {
        name: name.to_owned(),
        p50,
        p95,
        rows: "-".to_owned(),
        plan: None,
        error,
    }
}

/// A queued job of a new run, as the webhook records it.
fn queued_job(id: u64, run: u64) -> (QueuedJob, Placement) {
    (
        QueuedJob {
            id,
            run_id: run,
            installation_id: MAIN,
            repository: "acme/web-1".to_owned(),
            label: PROFILE.to_owned(),
            spec: labels::parse(PROFILE).expect("a profile label"),
        },
        Placement {
            name: "build".to_owned(),
            attempt: 1,
            workflow_name: Some("CI".to_owned()),
            head_branch: Some("main".to_owned()),
            head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        },
    )
}

/// Deletes the rows the operations created.
fn forget_fresh(td: &testdb::Db) {
    td.run(
        &format!("DELETE FROM usage_events WHERE job_id >= {FRESH_BASE}"),
        &[],
    );
    td.run(&format!("DELETE FROM jobs WHERE id >= {FRESH_BASE}"), &[]);
    td.run(&format!("DELETE FROM runs WHERE id >= {FRESH_BASE}"), &[]);
}

/// `jobs::record_queued`, a new run each time.
fn record_queued_op(td: &testdb::Db, s: &Settings, now: u64) -> Measured {
    let db = td.db();
    let result = td.wait(sample(s.warmup, s.iters, async |i| {
        let n = u64::try_from(i).unwrap_or(0);
        let (job, placement) = queued_job(FRESH_BASE + n, FRESH_BASE + n);
        let started = Instant::now();
        jobs::record_queued(db, &job, &placement, now)
            .await
            .map_err(text)?;
        Ok(started.elapsed())
    }));
    forget_fresh(td);
    measured_op("op webhook record_queued (batch)", result)
}

/// A new queued job, recorded untimed, for an operation to take.
async fn fresh_job(db: &Db, id: u64, run: u64, now: u64) -> Result<(), String> {
    db.run(
        jobs::QUEUED,
        &params![id, MAIN, "acme/web-1", run, PROFILE, now, "build", 1u64],
    )
    .await
    .map_err(text)?;
    Ok(())
}

/// `jobs::claim` through `first_locked`, on a new job each time, which
/// is deleted after so the tenant's runners stay under its limit.
fn claim_op(td: &testdb::Db, s: &Settings, now: u64) -> Measured {
    let db = td.db();
    let result = td.wait(sample(s.warmup, s.iters, async |i| {
        let id = FRESH_BASE + 100_000 + u64::try_from(i).unwrap_or(0);
        fresh_job(db, id, FRESH_BASE + 100_000, now).await?;
        let started = Instant::now();
        let claimed = jobs::claim(db, id, "hatchrun-x-1", now, MAIN, 50)
            .await
            .map_err(text)?;
        let elapsed = started.elapsed();
        db.run("DELETE FROM jobs WHERE id = $1", &params![id])
            .await
            .map_err(text)?;
        claimed
            .map(|_| elapsed)
            .ok_or_else(|| "not claimed".to_owned())
    }));
    forget_fresh(td);
    measured_op("op launch claim (first_locked)", result)
}

fn statement_table(rows: &[Measured]) -> String {
    let mut out = String::from(
        "## Statements\n\n\
         | statement | p50 ms | p95 ms | server ms | jit ms | rows | hit/read | top node | seq scans | sorts > 1000 |\n\
         |---|---:|---:|---:|---:|---:|---:|---|---|---|\n",
    );
    for row in rows {
        let (server, buffers, top, seq, sorts) = row.plan.as_ref().map_or_else(
            || {
                (
                    "- | -".to_owned(),
                    "-".to_owned(),
                    row.error.clone().unwrap_or_else(|| "-".to_owned()),
                    String::new(),
                    String::new(),
                )
            },
            |plan| {
                (
                    format!("{:.2} | {:.1}", plan.server_ms, plan.jit_ms),
                    format!("{}/{}", plan.hit, plan.read),
                    plan.top.clone(),
                    plan.seq_scans.join(", "),
                    plan.sorts.join(", "),
                )
            },
        );
        let top = match (&row.error, &row.plan) {
            (Some(err), Some(_)) => format!("{top} (error: {err})"),
            _ => top,
        };
        let _ = writeln!(
            out,
            "| {} | {:.2} | {:.2} | {server} | {} | {buffers} | {} | {seq} | {sorts} |",
            row.name,
            row.p50,
            row.p95,
            row.rows,
            top.replace('|', "/")
        );
    }
    out
}

// Round trips.

/// An operation's time through the proxy and on the direct link.
struct Trip {
    name: &'static str,
    wall: Result<f64, String>,
    local: Result<f64, String>,
    runs: usize,
}

fn trip_table(trips: &[Trip], rtt: Duration) -> String {
    // What one round trip costs through the proxy: SELECT 1's time over
    // the direct link's, a little over the RTT for the proxy's timers.
    let one = match trips.first() {
        Some(Trip {
            wall: Ok(wall),
            local: Ok(local),
            ..
        }) => wall - local,
        _ => ms(rtt),
    }
    .max(f64::EPSILON);
    let mut out = format!(
        "## Round trips at {} ms\n\n\
         Round trips are (wall - local) over SELECT 1's, {one:.1} ms: the time the link \
         added, in round trips. Medians of {} runs.\n\n\
         | operation | wall ms | local ms | round trips |\n|---|---:|---:|---:|\n",
        rtt.as_millis(),
        trips.first().map_or(0, |trip| trip.runs),
    );
    for trip in trips {
        let _ = match (&trip.wall, &trip.local) {
            (Ok(wall), Ok(local)) => writeln!(
                out,
                "| {} | {wall:.1} | {local:.1} | {:.1} |",
                trip.name,
                (wall - local) / one
            ),
            (Err(err), _) | (_, Err(err)) => {
                writeln!(out, "| {} | error: {err} | - | - |", trip.name)
            }
        };
    }
    out
}

/// Forwards each connection to `target`, holding every chunk for
/// `delay` in each direction.
async fn proxy(listener: TcpListener, target: String, delay: Duration) {
    while let Ok((client, _)) = listener.accept().await {
        let Ok(server) = TcpStream::connect(&target).await else {
            continue;
        };
        let _ = client.set_nodelay(true);
        let _ = server.set_nodelay(true);
        let (client_read, client_write) = client.into_split();
        let (server_read, server_write) = server.into_split();
        tokio::spawn(pipe(client_read, server_write, delay));
        tokio::spawn(pipe(server_read, client_write, delay));
    }
}

/// Copies `from` to `to`, each chunk `delay` after it arrived. Chunks
/// that arrive together leave together, as on a long link.
async fn pipe(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, delay: Duration) {
    let (send, mut receive) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((at, chunk)) = receive.recv().await {
            tokio::time::sleep_until(at).await;
            if to.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(n) = from.read(&mut buffer).await {
        if n == 0
            || send
                .send((tokio::time::Instant::now() + delay, buffer[..n].to_vec()))
                .is_err()
        {
            break;
        }
    }
    drop(send);
    let _ = writer.await;
}

/// A client on the schema whose every round trip takes `rtt`.
fn slow_client(td: &testdb::Db, rtt: Duration) -> Db {
    let config: tokio_postgres::Config = testdb::url().parse().expect("DATABASE_URL");
    let host = match config.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
        _ => "localhost".to_owned(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let address = td.wait(async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let address = listener.local_addr().expect("the proxy's address");
        tokio::spawn(proxy(listener, format!("{host}:{port}"), rtt / 2));
        address
    });
    let mut slow = tokio_postgres::Config::new();
    slow.host("127.0.0.1").port(address.port());
    if let Some(user) = config.get_user() {
        slow.user(user);
    }
    if let Some(password) = config.get_password() {
        slow.password(password);
    }
    if let Some(name) = config.get_dbname() {
        slow.dbname(name);
    }
    td.connect(&slow)
}

/// An operation the Worker runs.
#[derive(Clone, Copy)]
enum Op {
    Select,
    Batch,
    Queued,
    Progress,
    Launch,
    Dashboard,
    RunPage,
    CacheJob,
    Reconcile,
}

impl Op {
    const ALL: [Op; 9] = [
        Op::Select,
        Op::Batch,
        Op::Queued,
        Op::Progress,
        Op::Launch,
        Op::Dashboard,
        Op::RunPage,
        Op::CacheJob,
        Op::Reconcile,
    ];

    fn name(self) -> &'static str {
        match self {
            Op::Select => "SELECT 1",
            Op::Batch => "batch of one UPDATE (BEGIN, UPDATE, COMMIT)",
            Op::Queued => "webhook queued: unknown + record_queued",
            Op::Progress => "webhook progress completed: progress + usage::record",
            Op::Launch => "launch: tenant_query + claim + record_launch",
            Op::Dashboard => "dashboard page: session, authorize, list, jobs, counts, activity",
            Op::RunPage => "run page: run + jobs_of_runs",
            Op::CacheJob => "cache job lookup",
            Op::Reconcile => "reconciler pass (database statements)",
        }
    }

    /// Runs once on `db` and returns the time of the operation alone.
    /// Rows it needs beforehand go in through `fast`, with id `id`.
    async fn once(self, fast: &Db, db: &Db, fx: &Fixture, id: u64) -> Result<Duration, String> {
        let now = fx.now;
        match self {
            Op::Progress => return progress_once(fast, db, fx, id).await,
            Op::Launch => return launch_once(fast, db, now, id).await,
            _ => {}
        }
        let started = Instant::now();
        match self {
            Op::Select => {
                db.rows("SELECT 1", &[]).await.map_err(text)?;
            }
            Op::Batch => {
                db.batch(&[(reconcile::MARK_RECONCILED, params![TENANT, now])])
                    .await
                    .map_err(text)?;
            }
            Op::Queued => {
                let (job, placement) = queued_job(id, id);
                profiles::store::unknown(db, job.installation_id, &job.spec)
                    .await
                    .map_err(text)?;
                jobs::record_queued(db, &job, &placement, now)
                    .await
                    .map_err(text)?;
            }
            Op::Dashboard => dashboard_page(db, now).await?,
            Op::RunPage => {
                jobs::run::<Value>(db, fx.page[3], MAIN)
                    .await
                    .map_err(text)?;
                jobs::jobs_of_runs::<Value>(db, MAIN, json(&[fx.page[3]]))
                    .await
                    .map_err(text)?;
            }
            Op::CacheJob => {
                jobs::cache_job::<Value>(db, fx.running[0].0, MAIN, TENANT)
                    .await
                    .map_err(text)?;
            }
            Op::Reconcile => reconcile_pass(db, fx).await?,
            Op::Progress | Op::Launch => {}
        }
        Ok(started.elapsed())
    }
}

/// Each operation the way the Worker runs it, through a link whose
/// round trip takes `s.rtt` and on the direct one.
fn round_trips(td: &testdb::Db, s: &Settings, fx: &Fixture) -> Vec<Trip> {
    let slow = slow_client(td, s.rtt);
    let fast = td.db();
    let median =
        |result: Result<Vec<Duration>, String>| result.map(|samples| percentiles(samples).0);
    let mut trips = Vec::new();
    for (i, op) in (0u64..).zip(Op::ALL) {
        let base = FRESH_BASE + 200_000 + i * 100_000;
        let on = |db: &Db, base: u64| {
            median(td.wait(sample(1, s.rtt_runs, async |n| {
                let id = base + u64::try_from(n).unwrap_or(0);
                op.once(fast, db, fx, id).await
            })))
        };
        let wall = on(&slow, base);
        let local = on(fast, base + 50_000);
        println!(
            "bench: {}: {wall:?} through the proxy, {local:?} direct",
            op.name()
        );
        trips.push(Trip {
            name: op.name(),
            wall,
            local,
            runs: s.rtt_runs,
        });
    }
    forget_fresh(td);
    drop(slow);
    trips
}

/// A running job moving to completed, and its minutes.
async fn progress_once(fast: &Db, db: &Db, fx: &Fixture, id: u64) -> Result<Duration, String> {
    let now = fx.now;
    fresh_job(fast, id, fx.open_run.0, now - 10 * MINUTE).await?;
    let runner = format!("hatchrun-{id}-bench");
    jobs::progress(
        fast,
        id,
        "in_progress",
        Some(&runner),
        &Outcome::default(),
        now - 9 * MINUTE,
    )
    .await
    .map_err(text)?;
    let outcome = Outcome {
        conclusion: Some("success".to_owned()),
        started_at: Some(now - 9 * MINUTE),
        completed_at: Some(now),
    };
    let started = Instant::now();
    jobs::progress(db, id, "completed", Some(&runner), &outcome, now)
        .await
        .map_err(text)?;
    usage::record(db, id, now - 9 * MINUTE, now, now)
        .await
        .map_err(text)?;
    Ok(started.elapsed())
}

/// A launch's statements on a new job, which is deleted after so the
/// tenant's runners stay under its limit.
async fn launch_once(fast: &Db, db: &Db, now: u64, id: u64) -> Result<Duration, String> {
    fresh_job(fast, id, id, now).await?;
    let started = Instant::now();
    db.first::<Value>(&launch::tenant_query(), &params![MAIN])
        .await
        .map_err(text)?;
    jobs::claim(db, id, "hatchrun-x-2", now, MAIN, 50)
        .await
        .map_err(text)?
        .ok_or("not claimed")?;
    let launched = jobs::Launched {
        state: "launched",
        instance_id: Some("i-0bench".to_owned()),
        error: None,
        wait: None,
        next_launch_at: None,
    };
    jobs::record_launch(db, id, launched, now)
        .await
        .map_err(text)?;
    let elapsed = started.elapsed();
    fast.run("DELETE FROM jobs WHERE id = $1", &params![id])
        .await
        .map_err(text)?;
    Ok(elapsed)
}

/// The statements of `GET /api/tenants/acme/runs`, as `dashboard::runs`
/// sends them, without the GitHub call that checks the owner.
async fn dashboard_page(db: &Db, now: u64) -> Result<(), String> {
    db.first::<Value>(app::FIND_SESSION, &params![SESSION_KEY, now])
        .await
        .map_err(text)?;
    db.first::<Value>(&app::tenants::authorize_query(), &params![ORG, USER])
        .await
        .map_err(text)?;
    let page = RunsPage {
        group: None,
        sort: Sort::Started,
        dir: Direction::Desc,
        queued_at: None,
        id: None,
        limit: PAGE,
        fifth: Param::Null,
        search: None,
    };
    let runs: Vec<Value> = jobs::list_runs(db, MAIN, page).await.map_err(text)?;
    let ids: Vec<u64> = runs.iter().filter_map(|run| run["id"].as_u64()).collect();
    jobs::jobs_of_runs::<Value>(db, MAIN, json(&ids))
        .await
        .map_err(text)?;
    jobs::counts::<Value>(db, MAIN, now - DAY)
        .await
        .map_err(text)?;
    let since = (now / BUCKET_MS + 1 - DAY / BUCKET_MS) * BUCKET_MS;
    jobs::activity::<Value>(db, MAIN, since, BUCKET_MS)
        .await
        .map_err(text)?;
    Ok(())
}

/// The database statements of one reconciler pass, in its order, with
/// its concurrency, and none of its GitHub or AWS calls.
async fn reconcile_pass(db: &Db, fx: &Fixture) -> Result<(), String> {
    let now = fx.now;
    let limits = reconcile::plan::Limits::default();
    db.run(reconcile::TAKE_LEASE, &params![now, now + 5 * MINUTE])
        .await
        .map_err(text)?;
    jobs::open_jobs::<Value>(db, now - limits.sync_after, SYNC_PER_PASS)
        .await
        .map_err(text)?;
    let runs: Vec<Value> = jobs::runs_without_details(db, now - MINUTE, DETAILS_PER_PASS)
        .await
        .map_err(text)?;
    let ids: Vec<u64> = runs.iter().filter_map(|run| run["id"].as_u64()).collect();
    if !ids.is_empty() {
        jobs::details_tried(db, &json(&ids), now)
            .await
            .map_err(text)?;
    }
    let tenants: Vec<Value> = db
        .all(&reconcile::tenants_query(), &params![TENANTS_PER_PASS])
        .await
        .map_err(text)?;
    let (main_ids, main_runners) = fx.instances();
    let results: Vec<Result<(), String>> = stream::iter(&tenants)
        .map(|tenant| {
            let (ids, runners) = if tenant["installation_id"].as_u64() == Some(MAIN) {
                (main_ids.clone(), main_runners.clone())
            } else {
                ("[]".to_owned(), "[]".to_owned())
            };
            async move {
                let id = tenant["id"].as_str().unwrap_or_default();
                db.run(reconcile::MARK_RECONCILED, &params![id, now])
                    .await
                    .map_err(text)?;
                let installation = tenant["installation_id"].as_u64().unwrap_or_default();
                jobs::tenant_jobs::<Value>(db, installation, &ids, &runners)
                    .await
                    .map_err(text)?;
                Ok(())
            }
        })
        .buffer_unordered(TENANTS_AT_ONCE)
        .collect()
        .await;
    results.into_iter().collect::<Result<(), String>>()?;
    db.all::<Value>(&reconcile::catalog_query(), &params![now - DAY, now - HOUR])
        .await
        .map_err(text)?;
    jobs::unconnected::<Value>(db, now - limits.queued_grace, UNCONNECTED_PER_PASS)
        .await
        .map_err(text)?;
    usage::sweep(db, now - DAY, DELETE_BATCH, now)
        .await
        .map_err(text)?;
    usage::unsent(db, 500).await.map_err(text)?;
    retention(db, now).await?;
    db.first::<Value>(reconcile::HISTORY, &[])
        .await
        .map_err(text)?;
    db.run(reconcile::RECORD, &params![now, now, "{}", None::<&str>])
        .await
        .map_err(text)?;
    after_pass(db, now).await
}

/// The reconciler's retention batch.
async fn retention(db: &Db, now: u64) -> Result<(), String> {
    db.batch(&[
        (jobs::BACKFILL_JOBS, params![RETENTION, DELETE_BATCH]),
        (jobs::BACKFILL_RUNS, params![RETENTION, DELETE_BATCH]),
        (reconcile::DELETE_SESSIONS, params![now, DELETE_BATCH]),
        (jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
        (
            reconcile::DELETE_REDELIVERIES,
            params![now - 4 * DAY, DELETE_BATCH],
        ),
        (reconcile::DELETE_OFFERINGS, params![now]),
        (jobs::DELETE_UPLOADS, params![now - DAY, DELETE_BATCH]),
        (
            alerts::DELETE_FAILED,
            params![now - RETENTION, DELETE_BATCH],
        ),
        (alerts::DELETE_RATE_LIMITS, params![now - DAY]),
        (jobs::DELETE_RUNS, params![now, DELETE_BATCH]),
        (
            reconcile::DELETE_USAGE,
            params![now - 400 * DAY, DELETE_BATCH],
        ),
    ])
    .await
    .map_err(text)?;
    Ok(())
}

/// The reads of `alerts::after_pass`.
async fn after_pass(db: &Db, now: u64) -> Result<(), String> {
    db.first::<Value>(alerts::DATABASE, &[])
        .await
        .map_err(text)?;
    db.all::<Value>(&alerts::waiting_query(), &params![now - 5 * MINUTE, LISTED])
        .await
        .map_err(text)?;
    db.all::<Value>(alerts::FAILED_SINCE, &params![now - HOUR, LISTED])
        .await
        .map_err(text)?;
    db.all::<Value>(alerts::LOW_SINCE, &params![now - 15 * MINUTE, LISTED])
        .await
        .map_err(text)?;
    Ok(())
}
