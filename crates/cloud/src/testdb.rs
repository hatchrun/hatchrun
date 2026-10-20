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

use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use serde::de::DeserializeOwned;
use serde_json::Map;
use tokio::runtime::Runtime;

pub(crate) use crate::db::{Param, params};

const DEFAULT_URL: &str = "postgres://postgres:postgres@localhost:5432/postgres";

/// A schema of its own on the test server, with every migration
/// applied.
pub(crate) struct Db {
    runtime: Runtime,
    inner: crate::db::Db,
    schema: String,
}

/// The test server's URL.
pub(crate) fn url() -> String {
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
    pub(crate) fn new() -> Self {
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
    pub(crate) fn connect(&self, config: &tokio_postgres::Config) -> crate::db::Db {
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
    pub(crate) fn db(&self) -> &crate::db::Db {
        &self.inner
    }

    /// Waits for `future`, which may use [`Db::db`].
    pub(crate) fn wait<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Runs a statement for its effect and returns the rows it changed.
    pub(crate) fn run(&self, sql: &str, params: &[Param]) -> usize {
        self.try_run(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// As [`Db::run`], with the error instead of a panic.
    pub(crate) fn try_run(&self, sql: &str, params: &[Param]) -> Result<usize, String> {
        self.wait(self.inner.run(sql, params))
            .map(|n| usize::try_from(n).expect("changes fit usize"))
            .map_err(|err| err.to_string())
    }

    /// Runs statements in one transaction: one failing rolls them all
    /// back. Returns the rows each statement changed.
    pub(crate) fn batch(&self, statements: &[(&str, Vec<Param>)]) -> Result<Vec<usize>, String> {
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
    pub(crate) fn rows(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<Map<String, serde_json::Value>>, String> {
        self.wait(self.inner.rows(sql, params))
            .map_err(|err| err.to_string())
    }

    /// Every row deserialized into `T`.
    pub(crate) fn try_all<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<T>, String> {
        self.wait(self.inner.all(sql, params))
            .map_err(|err| err.to_string())
    }

    /// As [`Db::try_all`], failing the test on an error.
    pub(crate) fn all<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Vec<T> {
        self.try_all(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// The first row deserialized into `T`.
    pub(crate) fn first<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Option<T> {
        self.all(sql, params).into_iter().next()
    }

    /// One column of the first row.
    pub(crate) fn column<T: DeserializeOwned>(
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
    pub(crate) fn plan(&self, sql: &str, params: &[Param]) -> String {
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
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
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

    #[test]
    fn values_bind_as_their_declared_types() {
        let db = Db::new();
        db.run(
            "INSERT INTO rate_limits (installation_id, remaining, limited, seen_at) \
             VALUES ($1, $2, $3, $4)",
            &params![7u64, None::<u64>, true, 5u64],
        );
        let row: Row = db
            .first(
                "SELECT installation_id AS n, limited AS flag, NULL::text AS name \
                 FROM rate_limits",
                &[],
            )
            .unwrap();
        assert_eq!((row.n, row.flag, row.name), (7, true, None));
    }

    #[test]
    fn an_integer_is_no_boolean() {
        let db = Db::new();
        let err = db
            .try_run(
                "INSERT INTO rate_limits (installation_id, remaining, limited, seen_at) \
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
}
