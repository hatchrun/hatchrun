// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `hatchrun-ci-launches` queue: starts the instance of each job
//! the run object queued (see [`crate::run`]).
//!
//! A message names a job and the attempt it was queued for. The
//! consumer loads the job and does nothing when the attempt differs
//! (the job was re-run since) or the job is not queued any more. Then
//! it claims the launch, which only one consumer can, and:
//!
//! 1. signs the job token and a one-use ticket;
//! 2. starts the instance with the ticket in its user data (see
//!    [`crate::launcher`]);
//! 3. stores the boot document, the control plane's address and the job
//!    token, in the broker under the ticket, and terminates the
//!    instance when it cannot;
//! 4. records the instance, unless the job moved on meanwhile, which
//!    terminates the instance too.
//!
//! A launch that found no capacity or no answer is queued again after
//! 30 s, twice; any other failure, or the third, ends the job through
//! the run object (see [`next_step`]). The dead-letter consumer ends the
//! jobs whose messages ran out of retries.

use futures_util::future::join_all;
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::http::Request;
use hatchrun_cloud::start::{Cause, Failure};
use hatchrun_cloud::{jwt, webcrypto};
use hatchrun_workflow::agent::{BootDocument, CiBoot};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, MessageBatch, MessageExt, Method, RequestInit, console_error, console_log};

use crate::env::{Settings, run_stub};
use crate::launcher::{Backend, LaunchSpec, user_data};
use crate::store::{self, JobRow};
use crate::token::{self, Claims};

/// The dead-letter queue in `wrangler.toml`.
pub const DEAD_LETTERS: &str = "hatchrun-ci-launches-dlq";

/// How long a launch that found no capacity waits, in milliseconds.
const RETRY_AFTER_MS: u64 = 30_000;
/// Launch attempts a job gets.
const MAX_LAUNCH_ATTEMPTS: u32 = 3;
/// How long the broker keeps a boot document for its instance, in
/// seconds.
const TICKET_EXPIRES_IN: u64 = token::BOOT_SECS;
/// The message that ends a job whose launch message ran out of retries.
const DEAD_LETTER_MESSAGE: &str = "the launch did not finish";

/// The queue's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchMessage {
    /// The job to launch.
    pub job_id: u64,
    /// The attempt of the job it was queued for.
    pub attempt: u32,
}

/// What happens to a launch that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Queued again, not before `at_ms`.
    Retry {
        /// Unix milliseconds.
        at_ms: u64,
    },
    /// The job fails.
    Fail,
}

/// What to do after launch attempt `launch_attempts` failed on `cause`
/// at `now_ms`: capacity and connection failures wait 30 s and try
/// again until the third attempt; everything else fails the job.
#[must_use]
pub fn next_step(cause: &Cause, launch_attempts: u32, now_ms: u64) -> Next {
    match cause {
        Cause::NoCapacity | Cause::Unreachable if launch_attempts < MAX_LAUNCH_ATTEMPTS => {
            Next::Retry {
                at_ms: now_ms + RETRY_AFTER_MS,
            }
        }
        _ => Next::Fail,
    }
}

/// The body of the broker's `PUT /v1/tickets/{ticket}`: the document as
/// text, since the broker's `config` is a string, and its lifetime.
///
/// # Panics
///
/// Never: the document is plain strings.
#[must_use]
pub fn broker_body(document: &BootDocument) -> Value {
    json!({
        "config": serde_json::to_string(document).expect("a boot document serializes"),
        "expiresIn": TICKET_EXPIRES_IN,
    })
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// The consumer of `hatchrun-ci-launches`: launches a batch's jobs side
/// by side, acking each message once the job's launch is decided and
/// retrying it when nothing was.
pub async fn consume(batch: MessageBatch<LaunchMessage>, env: Env) {
    let messages = match batch.messages() {
        Ok(messages) => messages,
        Err(err) => {
            console_error!("launch queue: unreadable batch: {err}");
            batch.retry_all();
            return;
        }
    };
    let (settings, backend) = match start(&env).await {
        Ok(started) => started,
        Err(err) => {
            console_error!("launch queue: {err}");
            batch.retry_all();
            return;
        }
    };
    let results = join_all(
        messages
            .iter()
            .map(|message| launch(&env, &settings, &backend, *message.body())),
    )
    .await;
    for (message, result) in messages.iter().zip(results) {
        match result {
            Ok(()) => message.ack(),
            Err(err) => {
                console_error!("launch queue: job {}: {err}", message.body().job_id);
                message.retry();
            }
        }
    }
}

async fn start(env: &Env) -> Result<(Settings, Backend), String> {
    let settings = Settings::read(env)?;
    let backend = Backend::new(&settings).await?;
    Ok((settings, backend))
}

/// Launches the job of one message. An error means nothing was decided
/// and the message may be delivered again.
async fn launch(
    env: &Env,
    settings: &Settings,
    backend: &Backend,
    message: LaunchMessage,
) -> Result<(), String> {
    let db = Db::connect(env).await.map_err(|err| err.to_string())?;
    let Some(job) = store::job(&db, message.job_id)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(());
    };
    if job.attempt != message.attempt || job.status != "queued" {
        return Ok(());
    }
    let Some(job) = store::claim_launch(&db, message.job_id, now_ms())
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(());
    };
    match launch_claimed(&db, settings, backend, &job).await {
        Ok(Launched::Started | Launched::MovedOn) => Ok(()),
        Err(failure) => {
            console_error!(
                "job {}: launch {} failed: {}",
                job.id,
                job.launch_attempts,
                failure.message
            );
            record_failure(env, &db, &job, &failure).await
        }
    }
}

