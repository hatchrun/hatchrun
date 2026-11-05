// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Storage, email and the entry points the Worker calls.

use std::time::Duration;

use hatchrun_cloud::db::{Db, Param, params};
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::github::RateLimit;
use serde::Deserialize;
use worker::{Env, SendEmail, SendEmailBuilder, console_error};

use super::checks::{Database, Facts, FailedLaunch, PassRecord, check, failed_launches};
use super::health::{HealthRow, history};
use super::queries::{
    DATABASE, DELETE_FAILED, DELETE_RATE_LIMITS, FAILED_SINCE, LOW_SINCE, NOTE_RATE_LIMIT,
    RECORD_FAILED, RECORD_UNREADABLE,
};
use super::{
    Alert, BINDING, DELETE_BATCH, Email, FAILED_LAUNCH_MEMORY, FAILED_LAUNCHES_FOR, FROM, HISTORY,
    Key, LISTED, Mail, RATE_LIMIT_MEMORY, RATE_LIMITS_FOR, Seen, State, Store, TO, settle,
};

/// Every alert.
const ALERTS: &str = "SELECT key, state, detail, fired_at, resolved_at, sent FROM ci_alerts";

/// Writes an alert's row if it is still the row `$8`, `$9`, `$10` and
/// `$11` (state, `fired_at`, sent and detail) describe. A row another pass inserted or
/// changed in between is left alone.
const SAVE: &str = "INSERT INTO ci_alerts (key, state, detail, fired_at, resolved_at, sent, \
     updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
     ON CONFLICT (key) DO UPDATE SET state = excluded.state, detail = excluded.detail, \
     fired_at = excluded.fired_at, resolved_at = excluded.resolved_at, sent = excluded.sent, \
     updated_at = excluded.updated_at \
     WHERE ci_alerts.state = $8 AND ci_alerts.fired_at = $9 \
     AND ci_alerts.sent IS NOT DISTINCT FROM $10 AND ci_alerts.detail IS NOT DISTINCT FROM $11";

/// Claims the email saying `$2` for the incident of `$1` that fired at
/// `$5`, until `$4`: the row must still be that incident, due the email
/// (see `Alert::due`), and carry no claim that lasts past `$3`.
const CLAIM: &str = "UPDATE ci_alerts SET sending_until = $4 \
     WHERE key = $1 AND state = $2 AND fired_at = $5 AND sent IS DISTINCT FROM $2 \
     AND NOT ($2 = 'resolved' AND sent IS NULL) \
     AND (sending_until IS NULL OR sending_until <= $3)";

/// Notes that the email saying `$2` went out for the incident of `$1`
/// that fired at `$5`, and ends the claim `$4`. The row's state is not
/// matched: a firing email that was in flight when the alert resolved is
/// still noted, so the resolution follows it. Matches nothing when the
/// claim ran out and the row moved on.
const SENT: &str = "UPDATE ci_alerts SET sent = $2, sending_until = NULL, updated_at = $3 \
     WHERE key = $1 AND fired_at = $5 AND sending_until = $4";

/// Ends the claim `$2` on `$1`, if it is still the row's.
const RELEASE: &str =
    "UPDATE ci_alerts SET sending_until = NULL WHERE key = $1 AND sending_until = $2";

/// Stamps the end of a pass at `$1` and appends the pass `$2` (JSON) to
/// the history, keeping the last `$3`. One statement, so a pass that
/// outlives its lease and overlaps the next one cannot overwrite the
/// other's record.
const RECORD_PASS: &str = "\
    UPDATE ci_reconciler SET finished_at = $1, history = ( \
        SELECT COALESCE(jsonb_agg(entry ORDER BY n), '[]'::jsonb)::text FROM ( \
            SELECT entry, n FROM jsonb_array_elements( \
                COALESCE(NULLIF(ci_reconciler.history, '')::jsonb, '[]'::jsonb) || $2::jsonb \
            ) WITH ORDINALITY AS t(entry, n) ORDER BY n DESC LIMIT $3 \
        ) AS kept \
    ) WHERE id = 1 RETURNING history, finished_at";

/// When the last digest went out.
const DIGEST_AT: &str = "SELECT digest_at FROM ci_reconciler WHERE id = 1";

/// Records a digest at `$1` if the last one is still `$2`.
const DIGEST_CLAIM: &str =
    "UPDATE ci_reconciler SET digest_at = $1 WHERE id = 1 AND digest_at IS NOT DISTINCT FROM $2";

