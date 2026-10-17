// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs the Worker's D1 queries against SQLite in tests.
//!
//! D1 is SQLite, but the Worker only reaches it through JavaScript, and
//! that changes the types. Every number the Worker binds is a double, so
//! SQLite sees a REAL even where Rust had a `u64`: `?2 - 1` is REAL and
//! `(updated_at - ?2) / 900000` divides as floats. [`Db`] binds the same
//! way, numbers as REAL, strings as TEXT and nothing as NULL.
//!
//! Rows come back the way D1 hands them over: an object by column name,
//! INTEGER as an integer, REAL as a float, TEXT as a string. They are
//! deserialized into the Worker's own row structs, so a REAL in a `u32`
//! field fails the test as it would fail the request. This is stricter
//! than D1 in one place: D1 turns a whole REAL such as `2.0` into the
//! number 2, which then reads into an integer field, while here it stays
//! a float and fails. A query that hands an integer field a REAL only
//! works on the rows where the float happens to be whole.

use std::path::Path;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, params_from_iter};
use serde::de::DeserializeOwned;
use serde_json::{Map, Number};

/// A value bound to a statement, as D1 receives it from JavaScript.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Param {
    /// A JavaScript number, a REAL to SQLite.
    Num(f64),
    /// A JavaScript string.
    Text(String),
    /// `null`.
    Null,
}

impl From<u64> for Param {
    #[allow(clippy::cast_precision_loss)]
    fn from(value: u64) -> Self {
        // As `crate::int`, which refuses anything above 2^53.
        assert!(value < 1 << 53, "{value} is too large for D1");
        Param::Num(value as f64)
    }
}

impl From<u32> for Param {
    fn from(value: u32) -> Self {
        Param::Num(f64::from(value))
    }
}

impl From<i64> for Param {
    #[allow(clippy::cast_precision_loss)]
    fn from(value: i64) -> Self {
        Param::Num(value as f64)
    }
}

impl From<f64> for Param {
    fn from(value: f64) -> Self {
        Param::Num(value)
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Param::Text(value.to_owned())
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Param::Text(value)
    }
}

impl From<&String> for Param {
    fn from(value: &String) -> Self {
        Param::Text(value.clone())
    }
}

impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(value: Option<T>) -> Self {
        value.map_or(Param::Null, Into::into)
    }
}

impl Param {
    fn value(&self) -> Value {
        match self {
            Param::Num(n) => Value::Real(*n),
            Param::Text(s) => Value::Text(s.clone()),
            Param::Null => Value::Null,
        }
    }
}

/// The parameters of one statement, each converted as D1 would. A
/// `Vec` so that a batch can mix statements with different counts;
/// built without `vec!` so `&params![..]` passes clippy.
macro_rules! params {
    () => { Vec::<$crate::testdb::Param>::new() };
    ($($value:expr),+ $(,)?) => {
        <[$crate::testdb::Param]>::into_vec(Box::new([$($crate::testdb::Param::from($value)),+]))
    };
}
pub(crate) use params;

/// An in-memory database with every migration applied.
pub(crate) struct Db(Connection);

