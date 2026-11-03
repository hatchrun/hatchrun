// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Alerts by email on the reconciler, launches, GitHub's rate limit and
//! the database's connections.
//!
//! After each pass the reconciler checks its own history, the pass
//! summary and a few indexed queries (see [`check`]), and the
//! dead-letter queue's consumer checks the failed launches it records.
//! Each check finds an alert firing, clear, or unknown when its data
//! could not be read, which changes nothing. The `alerts` table keeps
//! one row per alert: a firing alert sends one email, appears in one
//! digest a day while it lasts, and sends one more when it resolves. An
//! email that fails to send is tried again on the next pass. Nothing
//! here fails a pass: errors end up in the log.
//!
//! Mail goes through Cloudflare Email Service from
//! `alerts@notify.hatchrun.io` to the `ALERT_EMAIL` inbox. A Worker
//! cannot tell that its own cron stopped, so `/healthz` answers 503
//! when the last pass is old or failing, for an uptime monitor outside
//! Cloudflare to poll (see [`health`]).

use std::fmt::Write;
use std::pin::pin;

use futures_util::future::{Either, select};
use serde::{Deserialize, Serialize};

use crate::reconcile::time;

mod checks;
mod health;
mod queries;
mod runtime;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

pub(crate) use checks::{PassRecord, remember};
pub(crate) use health::history;
#[cfg(feature = "worker")]
pub(crate) use health::{HEALTH, HealthRow, health};
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) use queries::{DATABASE, FAILED_SINCE, LOW_SINCE, waiting_query};
pub(crate) use queries::{DELETE_FAILED, DELETE_RATE_LIMITS};
pub(crate) use runtime::{after_pass, launches_failed, note_rate_limits};

/// The `send_email` binding in `wrangler.toml`.
const BINDING: &str = "ALERTS";
/// The secret with the inbox alerts go to, a secret so the address
/// stays out of the public `wrangler.toml`.
const TO: &str = "ALERT_EMAIL";
/// The sender, on the subdomain onboarded to Email Sending.
const FROM: &str = "alerts@notify.hatchrun.io";

const MINUTE: u64 = 60 * 1000;
const DAY: u64 = 24 * 60 * MINUTE;
/// How long a pass holds the claim on an email. One that crashes while
/// sending blocks the retry this long.
const CLAIM_FOR: u64 = 2 * MINUTE;
/// How long a send may take. Well inside [`CLAIM_FOR`], so a send that
/// is still running has not lost its claim.
const SEND_FOR: u64 = MINUTE;
/// Time a claim keeps in reserve, after the send, to record it.
const RECORD_FOR: u64 = MINUTE / 2;
const _: () = assert!(SEND_FOR + RECORD_FOR < CLAIM_FOR);
/// Bytes in a gigabyte.
const GB: u64 = 1_000_000_000;
/// The share of the database's connections in use, in percent, at which
/// its alert fires.
const CONNECTIONS_PERCENT: u64 = 80;

/// Passes the `reconciler` row remembers.
pub(crate) const HISTORY: usize = 10;
/// Passes in a row with errors before the reconciler counts as failing.
const FAILING_PASSES: usize = 5;
/// Passes in a row with work left before it is out of budget.
const BUSY_PASSES: usize = HISTORY;
/// How old the last pass may be before `/healthz` says the reconciler
/// stopped.
const STALE: u64 = 5 * MINUTE;
/// How long a job may wait for a runner.
const WAITING: u64 = 5 * MINUTE;
/// How long a failed launch keeps its alert firing.
const FAILED_LAUNCHES_FOR: u64 = 60 * MINUTE;
/// How long a low rate limit keeps its alert firing. GitHub's limit
/// resets every hour.
const RATE_LIMITS_FOR: u64 = 15 * MINUTE;
/// Rows a detail lists at most.
const LISTED: u32 = 10;

/// An alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// Errors in [`FAILING_PASSES`] passes in a row.
    ReconcilerFailing,
    /// Work left in [`BUSY_PASSES`] passes in a row: the reconcile
    /// queue per tenant (#105) is due.
    ReconcilerBusy,
    /// A launch message reached the dead-letter queue.
    LaunchFailed,
    /// A verified tenant's job has waited [`WAITING`] for a runner.
    JobsWaiting,
    /// GitHub refused a request for the rate limit, or an installation
    /// has little left.
    RateLimit,
    /// The database has [`CONNECTIONS_PERCENT`] of its connections in
    /// use. Hyperdrive pools the Worker's, so this is Hyperdrive's pool,
    /// the migrations and anyone connected by hand.
    DatabaseConnections,
}

