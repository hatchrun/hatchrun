// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The database: `PlanetScale` Postgres, reached through the `HYPERDRIVE`
//! binding with tokio-postgres.
//!
//! Every round trip to Hyperdrive costs more than the query behind it,
//! so a statement goes out as one unnamed Parse, Bind and Execute with
//! the types of its [`Param`]s: integers as `bigint`, text as `text`,
//! booleans as `boolean`, and NULL untyped for Postgres to infer. A
//! transaction goes out whole, `BEGIN` to `COMMIT`, in one round trip;
//! after a failed statement Postgres refuses the rest and turns the
//! `COMMIT` into a rollback. Rows come back as JSON objects by column
//! name and deserialize into the caller's row structs.
//!
//! One connection serves a request, an invocation of the reconciler or
//! a queue batch. Statements from several tasks share it, pipelined; a
//! transaction has it to itself, so no other task's statement lands
//! inside one.

use std::error::Error as StdError;

use async_lock::RwLock;
use futures_util::future::join_all;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};
use tokio_postgres::types::{IsNull, Kind, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, Row};

/// Hyperdrive binding in `wrangler.toml`.
#[cfg(target_arch = "wasm32")]
const HYPERDRIVE: &str = "HYPERDRIVE";

/// A value bound to a statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Param {
    /// An integer, sent as `bigint`.
    Int(i64),
    /// An integer past `i64::MAX`, which no column holds.
    TooLarge(u64),
    /// Sent as `double precision`.
    Float(f64),
    Bool(bool),
    /// Sent as `text`; `$1::json` and the like cast it.
    Text(String),
    /// Sent untyped, as Postgres infers it.
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
    /// Encodes the value as the type [`Param::declared`] gave it.
    fn to_sql(&self, ty: &Type, out: &mut bytes::BytesMut) -> Result<IsNull, BoxError> {
        match self {
            Param::Null => Ok(IsNull::Yes),
            Param::TooLarge(value) => Err(format!("{value} is too large for Postgres").into()),
            Param::Int(value) => value.to_sql(ty, out),
            Param::Float(value) => value.to_sql(ty, out),
            Param::Bool(value) => value.to_sql(ty, out),
            Param::Text(value) => value.as_str().to_sql(ty, out),
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

impl Param {
    /// The type the statement declares for this value. NULL goes out
    /// with type 0, which leaves it to Postgres.
    fn declared(&self) -> Type {
        match self {
            Param::Int(_) | Param::TooLarge(_) => Type::INT8,
            Param::Float(_) => Type::FLOAT8,
            Param::Bool(_) => Type::BOOL,
            Param::Text(_) => Type::TEXT,
            Param::Null => Type::new("unknown".to_owned(), 0, Kind::Simple, String::new()),
        }
    }
}

/// An open connection.
pub(crate) struct Db {
    client: Client,
    /// Shared by single statements, held alone by a transaction.
    turn: RwLock<()>,
}

fn error(err: impl std::fmt::Display) -> worker::Error {
    worker::Error::RustError(format!("database: {err}"))
}

type Typed<'a> = Vec<(&'a (dyn ToSql + Sync), Type)>;

fn typed(params: &[Param]) -> Typed<'_> {
    params
        .iter()
        .map(|p| (p as &(dyn ToSql + Sync), p.declared()))
        .collect()
}

impl Db {
    /// Wraps a client whose connection something else drives.
    #[cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
    pub(crate) fn new(client: Client) -> Self {
        Db {
            client,
            turn: RwLock::new(()),
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
        let _turn = self.turn.read().await;
        self.client
            .execute_typed(sql, &typed(params))
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
            let _turn = self.turn.read().await;
            self.client
                .query_typed(sql, &typed(params))
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
    /// As [`Db::all`]; the transaction rolls back.
    pub(crate) async fn first_locked<T: DeserializeOwned>(
        &self,
        key: i64,
        sql: &str,
        params: &[Param],
    ) -> worker::Result<Option<T>> {
        let lock = [Param::Int(key)];
        let statements = [
            (
                "SELECT true AS locked FROM pg_advisory_xact_lock($1)",
                &lock[..],
            ),
            (sql, params),
        ];
        let mut results = self.transaction(&statements).await?;
        results
            .pop()
            .and_then(|rows| rows.into_iter().next())
            .map(|row| decode(row, sql))
            .transpose()
    }

    /// Runs statements in one transaction: one failing rolls them all
    /// back. Returns the rows each statement changed.
    ///
    /// # Errors
    ///
    /// The first statement that failed.
    pub(crate) async fn batch(
        &self,
        statements: &[(&str, Vec<Param>)],
    ) -> worker::Result<Vec<u64>> {
        let statements: Vec<(&str, &[Param])> = statements
            .iter()
            .map(|(sql, params)| (*sql, params.as_slice()))
            .collect();
        let values: Vec<Typed> = statements.iter().map(|(_, params)| typed(params)).collect();
        let _turn = self.turn.write().await;
        let (begin, done, commit) = futures_util::join!(
            self.client.batch_execute("BEGIN"),
            join_all(
                statements
                    .iter()
                    .zip(&values)
                    .map(|((sql, _), values)| self.client.execute_typed(sql, values))
            ),
            self.client.batch_execute("COMMIT"),
        );
        begin.map_err(error)?;
        let changes = statements
            .iter()
            .zip(done)
            .map(|((sql, _), done)| done.map_err(|err| statement_error(&err, sql)))
            .collect::<worker::Result<Vec<u64>>>()?;
        commit.map_err(error)?;
        Ok(changes)
    }

    /// Runs statements in one transaction, sent together, and returns
    /// each one's rows.
    async fn transaction(
        &self,
        statements: &[(&str, &[Param])],
    ) -> worker::Result<Vec<Vec<Map<String, Value>>>> {
        let values: Vec<Typed> = statements.iter().map(|(_, params)| typed(params)).collect();
        let _turn = self.turn.write().await;
        let (begin, done, commit) = futures_util::join!(
            self.client.batch_execute("BEGIN"),
            join_all(
                statements
                    .iter()
                    .zip(&values)
                    .map(|((sql, _), values)| self.client.query_typed(sql, values))
            ),
            self.client.batch_execute("COMMIT"),
        );
        begin.map_err(error)?;
        let rows = statements
            .iter()
            .zip(done)
            .map(|((sql, _), done)| {
                done.map_err(|err| statement_error(&err, sql))?
                    .iter()
                    .map(|row| json(row).map_err(error))
                    .collect()
            })
            .collect::<worker::Result<Vec<_>>>()?;
        commit.map_err(error)?;
        Ok(rows)
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
