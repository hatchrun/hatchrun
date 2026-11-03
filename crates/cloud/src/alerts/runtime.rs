// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Storage, email and the entry points the Worker calls.

use serde::Deserialize;
use worker::{Env, SendEmail, SendEmailBuilder, console_error};

use super::checks::{Database, Facts, FailedLaunch, PassRecord, check, failed_launches};
use super::queries::{
    DATABASE, FAILED_SINCE, LOW_SINCE, NOTE_RATE_LIMIT, RECORD_FAILED, waiting_query,
};
use super::{
    Alert, BINDING, Email, FAILED_LAUNCHES_FOR, FROM, Key, LISTED, Mail, RATE_LIMITS_FOR, State,
    Store, TO, WAITING, settle,
};
use crate::db::{Db, Param, params};
use crate::fetch::Counted;
use crate::github::RateLimit;

/// Every alert.
const ALERTS: &str = "SELECT key, state, detail, fired_at, resolved_at, sent FROM alerts";

/// Writes an alert's row if it is still the row `$8`, `$9` and `$10`
/// (state, `fired_at` and sent) describe. A row another pass inserted or
/// changed in between is left alone.
const SAVE: &str = "INSERT INTO alerts (key, state, detail, fired_at, resolved_at, sent, \
     updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
     ON CONFLICT (key) DO UPDATE SET state = excluded.state, detail = excluded.detail, \
     fired_at = excluded.fired_at, resolved_at = excluded.resolved_at, sent = excluded.sent, \
     updated_at = excluded.updated_at \
     WHERE alerts.state = $8 AND alerts.fired_at = $9 AND alerts.sent IS NOT DISTINCT FROM $10";

/// Claims the email saying `$2` for `$1` until `$4`: the row must still
/// be due it (see `Alert::due`) and carry no claim that lasts past `$3`.
const CLAIM: &str = "UPDATE alerts SET sending_until = $4 \
     WHERE key = $1 AND state = $2 AND sent IS DISTINCT FROM $2 \
     AND NOT ($2 = 'resolved' AND sent IS NULL) \
     AND (sending_until IS NULL OR sending_until <= $3)";

/// Notes that the email saying `$2` went out and ends the claim.
const SENT: &str =
    "UPDATE alerts SET sent = $2, sending_until = NULL, updated_at = $3 WHERE key = $1";

/// Ends the claim on `$1`.
const RELEASE: &str = "UPDATE alerts SET sending_until = NULL WHERE key = $1";

/// When the last digest went out.
const DIGEST_AT: &str = "SELECT digest_at FROM reconciler WHERE id = 1";

/// Notes that a digest went out at `$1`.
const DIGESTED: &str = "UPDATE reconciler SET digest_at = $1 WHERE id = 1";

/// Alerts kept in the database.
pub(crate) struct Kept<'a>(pub &'a Db);