impl Db {
    /// A fresh database, migrated in file name order as wrangler does.
    pub(crate) fn new() -> Self {
        let conn = Connection::open_in_memory().expect("open SQLite");
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read migrations")
            .map(|entry| entry.expect("read migrations").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no migrations in {}", dir.display());
        for file in files {
            let sql = std::fs::read_to_string(&file).expect("read migration");
            conn.execute_batch(&sql)
                .unwrap_or_else(|err| panic!("{}: {err}", file.display()));
        }
        Db(conn)
    }

    /// Runs a statement for its effect, as `.run()`, and returns the
    /// rows it changed.
    pub(crate) fn run(&self, sql: &str, params: &[Param]) -> usize {
        self.try_run(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// As [`Db::run`], with the error instead of a panic.
    pub(crate) fn try_run(&self, sql: &str, params: &[Param]) -> Result<usize, String> {
        let mut stmt = self.0.prepare(sql).map_err(|err| err.to_string())?;
        check_arity(&stmt, params)?;
        // A query rather than `execute`, which refuses a statement that
        // returns rows; D1 runs those too.
        let mut rows = stmt
            .query(params_from_iter(params.iter().map(Param::value)))
            .map_err(|err| err.to_string())?;
        while rows.next().map_err(|err| err.to_string())?.is_some() {}
        drop(rows);
        Ok(usize::try_from(self.0.changes()).expect("changes fit usize"))
    }

    /// Runs statements in one transaction, as `.batch()`: one failing
    /// rolls them all back. Returns the rows each statement changed, as
    /// the `changes` of each result's meta.
    pub(crate) fn batch(&self, statements: &[(&str, Vec<Param>)]) -> Result<Vec<usize>, String> {
        // Rolls back on drop, so an error or a panic leaves no
        // transaction open.
        let tx = self
            .0
            .unchecked_transaction()
            .map_err(|err| err.to_string())?;
        let mut changes = Vec::with_capacity(statements.len());
        for (sql, params) in statements {
            changes.push(
                self.try_run(sql, params)
                    .map_err(|err| format!("{err}\n{sql}"))?,
            );
        }
        tx.commit().map_err(|err| err.to_string())?;
        Ok(changes)
    }

    /// Every row as an object by column name, as D1 hands it over.
    pub(crate) fn rows(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<Map<String, serde_json::Value>>, String> {
        let mut stmt = self.0.prepare(sql).map_err(|err| err.to_string())?;
        check_arity(&stmt, params)?;
        let names: Vec<String> = stmt.column_names().iter().map(|&n| n.to_owned()).collect();
        let mut rows = stmt
            .query(params_from_iter(params.iter().map(Param::value)))
            .map_err(|err| err.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|err| err.to_string())? {
            let mut object = Map::new();
            for (i, name) in names.iter().enumerate() {
                let value = row.get_ref(i).map_err(|err| err.to_string())?;
                object.insert(name.clone(), json(value));
            }
            out.push(object);
        }
        Ok(out)
    }

    /// Every row deserialized into `T`, as `.all()` then `.results()`.
    pub(crate) fn try_all<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
    ) -> Result<Vec<T>, String> {
        self.rows(sql, params)?
            .into_iter()
            .map(|row| {
                let text = serde_json::Value::Object(row.clone());
                serde_json::from_value(text).map_err(|err| format!("{err} in {row:?}"))
            })
            .collect()
    }

    /// As [`Db::try_all`], failing the test on an error.
    pub(crate) fn all<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Vec<T> {
        self.try_all(sql, params)
            .unwrap_or_else(|err| panic!("{err}\n{sql}"))
    }

    /// The first row deserialized into `T`, as `.first(None)`.
    pub(crate) fn first<T: DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Option<T> {
        self.all(sql, params).into_iter().next()
    }

    /// One column of the first row, as `.first(Some(column))`.
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
}

/// D1 refuses a statement bound with fewer values than it numbers, and
/// SQLite would read the missing ones as NULL.
fn check_arity(stmt: &rusqlite::Statement<'_>, params: &[Param]) -> Result<(), String> {
    let wanted = stmt.parameter_count();
    if wanted == params.len() {
        Ok(())
    } else {
        Err(format!(
            "the statement takes {wanted} values, bound {}",
            params.len()
        ))
    }
}

fn json(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::Number(i.into()),
        ValueRef::Real(f) => {
            Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => serde_json::Value::Array(b.iter().map(|&byte| byte.into()).collect()),
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
    struct Whole {
        n: u64,
    }

    #[test]
    fn numbers_bind_as_real() {
        let db = Db::new();
        let rows = db
            .rows(
                "SELECT typeof(?1) AS t, ?2 / 2 AS half",
                &params![5u64, 3u64],
            )
            .unwrap();
        assert_eq!(rows[0]["t"], "real");
        assert_eq!(rows[0]["half"], 1.5);
    }

    #[test]
    fn a_real_does_not_read_into_an_integer() {
        let db = Db::new();
        assert!(
            db.try_all::<Whole>("SELECT ?1 AS n", &params![2u64])
                .is_err()
        );
        let whole = db.try_all::<Whole>("SELECT 2 AS n", &[]).unwrap();
        assert_eq!(whole[0].n, 2);
    }

    #[test]
    fn strict_tables_store_a_whole_real_as_an_integer() {
        let db = Db::new();
        db.run(
            "INSERT INTO used_tokens (jti, expires_at) VALUES (?1, ?2)",
            &params!["a", 7u64],
        );
        let rows = db
            .rows("SELECT typeof(expires_at) AS t FROM used_tokens", &[])
            .unwrap();
        assert_eq!(rows[0]["t"], "integer");
    }

    #[test]
    fn a_missing_value_is_an_error() {
        let db = Db::new();
        assert!(db.rows("SELECT ?1, ?2", &params![1u64]).is_err());
    }
}
