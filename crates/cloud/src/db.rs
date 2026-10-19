// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The database: `PlanetScale` Postgres, reached through the `HYPERDRIVE`
//! binding with tokio-postgres.
//!
//! Statements take [`Param`]s, which bind to whatever type Postgres
//! infers for each placeholder, so a `u64` binds to a `bigint` column
//! and a `0` or `1` to a `boolean` one. Rows come back as JSON objects
//! by column name and deserialize into the caller's row structs, as
//! they did from D1.
//!
//! One connection serves a request, an invocation of the reconciler or
//! a queue batch. Its statements take turns behind a lock, and
//! [`Db::batch`] holds the lock for its whole transaction, so another
//! task on the same connection cannot slip a statement into it.

use std::error::Error as StdError;

use futures_util::lock::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, Row};

/// Hyperdrive binding in `wrangler.toml`.
#[cfg(target_arch = "wasm32")]
const HYPERDRIVE: &str = "HYPERDRIVE";

/// A value bound to a statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Param {
    /// An integer, bound to any integer, `double precision` or, as 0
    /// or 1, `boolean` placeholder.
    Int(i64),
    /// An integer past `i64::MAX`, which no column holds.
    TooLarge(u64),
    Float(f64),
    Bool(bool),
    Text(String),
    Null,
}

impl From<u64> for Param {
    fn from(value: u64) -> Self {
        i64::try_from(value).map_or(Param::TooLarge(value), Param::Int)
    }
}

impl From<u32> for Param {
    fn from(value: u32) -> Self {
        Param::Int(value.into())
    }
}

impl From<u16> for Param {
    fn from(value: u16) -> Self {
        Param::Int(value.into())
    }
}

impl From<u8> for Param {
    fn from(value: u8) -> Self {
        Param::Int(value.into())
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Param::Int(value)
    }
}

impl From<i32> for Param {
    fn from(value: i32) -> Self {
        Param::Int(value.into())
    }
}

impl From<usize> for Param {
    fn from(value: usize) -> Self {
        u64::try_from(value).map_or(Param::TooLarge(u64::MAX), Param::from)
    }
}

impl From<f64> for Param {
    fn from(value: f64) -> Self {
        Param::Float(value)
    }
}

