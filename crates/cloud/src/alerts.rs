// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Alerts by email on the reconciler, launches, GitHub's rate limit and
//! the database's size.
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

use serde::{Deserialize, Serialize};
use worker::{D1Database, Env, SendEmail, SendEmailBuilder, console_error};

use crate::fetch::Counted;
use crate::github::RateLimit;
use crate::reconcile::{VERIFIED, time};
use crate::{changes, int};

/// The `send_email` binding in `wrangler.toml`.
const BINDING: &str = "ALERTS";
/// The variable with the inbox alerts go to.
const TO: &str = "ALERT_EMAIL";
/// The sender, on the subdomain onboarded to Email Sending.
const FROM: &str = "alerts@notify.hatchrun.io";

const MINUTE: u64 = 60 * 1000;
const DAY: u64 = 24 * 60 * MINUTE;
/// Bytes in a gigabyte, as D1's limits count them.
const GB: usize = 1_000_000_000;

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
    /// The database is past 4 GB: `JobStore` (#103) is due.
    Size4,
    /// Past 5 GB: sharding the jobs (#104) is due.
    Size5,
    /// Past 8 GB, near D1's 10 GB limit.
    Size8,
}

/// The size alerts, with the size in GB each fires at and what it
/// means.
const SIZES: [(Key, usize, &str); 3] = [
    (
        Key::Size4,
        4,
        "Time to move job storage behind JobStore (#103).",
    ),
    (Key::Size5, 5, "Time to shard the jobs (#104)."),
    (
        Key::Size8,
        8,
        "D1 stops taking writes at 10 GB; the jobs need to move now.",
    ),
];

impl Key {
    /// Its row's key.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Key::ReconcilerFailing => "reconciler_failing",
            Key::ReconcilerBusy => "reconciler_busy",
            Key::LaunchFailed => "launch_failed",
            Key::JobsWaiting => "jobs_waiting",
            Key::RateLimit => "github_rate_limit",
            Key::Size4 => "d1_size_4gb",
            Key::Size5 => "d1_size_5gb",
            Key::Size8 => "d1_size_8gb",
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
        "d1_size_4gb" => "D1 past 4 GB",
        "d1_size_5gb" => "D1 past 5 GB",
        "d1_size_8gb" => "D1 past 8 GB",
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
            fired_at: now,
            resolved_at: None,
            sent: alert.and_then(|alert| alert.sent),
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

/// Where alerts are kept: D1, or SQLite in tests.
pub(crate) trait Store {
    /// Every alert.
    async fn alerts(&self) -> Result<Vec<Alert>, String>;
    /// Writes an alert's row.
    async fn save(&self, alert: &Alert, now: u64) -> Result<(), String>;
    /// When the last digest went out.
    async fn digest_at(&self) -> Result<Option<u64>, String>;
    /// Notes that a digest went out at `at`.
    async fn digested(&self, at: u64) -> Result<(), String>;
}

/// Where emails go: the `send_email` binding, or a list in tests.
pub(crate) trait Mail {
    /// Sends `email` to the alert inbox.
    async fn send(&self, email: &Email) -> Result<(), String>;
}

/// Every alert.
const ALERTS: &str = "SELECT key, state, detail, fired_at, resolved_at, sent FROM alerts";

/// Writes an alert's row.
const SAVE: &str = "INSERT INTO alerts (key, state, detail, fired_at, resolved_at, sent, \
     updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
     ON CONFLICT (key) DO UPDATE SET state = excluded.state, detail = excluded.detail, \
     fired_at = excluded.fired_at, resolved_at = excluded.resolved_at, sent = excluded.sent, \
     updated_at = excluded.updated_at";

/// When the last digest went out.
const DIGEST_AT: &str = "SELECT digest_at FROM reconciler WHERE id = 1";

/// Notes that a digest went out at `?1`.
const DIGESTED: &str = "UPDATE reconciler SET digest_at = ?1 WHERE id = 1";

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
    let mut alerts = match store.alerts().await {
        Ok(alerts) => alerts,
        Err(err) => return vec![format!("alerts: {err}")],
    };
    for (key, seen) in seen {
        let at = alerts.iter().position(|alert| alert.key == key.as_str());
        let Some(next) = observe(at.map(|at| &alerts[at]), *key, seen, now) else {
            continue;
        };
        if let Err(err) = store.save(&next, now).await {
            errors.push(format!("alert {}: {err}", key.as_str()));
            continue;
        }
        match at {
            Some(at) => alerts[at] = next,
            None => alerts.push(next),
        }
    }

    // Alerts the inbox already heard fire; one that fires now has its
    // own email.
    let still: Vec<Alert> = alerts
        .iter()
        .filter(|alert| alert.state == State::Firing && alert.sent == Some(State::Firing))
        .cloned()
        .collect();
    for alert in &mut alerts {
        let Some(state) = alert.due() else {
            continue;
        };
        if let Err(err) = mail.send(&notice(alert, state, now)).await {
            errors.push(format!("alert {}: email: {err}", alert.key));
            continue;
        }
        alert.sent = Some(state);
        if let Err(err) = store.save(alert, now).await {
            errors.push(format!("alert {}: {err}", alert.key));
        }
    }
    if digest && let Err(err) = send_digest(store, mail, now, &still).await {
        errors.push(format!("digest: {err}"));
    }
    errors
}

