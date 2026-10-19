// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The launch queue between the webhook and `launch`.
//!
//! The webhook records a queued job and sends one [`Launch`] message for
//! it to the `hatchrun-launches` queue; this Worker consumes the queue
//! and launches each job in an invocation of its own, so a launch no
//! longer dies with the webhook request that recorded the job.
//!
//! The queue delivers each message at least once and in any order.
//! `launch` claims the job before it starts anything, so a message
//! delivered twice, or a job the reconciler launched first, launches
//! nothing the second time. A message is acked once `launch` has
//! decided: it launched, refused the tenant, found the job taken, or
//! recorded a failure for the reconciler to retry (see
//! [`crate::reconcile::retry`]). An error before that, such as the database
//! not answering, retries the message after [`RETRY_DELAY`]. After the
//! consumer's `max_retries` the message goes to `hatchrun-launches-dlq`,
//! and the reconciler launches the job again once its grace is up. This
//! Worker consumes the dead-letter queue too: it records each message
//! in `failed_launches` and fires the failed launch alert (see
//! [`crate::alerts`]).

use std::future::Future;

use serde::{Deserialize, Serialize};
use worker::{Env, MessageBatch, MessageExt, QueueRetryOptionsBuilder, console_error};

use crate::db::Db;
use crate::webhook::QueuedJob;
use crate::{alerts, labels, launch, now_ms};

/// Queue producer binding in `wrangler.toml`.
pub(crate) const QUEUE: &str = "LAUNCHES";
/// The dead-letter queue in `wrangler.toml`.
pub(crate) const DEAD_LETTERS: &str = "hatchrun-launches-dlq";
/// How long a message waits before it is delivered again, seconds. With
/// the consumer's three retries a message gives up within a minute, the
/// reconciler's grace for a queued job without a runner.
const RETRY_DELAY: u32 = 15;

/// A job to launch, as the webhook sends it. The label is parsed again
/// on the way out; it was checked when the job was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Launch {
    pub id: u64,
    pub run_id: u64,
    pub installation_id: u64,
    pub repository: String,
    pub label: String,
}

impl From<&QueuedJob> for Launch {
    fn from(job: &QueuedJob) -> Self {
        Self {
            id: job.id,
            run_id: job.run_id,
            installation_id: job.installation_id,
            repository: job.repository.clone(),
            label: job.label.clone(),
        }
    }
}

impl Launch {
    /// The job as `launch` takes it.
    fn job(self) -> Result<QueuedJob, String> {
        let spec = labels::parse(&self.label).map_err(|err| format!("{}: {err}", self.label))?;
        Ok(QueuedJob {
            id: self.id,
            run_id: self.run_id,
            installation_id: self.installation_id,
            repository: self.repository,
            label: self.label,
            spec,
        })
    }
}

/// Where launch messages go: the queue, or a list in tests.
pub(crate) trait Producer {
    fn send(&self, message: Launch) -> impl Future<Output = Result<(), String>>;
}

impl Producer for worker::Queue {
    async fn send(&self, message: Launch) -> Result<(), String> {
        worker::Queue::send(self, message)
            .await
            .map_err(|err| err.to_string())
    }
}

/// Asks for `job` to be launched.
pub(crate) async fn enqueue(queue: &impl Producer, job: &QueuedJob) -> Result<(), String> {
    queue.send(Launch::from(job)).await
}

/// What launches a job: `launch`, or a stand-in in tests. An error means
/// nothing was decided and the job may launch on a later try.
pub(crate) trait Launcher {
    fn launch(&self, job: &QueuedJob) -> impl Future<Output = Result<(), String>>;
}

impl Launcher for Env {
    async fn launch(&self, job: &QueuedJob) -> Result<(), String> {
        launch::try_launch(self, job)
            .await
            .map_err(|err| err.to_string())
    }
}

/// What becomes of a message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `launch` decided; the queue forgets the message.
    Ack,
    /// The message cannot launch anything, such as a label that no
    /// longer parses, and is acked; the reconciler logs the job too.
    Drop(String),
    /// Nothing was decided; the message is delivered again after
    /// [`RETRY_DELAY`].
    Retry(String),
}

/// Launches the job of one message. A body that is not a [`Launch`],
/// such as one sent by a Worker version with another message format,
/// is retried, so after `max_retries` it waits in the dead-letter queue
/// rather than disappearing.
pub(crate) async fn deliver(launcher: &impl Launcher, body: serde_json::Value) -> Verdict {
    let message = match Launch::deserialize(body) {
        Ok(message) => message,
        Err(err) => return Verdict::Retry(format!("unreadable message: {err}")),
    };
    let job = match message.job() {
        Ok(job) => job,
        Err(err) => return Verdict::Drop(err),
    };
    match launcher.launch(&job).await {
        Ok(()) => Verdict::Ack,
        Err(err) => Verdict::Retry(err),
    }
}

