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
//! A message nobody can claim is planned elsewhere or done already, and
//! is acked. A message that runs out of retries reaches the dead-letter
//! queue, whose consumer logs it and acks it and leaves the delivery
//! waiting: the reconciler queues it again once its last retry is past,
//! and fails it with the failed delivery alert at
//! [`store::MAX_ATTEMPTS`]. Failing it from the dead-letter queue could
//! end a delivery that a message the reconciler queued since is about
//! to plan.

use hatchrun_cloud::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker::{
    Env, MessageBatch, MessageExt, QueueRetryOptions, QueueRetryOptionsBuilder, console_error,
};

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
    let outcome = webhook::plan_delivery(env, &db, &claim.event, &claim.guid, &claim.payload).await;
    // The release says when the queue sends the message again, so the
    // reconciler does not queue a second one meanwhile.
    let release = async |secs: u32| {
        let retry_at = now_ms() + u64::from(secs) * 1000;
        match store::release_delivery(&db, guid, token, retry_at).await {
            Err(err) => {
                console_error!("delivery {guid}: release: {err}");
                Err(retry_after_secs(secs))
            }
            // Another consumer owns it now: ack and do nothing else.
            Ok(false) => Ok(()),
            Ok(true) => Err(retry_after_secs(secs)),
        }
    };
    match outcome {
        Ok(Outcome::Done(_) | Outcome::Ignored(_)) => {
            if let Err(err) = store::finish_delivery(&db, guid, token, "done", None, now_ms()).await
            {
                console_error!("delivery {guid}: finish: {err}");
                return Err(retry_after_secs(backoff_secs(token)));
            }
            // Zero rows means another consumer owns the delivery now:
            // ack and do nothing else.
            Ok(())
        }
        Ok(Outcome::Failed(error)) => {
            if let Err(err) =
                store::finish_delivery(&db, guid, token, "failed", Some(&error), now_ms()).await
            {
                console_error!("delivery {guid}: finish: {err}");
                return Err(retry_after_secs(backoff_secs(token)));
            }
            Ok(())
        }
        Ok(Outcome::Retry) => release(RETRY_AFTER_SECS).await,
        Err(err) => {
            console_error!("delivery {guid}: plan: {err}");
            release(backoff_secs(token)).await
        }
    }
}

/// The consumer of the dead-letter queue: logs each message and acks
/// it. The delivery stays waiting in `ci_deliveries`, where the
/// reconciler queues it again and fails it at
/// [`store::MAX_ATTEMPTS`], which fires the failed delivery alert.
pub fn dead_letters(batch: &MessageBatch<Value>) {
    let messages = match batch.messages() {
        Ok(messages) => messages,
        Err(err) => {
            console_error!("delivery dead letters: unreadable batch: {err}");
            batch.ack_all();
            return;
        }
    };
    for message in &messages {
        match serde_json::from_value::<DeliveryMessage>(message.body().clone()) {
            Ok(delivery) => console_error!(
                "delivery {}: its message ran out of retries; the reconciler sends it again",
                delivery.guid
            ),
            Err(err) => {
                let body: String = message.body().to_string().chars().take(300).collect();
                console_error!(
                    "delivery dead letters: unreadable message {}: {err}: {body}",
                    message.id()
                );
            }
        }
        message.ack();
    }
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
    }

    #[test]
    fn the_message_carries_the_delivery() {
        let text = serde_json::to_string(&DeliveryMessage { guid: "abc".into() }).unwrap();
        assert_eq!(text, r#"{"guid":"abc"}"#);
    }
}