impl Store for Kept<'_> {
    async fn alerts(&self) -> Result<Vec<Alert>, String> {
        self.0.all(ALERTS, &[]).await.map_err(text)
    }

    async fn save(&self, prev: Option<&Alert>, next: &Alert, now: u64) -> Result<bool, String> {
        // A new alert has no row to match, so a row found is another pass's.
        let changed = self
            .0
            .run(
                SAVE,
                &params![
                    next.key.as_str(),
                    next.state.as_str(),
                    next.detail.as_str(),
                    next.fired_at,
                    next.resolved_at,
                    next.sent.map(State::as_str),
                    now,
                    prev.map(|prev| prev.state.as_str()),
                    prev.map(|prev| prev.fired_at),
                    prev.and_then(|prev| prev.sent).map(State::as_str),
                ],
            )
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn claim(&self, key: &str, state: State, now: u64, until: u64) -> Result<bool, String> {
        let changed = self
            .0
            .run(CLAIM, &params![key, state.as_str(), now, until])
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn sent(&self, key: &str, state: State, now: u64) -> Result<(), String> {
        self.0
            .run(SENT, &params![key, state.as_str(), now])
            .await
            .map_err(text)?;
        Ok(())
    }

    async fn release(&self, key: &str) -> Result<(), String> {
        self.0.run(RELEASE, &params![key]).await.map_err(text)?;
        Ok(())
    }

    async fn digest_at(&self) -> Result<Option<u64>, String> {
        #[derive(Deserialize)]
        struct Row {
            digest_at: Option<u64>,
        }
        Ok(self
            .0
            .first::<Row>(DIGEST_AT, &[])
            .await
            .map_err(text)?
            .and_then(|row| row.digest_at))
    }

    async fn digested(&self, at: u64) -> Result<(), String> {
        self.0.run(DIGESTED, &params![at]).await.map_err(text)?;
        Ok(())
    }
}

/// The `ALERTS` binding and the inbox, or why there is none.
pub(crate) struct Inbox(Result<(SendEmail, String), String>);

impl Inbox {
    pub(crate) fn new(env: &Env) -> Self {
        let to = env
            .secret(TO)
            .map(|value| value.to_string())
            .ok()
            .filter(|to| !to.is_empty())
            .ok_or_else(|| format!("{TO} is not set"));
        Self(to.and_then(|to| {
            env.send_email(BINDING)
                .map(|binding| (binding, to))
                .map_err(text)
        }))
    }
}

impl Mail for Inbox {
    async fn send(&self, email: &Email) -> Result<(), String> {
        let (binding, to) = self.0.as_ref().map_err(Clone::clone)?;
        let message = SendEmailBuilder::builder(FROM, to, &email.subject)
            .text(&email.text)
            .build();
        binding
            .send_with_builder(&message)
            .await
            .map(|_| ())
            .map_err(|err| {
                js_sys::Reflect::get(&err, &"message".into())
                    .ok()
                    .and_then(|message| message.as_string())
                    .unwrap_or_else(|| "send failed".to_owned())
            })
    }
}

/// Runs the checks after a pass and sends what is due. `history` is the
/// pass history with this pass in it, `None` when it could not be read.
pub(crate) async fn after_pass(env: &Env, db: &Db, now: u64, history: Option<Vec<PassRecord>>) {
    let facts = Facts {
        history,
        database: logged(
            "database",
            db.first::<Database>(DATABASE, &[])
                .await
                .map_err(text)
                .and_then(|row| row.ok_or_else(|| "no row".to_owned())),
        ),
        waiting: logged(
            "waiting jobs",
            since(db, &waiting_query(), now.saturating_sub(WAITING)).await,
        ),
        failed: logged("failed launches", recent_failures(db, now).await),
        rate_limits: logged(
            "rate limits",
            since(db, LOW_SINCE, now.saturating_sub(RATE_LIMITS_FOR)).await,
        ),
    };
    for err in settle(&Kept(db), &Inbox::new(env), now, &check(&facts), true).await {
        console_error!("alerts: {err}");
    }
}

/// Records failed launches from the dead-letter queue, as
/// `(message id, job id, installation id)`, and fires their alert.
/// Returns whether they were recorded; the alert's errors are logged.
pub(crate) async fn launches_failed(
    env: &Env,
    db: &Db,
    now: u64,
    failed: &[(String, Option<u64>, Option<u64>)],
) -> Result<(), String> {
    if failed.is_empty() {
        return Ok(());
    }
    let statements: Vec<(&str, Vec<Param>)> = failed
        .iter()
        .map(|(message, job, installation)| {
            (
                RECORD_FAILED,
                params![message.as_str(), *job, *installation, now],
            )
        })
        .collect();
    let recorded = db.batch(&statements).await.map_err(text)?;
    if recorded.iter().all(|changed| *changed == 0) {
        return Ok(());
    }
    let Some(failed) = logged("failed launches", recent_failures(db, now).await) else {
        return Ok(());
    };
    let seen = [(Key::LaunchFailed, failed_launches(&failed))];
    for err in settle(&Kept(db), &Inbox::new(env), now, &seen, false).await {
        console_error!("alerts: {err}");
    }
    Ok(())
}

/// Notes the installations a step's or launch's GitHub calls found low
/// on requests. Errors are logged.
pub(crate) async fn note_rate_limits(db: &Db, calls: &Counted, now: u64) {
    let low = calls.low();
    if low.is_empty() {
        return;
    }
    let statement = |(installation_id, limit): (u64, RateLimit)| {
        (
            NOTE_RATE_LIMIT,
            params![installation_id, limit.remaining, limit.limited, now],
        )
    };
    let statements: Vec<(&str, Vec<Param>)> = low.into_iter().map(statement).collect();
    if let Err(err) = db.batch(&statements).await {
        console_error!("alerts: rate limits: {err}");
    }
}

async fn recent_failures(db: &Db, now: u64) -> Result<Vec<FailedLaunch>, String> {
    since(db, FAILED_SINCE, now.saturating_sub(FAILED_LAUNCHES_FOR)).await
}

/// The rows of a query that takes a time, `$1`, and at most [`LISTED`]
/// rows, `$2`.
pub(super) async fn since<T: for<'de> Deserialize<'de>>(
    db: &Db,
    sql: &str,
    at: u64,
) -> Result<Vec<T>, String> {
    db.all(sql, &params![at, LISTED]).await.map_err(text)
}

/// The value, or `None` with the error logged.
fn logged<T>(what: &str, result: Result<T, String>) -> Option<T> {
    result
        .map_err(|err| console_error!("alerts: {what}: {err}"))
        .ok()
}

#[allow(clippy::needless_pass_by_value)]
fn text(err: worker::Error) -> String {
    err.to_string()
}