impl Key {
    /// Its row's key.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Key::ReconcilerFailing => "reconciler_failing",
            Key::ReconcilerBusy => "reconciler_busy",
            Key::LaunchFailed => "launch_failed",
            Key::JobsWaiting => "jobs_waiting",
            Key::RateLimit => "github_rate_limit",
            Key::DatabaseConnections => "database_connections",
        }
    }
}

/// The title of an alert's emails, by its row's key.
fn title(key: &str) -> &str {
    match key {
        "reconciler_failing" => "Reconciler failing",
        "reconciler_busy" => "Reconciler out of budget",
        "launch_failed" => "Launch failed for good",
        "jobs_waiting" => "Jobs waiting for a runner",
        "github_rate_limit" => "GitHub rate limit",
        "database_connections" => "Database connections near the limit",
        other => other,
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The alert fires, and why.
    Firing(String),
    /// Nothing is wrong.
    Clear,
    /// The check's data could not be read; the alert stays as it is.
    Unknown,
}

/// An alert's state, and what an email said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Firing,
    Resolved,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Firing => "firing",
            State::Resolved => "resolved",
        }
    }
}

/// A row of `alerts`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Alert {
    pub key: String,
    pub state: State,
    pub detail: String,
    pub fired_at: u64,
    pub resolved_at: Option<u64>,
    /// What the last email said, `None` before the first.
    pub sent: Option<State>,
}

impl Alert {
    /// The email due for this alert: firing when it fires and the
    /// inbox has not heard, resolved when it resolved after the inbox
    /// heard it fire.
    fn due(&self) -> Option<State> {
        match (self.state, self.sent) {
            (State::Firing, Some(State::Firing)) => None,
            (State::Firing, _) => Some(State::Firing),
            (State::Resolved, Some(State::Firing)) => Some(State::Resolved),
            (State::Resolved, _) => None,
        }
    }
}

/// The row `alert` becomes after a check found `seen` at `now`, or
/// `None` when it stays as it is.
fn observe(alert: Option<&Alert>, key: Key, seen: &Seen, now: u64) -> Option<Alert> {
    match (alert, seen) {
        (Some(alert), Seen::Firing(detail)) if alert.state == State::Firing => {
            (alert.detail != *detail).then(|| Alert {
                detail: detail.clone(),
                ..alert.clone()
            })
        }
        (_, Seen::Firing(detail)) => Some(Alert {
            key: key.as_str().to_owned(),
            state: State::Firing,
            detail: detail.clone(),
            // Strictly later than the incident before, so `fired_at`
            // tells the incidents of a key apart.
            fired_at: alert.map_or(now, |alert| now.max(alert.fired_at + 1)),
            resolved_at: None,
            // A resolution email that failed leaves `Firing` here, and
            // the new incident has not been announced.
            sent: alert
                .and_then(|alert| alert.sent)
                .filter(|sent| *sent != State::Firing),
        }),
        (Some(alert), Seen::Clear) if alert.state == State::Firing => Some(Alert {
            state: State::Resolved,
            resolved_at: Some(now),
            ..alert.clone()
        }),
        (_, Seen::Clear | Seen::Unknown) => None,
    }
}

/// An email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Email {
    pub subject: String,
    pub text: String,
}

/// The email `alert` is due, saying `state`.
fn notice(alert: &Alert, state: State, now: u64) -> Email {
    let title = title(&alert.key);
    match state {
        State::Firing => Email {
            subject: format!("[hatchrun] Firing: {title}"),
            text: format!(
                "{title}, since {}.\n\n{}\n",
                time::format(alert.fired_at),
                alert.detail
            ),
        },
        State::Resolved => Email {
            subject: format!("[hatchrun] Resolved: {title}"),
            text: format!(
                "{title} resolved at {} after {}.\n\nLast seen:\n{}\n",
                time::format(alert.resolved_at.unwrap_or(now)),
                duration(
                    alert
                        .resolved_at
                        .unwrap_or(now)
                        .saturating_sub(alert.fired_at)
                ),
                alert.detail
            ),
        },
    }
}

/// The daily email of alerts still firing.
fn digest(firing: &[&Alert], now: u64) -> Email {
    let mut text = format!("Alerts still firing at {}:\n", time::format(now));
    for alert in firing {
        let _ = write!(
            text,
            "\n{}, for {}:\n{}\n",
            title(&alert.key),
            duration(now.saturating_sub(alert.fired_at)),
            alert.detail
        );
    }
    Email {
        subject: format!("[hatchrun] Still firing: {} alerts", firing.len()),
        text,
    }
}