/// How a claimed launch ended without a failure.
enum Launched {
    /// The instance runs and is recorded.
    Started,
    /// The job changed while the instance started, and the instance was
    /// terminated.
    MovedOn,
}

/// Everything after the claim.
async fn launch_claimed(
    db: &Db,
    settings: &Settings,
    backend: &Backend,
    job: &JobRow,
) -> Result<Launched, Failure> {
    let internal = |err: String| Failure::new(Cause::Internal, err);
    let now = now_ms();
    let ticket = jwt::b64url(webcrypto::random_bytes(32).map_err(|err| internal(err.to_string()))?);
    let attempt = i32::try_from(job.attempt).map_err(|err| internal(err.to_string()))?;
    let job_token = token::sign(
        &settings.token_key,
        &Claims {
            job: job.id,
            run: job.run_id,
            attempt,
            exp: now / 1000 + token::lifetime_secs(job.timeout_minutes),
        },
    );
    let document = BootDocument {
        ci: CiBoot {
            url: settings.ci_url.clone(),
            token: job_token,
        },
    };
    let spec = LaunchSpec {
        job_id: job.id,
        name: format!("ci-{}-{}", job.id, job.launch_attempts),
        user_data: user_data(&settings.broker_url, &ticket),
    };
    let instance_id = backend.launch(db, &spec).await?;
    if let Err(err) = store_ticket(settings, &ticket, &document).await {
        backend.terminate(&instance_id).await;
        return Err(Failure::new(Cause::Broker, err));
    }
    match store::launched(db, job.id, &instance_id, now_ms()).await {
        Ok(true) => {
            console_log!("job {}: launched {instance_id}", job.id);
            Ok(Launched::Started)
        }
        Ok(false) => {
            console_log!("job {}: moved on, terminating {instance_id}", job.id);
            backend.terminate(&instance_id).await;
            Ok(Launched::MovedOn)
        }
        Err(err) => {
            backend.terminate(&instance_id).await;
            Err(internal(err.to_string()))
        }
    }
}

/// Stores the boot document in the broker under `ticket`.
async fn store_ticket(
    settings: &Settings,
    ticket: &str,
    document: &BootDocument,
) -> Result<(), String> {
    let request = Request::new(
        "PUT",
        format!("{}/v1/tickets/{ticket}", settings.broker_url),
    )
    .header("Authorization", format!("Bearer {}", settings.broker_token))
    .header("Content-Type", "application/json")
    .body(broker_body(document).to_string());
    let response = send(&request)
        .await
        .map_err(|err| format!("store boot document: {err}"))?;
    if response.ok() {
        Ok(())
    } else {
        Err(format!(
            "store boot document: broker answered {}: {}",
            response.status,
            response.text()
        ))
    }
}

/// Queues the job again or ends it, as [`next_step`] says. An error
/// means the outcome could not be recorded and the message is retried.
async fn record_failure(env: &Env, db: &Db, job: &JobRow, failure: &Failure) -> Result<(), String> {
    let now = now_ms();
    match next_step(&failure.cause, job.launch_attempts, now) {
        Next::Retry { at_ms } => {
            store::launch_failed(db, job.id, &failure.message, Some(at_ms), now)
                .await
                .map_err(|err| err.to_string())?;
            Ok(())
        }
        Next::Fail => {
            if let Err(err) = end_job(env, db, job, &failure.message).await {
                // The job stays launching and no message is left for it.
                // Queue it again so the reconciler launches it, and the
                // next failure ends it here.
                console_error!("job {}: could not end it: {err}", job.id);
                store::launch_failed(
                    db,
                    job.id,
                    &failure.message,
                    Some(now + RETRY_AFTER_MS),
                    now,
                )
                .await
                .map_err(|err| err.to_string())?;
            }
            Ok(())
        }
    }
}