/// Sends the digest of `firing` once a day. With nothing firing the day
/// starts again, so an alert that fires later waits up to a day for its
/// first digest rather than getting one minutes after its own email.
async fn send_digest(
    store: &impl Store,
    mail: &impl Mail,
    now: u64,
    firing: &[Alert],
) -> Result<(), String> {
    let at = store.digest_at().await?;
    if at.is_some_and(|at| now < at + DAY) {
        return Ok(());
    }
    if at.is_some() && !firing.is_empty() {
        let firing: Vec<&Alert> = firing.iter().collect();
        mail.send(&digest(&firing, now)).await?;
    }
    store.digested(now).await
}

/// One pass, as the `reconciler` row remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PassRecord {
    /// When it started.
    pub at: u64,
    /// Steps or tenants that failed.
    pub errors: u32,
    /// Whether it stopped at a limit with work left.
    pub more: bool,
}

/// `history` with `pass` added, newest last, [`HISTORY`] at most.
pub(crate) fn remember(mut history: Vec<PassRecord>, pass: PassRecord) -> Vec<PassRecord> {
    history.push(pass);
    let extra = history.len().saturating_sub(HISTORY);
    history.drain(..extra);
    history
}

/// The last `count` passes, when there are that many and each says
/// `test`.
fn in_a_row(history: &[PassRecord], count: usize, test: impl Fn(&PassRecord) -> bool) -> bool {
    history.len() >= count && history[history.len() - count..].iter().all(test)
}

/// Whether the last passes all had errors.
fn failing(history: &[PassRecord]) -> bool {
    in_a_row(history, FAILING_PASSES, |pass| pass.errors > 0)
}

/// A verified tenant with jobs waiting for a runner.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Waiting {
    pub org: String,
    pub jobs: u32,
    pub oldest: u64,
}

/// A row of `failed_launches`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct FailedLaunch {
    pub job_id: Option<u64>,
    pub installation_id: Option<u64>,
    pub error: Option<String>,
    pub attempts: Option<u32>,
    pub failed_at: u64,
}

/// A row of `rate_limits`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LowRateLimit {
    pub installation_id: u64,
    pub remaining: Option<u32>,
    pub limited: u8,
}

/// What the checks after a pass read; `None` where the read failed.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    /// The last passes, newest last, this one among them.
    pub history: Option<Vec<PassRecord>>,
    /// The database's size after this pass's retention deletes.
    pub db_bytes: Option<usize>,
    pub waiting: Option<Vec<Waiting>>,
    pub failed: Option<Vec<FailedLaunch>>,
    pub rate_limits: Option<Vec<LowRateLimit>>,
}

/// What each alert's check finds in `facts`. The reconciler stopping is
/// not among them; `/healthz` tells it to the uptime monitor.
pub(crate) fn check(facts: &Facts) -> Vec<(Key, Seen)> {
    let mut seen = Vec::new();
    let history = facts.history.as_deref();
    seen.push((
        Key::ReconcilerFailing,
        history.map_or(Seen::Unknown, |history| {
            if failing(history) {
                let errors: Vec<String> = history[history.len() - FAILING_PASSES..]
                    .iter()
                    .map(|pass| pass.errors.to_string())
                    .collect();
                Seen::Firing(format!(
                    "The last {FAILING_PASSES} passes had errors: {}. The log has each.",
                    errors.join(", ")
                ))
            } else {
                Seen::Clear
            }
        }),
    ));
    seen.push((
        Key::ReconcilerBusy,
        history.map_or(Seen::Unknown, |history| {
            if in_a_row(history, BUSY_PASSES, |pass| pass.more) {
                Seen::Firing(format!(
                    "The last {BUSY_PASSES} passes stopped at a limit with work left. \
                     Time for a reconcile queue per tenant (#105)."
                ))
            } else {
                Seen::Clear
            }
        }),
    ));
    seen.push((
        Key::LaunchFailed,
        facts
            .failed
            .as_deref()
            .map_or(Seen::Unknown, failed_launches),
    ));
    seen.push((
        Key::JobsWaiting,
        facts.waiting.as_deref().map_or(Seen::Unknown, jobs_waiting),
    ));
    seen.push((
        Key::RateLimit,
        facts
            .rate_limits
            .as_deref()
            .map_or(Seen::Unknown, rate_limited),
    ));
    for (key, gb, meaning) in SIZES {
        seen.push((
            key,
            facts.db_bytes.map_or(Seen::Unknown, |bytes| {
                if bytes >= gb * GB {
                    Seen::Firing(format!(
                        "The database is {}.{} GB. {meaning}",
                        bytes / GB,
                        bytes % GB / (GB / 10)
                    ))
                } else {
                    Seen::Clear
                }
            }),
        ));
    }
    seen
}

/// The jobs waiting alert, from the tenants with jobs waiting.
fn jobs_waiting(waiting: &[Waiting]) -> Seen {
    if waiting.is_empty() {
        return Seen::Clear;
    }
    let lines: Vec<String> = waiting
        .iter()
        .map(|tenant| {
            format!(
                "{}: {} jobs, the oldest queued at {}",
                tenant.org,
                tenant.jobs,
                time::format(tenant.oldest)
            )
        })
        .collect();
    Seen::Firing(format!(
        "Jobs queued over {} min with no runner launching:\n{}",
        WAITING / MINUTE,
        lines.join("\n")
    ))
}

