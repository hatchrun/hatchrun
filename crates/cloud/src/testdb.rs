// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs the Worker's queries against a real Postgres in tests.
//!
//! `DATABASE_URL` names the server, `postgres://postgres:postgres@
//! localhost:5432/postgres` when unset; CI starts one as a service.
//! Each [`Db`] gets its own schema with every migration applied, so
//! tests run side by side, and drops it when the test ends.
//!
//! The statements go through [`crate::db::Db`], the Worker's own
//! client, so values bind and rows deserialize as they do in
//! production. [`Db`] waits on each one, for tests written as plain
//! functions.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::new_without_default
)]

use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use serde::de::DeserializeOwned;
use serde_json::Map;
use tokio::runtime::Runtime;

pub use crate::db::{Param, params};

const DEFAULT_URL: &str = "postgres://postgres:postgres@localhost:5432/postgres";

/// A schema of its own on the test server, with every migration
/// applied.
pub struct Db {
    runtime: Runtime,
    inner: crate::db::Db,
    schema: String,
}

/// The test server's URL.
#[must_use]
pub fn url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

/// The migrations, in file name order.
fn migrations() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read migrations")
        .map(|entry| entry.expect("read migrations").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no migrations in {}", dir.display());
    files
        .iter()
        .map(|file| std::fs::read_to_string(file).expect("read migration"))
        .collect()
}