/// The consumer: launches a batch's jobs side by side and acks or
/// retries each message. Bodies are read raw and parsed in [`deliver`],
/// so a message that does not read is retried like any other.
pub(crate) async fn consume(batch: MessageBatch<serde_json::Value>, env: Env) {
    let messages: Vec<_> = batch.raw_iter().collect();
    let verdicts = futures_util::future::join_all(
        messages
            .iter()
            .map(|message| deliver(&env, json(&message.body()))),
    )
    .await;
    let retry = QueueRetryOptionsBuilder::new()
        .with_delay_seconds(RETRY_DELAY)
        .build();
    for (message, verdict) in messages.iter().zip(verdicts) {
        let id = message.id();
        match verdict {
            Verdict::Ack => message.ack(),
            Verdict::Drop(err) => {
                console_error!("launch queue: message {id}: {err}");
                message.ack();
            }
            Verdict::Retry(err) => {
                console_error!(
                    "launch queue: message {id}: {err}, trying again in {RETRY_DELAY} s"
                );
                message.retry_with_options(&retry);
            }
        }
    }
}

/// A dead letter as `failed_launches` records it: the message id, and
/// the job and installation when the body reads.
fn dead_letter(id: String, body: serde_json::Value) -> (String, Option<u64>, Option<u64>) {
    match Launch::deserialize(body) {
        Ok(message) => (id, Some(message.id), Some(message.installation_id)),
        Err(_) => (id, None, None),
    }
}

/// The dead-letter queue's consumer: records the batch's launches as
/// failed and acks them, or retries the batch when the database does not
/// take them. The alert's own errors do not hold the messages back.
pub(crate) async fn dead_letters(batch: MessageBatch<serde_json::Value>, env: Env) {
    let messages: Vec<_> = batch.raw_iter().collect();
    let failed: Vec<_> = messages
        .iter()
        .map(|message| dead_letter(message.id(), json(&message.body())))
        .collect();
    let recorded = match Db::connect(&env).await {
        Ok(db) => alerts::launches_failed(&env, &db, now_ms(), &failed).await,
        Err(err) => Err(err.to_string()),
    };
    match recorded {
        Ok(()) => {
            for ((id, job, _), message) in failed.iter().zip(&messages) {
                let job = job.map_or_else(|| "unreadable".to_owned(), |job| job.to_string());
                console_error!("launch queue: message {id}: job {job}: out of retries");
                message.ack();
            }
        }
        Err(err) => {
            console_error!("launch dead letters: {err}");
            for message in &messages {
                message.retry();
            }
        }
    }
}