/// The rate limit alert, from the installations found low lately.
fn rate_limited(limits: &[LowRateLimit]) -> Seen {
    if limits.is_empty() {
        return Seen::Clear;
    }
    let lines: Vec<String> = limits
        .iter()
        .map(|limit| match (limit.limited, limit.remaining) {
            (1, _) => format!("installation {}: rate limited", limit.installation_id),
            (_, Some(left)) => format!(
                "installation {}: {left} requests left",
                limit.installation_id
            ),
            (_, None) => format!("installation {}: low", limit.installation_id),
        })
        .collect();
    Seen::Firing(format!(
        "GitHub answers in the last {} min:\n{}",
        RATE_LIMITS_FOR / MINUTE,
        lines.join("\n")
    ))
}

/// The failed launch alert, from the latest failures.
fn failed_launches(failed: &[FailedLaunch]) -> Seen {
    if failed.is_empty() {
        return Seen::Clear;
    }
    let known = |value: Option<u64>| value.map_or_else(|| "?".to_owned(), |id| id.to_string());
    let lines: Vec<String> = failed
        .iter()
        .map(|launch| {
            format!(
                "job {} of installation {}, {} attempts, at {}: {}",
                known(launch.job_id),
                known(launch.installation_id),
                known(launch.attempts.map(u64::from)),
                time::format(launch.failed_at),
                launch.error.as_deref().unwrap_or("no error recorded")
            )
        })
        .collect();
    Seen::Firing(format!(
        "Launch messages reached hatchrun-launches-dlq in the last {} min{}:\n{}\n\n\
         The reconciler launches their jobs again; the error says why the queue could not.",
        FAILED_LAUNCHES_FOR / MINUTE,
        if failed.len() == LISTED as usize {
            ", the latest of them"
        } else {
            ""
        },
        lines.join("\n")
    ))
}

/// Verified tenants with jobs queued before `?1` that no runner is
/// launching for, while they have room for another runner, at most
/// `?2`. A job waiting for a backoff counts; one waiting for its owner
/// to finish the setup does not. Reads `jobs_open` per tenant.
pub(crate) fn waiting_query() -> String {
    format!(
        "SELECT tenants.org_login AS org, COUNT(*) AS jobs, MIN(jobs.queued_at) AS oldest \
         FROM tenants JOIN jobs ON jobs.installation_id = tenants.installation_id \
         WHERE {VERIFIED} AND jobs.status <> 'completed' AND jobs.status = 'queued' \
         AND (jobs.launch_state IS NULL OR jobs.launch_retry = 'backoff') \
         AND jobs.queued_at < ?1 \
         AND (SELECT COUNT(*) FROM jobs AS busy \
           WHERE busy.installation_id = tenants.installation_id \
           AND busy.status <> 'completed' \
           AND busy.launch_state IN ('launching', 'launched')) < tenants.max_runners \
         GROUP BY tenants.id ORDER BY oldest LIMIT ?2"
    )
}

/// Launches that failed since `?1`, newest first, at most `?2`.
pub(crate) const FAILED_SINCE: &str = "SELECT job_id, installation_id, error, attempts, \
     failed_at FROM failed_launches WHERE failed_at >= ?1 ORDER BY failed_at DESC LIMIT ?2";

/// Installations whose rate limit was low since `?1`, at most `?2`.
pub(crate) const LOW_SINCE: &str = "SELECT installation_id, remaining, limited FROM rate_limits \
     WHERE seen_at >= ?1 ORDER BY seen_at DESC LIMIT ?2";

/// Notes an installation's low rate limit at `?4`.
pub(crate) const NOTE_RATE_LIMIT: &str = "INSERT INTO rate_limits \
     (installation_id, remaining, limited, seen_at) VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT (installation_id) DO UPDATE SET remaining = excluded.remaining, \
     limited = excluded.limited, seen_at = excluded.seen_at";

/// Records a failed launch message `?1`, with its job's error and
/// attempts, once.
pub(crate) const RECORD_FAILED: &str = "INSERT INTO failed_launches \
     (message_id, job_id, installation_id, error, attempts, failed_at) \
     VALUES (?1, ?2, ?3, (SELECT launch_error FROM jobs WHERE id = ?2), \
       (SELECT launch_attempts FROM jobs WHERE id = ?2), ?4) \
     ON CONFLICT (message_id) DO NOTHING";

/// Deletes up to `?2` failed launches recorded before `?1`.
pub(crate) const DELETE_FAILED: &str = "DELETE FROM failed_launches WHERE id IN \
     (SELECT id FROM failed_launches WHERE failed_at < ?1 LIMIT ?2)";

/// Deletes rate limits seen before `?1`.
pub(crate) const DELETE_RATE_LIMITS: &str = "DELETE FROM rate_limits WHERE seen_at < ?1";

/// The `reconciler` row as `/healthz` reads it.
pub(crate) const HEALTH: &str = "SELECT finished_at, history FROM reconciler WHERE id = 1";

/// A row of [`HEALTH`].
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HealthRow {
    pub finished_at: Option<u64>,
    pub history: Option<String>,
}

/// A pass history as the `reconciler` row keeps it; an unreadable one
/// is empty.
pub(crate) fn history(text: Option<&str>) -> Vec<PassRecord> {
    text.and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}