/// Takes back the digest at `$1`, restoring `$2`, unless another came
/// after it.
const DIGEST_UNDO: &str = "UPDATE ci_reconciler SET digest_at = $2 WHERE id = 1 AND digest_at = $1";

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
                    prev.map(|prev| prev.detail.as_str()),
                ],
            )
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn claim(
        &self,
        alert: &Alert,
        state: State,
        now: u64,
        until: u64,
    ) -> Result<bool, String> {
        let changed = self
            .0
            .run(
                CLAIM,
                &params![
                    alert.key.as_str(),
                    state.as_str(),
                    now,
                    until,
                    alert.fired_at
                ],
            )
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn sent(
        &self,
        alert: &Alert,
        state: State,
        until: u64,
        now: u64,
    ) -> Result<bool, String> {
        let changed = self
            .0
            .run(
                SENT,
                &params![
                    alert.key.as_str(),
                    state.as_str(),
                    now,
                    until,
                    alert.fired_at
                ],
            )
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn release(&self, key: &str, until: u64) -> Result<(), String> {
        self.0
            .run(RELEASE, &params![key, until])
            .await
            .map_err(text)?;
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

    async fn digest_claim(&self, last: Option<u64>, at: u64) -> Result<bool, String> {
        let changed = self
            .0
            .run(DIGEST_CLAIM, &params![at, last])
            .await
            .map_err(text)?;
        Ok(changed > 0)
    }

    async fn digest_undo(&self, last: Option<u64>, at: u64) -> Result<(), String> {
        self.0
            .run(DIGEST_UNDO, &params![at, last])
            .await
            .map_err(text)?;
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

    fn now(&self) -> u64 {
        worker::Date::now().as_millis()
    }

    async fn sleep(&self, ms: u64) {
        worker::Delay::from(Duration::from_millis(ms)).await;
    }
}

/// Records the pass that started at `now` with `errors` failed steps in
/// the `ci_reconciler` row, runs the checks and sends what is due.
pub(crate) async fn after_pass(env: &Env, db: &Db, now: u64, errors: u32) {
    // Read once the stored alerts are, so what a check found is not older
    // than the rows it changes.
    let read = async || {
        let history = logged(
            "pass history",
            record_pass(db, now, worker::Date::now().as_millis(), errors).await,
        );
        let facts = Facts {
            history,
            database: logged(
                "database",
                db.first::<Database>(DATABASE, &[])
                    .await
                    .map_err(text)
                    .and_then(|row| row.ok_or_else(|| "no row".to_owned())),
            ),
            failed: logged("failed launches", recent_failures(db, now).await),
            rate_limits: logged(
                "rate limits",
                since(db, LOW_SINCE, now.saturating_sub(RATE_LIMITS_FOR), LISTED).await,
            ),
        };
        check(&facts)
    };
    for err in settle(&Kept(db), &Inbox::new(env), now, read, true).await {
        console_error!("alerts: {err}");
    }
    if let Err(err) = prune(db, now).await {
        console_error!("alerts: prune: {err}");
    }
}

/// Adds the pass that started at `now` and had `errors` failed steps to
/// the history in the `ci_reconciler` row, and stamps the time it
/// `finished`, which `/healthz` reads. Returns the history.
pub(super) async fn record_pass(
    db: &Db,
    now: u64,
    finished: u64,
    errors: u32,
) -> Result<Vec<PassRecord>, String> {
    let record =
        serde_json::to_string(&[PassRecord { at: now, errors }]).map_err(|err| err.to_string())?;
    let row = db
        .first::<HealthRow>(
            RECORD_PASS,
            &params![finished, record.as_str(), HISTORY as u64],
        )
        .await
        .map_err(text)?
        .ok_or_else(|| "no row".to_owned())?;
    history(row.history.as_deref()).ok_or_else(|| "the stored history does not read".to_owned())
}

/// Deletes failed launches and rate limits nobody needs any more.
pub(super) async fn prune(db: &Db, now: u64) -> Result<(), String> {
    db.batch(&[
        (
            DELETE_FAILED,
            params![now.saturating_sub(FAILED_LAUNCH_MEMORY), DELETE_BATCH],
        ),
        (
            DELETE_RATE_LIMITS,
            params![now.saturating_sub(RATE_LIMIT_MEMORY)],
        ),
    ])
    .await
    .map(|_| ())
    .map_err(text)
}

/// Records failed launches from the dead-letter queue, as
/// `(message id, job id)`, and the messages that did not read, as
/// `(message id, error)` (see [`super::unreadable`]), and fires their
/// alert. Errors are returned when they could not be recorded; the
/// alert's are logged.
pub(crate) async fn launches_failed(
    env: &Env,
    db: &Db,
    now: u64,
    failed: &[(String, u64)],
    unreadable: &[(String, String)],
) -> Result<(), String> {
    if failed.is_empty() && unreadable.is_empty() {
        return Ok(());
    }
    let mut statements: Vec<(&str, Vec<Param>)> = failed
        .iter()
        .map(|(message, job)| (RECORD_FAILED, params![message.as_str(), *job, now]))
        .collect();
    statements.extend(unreadable.iter().map(|(message, error)| {
        (
            RECORD_UNREADABLE,
            params![message.as_str(), error.as_str(), now],
        )
    }));
    let recorded = db.batch(&statements).await.map_err(text)?;
    if recorded.iter().all(|changed| *changed == 0) {
        return Ok(());
    }
    // Unreadable failures leave the alert as it is.
    let read = async || {
        let seen = match logged("failed launches", recent_failures(db, now).await) {
            Some(failed) => failed_launches(&failed),
            None => Seen::Unknown,
        };
        vec![(Key::LaunchFailed, seen)]
    };
    for err in settle(&Kept(db), &Inbox::new(env), now, read, false).await {
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
    // One more than a detail lists, so it can say when there are more.
    since(
        db,
        FAILED_SINCE,
        now.saturating_sub(FAILED_LAUNCHES_FOR),
        LISTED + 1,
    )
    .await
}

/// The rows of a query that takes a time, `$1`, and at most `limit`
/// rows, `$2`.
pub(super) async fn since<T: for<'de> Deserialize<'de>>(
    db: &Db,
    sql: &str,
    at: u64,
    limit: u32,
) -> Result<Vec<T>, String> {
    db.all(sql, &params![at, limit]).await.map_err(text)
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