impl From<bool> for Param {
    fn from(value: bool) -> Self {
        Param::Bool(value)
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

impl<T: Into<Param> + Clone> From<&Option<T>> for Param {
    fn from(value: &Option<T>) -> Self {
        value.clone().map_or(Param::Null, Into::into)
    }
}

type BoxError = Box<dyn StdError + Sync + Send>;

impl ToSql for Param {
    fn to_sql(&self, ty: &Type, out: &mut bytes::BytesMut) -> Result<IsNull, BoxError> {
        match self {
            Param::Null => Ok(IsNull::Yes),
            Param::TooLarge(value) => Err(format!("{value} is too large for Postgres").into()),
            Param::Int(value) => match *ty {
                Type::INT8 => value.to_sql(ty, out),
                Type::INT4 => i32::try_from(*value)?.to_sql(ty, out),
                Type::INT2 => i16::try_from(*value)?.to_sql(ty, out),
                #[allow(clippy::cast_precision_loss)]
                Type::FLOAT8 => (*value as f64).to_sql(ty, out),
                Type::BOOL => match value {
                    0 => false.to_sql(ty, out),
                    1 => true.to_sql(ty, out),
                    _ => Err(format!("{value} is not a boolean").into()),
                },
                _ => Err(format!("an integer bound to {ty}").into()),
            },
            Param::Float(value) => match *ty {
                Type::FLOAT8 => value.to_sql(ty, out),
                _ => Err(format!("a float bound to {ty}").into()),
            },
            Param::Bool(value) => match *ty {
                Type::BOOL => value.to_sql(ty, out),
                Type::INT8 => i64::from(*value).to_sql(ty, out),
                Type::INT4 => i32::from(*value).to_sql(ty, out),
                _ => Err(format!("a boolean bound to {ty}").into()),
            },
            Param::Text(value) => match *ty {
                Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
                    value.as_str().to_sql(ty, out)
                }
                // json's binary form is its text; jsonb's starts with
                // its version, 1. Postgres checks the text either way.
                Type::JSON => {
                    out.extend_from_slice(value.as_bytes());
                    Ok(IsNull::No)
                }
                Type::JSONB => {
                    out.extend_from_slice(&[1]);
                    out.extend_from_slice(value.as_bytes());
                    Ok(IsNull::No)
                }
                _ => Err(format!("text bound to {ty}").into()),
            },
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// The values of one statement. Each converts with [`Param::from`].
macro_rules! params {
    () => { Vec::<$crate::db::Param>::new() };
    ($($value:expr),+ $(,)?) => {
        <[$crate::db::Param]>::into_vec(Box::new([$($crate::db::Param::from($value)),+]))
    };
}
pub(crate) use params;

/// An open connection.
pub(crate) struct Db {
    client: Client,
    turn: Mutex<()>,
}

fn error(err: impl std::fmt::Display) -> worker::Error {
    worker::Error::RustError(format!("database: {err}"))
}

fn refs(params: &[Param]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

impl Db {
    /// Wraps a client whose connection something else drives.
    #[cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
    pub(crate) fn new(client: Client) -> Self {
        Db {
            client,
            turn: Mutex::new(()),
        }
    }

    /// Connects through Hyperdrive. The connection runs on its own
    /// task until the client drops.
    ///
    /// # Errors
    ///
    /// No binding, or Hyperdrive refused the connection.
    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn connect(env: &worker::Env) -> worker::Result<Db> {
        use worker::postgres_tls::PassthroughTls;
        use worker::{SecureTransport, Socket};

        let hyperdrive = env.hyperdrive(HYPERDRIVE)?;
        let socket = Socket::builder()
            .secure_transport(SecureTransport::StartTls)
            .connect(hyperdrive.host(), hyperdrive.port())?;
        let config = hyperdrive
            .connection_string()
            .parse::<tokio_postgres::Config>()
            .map_err(error)?;
        let (client, connection) = config
            .connect_raw(socket, PassthroughTls)
            .await
            .map_err(error)?;
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = connection.await {
                worker::console_error!("database connection: {err}");
            }
        });
        Ok(Db::new(client))
    }

    /// Not reachable outside a Worker; tests connect with
    /// `testdb::Db`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn connect(_env: &worker::Env) -> impl Future<Output = worker::Result<Db>> + use<> {
        std::future::ready(Err(error("no Hyperdrive outside a Worker")))
    }

    /// Runs a statement for its effect and returns the rows it changed.
    ///
    /// # Errors
    ///
    /// Postgres refused the statement or a value.
    pub(crate) async fn run(&self, sql: &str, params: &[Param]) -> worker::Result<u64> {
        let _turn = self.turn.lock().await;
        self.client
            .execute(sql, &refs(params))
            .await
            .map_err(|err| statement_error(&err, sql))
    }

    /// Every row deserialized into `T`.
    ///
    /// # Errors
    ///
    /// Postgres refused the statement, or a row does not fit `T`.
    pub(crate) async fn all<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
    ) -> worker::Result<Vec<T>> {
        self.rows(sql, params)
            .await?
            .into_iter()
            .map(|row| decode(row, sql))
            .collect()
    }

    /// The first row deserialized into `T`.
    ///
    /// # Errors
    ///
    /// As [`Db::all`].
    pub(crate) async fn first<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
    ) -> worker::Result<Option<T>> {
        self.rows(sql, params)
            .await?
            .into_iter()
            .next()
            .map(|row| decode(row, sql))
            .transpose()
    }

    /// Rows as JSON objects by column name.
    ///
    /// # Errors
    ///
    /// Postgres refused the statement, or a column has a type rows do
    /// not carry.
    pub(crate) async fn rows(
        &self,
        sql: &str,
        params: &[Param],
    ) -> worker::Result<Vec<Map<String, Value>>> {
        let rows = {
            let _turn = self.turn.lock().await;
            self.client
                .query(sql, &refs(params))
                .await
                .map_err(|err| statement_error(&err, sql))?
        };
        rows.iter().map(|row| json(row).map_err(error)).collect()
    }