/// `/healthz`'s status and body at `now`: 200 while the last pass is
/// recent and the passes are not all failing, 503 otherwise.
pub(crate) fn health(row: &HealthRow, now: u64) -> (u16, serde_json::Value) {
    let age = row.finished_at.map(|at| now.saturating_sub(at));
    let status = if age.is_none_or(|age| age > STALE) {
        "stale"
    } else if failing(&history(row.history.as_deref())) {
        "failing"
    } else {
        "ok"
    };
    let body = serde_json::json!({
        "status": status,
        "lastPass": row.finished_at.map(time::format),
        "ageSeconds": age.map(|age| age / 1000),
    });
    (if status == "ok" { 200 } else { 503 }, body)
}

/// Alerts kept in D1.
pub(crate) struct D1<'a>(pub &'a D1Database);

impl Store for D1<'_> {
    async fn alerts(&self) -> Result<Vec<Alert>, String> {
        self.0
            .prepare(ALERTS)
            .all()
            .await
            .map_err(text)?
            .results::<Alert>()
            .map_err(text)
    }

    async fn save(&self, alert: &Alert, now: u64) -> Result<(), String> {
        let resolved_at = match alert.resolved_at {
            Some(at) => int(at).map_err(text)?,
            None => wasm_bindgen::JsValue::NULL,
        };
        self.0
            .prepare(SAVE)
            .bind(&[
                alert.key.as_str().into(),
                alert.state.as_str().into(),
                alert.detail.as_str().into(),
                int(alert.fired_at).map_err(text)?,
                resolved_at,
                alert
                    .sent
                    .map_or(wasm_bindgen::JsValue::NULL, |sent| sent.as_str().into()),
                int(now).map_err(text)?,
            ])
            .map_err(text)?
            .run()
            .await
            .map_err(text)?;
        Ok(())
    }

    async fn digest_at(&self) -> Result<Option<u64>, String> {
        self.0
            .prepare(DIGEST_AT)
            .first::<Option<u64>>(Some("digest_at"))
            .await
            .map(Option::flatten)
            .map_err(text)
    }

    async fn digested(&self, at: u64) -> Result<(), String> {
        self.0
            .prepare(DIGESTED)
            .bind(&[int(at).map_err(text)?])
            .map_err(text)?
            .run()
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
            .var(TO)
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
pub(crate) async fn after_pass(
    env: &Env,
    db: &D1Database,
    now: u64,
    history: Option<Vec<PassRecord>>,
    db_bytes: Option<usize>,
) {
    let facts = Facts {
        history,
        db_bytes,
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
    for err in settle(&D1(db), &Inbox::new(env), now, &check(&facts), true).await {
        console_error!("alerts: {err}");
    }
}

/// Records failed launches from the dead-letter queue, as
/// `(message id, job id, installation id)`, and fires their alert.
/// Returns whether they were recorded; the alert's errors are logged.
pub(crate) async fn launches_failed(
    env: &Env,
    db: &D1Database,
    now: u64,
    failed: &[(String, Option<u64>, Option<u64>)],
) -> Result<(), String> {
    if failed.is_empty() {
        return Ok(());
    }
    let id = |value: Option<u64>| value.map_or(Ok(wasm_bindgen::JsValue::NULL), int);
    let mut statements = Vec::with_capacity(failed.len());
    for (message, job, installation) in failed {
        statements.push(
            db.prepare(RECORD_FAILED)
                .bind(&[
                    message.as_str().into(),
                    id(*job).map_err(text)?,
                    id(*installation).map_err(text)?,
                    int(now).map_err(text)?,
                ])
                .map_err(text)?,
        );
    }
    let recorded = db.batch(statements).await.map_err(text)?;
    if recorded.iter().all(|result| changes(result) == 0) {
        return Ok(());
    }
    let Some(failed) = logged("failed launches", recent_failures(db, now).await) else {
        return Ok(());
    };
    let seen = [(Key::LaunchFailed, failed_launches(&failed))];
    for err in settle(&D1(db), &Inbox::new(env), now, &seen, false).await {
        console_error!("alerts: {err}");
    }
    Ok(())
}

/// Notes the installations a step's or launch's GitHub calls found low
/// on requests. Errors are logged.
pub(crate) async fn note_rate_limits(db: &D1Database, calls: &Counted, now: u64) {
    let low = calls.low();
    if low.is_empty() {
        return;
    }
    let statement = |(installation_id, limit): (u64, RateLimit)| {
        db.prepare(NOTE_RATE_LIMIT).bind(&[
            int(installation_id)?,
            limit
                .remaining
                .map_or(wasm_bindgen::JsValue::NULL, Into::into),
            u32::from(limit.limited).into(),
            int(now)?,
        ])
    };
    let noted = match low
        .into_iter()
        .map(statement)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(statements) => db.batch(statements).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = noted {
        console_error!("alerts: rate limits: {err}");
    }
}

async fn recent_failures(db: &D1Database, now: u64) -> Result<Vec<FailedLaunch>, String> {
    since(db, FAILED_SINCE, now.saturating_sub(FAILED_LAUNCHES_FOR)).await
}

/// The rows of a query that takes a time, `?1`, and at most [`LISTED`]
/// rows, `?2`.
async fn since<T: for<'de> Deserialize<'de>>(
    db: &D1Database,
    sql: &str,
    at: u64,
) -> Result<Vec<T>, String> {
    db.prepare(sql)
        .bind(&[int(at).map_err(text)?, LISTED.into()])
        .map_err(text)?
        .all()
        .await
        .map_err(text)?
        .results::<T>()
        .map_err(text)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::testdb::{Db, block_on, params};

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 60 * MINUTE;
    const NONE: [&str; 0] = [];

    // The Worker's statements against SQLite; it answers at once.
    #[allow(clippy::unused_async_trait_impl)]
    impl Store for Db {
        async fn alerts(&self) -> Result<Vec<Alert>, String> {
            self.try_all(ALERTS, &[])
        }

        async fn save(&self, alert: &Alert, now: u64) -> Result<(), String> {
            self.try_run(
                SAVE,
                &params![
                    alert.key.as_str(),
                    alert.state.as_str(),
                    alert.detail.as_str(),
                    alert.fired_at,
                    alert.resolved_at,
                    alert.sent.map(State::as_str),
                    now
                ],
            )
            .map(|_| ())
        }

        async fn digest_at(&self) -> Result<Option<u64>, String> {
            Ok(self
                .column::<Option<u64>>(DIGEST_AT, &[], "digest_at")
                .flatten())
        }

        async fn digested(&self, at: u64) -> Result<(), String> {
            self.try_run(DIGESTED, &params![at]).map(|_| ())
        }
    }

    /// Collects emails, or fails them while `down`.
    #[derive(Default)]
    struct Outbox {
        sent: RefCell<Vec<Email>>,
        down: Cell<bool>,
    }

    #[allow(clippy::unused_async_trait_impl)]
    impl Mail for Outbox {
        async fn send(&self, email: &Email) -> Result<(), String> {
            if self.down.get() {
                return Err("Email Service unavailable".to_owned());
            }
            self.sent.borrow_mut().push(email.clone());
            Ok(())
        }
    }

    impl Outbox {
        /// The subjects sent since the last call.
        fn take(&self) -> Vec<String> {
            self.sent
                .borrow_mut()
                .drain(..)
                .map(|email| email.subject)
                .collect()
        }
    }

    fn pass(db: &Db, outbox: &Outbox, now: u64, seen: &[(Key, Seen)]) -> Vec<String> {
        block_on(settle(db, outbox, now, seen, true))
    }

    fn firing(detail: &str) -> Seen {
        Seen::Firing(detail.to_owned())
    }

    fn found(seen: &[(Key, Seen)], key: Key) -> &Seen {
        &seen.iter().find(|(at, _)| *at == key).unwrap().1
    }

    fn passes(errors: &[u32], more: bool) -> Vec<PassRecord> {
        errors
            .iter()
            .enumerate()
            .map(|(at, errors)| PassRecord {
                at: at as u64,
                errors: *errors,
                more,
            })
            .collect()
    }

    fn alert(db: &Db, key: Key) -> Alert {
        db.first(
            "SELECT key, state, detail, fired_at, resolved_at, sent FROM alerts WHERE key = ?1",
            &params![key.as_str()],
        )
        .unwrap()
    }

    #[test]
    fn the_history_keeps_the_last_passes() {
        let mut history = Vec::new();
        for at in 0..15 {
            history = remember(
                history,
                PassRecord {
                    at,
                    errors: 0,
                    more: false,
                },
            );
        }
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history[0].at, 5);
        assert_eq!(history[HISTORY - 1].at, 14);
        // As the reconciler row keeps it.
        let text = serde_json::to_string(&history).unwrap();
        assert_eq!(super::history(Some(&text)), history);
        assert_eq!(super::history(Some("not json")), []);
        assert_eq!(super::history(None), []);
    }

    #[test]
    fn checks_read_the_pass_history() {
        let facts = |history: Vec<PassRecord>| Facts {
            history: Some(history),
            ..Facts::default()
        };
        let seen = check(&facts(passes(&[0, 2, 1, 3, 1, 1], false)));
        assert!(matches!(
            found(&seen, Key::ReconcilerFailing),
            Seen::Firing(detail) if detail.contains("2, 1, 3, 1, 1")
        ));
        assert_eq!(*found(&seen, Key::ReconcilerBusy), Seen::Clear);
        // One clean pass among the last five clears it.
        let seen = check(&facts(passes(&[1, 1, 1, 0, 1, 1], false)));
        assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);
        // Too few passes to tell.
        let seen = check(&facts(passes(&[1, 1, 1, 1], false)));
        assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);

        let seen = check(&facts(passes(&[0; 10], true)));
        assert!(
            matches!(found(&seen, Key::ReconcilerBusy), Seen::Firing(detail) if detail.contains("#105"))
        );
        let seen = check(&facts(passes(&[0; 9], true)));
        assert_eq!(*found(&seen, Key::ReconcilerBusy), Seen::Clear);

        // Data that could not be read changes nothing.
        let seen = check(&Facts::default());
        assert!(seen.iter().all(|(_, seen)| *seen == Seen::Unknown));
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn checks_read_the_database_and_github() {
        let facts = Facts {
            db_bytes: Some(5_400_000_000),
            waiting: Some(vec![Waiting {
                org: "acme".to_owned(),
                jobs: 3,
                oldest: NOW - 20 * MINUTE,
            }]),
            failed: Some(vec![FailedLaunch {
                job_id: Some(5),
                installation_id: Some(42),
                error: Some("RunInstances: InsufficientInstanceCapacity".to_owned()),
                attempts: Some(4),
                failed_at: NOW,
            }]),
            rate_limits: Some(vec![
                LowRateLimit {
                    installation_id: 42,
                    remaining: Some(0),
                    limited: 1,
                },
                LowRateLimit {
                    installation_id: 43,
                    remaining: Some(1200),
                    limited: 0,
                },
            ]),
            ..Facts::default()
        };
        let seen = check(&facts);
        assert!(
            matches!(found(&seen, Key::Size4), Seen::Firing(detail) if detail.contains("5.4 GB") && detail.contains("#103"))
        );
        assert!(
            matches!(found(&seen, Key::Size5), Seen::Firing(detail) if detail.contains("#104"))
        );
        assert_eq!(*found(&seen, Key::Size8), Seen::Clear);
        assert!(
            matches!(found(&seen, Key::JobsWaiting), Seen::Firing(detail) if detail.contains("acme: 3 jobs"))
        );
        assert!(matches!(
            found(&seen, Key::LaunchFailed),
            Seen::Firing(detail) if detail.contains("job 5 of installation 42, 4 attempts")
                && detail.contains("InsufficientInstanceCapacity")
        ));
        assert!(matches!(
            found(&seen, Key::RateLimit),
            Seen::Firing(detail) if detail.contains("installation 42: rate limited")
                && detail.contains("installation 43: 1200 requests left")
        ));

        let clear = check(&Facts {
            db_bytes: Some(3_999_999_999),
            waiting: Some(Vec::new()),
            failed: Some(Vec::new()),
            rate_limits: Some(Vec::new()),
            ..Facts::default()
        });
        for key in [
            Key::Size4,
            Key::Size5,
            Key::Size8,
            Key::JobsWaiting,
            Key::LaunchFailed,
            Key::RateLimit,
        ] {
            assert_eq!(*found(&clear, key), Seen::Clear, "{key:?}");
        }
    }

    #[test]
    fn an_alert_emails_once_when_it_fires_and_once_when_it_resolves() {
        let db = Db::new();
        let outbox = Outbox::default();
        let key = Key::ReconcilerFailing;
        assert_eq!(pass(&db, &outbox, NOW, &[(key, firing("errors"))]), NONE);
        assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
        // Still firing: no email, the detail follows the last check.
        pass(&db, &outbox, NOW + MINUTE, &[(key, firing("more errors"))]);
        pass(&db, &outbox, NOW + 2 * MINUTE, &[(key, Seen::Unknown)]);
        assert_eq!(outbox.take(), NONE);
        let row = alert(&db, key);
        assert_eq!(row.detail, "more errors");
        assert_eq!(row.fired_at, NOW);

        pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
        assert_eq!(outbox.take(), ["[hatchrun] Resolved: Reconciler failing"]);
        pass(&db, &outbox, NOW + HOUR + MINUTE, &[(key, Seen::Clear)]);
        assert_eq!(outbox.take(), NONE);
        let row = alert(&db, key);
        assert_eq!(row.state, State::Resolved);
        assert_eq!(row.resolved_at, Some(NOW + HOUR));
        assert_eq!(row.sent, Some(State::Resolved));

        // It fires again, from the new time.
        pass(&db, &outbox, NOW + 2 * HOUR, &[(key, firing("errors"))]);
        assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
        assert_eq!(alert(&db, key).fired_at, NOW + 2 * HOUR);
    }

    #[test]
    fn the_emails_say_what_and_since_when() {
        let fired = Alert {
            key: Key::JobsWaiting.as_str().to_owned(),
            state: State::Firing,
            detail: "acme: 3 jobs".to_owned(),
            fired_at: NOW,
            resolved_at: None,
            sent: None,
        };
        let email = notice(&fired, State::Firing, NOW);
        assert_eq!(
            email.subject,
            "[hatchrun] Firing: Jobs waiting for a runner"
        );
        assert!(email.text.contains("since 2027-01-15T08:00:00Z"));
        assert!(email.text.contains("acme: 3 jobs"));
        let resolved = Alert {
            state: State::Resolved,
            resolved_at: Some(NOW + 2 * HOUR + 5 * MINUTE),
            ..fired
        };
        let email = notice(&resolved, State::Resolved, NOW);
        assert!(email.text.contains("after 2 h 5 min"));
        assert_eq!(duration(3 * DAY + 4 * HOUR), "3 d 4 h");
        assert_eq!(duration(12 * MINUTE + 5000), "12 min");
    }

    #[test]
    fn a_failed_send_is_tried_on_the_next_pass() {
        let db = Db::new();
        let outbox = Outbox::default();
        let key = Key::LaunchFailed;
        outbox.down.set(true);
        let errors = pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
        assert!(errors[0].contains("unavailable"), "{errors:?}");
        let row = alert(&db, key);
        assert_eq!((row.state, row.sent), (State::Firing, None));

        outbox.down.set(false);
        assert_eq!(
            pass(&db, &outbox, NOW + MINUTE, &[(key, Seen::Unknown)]),
            NONE
        );
        assert_eq!(outbox.take(), ["[hatchrun] Firing: Launch failed for good"]);
        assert_eq!(alert(&db, key).fired_at, NOW);

        // A resolve that fails goes out later too.
        outbox.down.set(true);
        pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
        outbox.down.set(false);
        pass(&db, &outbox, NOW + HOUR + MINUTE, &[]);
        assert_eq!(
            outbox.take(),
            ["[hatchrun] Resolved: Launch failed for good"]
        );
    }

    #[test]
    fn an_alert_that_resolves_before_its_email_sends_nothing() {
        let db = Db::new();
        let outbox = Outbox::default();
        let key = Key::RateLimit;
        outbox.down.set(true);
        pass(&db, &outbox, NOW, &[(key, firing("installation 42"))]);
        pass(&db, &outbox, NOW + MINUTE, &[(key, Seen::Clear)]);
        outbox.down.set(false);
        assert_eq!(
            pass(&db, &outbox, NOW + 2 * MINUTE, &[(key, Seen::Clear)]),
            NONE
        );
        assert_eq!(outbox.take(), NONE);
    }

    #[test]
    fn firing_alerts_go_out_in_one_digest_a_day() {
        let db = Db::new();
        let outbox = Outbox::default();
        // The first pass starts the day.
        pass(&db, &outbox, NOW, &[]);
        pass(&db, &outbox, NOW + HOUR, &[(Key::Size4, firing("4.1 GB"))]);
        pass(
            &db,
            &outbox,
            NOW + HOUR,
            &[(Key::JobsWaiting, firing("acme"))],
        );
        assert_eq!(outbox.take().len(), 2);
        pass(&db, &outbox, NOW + 2 * HOUR, &[]);
        assert_eq!(outbox.take(), NONE);

        // A day on, one digest with both; one that fires in the same
        // pass has its own email instead.
        outbox.down.set(true);
        let errors = pass(&db, &outbox, NOW + DAY, &[(Key::Size5, firing("5.1 GB"))]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        outbox.down.set(false);
        pass(&db, &outbox, NOW + DAY + MINUTE, &[]);
        let sent = outbox.sent.borrow_mut().drain(..).collect::<Vec<_>>();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].subject, "[hatchrun] Firing: D1 past 5 GB");
        assert_eq!(sent[1].subject, "[hatchrun] Still firing: 2 alerts");
        assert!(sent[1].text.contains("D1 past 4 GB, for 23 h 1 min"));
        assert!(sent[1].text.contains("Jobs waiting for a runner"));
        pass(&db, &outbox, NOW + DAY + 2 * MINUTE, &[]);
        assert_eq!(outbox.take(), NONE);

        // The next day's has all three.
        pass(&db, &outbox, NOW + 2 * DAY + MINUTE, &[]);
        assert_eq!(outbox.take(), ["[hatchrun] Still firing: 3 alerts"]);

        // With nothing firing there is no digest.
        for key in [Key::Size4, Key::Size5, Key::JobsWaiting] {
            pass(&db, &outbox, NOW + 2 * DAY + HOUR, &[(key, Seen::Clear)]);
        }
        assert_eq!(outbox.take().len(), 3);
        pass(&db, &outbox, NOW + 3 * DAY + MINUTE, &[]);
        assert_eq!(outbox.take(), NONE);
    }

    #[test]
    fn healthz_says_when_the_reconciler_stopped_or_fails() {
        let db = Db::new();
        let read = |db: &Db| db.first::<HealthRow>(HEALTH, &[]).unwrap();
        // No pass yet.
        let (status, body) = health(&read(&db), NOW);
        assert_eq!(status, 503);
        assert_eq!(body["status"], "stale");

        let record = "UPDATE reconciler SET finished_at = ?1, history = ?2 WHERE id = 1";
        let clean = serde_json::to_string(&passes(&[0, 3, 0], false)).unwrap();
        db.run(record, &params![NOW, clean.as_str()]);
        let (status, body) = health(&read(&db), NOW + 30_000);
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["ageSeconds"], 30);
        assert_eq!(body["lastPass"], "2027-01-15T08:00:00Z");

        let (status, body) = health(&read(&db), NOW + 6 * MINUTE);
        assert_eq!(status, 503);
        assert_eq!(body["status"], "stale");

        let failing = serde_json::to_string(&passes(&[1, 2, 1, 1, 4], false)).unwrap();
        db.run(record, &params![NOW, failing.as_str()]);
        let (status, body) = health(&read(&db), NOW + 30_000);
        assert_eq!(status, 503);
        assert_eq!(body["status"], "failing");
    }

    fn job(db: &Db, id: u64, installation_id: u64, queued_at: u64, launch: Option<&str>) {
        db.run(
            "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
             queued_at, updated_at, launch_state) \
             VALUES (?1, ?2, 'acme/app', 7, 'hatchrun', 'queued', ?3, ?3, ?4)",
            &params![id, installation_id, queued_at, launch],
        );
    }

    fn tenant(db: &Db, org: &str, installation_id: u64, verified: bool) {
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
             launch_template_id, subnet_ids, max_runners, created_at, updated_at) \
             VALUES (?1, ?2, ?1, ?2, ?3, 'eu-west-1', 'lt-1', 'subnet-1', 2, ?4, ?4)",
            &params![
                org,
                installation_id,
                verified.then_some("arn:aws:iam::1:role/r"),
                NOW
            ],
        );
    }

    #[test]
    fn jobs_wait_when_their_tenant_has_room_and_nothing_launches() {
        let db = Db::new();
        let old = NOW - 10 * MINUTE;
        tenant(&db, "acme", 42, true);
        job(&db, 1, 42, old, None);
        job(&db, 2, 42, old + 1000, None);
        job(&db, 3, 42, NOW - MINUTE, None);
        job(&db, 4, 42, old, Some("launching"));
        // A backoff counts; a job waiting for its owner does not.
        job(&db, 5, 42, old, Some("failed"));
        job(&db, 6, 42, old, Some("failed"));
        db.run("UPDATE jobs SET launch_retry = 'backoff' WHERE id = 5", &[]);
        db.run("UPDATE jobs SET launch_retry = 'setup' WHERE id = 6", &[]);
        tenant(&db, "unverified", 43, false);
        job(&db, 7, 43, old, None);

        let waiting =
            |db: &Db| db.all::<Waiting>(&waiting_query(), &params![NOW - WAITING, LISTED]);
        assert_eq!(
            waiting(&db),
            [Waiting {
                org: "acme".to_owned(),
                jobs: 3,
                oldest: old,
            }]
        );
        // At max_runners, jobs wait for a runner to finish.
        job(&db, 8, 42, old, Some("launched"));
        assert_eq!(waiting(&db), []);

        let plan = db.plan(&waiting_query(), &params![NOW - WAITING, LISTED]);
        assert!(!plan.contains("SCAN jobs"), "{plan}");
        assert!(plan.contains("jobs_open"), "{plan}");
    }

    #[test]
    fn the_dead_letter_queue_records_each_launch_once() {
        let db = Db::new();
        job(&db, 5, 42, NOW - HOUR, Some("failed"));
        db.run(
            "UPDATE jobs SET launch_error = 'RunInstances: InsufficientInstanceCapacity', \
             launch_attempts = 4 WHERE id = 5",
            &[],
        );
        let record = |message: &str, job: Option<u64>, installation: Option<u64>, at: u64| {
            db.run(RECORD_FAILED, &params![message, job, installation, at])
        };
        assert_eq!(record("m1", Some(5), Some(42), NOW), 1);
        // The queue may deliver it again.
        assert_eq!(record("m1", Some(5), Some(42), NOW + 1), 0);
        // A body that did not read.
        assert_eq!(record("m2", None, None, NOW + 2), 1);

        let failed = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW - HOUR, LISTED]);
        assert_eq!(failed.len(), 2);
        assert_eq!(
            failed[1].error.as_deref(),
            Some("RunInstances: InsufficientInstanceCapacity")
        );
        assert_eq!(failed[1].attempts, Some(4));
        assert_eq!(failed[0].job_id, None);
        let seen = failed_launches(&failed);
        assert!(
            matches!(&seen, Seen::Firing(detail) if detail.contains("job ? of installation ?"))
        );

        let outbox = Outbox::default();
        let fire = |now| {
            block_on(settle(
                &db,
                &outbox,
                now,
                &[(Key::LaunchFailed, seen.clone())],
                false,
            ))
        };
        assert_eq!(fire(NOW), NONE);
        assert_eq!(fire(NOW + 5), NONE);
        assert_eq!(outbox.take(), ["[hatchrun] Firing: Launch failed for good"]);
        // The consumer sends no digest.
        assert_eq!(block_on(Store::digest_at(&db)), Ok(None));

        // An hour on, the reconciler finds none and resolves it.
        let recent = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW + 2, LISTED]);
        assert_eq!(recent.len(), 1);
        let none = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW + HOUR, LISTED]);
        assert_eq!(failed_launches(&none), Seen::Clear);

        assert_eq!(db.run(DELETE_FAILED, &params![NOW + 1, 1000u32]), 1);
    }

    #[test]
    fn low_rate_limits_are_kept_per_installation() {
        let db = Db::new();
        let note = |installation: u64, remaining: Option<u32>, limited: u32, at: u64| {
            db.run(
                NOTE_RATE_LIMIT,
                &params![installation, remaining, limited, at],
            )
        };
        note(42, Some(1500), 0, NOW - 20 * MINUTE);
        note(42, Some(0), 1, NOW - MINUTE);
        note(43, Some(1900), 0, NOW - 20 * MINUTE);
        let low = db.all::<LowRateLimit>(LOW_SINCE, &params![NOW - RATE_LIMITS_FOR, LISTED]);
        assert_eq!(
            low,
            [LowRateLimit {
                installation_id: 42,
                remaining: Some(0),
                limited: 1,
            }]
        );
        assert_eq!(db.run(DELETE_RATE_LIMITS, &params![NOW - 10 * MINUTE]), 1);
    }

    #[test]
    fn alert_queries_read_through_indexes() {
        let db = Db::new();
        for (name, sql, index) in [
            ("FAILED_SINCE", FAILED_SINCE, "failed_launches_age"),
            ("DELETE_FAILED", DELETE_FAILED, "failed_launches_age"),
            ("LOW_SINCE", LOW_SINCE, "rate_limits_seen"),
        ] {
            let params = if sql.contains("?2") {
                params![NOW, LISTED]
            } else {
                params![NOW]
            };
            let plan = db.plan(sql, &params);
            assert!(plan.contains(index), "{name} without {index}\n{plan}");
        }
    }
}