/// `ms` as `3 d 4 h`, `2 h 5 min` or `12 min`.
fn duration(ms: u64) -> String {
    let minutes = ms / MINUTE;
    let (days, hours, minutes) = (minutes / (24 * 60), minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else {
        format!("{minutes} min")
    }
}

/// Where alerts are kept.
pub(crate) trait Store {
    /// Every alert.
    async fn alerts(&self) -> Result<Vec<Alert>, String>;
    /// Writes the row `next`, which `prev` (`None` for a new alert)
    /// became after a check. Writes nothing and returns `false` when the
    /// stored row is no longer `prev`, because another pass changed it.
    async fn save(&self, prev: Option<&Alert>, next: &Alert, now: u64) -> Result<bool, String>;
    /// Claims the email saying `state` for the incident `alert` holds
    /// until `until`. Returns `false` when the row is not that incident
    /// or not due that email any more, or another pass holds a claim
    /// that has not ended at `now`.
    async fn claim(
        &self,
        alert: &Alert,
        state: State,
        now: u64,
        until: u64,
    ) -> Result<bool, String>;
    /// Notes that the email saying `state` went out for the incident
    /// `alert` holds and ends the claim `until`, whatever state the row
    /// is in by now. Returns `false`, and writes nothing, when the row no
    /// longer carries that claim or incident.
    async fn sent(&self, alert: &Alert, state: State, until: u64, now: u64)
    -> Result<bool, String>;
    /// Ends the claim `until` on `key` after an email that failed to
    /// send. Does nothing when the row no longer carries that claim.
    async fn release(&self, key: &str, until: u64) -> Result<(), String>;
    /// When the last digest went out.
    async fn digest_at(&self) -> Result<Option<u64>, String>;
    /// Records a digest at `at` if the last one is still `last`, the
    /// value [`Store::digest_at`] gave. Returns `false` when another
    /// pass recorded one first.
    async fn digest_claim(&self, last: Option<u64>, at: u64) -> Result<bool, String>;
    /// Takes back the digest recorded at `at`, so the one before it,
    /// `last`, stands again. Does nothing when another has been recorded
    /// since.
    async fn digest_undo(&self, last: Option<u64>, at: u64) -> Result<(), String>;
}

/// Where emails go, and the clock and timer that bound a send: the
/// `send_email` binding and the Worker's, or a list and a fake in tests.
pub(crate) trait Mail {
    /// Sends `email` to the alert inbox.
    async fn send(&self, email: &Email) -> Result<(), String>;
    /// The time now, in milliseconds.
    fn now(&self) -> u64;
    /// Completes after `ms` milliseconds.
    async fn sleep(&self, ms: u64);
}

/// Applies what the checks in `seen` found at `now` and sends the
/// emails that are due; with `digest`, also the daily digest. Returns
/// what went wrong, for the log. An email that fails stays due.
pub(crate) async fn settle(
    store: &impl Store,
    mail: &impl Mail,
    now: u64,
    seen: &[(Key, Seen)],
    digest: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut unsaved: Vec<&str> = Vec::new();
    let mut alerts = match store.alerts().await {
        Ok(alerts) => alerts,
        Err(err) => return vec![format!("alerts: {err}")],
    };
    for (key, seen) in seen {
        let at = alerts.iter().position(|alert| alert.key == key.as_str());
        let prev = at.map(|at| &alerts[at]);
        let Some(next) = observe(prev, *key, seen, now) else {
            continue;
        };
        match store.save(prev, &next, now).await {
            Ok(true) => {}
            // Another pass changed the row after this one read it. Its
            // snapshot is stale until the next pass reads it again.
            Ok(false) => {
                unsaved.push(key.as_str());
                continue;
            }
            Err(err) => {
                errors.push(format!("alert {}: {err}", key.as_str()));
                // A stored row in another state is stale until the next
                // pass observes it again. One that only lost a new
                // detail is not.
                if at.is_none_or(|at| alerts[at].state != next.state) {
                    unsaved.push(key.as_str());
                }
                continue;
            }
        }
        match at {
            Some(at) => alerts[at] = next,
            None => alerts.push(next),
        }
    }

    // Alerts the inbox already heard fire; one that fires now has its
    // own email.
    let heard = |alert: &&Alert| alert.state == State::Firing && alert.sent == Some(State::Firing);
    let still: Vec<Alert> = alerts
        .iter()
        .filter(heard)
        .filter(|alert| !unsaved.contains(&alert.key.as_str()))
        .cloned()
        .collect();
    // A digest without an alert whose row is stale would claim the day
    // and miss it until tomorrow, so the next pass sends it instead.
    let held_back = alerts
        .iter()
        .filter(heard)
        .any(|alert| unsaved.contains(&alert.key.as_str()));
    for alert in &alerts {
        if unsaved.contains(&alert.key.as_str()) {
            continue;
        }
        let Some(state) = alert.due() else {
            continue;
        };
        // Only the pass that wins the claim sends, so a pass racing this
        // one cannot send the same email. The claim starts when it is
        // made, not when the pass did: earlier sends may have taken time.
        let at = mail.now();
        let until = at + CLAIM_FOR;
        match store.claim(alert, state, at, until).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(err) => {
                errors.push(format!("alert {}: {err}", alert.key));
                continue;
            }
        }
        send_claimed(store, mail, alert, state, now, until, &mut errors).await;
    }
    if digest
        && !held_back
        && let Err(err) = send_digest(store, mail, now, &still).await
    {
        errors.push(format!("digest: {err}"));
    }
    errors
}

