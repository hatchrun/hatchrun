// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `hatchrun-ci-deliveries` queue: plans each stored webhook
//! delivery once.
//!
//! The intake stores every verified delivery and answers 202 before
//! anything reads it (see [`crate::webhook`]). A message names a
//! delivery by its `X-GitHub-Delivery` id; the payload stays in
//! `ci_deliveries`, since payloads can pass the queue's 128 KB message
//! limit. The consumer claims the delivery, which only one consumer
//! can, and runs the planning (see [`crate::webhook::plan_delivery`])
//! from the stored payload. The claim returns a token, and finishing
//! or releasing only touches the row while it still holds that claim,
//! so a consumer whose lease ran out cannot end what another consumer
//! claimed since:
//!
//! - `Done` and `Ignored` finish it as done, dropping the payload;
//! - an unparseable payload finishes it as failed: asking again would
//!   send the same bytes;
//! - `Retry` releases it back to `received` and the message is sent
//!   again after 5 s;
//! - any other error releases it and sends the message again with
//!   backoff, so a failing plan retries from the stored payload.
//!
//! A message nobody can claim is done already and is acked. The
//! dead-letter consumer finishes the deliveries that still wait as
//! failed and raises their alert like failed launches do (see
//! [`crate::alerts`]); ones already done or failed are acked without an
//! alert.

use hatchrun_cloud::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker::{
    Env, MessageBatch, MessageExt, QueueRetryOptions, QueueRetryOptionsBuilder, console_error,
};

use crate::alerts;
use crate::store;
use crate::webhook::{self, Outcome};

/// The queue's producer binding in `wrangler.toml`.
pub const BINDING: &str = "CI_DELIVERIES";

/// The queue in `wrangler.toml`.
pub const QUEUE: &str = "hatchrun-ci-deliveries";

/// The dead-letter queue in `wrangler.toml`.
pub const DEAD_LETTERS: &str = "hatchrun-ci-deliveries-dlq";

/// How long a `Retry` waits before the delivery is planned again, in
/// seconds.
const RETRY_AFTER_SECS: u32 = 5;

/// How long an unreadable message waits before it is delivered again,
/// in seconds.
const REDELIVER_AFTER_SECS: u32 = 15;

/// The first backoff of a failed plan, in seconds, doubling per claim
/// up to [`BACKOFF_CAP_SECS`].
const BACKOFF_SECS: u32 = 10;

/// The most a failed plan waits before it is planned again, in
/// seconds.
const BACKOFF_CAP_SECS: u32 = 300;

/// The message that ends a delivery whose planning ran out of retries.
const DEAD_LETTER_MESSAGE: &str = "the delivery did not finish planning";

/// The queue's message: the delivery to plan, by its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryMessage {
    /// The `X-GitHub-Delivery` id, as stored in `ci_deliveries`.
    pub guid: String,
}