/// A message body as JSON, or `null` when it is not JSON, which
/// [`deliver`] retries as unreadable.
fn json(body: &wasm_bindgen::JsValue) -> serde_json::Value {
    js_sys::JSON::stringify(body)
        .ok()
        .and_then(|text| text.as_string())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::cell::{Cell, RefCell};

    use futures_util::FutureExt;

    use super::*;
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;
    const INSTALLATION: u64 = 42;

    /// Collects what the webhook sends.
    #[derive(Default)]
    struct Sent(RefCell<Vec<Launch>>);

    impl Producer for Sent {
        fn send(&self, message: Launch) -> impl Future<Output = Result<(), String>> {
            self.0.borrow_mut().push(message);
            std::future::ready(Ok(()))
        }
    }

    /// `launch` up to the claim, against Postgres: claims the job with the
    /// Worker's own statement and counts the runners it would start.
    /// `down` fails the next calls before the claim, as the database would.
    struct Claims {
        db: Db,
        down: Cell<u32>,
        started: RefCell<Vec<u64>>,
    }

    impl Claims {
        fn new() -> Self {
            let db = Db::new();
            db.run(
                "INSERT INTO tenants (id, org_id, org_login, installation_id, max_runners, \
                 created_at, updated_at) VALUES ('t1', 9, 'acme', $1, 10, $2, $2)",
                &params![INSTALLATION, NOW],
            );
            Self {
                db,
                down: Cell::new(0),
                started: RefCell::new(Vec::new()),
            }
        }

        fn record(&self, id: u64) {
            self.db.run(
                "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
                 queued_at, updated_at) VALUES ($1, $2, 'acme/app', 77, 'hatchrun', 'queued', \
                 $3, $3)",
                &params![id, INSTALLATION, NOW],
            );
        }
    }

    impl Launcher for Claims {
        fn launch(&self, job: &QueuedJob) -> impl Future<Output = Result<(), String>> {
            if self.down.get() > 0 {
                self.down.set(self.down.get() - 1);
                return std::future::ready(Err("database unavailable".to_owned()));
            }
            let claimed = self.db.run(
                launch::CLAIM,
                &params![
                    job.id,
                    format!("hatchrun-{}-00", job.id),
                    NOW,
                    job.installation_id,
                    10u32
                ],
            );
            if claimed == 1 {
                self.started.borrow_mut().push(job.id);
            }
            std::future::ready(Ok(()))
        }
    }

    fn job(id: u64, label: &str) -> QueuedJob {
        QueuedJob {
            id,
            run_id: 77,
            installation_id: INSTALLATION,
            repository: "acme/app".to_owned(),
            label: label.to_owned(),
            spec: labels::parse(label).unwrap(),
        }
    }

    fn deliver_now(launcher: &impl Launcher, message: Launch) -> Verdict {
        deliver(launcher, serde_json::to_value(message).unwrap())
            .now_or_never()
            .unwrap()
    }

    #[test]
    fn the_webhook_enqueues_the_job_for_launch() {
        let sent = Sent::default();
        let queued = job(5, "hatchrun-4cpu-arm64/spot=true");
        enqueue(&sent, &queued).now_or_never().unwrap().unwrap();
        let sent = sent.0.into_inner();
        assert_eq!(sent.len(), 1);
        // The message survives the queue's JSON and comes out as the job.
        let body = serde_json::to_string(&sent[0]).unwrap();
        let message: Launch = serde_json::from_str(&body).unwrap();
        assert_eq!(message.job().unwrap(), queued);
    }

    #[test]
    fn the_consumer_launches_the_job() {
        let claims = Claims::new();
        claims.record(5);
        let verdict = deliver_now(&claims, Launch::from(&job(5, "hatchrun")));
        assert_eq!(verdict, Verdict::Ack);
        assert_eq!(*claims.started.borrow(), [5]);
        let state: Option<String> =
            claims
                .db
                .column("SELECT launch_state FROM jobs", &[], "launch_state");
        assert_eq!(state.as_deref(), Some("launching"));
    }

    #[test]
    fn a_message_delivered_twice_launches_once() {
        let claims = Claims::new();
        claims.record(5);
        let message = Launch::from(&job(5, "hatchrun"));
        assert_eq!(deliver_now(&claims, message.clone()), Verdict::Ack);
        assert_eq!(deliver_now(&claims, message), Verdict::Ack);
        assert_eq!(*claims.started.borrow(), [5]);
    }

    #[test]
    fn an_error_before_the_claim_retries() {
        let claims = Claims::new();
        claims.record(5);
        claims.down.set(1);
        let message = Launch::from(&job(5, "hatchrun"));
        assert!(matches!(
            deliver_now(&claims, message.clone()),
            Verdict::Retry(err) if err.contains("unavailable")
        ));
        assert!(claims.started.borrow().is_empty());
        // The retry launches it.
        assert_eq!(deliver_now(&claims, message), Verdict::Ack);
        assert_eq!(*claims.started.borrow(), [5]);
    }

    #[test]
    fn a_label_that_does_not_parse_is_dropped() {
        let claims = Claims::new();
        let mut message = Launch::from(&job(5, "hatchrun"));
        message.label = "ubuntu-latest".to_owned();
        assert!(matches!(
            deliver_now(&claims, message),
            Verdict::Drop(err) if err.starts_with("ubuntu-latest")
        ));
        assert!(claims.started.borrow().is_empty());
    }

    #[test]
    fn a_dead_letter_names_its_job_when_it_reads() {
        let message = Launch::from(&job(5, "hatchrun"));
        assert_eq!(
            dead_letter("m1".to_owned(), serde_json::to_value(message).unwrap()),
            ("m1".to_owned(), Some(5), Some(INSTALLATION))
        );
        assert_eq!(
            dead_letter("m2".to_owned(), serde_json::json!({ "job": 5 })),
            ("m2".to_owned(), None, None)
        );
    }

    #[test]
    fn an_unreadable_message_retries_into_the_dead_letter_queue() {
        let claims = Claims::new();
        let body = serde_json::json!({ "job": 5 });
        let verdict = deliver(&claims, body).now_or_never().unwrap();
        assert!(matches!(verdict, Verdict::Retry(err) if err.starts_with("unreadable")));
        assert!(claims.started.borrow().is_empty());
    }
}