/// Sends the email saying `state` for `alert`, whose claim `until` the
/// pass holds, and records it. Errors are added to `errors`.
async fn send_claimed(
    store: &impl Store,
    mail: &impl Mail,
    alert: &Alert,
    state: State,
    now: u64,
    until: u64,
    errors: &mut Vec<String>,
) {
    // Claiming took time. With too little of the claim left for the
    // send and for recording it, give it back for the next pass.
    if until.saturating_sub(mail.now()) < SEND_FOR + RECORD_FOR {
        errors.push(format!(
            "alert {}: claim too close to its end to send",
            alert.key
        ));
        if let Err(err) = store.release(&alert.key, until).await {
            errors.push(format!("alert {}: {err}", alert.key));
        }
        return;
    }
    let Some(result) = send_bounded(mail, &notice(alert, state, now)).await else {
        // The email may have gone out. The claim is left to run
        // out, so no pass sends it while this one still could.
        errors.push(format!(
            "alert {}: email: no answer in {} s, claim left to run out",
            alert.key,
            SEND_FOR / 1000
        ));
        return;
    };
    if let Err(err) = result {
        errors.push(format!("alert {}: email: {err}", alert.key));
        if let Err(err) = store.release(&alert.key, until).await {
            errors.push(format!("alert {}: {err}", alert.key));
        }
        return;
    }
    // The email is out. If both writes fail the claim runs out and
    // the email goes again: delivery is at-least-once here.
    let at = mail.now();
    let (noted, retried) = match store.sent(alert, state, until, at).await {
        Ok(noted) => (Ok(noted), false),
        Err(_) => (store.sent(alert, state, until, at).await, true),
    };
    match noted {
        Ok(true) => {}
        // The first write may have committed before it errored, and
        // then there is no claim left for the retry to find.
        Ok(false) if retried => errors.push(format!(
            "alert {}: first write of the record failed, the retry found the claim ended",
            alert.key
        )),
        Ok(false) => errors.push(format!(
            "alert {}: email sent after its claim was lost, it may go out again",
            alert.key
        )),
        Err(err) => errors.push(format!("alert {}: {err}", alert.key)),
    }
}

/// Sends `email`, or returns `None` when it is not answered within
/// [`SEND_FOR`].
async fn send_bounded(mail: &impl Mail, email: &Email) -> Option<Result<(), String>> {
    match select(pin!(mail.send(email)), pin!(mail.sleep(SEND_FOR))).await {
        Either::Left((result, _)) => Some(result),
        Either::Right(((), _)) => None,
    }
}

/// Sends the digest of `firing` once a day. With nothing firing the day
/// starts again, so an alert that fires later waits up to a day for its
/// first digest rather than getting one minutes after its own email.
/// The day is claimed before the send, so only one pass sends; a failed
/// send gives it back, one that got no answer keeps it.
async fn send_digest(
    store: &impl Store,
    mail: &impl Mail,
    now: u64,
    firing: &[Alert],
) -> Result<(), String> {
    let last = store.digest_at().await?;
    if last.is_some_and(|last| now < last + DAY) {
        return Ok(());
    }
    let claimed = store.digest_claim(last, now).await?;
    if !claimed || last.is_none() || firing.is_empty() {
        return Ok(());
    }
    let firing: Vec<&Alert> = firing.iter().collect();
    match send_bounded(mail, &digest(&firing, now)).await {
        Some(Ok(())) => Ok(()),
        Some(Err(err)) => {
            // A claim left in place holds the digest back for a day.
            let undone = match store.digest_undo(last, now).await {
                Err(_) => store.digest_undo(last, now).await,
                undone => undone,
            };
            Err(match undone {
                Ok(()) => err,
                Err(undo) => format!("{err}; the digest claim could not be undone: {undo}"),
            })
        }
        None => Err(format!("no answer in {} s", SEND_FOR / 1000)),
    }
}