/// How long a failed plan with `attempts` claims waits, in seconds:
/// 10 s, doubling per claim, at most 5 min.
#[must_use]
pub fn backoff_secs(attempts: u64) -> u32 {
    let shift = attempts.saturating_sub(1).min(5);
    BACKOFF_SECS
        .saturating_mul(1 << shift)
        .min(BACKOFF_CAP_SECS)
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn retry_after_secs(secs: u32) -> QueueRetryOptions {
    QueueRetryOptionsBuilder::new()
        .with_delay_seconds(secs)
        .build()
}

/// The consumer of `hatchrun-ci-deliveries`: claims each message's
/// delivery and plans it from the stored payload, acking each message
/// once its delivery is decided and retrying it when nothing was.
pub async fn consume(batch: MessageBatch<Value>, env: Env) {
    let messages = match batch.messages() {
        Ok(messages) => messages,
        Err(err) => {
            console_error!("delivery queue: unreadable batch: {err}");
            batch.retry_all_with_options(&retry_after_secs(REDELIVER_AFTER_SECS));
            return;
        }
    };
    for message in &messages {
        let guid = match serde_json::from_value::<DeliveryMessage>(message.body().clone()) {
            Ok(message) => message.guid,
            Err(err) => {
                console_error!("delivery queue: unreadable message {}: {err}", message.id());
                message.retry_with_options(&retry_after_secs(REDELIVER_AFTER_SECS));
                continue;
            }
        };
        match plan(&env, &guid).await {
            Ok(()) => message.ack(),
            Err(retry) => message.retry_with_options(&retry),
        }
    }
}

/// Plans delivery `guid`: claims it and runs the planning from the
/// stored payload. `Ok` means the message is decided; `Err` carries
/// how long the redelivery waits.
async fn plan(env: &Env, guid: &str) -> Result<(), QueueRetryOptions> {
    let db = Db::connect(env).await.map_err(|err| {
        console_error!("delivery {guid}: database: {err}");
        retry_after_secs(REDELIVER_AFTER_SECS)
    })?;
    let claim = store::claim_delivery(&db, guid, now_ms())
        .await
        .map_err(|err| {
            console_error!("delivery {guid}: claim: {err}");
            retry_after_secs(REDELIVER_AFTER_SECS)
        })?;
    let Some(claim) = claim else {
        // Another consumer plans it, or it is done already.
        return Ok(());
    };
    let token = claim.attempts;
    match webhook::plan_delivery(env, &db, &claim.event, &claim.guid, &claim.payload).await {
        Ok(Outcome::Done(_) | Outcome::Ignored(_)) => {
            if let Err(err) = store::finish_delivery(&db, guid, token, "done", None).await {
                console_error!("delivery {guid}: finish: {err}");
                return Err(retry_after_secs(backoff_secs(token)));
            }
            // Zero rows means another consumer owns the delivery now:
            // ack and do nothing else.
            Ok(())
        }
        Ok(Outcome::Failed(error)) => {
            if let Err(err) = store::finish_delivery(&db, guid, token, "failed", Some(&error)).await
            {
                console_error!("delivery {guid}: finish: {err}");
                return Err(retry_after_secs(backoff_secs(token)));
            }
            Ok(())
        }
        Ok(Outcome::Retry) => {
            match store::release_delivery(&db, guid, token).await {
                Err(err) => {
                    console_error!("delivery {guid}: release: {err}");
                    Err(retry_after_secs(RETRY_AFTER_SECS))
                }
                // Another consumer owns it now: ack and do nothing else.
                Ok(false) => Ok(()),
                Ok(true) => Err(retry_after_secs(RETRY_AFTER_SECS)),
            }
        }
        Err(err) => {
            console_error!("delivery {guid}: plan: {err}");
            match store::release_delivery(&db, guid, token).await {
                Err(err) => {
                    console_error!("delivery {guid}: release: {err}");
                    Err(retry_after_secs(backoff_secs(token)))
                }
                Ok(false) => Ok(()),
                Ok(true) => Err(retry_after_secs(backoff_secs(token))),
            }
        }
    }
}

/// The consumer of the dead-letter queue: finishes the deliveries
/// that still wait as failed, records them and fires their alert, like
/// failed launches. Deliveries already done or failed are acked without
/// an alert, so a redelivery never flips a done delivery to failed. A
/// message that does not read is recorded with its body and left
/// unacked, which the queue counts as done, since no delivery can be
/// named; a delivery that cannot be finished is retried, and so is the
/// whole batch when the failures cannot be recorded.
pub async fn dead_letters(batch: MessageBatch<Value>, env: Env) {
    let messages = match batch.messages() {
        Ok(messages) => messages,
        Err(err) => {
            console_error!("delivery dead letters: unreadable batch: {err}");
            batch.retry_all();
            return;
        }
    };
    let db = match Db::connect(&env).await {
        Ok(db) => db,
        Err(err) => {
            console_error!("delivery dead letters: database: {err}");
            batch.retry_all();
            return;
        }
    };
    let mut planned: Vec<(worker::Message<Value>, DeliveryMessage)> = Vec::new();
    let mut unreadable: Vec<(String, String)> = Vec::new();
    for message in messages {
        match serde_json::from_value::<DeliveryMessage>(message.body().clone()) {
            Ok(delivers) => planned.push((message, delivers)),
            Err(err) => {
                let body = message.body().to_string();
                let body = body.chars().take(300).collect::<String>();
                console_error!(
                    "delivery dead letters: unreadable message {}: {err}",
                    message.id()
                );
                unreadable.push((message.id(), alerts::unreadable(&err.to_string(), &body)));
            }
        }
    }
    let (pending, settled) = partition(&db, planned).await;
    if let Err(err) = record_failures(&env, &db, &pending, &unreadable).await {
        // Nothing is acked, so the queue delivers the batch again and
        // the alert still gets its chance.
        console_error!("delivery dead letters: alert: {err}");
        batch.retry_all();
        return;
    }
    for (message, delivers) in &pending {
        match dead_letter(&db, &delivers.guid).await {
            // False means another consumer owned it after all: ack and
            // do nothing else.
            Ok(_) => message.ack(),
            Err(err) => {
                console_error!("delivery dead letters: {}: {err}", delivers.guid);
                message.retry();
            }
        }
    }
    for message in &settled {
        message.ack();
    }
}

/// Splits `planned` into the deliveries that still wait and the ones
/// already done or failed. A delivery whose state cannot be read counts
/// as waiting: finishing it decides.
async fn partition(
    db: &Db,
    planned: Vec<(worker::Message<Value>, DeliveryMessage)>,
) -> (
    Vec<(worker::Message<Value>, DeliveryMessage)>,
    Vec<worker::Message<Value>>,
) {
    let mut pending = Vec::with_capacity(planned.len());
    let mut settled = Vec::new();
    for (message, delivers) in planned {
        match store::delivery_state(db, &delivers.guid).await {
            Ok(Some(state)) if state == "done" || state == "failed" => settled.push(message),
            _ => pending.push((message, delivers)),
        }
    }
    (pending, settled)
}

/// Records the deliveries of `pending` and the `unreadable` messages,
/// and fires the failed delivery alert before the deliveries are
/// finished, so a redelivery still finds them. A failure to record is
/// returned: the batch is then retried, since acking it would lose the
/// alert.
async fn record_failures(
    env: &Env,
    db: &Db,
    pending: &[(worker::Message<Value>, DeliveryMessage)],
    unreadable: &[(String, String)],
) -> Result<(), String> {
    let failed: Vec<(String, String)> = pending
        .iter()
        .map(|(message, delivers)| (message.id(), dead_error(&delivers.guid)))
        .collect();
    let mut unknown: Vec<(String, String)> = failed;
    unknown.extend(unreadable.iter().cloned());
    if unknown.is_empty() {
        return Ok(());
    }
    alerts::deliveries_failed(env, db, now_ms(), &unknown).await
}

/// Finishes delivery `guid` as failed, if it still waits: it ran out of
/// retries. Returns whether it was finished: false means it is done or
/// failed already.
async fn dead_letter(db: &Db, guid: &str) -> Result<bool, String> {
    store::finish_dead_letter(db, guid, &dead_error(guid))
        .await
        .map_err(|err| err.to_string())
}

/// Why a dead-letter delivery failed.
fn dead_error(guid: &str) -> String {
    format!("{DEAD_LETTER_MESSAGE}: {guid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_plan_waits_longer_each_claim_up_to_five_minutes() {
        assert_eq!(backoff_secs(0), 10);
        assert_eq!(backoff_secs(1), 10);
        assert_eq!(backoff_secs(2), 20);
        assert_eq!(backoff_secs(3), 40);
        assert_eq!(backoff_secs(6), 300);
        assert_eq!(backoff_secs(50), 300);
        assert_eq!(RETRY_AFTER_SECS, 5);
        assert_eq!(DEAD_LETTER_MESSAGE, "the delivery did not finish planning");
    }

    #[test]
    fn the_message_carries_the_delivery() {
        let text = serde_json::to_string(&DeliveryMessage { guid: "abc".into() }).unwrap();
        assert_eq!(text, r#"{"guid":"abc"}"#);
    }
}