/// Ends attempt `job.attempt` of the job as failed, through the run's
/// object.
async fn end_job(env: &Env, db: &Db, job: &JobRow, message: &str) -> Result<(), String> {
    let run = store::run(db, job.run_id)
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("run {} is gone", job.run_id))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(json!({ "message": message }).to_string().into()));
    let url = format!(
        "https://run/failed?job={}&attempt={}&run={}&tenant={}",
        job.id, job.attempt, run.id, run.tenant_id
    );
    let request = worker::Request::new_with_init(&url, &init).map_err(|err| err.to_string())?;
    let response = run_stub(env, run.id)
        .map_err(|err| err.to_string())?
        .fetch_with_request(request)
        .await
        .map_err(|err| err.to_string())?;
    match response.status_code() {
        // 409: the attempt ended or was replaced; nothing left to end.
        200..=299 | 409 => Ok(()),
        status => Err(format!("run {}: failed answered {status}", run.id)),
    }
}

/// The consumer of the dead-letter queue: ends the jobs whose launch
/// messages ran out of retries. A message that does not read is acked,
/// since no job can be named; a job that cannot be ended is retried.
pub async fn dead_letters(batch: MessageBatch<LaunchMessage>, env: Env) {
    let messages = match batch.messages() {
        Ok(messages) => messages,
        Err(err) => {
            console_error!("launch dead letters: unreadable batch: {err}");
            batch.ack_all();
            return;
        }
    };
    let results = join_all(
        messages
            .iter()
            .map(|message| dead_letter(&env, *message.body())),
    )
    .await;
    for (message, result) in messages.iter().zip(results) {
        match result {
            Ok(()) => message.ack(),
            Err(err) => {
                console_error!("launch dead letters: job {}: {err}", message.body().job_id);
                message.retry();
            }
        }
    }
}

async fn dead_letter(env: &Env, message: LaunchMessage) -> Result<(), String> {
    let db = Db::connect(env).await.map_err(|err| err.to_string())?;
    let Some(job) = store::job(&db, message.job_id)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(());
    };
    if job.attempt != message.attempt || job.status == "completed" {
        return Ok(());
    }
    end_job(env, &db, &job, DEAD_LETTER_MESSAGE).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_capacity_twice_then_fails() {
        assert_eq!(
            next_step(&Cause::NoCapacity, 1, 0),
            Next::Retry { at_ms: 30_000 }
        );
        assert_eq!(
            next_step(&Cause::NoCapacity, 2, 5_000),
            Next::Retry { at_ms: 35_000 }
        );
        assert_eq!(next_step(&Cause::NoCapacity, 3, 0), Next::Fail);
    }

    #[test]
    fn unreachable_retries_and_other_causes_fail() {
        assert_eq!(
            next_step(&Cause::Unreachable, 1, 0),
            Next::Retry { at_ms: 30_000 }
        );
        assert_eq!(next_step(&Cause::Unreachable, 3, 0), Next::Fail);
        for cause in [
            Cause::Aws("AccessDenied".into()),
            Cause::Broker,
            Cause::NoImage,
            Cause::Internal,
            Cause::NoInstanceType,
        ] {
            assert_eq!(next_step(&cause, 1, 0), Next::Fail, "{cause:?}");
        }
    }

    #[test]
    fn broker_body_holds_the_document_as_text() {
        let document = BootDocument {
            ci: CiBoot {
                url: "https://ci.example".into(),
                token: "v1.x.y".into(),
            },
        };
        let body = broker_body(&document);
        let config = body["config"].as_str().expect("config is text");
        assert_eq!(
            serde_json::from_str::<BootDocument>(config).unwrap(),
            document
        );
        assert_eq!(body["expiresIn"], 900);
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[test]
    fn the_message_carries_job_and_attempt() {
        let text = serde_json::to_string(&LaunchMessage {
            job_id: 7,
            attempt: 2,
        })
        .unwrap();
        assert_eq!(text, r#"{"job_id":7,"attempt":2}"#);
    }
}