    /// The first row of `sql`, run in a transaction that first takes
    /// the advisory lock `key`. Statements that read rows to decide what
    /// they write, such as a count against a limit, see what an earlier
    /// holder of the lock wrote: each statement in a transaction reads
    /// as of its own start, after the lock is held.
    ///
    /// # Errors
    ///
    /// As [`Db::all`], after the rollback.
    pub(crate) async fn first_locked<T: DeserializeOwned>(
        &self,
        key: i64,
        sql: &str,
        params: &[Param],
    ) -> worker::Result<Option<T>> {
        let _turn = self.turn.lock().await;
        self.client.batch_execute("BEGIN").await.map_err(error)?;
        let rows = async {
            self.client
                .execute("SELECT pg_advisory_xact_lock($1)", &[&key])
                .await
                .map_err(error)?;
            self.client
                .query(sql, &refs(params))
                .await
                .map_err(|err| statement_error(&err, sql))
        }
        .await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(err) => {
                let _ = self.client.batch_execute("ROLLBACK").await;
                return Err(err);
            }
        };
        self.client.batch_execute("COMMIT").await.map_err(error)?;
        rows.first()
            .map(|row| json(row).map_err(error).and_then(|row| decode(row, sql)))
            .transpose()
    }

    /// Runs statements in one transaction: one failing rolls them all
    /// back. Returns the rows each statement changed.
    ///
    /// # Errors
    ///
    /// The statement that failed, after the rollback.
    pub(crate) async fn batch(
        &self,
        statements: &[(&str, Vec<Param>)],
    ) -> worker::Result<Vec<u64>> {
        let _turn = self.turn.lock().await;
        self.client.batch_execute("BEGIN").await.map_err(error)?;
        let mut changes = Vec::with_capacity(statements.len());
        for (sql, params) in statements {
            match self.client.execute(*sql, &refs(params)).await {
                Ok(count) => changes.push(count),
                Err(err) => {
                    let _ = self.client.batch_execute("ROLLBACK").await;
                    return Err(statement_error(&err, sql));
                }
            }
        }
        self.client.batch_execute("COMMIT").await.map_err(error)?;
        Ok(changes)
    }
}

/// The server's message for a failed statement, with the statement.
fn statement_error(err: &tokio_postgres::Error, sql: &str) -> worker::Error {
    let message = err
        .as_db_error()
        .map_or_else(|| err.to_string(), |db| db.message().to_owned());
    let first_line = sql.lines().next().unwrap_or_default();
    error(format!("{message} in {first_line}"))
}

fn decode<T: DeserializeOwned>(row: Map<String, Value>, sql: &str) -> worker::Result<T> {
    serde_json::from_value(Value::Object(row)).map_err(|err| {
        error(format!(
            "{err} reading {}",
            sql.lines().next().unwrap_or_default()
        ))
    })
}

/// A row as a JSON object by column name. Integers, floats, booleans
/// and text carry over; NULL becomes `null`.
fn json(row: &Row) -> Result<Map<String, Value>, String> {
    let mut object = Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        let get_err = |err: tokio_postgres::Error| format!("{}: {err}", column.name());
        let value = match *column.type_() {
            Type::INT8 => row
                .try_get::<_, Option<i64>>(i)
                .map_err(get_err)?
                .map(Value::from),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(i)
                .map_err(get_err)?
                .map(Value::from),
            Type::INT2 => row
                .try_get::<_, Option<i16>>(i)
                .map_err(get_err)?
                .map(Value::from),
            Type::FLOAT8 => row
                .try_get::<_, Option<f64>>(i)
                .map_err(get_err)?
                .and_then(Number::from_f64)
                .map(Value::Number),
            Type::FLOAT4 => row
                .try_get::<_, Option<f32>>(i)
                .map_err(get_err)?
                .and_then(|f| Number::from_f64(f.into()))
                .map(Value::Number),
            Type::BOOL => row
                .try_get::<_, Option<bool>>(i)
                .map_err(get_err)?
                .map(Value::from),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row
                .try_get::<_, Option<String>>(i)
                .map_err(get_err)?
                .map(Value::from),
            ref other => {
                return Err(format!(
                    "column {} is {other}; cast it to bigint, double precision or text",
                    column.name()
                ));
            }
        };
        object.insert(column.name().to_owned(), value.unwrap_or(Value::Null));
    }
    Ok(object)
}