impl Db {
    /// A fresh schema, migrated.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Tests migrate one at a time: a migration may change the
        // database itself, which two sessions cannot do at once.
        static MIGRATING: Mutex<()> = Mutex::new(());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("start a runtime");
        let url = url();
        let schema = format!(
            "test_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let migrating = MIGRATING.lock().unwrap_or_else(PoisonError::into_inner);
        let client = runtime.block_on(async {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap_or_else(|err| panic!("connect to {url} (set DATABASE_URL): {err}"));
            tokio::spawn(connection);
            client
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
                     SET search_path TO {schema}"
                ))
                .await
                .expect("create the schema");
            for sql in migrations() {
                client
                    .batch_execute(&sql)
                    .await
                    .unwrap_or_else(|err| panic!("migrate: {err:?}"));
            }
            client
        });
        drop(migrating);
        Db {
            runtime,
            inner: crate::db::Db::new(client),
            schema,
        }
    }

    /// Another client on this schema, through `config`, such as one
    /// that reaches the server by another address. Its connection runs
    /// on this runtime.
    pub fn connect(&self, config: &tokio_postgres::Config) -> crate::db::Db {
        let client = self.runtime.block_on(async {
            let (client, connection) = config
                .connect(tokio_postgres::NoTls)
                .await
                .unwrap_or_else(|err| panic!("connect a second client: {err}"));
            tokio::spawn(connection);
            client
                .batch_execute(&format!("SET search_path TO {}", self.schema))
                .await
                .expect("set the search path");
            client
        });
        crate::db::Db::new(client)
    }

    /// The Worker's client, for the functions that take one.
    pub fn db(&self) -> &crate::db::Db {
        &self.inner
    }

    /// Waits for `future`, which may use [`Db::db`].
    pub fn wait<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Runs a statement for its effect and returns the rows it changed.
    pub fn run(&self, sql: &str, params: &[Param]) -> usize {
        self.try_run(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// As [`Db::run`], with the error instead of a panic.
    pub fn try_run(&self, sql: &str, params: &[Param]) -> Result<usize, String> {
        self.wait(self.inner.run(sql, params))
            .map(|n| usize::try_from(n).expect("changes fit usize"))
            .map_err(|err| err.to_string())
    }

    /// Runs statements in one transaction: one failing rolls them all
    /// back. Returns the rows each statement changed.
    pub fn batch(&self, statements: &[(&str, Vec<Param>)]) -> Result<Vec<usize>, String> {
        self.wait(self.inner.batch(statements))
            .map(|changes| {
                changes
                    .into_iter()
                    .map(|n| usize::try_from(n).expect("changes fit usize"))
                    .collect()
            })
            .map_err(|err| err.to_string())
    }

    /// Every row as an object by column name.
    pub fn rows(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<Map<String, serde_json::Value>>, String> {
        self.wait(self.inner.rows(sql, params))
            .map_err(|err| err.to_string())
    }

    /// Every row deserialized into `T`.
    pub fn try_all<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<T>, String> {
        self.wait(self.inner.all(sql, params))
            .map_err(|err| err.to_string())
    }

    /// As [`Db::try_all`], failing the test on an error.
    pub fn all<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Vec<T> {
        self.try_all(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// The first row deserialized into `T`.
    pub fn first<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Option<T> {
        self.all(sql, params).into_iter().next()
    }

    /// One column of the first row.
    pub fn column<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
        column: &str,
    ) -> Option<T> {
        let row = self
            .rows(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
            .into_iter()
            .next()?;
        let value = row
            .get(column)
            .unwrap_or_else(|| panic!("no column {column} in {row:?}\n{sql}"))
            .clone();
        Some(serde_json::from_value(value).unwrap_or_else(|err| panic!("{err}\n{sql}")))
    }

    /// Postgres's plan for a statement with sequential scans turned
    /// off, one node per line, such as `Index Scan using jobs_open on
    /// jobs`. Test tables are small enough that the planner would
    /// otherwise scan every one; with scans off it shows which index a
    /// large table would use, or a `Seq Scan` where none fits.
    pub fn plan(&self, sql: &str, params: &[Param]) -> String {
        self.run("SET enable_seqscan = off", &[]);
        let plan = self
            .rows(&format!("EXPLAIN (COSTS OFF) {sql}"), params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
            .iter()
            .map(|row| {
                row["QUERY PLAN"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.run("RESET enable_seqscan", &[]);
        plan
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema);
        let _ = self.runtime.block_on(self.inner.run(&sql, &[]));
    }
}

/// Runs `future` to its end on this thread. The tests' stand-ins for
/// GitHub and KV answer at once, so it never waits.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct Row {
        n: u64,
        flag: bool,
        name: Option<String>,
    }

    /// A table of the column types the tests bind: a bigint, a nullable
    /// bigint and a boolean.
    fn probe(db: &Db) {
        db.run(
            "CREATE TABLE probe (id bigint, remaining bigint, limited boolean, seen_at bigint)",
            &[],
        );
    }

    #[test]
    fn values_bind_as_their_declared_types() {
        let db = Db::new();
        probe(&db);
        db.run(
            "INSERT INTO probe (id, remaining, limited, seen_at) \
             VALUES ($1, $2, $3, $4)",
            &params![7u64, None::<u64>, true, 5u64],
        );
        let row: Row = db
            .first(
                "SELECT id AS n, limited AS flag, NULL::text AS name FROM probe",
                &[],
            )
            .unwrap();
        assert_eq!((row.n, row.flag, row.name), (7, true, None));
    }

    #[test]
    fn an_integer_is_no_boolean() {
        let db = Db::new();
        probe(&db);
        let err = db
            .try_run(
                "INSERT INTO probe (id, remaining, limited, seen_at) \
                 VALUES ($1, $2, $3, $4)",
                &params![7u64, None::<u64>, 1u64, 5u64],
            )
            .unwrap_err();
        assert!(err.contains("boolean"), "{err}");
    }

    #[test]
    fn a_batch_rolls_back_whole() {
        let db = Db::new();
        let insert = "INSERT INTO used_tokens (jti, expires_at) VALUES ($1, $2)";
        assert!(
            db.batch(&[(insert, params!["a", 1u64]), (insert, params!["a", 2u64])])
                .is_err()
        );
        let count: i64 = db
            .column("SELECT COUNT(*) AS n FROM used_tokens", &[], "n")
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            db.batch(&[(insert, params!["a", 1u64]), (insert, params!["b", 2u64])])
                .unwrap(),
            vec![1, 1]
        );
    }

    #[test]
    fn a_numeric_column_says_to_cast_it() {
        let db = Db::new();
        let err = db.rows("SELECT SUM(1::bigint) AS n", &[]).unwrap_err();
        assert!(err.contains("cast it"), "{err}");
    }

    #[test]
    fn a_missing_value_is_an_error() {
        let db = Db::new();
        assert!(
            db.rows("SELECT $1::bigint, $2::bigint", &params![1u64])
                .is_err()
        );
    }

    static MIGRATION_SCHEMA_NEXT: AtomicU64 = AtomicU64::new(0);
    static MIGRATION_LOCK: Mutex<()> = Mutex::new(());

    /// The migration files in name order, each with its file name.
    fn migration_files() -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read migrations")
            .map(|entry| entry.expect("read migrations").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|file| {
                let name = file
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned();
                let sql = std::fs::read_to_string(&file).expect("read migration");
                (name, sql)
            })
            .collect()
    }

    /// Old rows with the tiers their tenants had: t1 keeps 30 days, t2
    /// keeps 90. Runs 1 and 2 are t1's January and February 2024, run 3
    /// is t2's January 2024.
    const MIGRATION_SEED: &str = "
        INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at, \
                             retention_days) \
        VALUES ('t1', 1, 'acme', 99, 0, 0, 30), ('t2', 2, 'initech', 100, 0, 0, 90);
        INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
                              private, updated_at) \
        VALUES (7, 't1', 99, 'acme/app', 'main', false, 0),
               (8, 't2', 100, 'initech/app', 'main', false, 0);
        INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, event, ref, \
                             sha, actor, status, created_at, updated_at, expires_at, trust, \
                             author_login, author_association, gate, gate_status) \
        VALUES ('t1', 7, 'ci', 1, 'd1', 'push', 'refs/heads/main', 'abc', 'me', 'completed', \
                1704067200000, 1704067200000, 1704067200000 + 30::bigint * 86400000, \
                'trusted', 'me', '', 'none', 'none'),
               ('t1', 7, 'ci', 2, 'd2', 'push', 'refs/heads/main', 'def', 'me', 'completed', \
                1706745600000, 1706745600000, 1706745600000 + 30::bigint * 86400000, \
                'trusted', 'me', '', 'none', 'none'),
               ('t2', 8, 'ci', 1, 'e1', 'push', 'refs/heads/main', 'abc', 'you', 'completed', \
                1704067200000, 1704067200000, 1704067200000 + 90::bigint * 86400000, \
                'trusted', 'you', '', 'none', 'none');
        INSERT INTO ci_jobs (run_id, plan_id, name, status, timeout_minutes, token_permissions, \
                             secret_refs, needs, created_at, updated_at) \
        SELECT id, 'build', 'build', 'completed', 10, '{\"metadata\":\"read\"}', \
               '{\"all\":false,\"names\":[]}', '[]', created_at, created_at FROM ci_runs;
        INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, ended_at) \
        SELECT j.id, 1, r.tenant_id, 'x', 60, 60, r.created_at + 60000 \
        FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id;
        INSERT INTO ci_audit (tenant_id, at, actor, action, scope, repo, target) \
        VALUES ('t1', 1704067200000, 'me', 'secret.put', 'org', NULL, 'TOKEN'),
               ('t2', 1706745600000, 'you', 'var.put', 'repo', 'initech/app', 'MODE');";

    async fn migration_count(client: &tokio_postgres::Client, table: &str) -> i64 {
        client
            .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
            .await
            .expect("count")
            .get(0)
    }

    async fn migration_tiers(
        client: &tokio_postgres::Client,
        table: &str,
        id: &str,
    ) -> Vec<(i64, i16)> {
        client
            .query(
                &format!("SELECT {id}, tier FROM {table} ORDER BY {id}"),
                &[],
            )
            .await
            .expect("tiers")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    /// 0027 moves the old rows into tiered month partitions: the runs,
    /// jobs, usage and audit rows land with the tier their tenant had,
    /// in the month they were recorded, and the dropped foreign keys
    /// are gone.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn migration_0027_moves_existing_rows_into_tiered_partitions() {
        let _lock = MIGRATION_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("start a runtime");
        let schema = format!(
            "mig27_{}_{}",
            std::process::id(),
            MIGRATION_SCHEMA_NEXT.fetch_add(1, Ordering::Relaxed)
        );
        runtime.block_on(async {
            let (client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
                .await
                .unwrap_or_else(|err| panic!("connect to {}: {err}", url()));
            tokio::spawn(connection);
            client
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
                     SET search_path TO {schema}"
                ))
                .await
                .expect("create the schema");
            let files = migration_files();
            for (name, sql) in files
                .iter()
                .filter(|(name, _)| name.as_str() < "0027_ci_partitions.sql")
            {
                client
                    .batch_execute(sql)
                    .await
                    .unwrap_or_else(|err| panic!("migrate {name}: {err:?}"));
            }
            client
                .batch_execute(MIGRATION_SEED)
                .await
                .expect("seed old rows");
            let (_, latest) = files
                .iter()
                .find(|(name, _)| name == "0027_ci_partitions.sql")
                .expect("0027 exists");
            client.batch_execute(latest).await.expect("apply 0027");

            assert_eq!(migration_count(&client, "ci_runs").await, 3);
            assert_eq!(migration_count(&client, "ci_jobs").await, 3);
            assert_eq!(migration_count(&client, "ci_usage").await, 3);
            assert_eq!(migration_count(&client, "ci_audit").await, 2);

            assert_eq!(
                migration_tiers(&client, "ci_runs", "id").await,
                [(1, 30), (2, 30), (3, 90)]
            );
            assert_eq!(
                migration_tiers(&client, "ci_jobs", "id").await,
                [(1, 30), (2, 30), (3, 90)]
            );
            assert_eq!(migration_count(&client, "ci_audit_202401").await, 1);
            assert_eq!(migration_count(&client, "ci_audit_202402").await, 1);

            let months: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM pg_tables WHERE schemaname = current_schema() \
                     AND tablename IN ('ci_runs_t30_202401', 'ci_runs_t30_202402', \
                     'ci_runs_t90_202401', 'ci_jobs_t30_202401', 'ci_jobs_t30_202402', \
                     'ci_jobs_t90_202401', 'ci_usage_202401', 'ci_usage_202402', \
                     'ci_audit_202401', 'ci_audit_202402')",
                    &[],
                )
                .await
                .expect("partitions")
                .get(0);
            assert_eq!(months, 10);

            let fkeys: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM pg_constraint WHERE contype = 'f' \
                     AND connamespace = current_schema()::regnamespace AND conname IN \
                     ('ci_jobs_run_id_fkey', 'ci_artifacts_run_id_fkey', \
                      'ci_artifacts_job_id_fkey', 'ci_concurrency_run_id_fkey', \
                      'ci_concurrency_job_id_fkey')",
                    &[],
                )
                .await
                .expect("foreign keys")
                .get(0);
            assert_eq!(fkeys, 0);

            for (sequence, last) in [
                ("ci_runs_id_seq", 3),
                ("ci_jobs_id_seq", 3),
                ("ci_audit_id_seq", 2),
            ] {
                let value: i64 = client
                    .query_one(&format!("SELECT last_value FROM {sequence}"), &[])
                    .await
                    .expect("sequence")
                    .get(0);
                assert_eq!(value, last, "{sequence}");
            }

            // A new row routes into its tier and month.
            client
                .batch_execute(
                    "INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, \
                     event, ref, sha, actor, status, created_at, updated_at, tier, trust, \
                     author_login, author_association, gate, gate_status) \
                     VALUES ('t1', 7, 'ci', 3, 'd3', 'push', 'refs/heads/main', 'ghi', 'me', \
                     'queued', 1706745600000, 1706745600000, 30, 'trusted', 'me', '', 'none', \
                     'none')",
                )
                .await
                .expect("route a new row");
            let routed: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM ci_runs_t30_202402 WHERE delivery = 'd3'",
                    &[],
                )
                .await
                .expect("routed row")
                .get(0);
            assert_eq!(routed, 1);

            client
                .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
                .await
                .expect("drop the schema");
        });
    }
}
