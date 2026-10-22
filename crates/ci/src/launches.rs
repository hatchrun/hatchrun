// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `hatchrun-ci-launches` queue: starts the instance of each job
//! the run object queued (see [`crate::run`]).
//!
//! A message names a job and the attempt it was queued for. The
//! consumer loads the job and does nothing when the attempt differs
//! (the job was re-run since) or the job is not queued any more. Then
//! it checks the tenant's caps (see [`crate::billing::admission`]): a
//! tenant without a card, at its spend cap or running its most jobs
//! has the job held, queued with its reason and looked at again 30 s
//! later (see [`hold_job`]). Otherwise it claims the launch, which only
//! one consumer can, and:
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
use hatchrun_cloud::ci_wake::{self, Woken};
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::http::Request;
use hatchrun_cloud::start::{Cause, Failure};
use hatchrun_cloud::{jwt, webcrypto};
use hatchrun_workflow::agent::{BootDocument, CiBoot};
use hatchrun_workflow::{DEFAULT_SPEND_CAP_MICROS, MAX_RUNNING_JOBS, Template, template};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{
    Env, MessageBatch, MessageExt, Method, QueueRetryOptions, QueueRetryOptionsBuilder,
    RequestInit, console_error, console_log,
};

use crate::billing::{self, HOLD_RECHECK_MS, Hold};
use crate::env::{Settings, run_stub};
use crate::environments::{self, Gate};
use crate::launcher::{Backend, LaunchSpec, user_data};
use crate::reconcile::{self, LAUNCH_QUIET_MS};
use crate::run::LAUNCHES;
use crate::store::{self, Claim, JobRow};
use crate::token::{self, Claims};

/// The dead-letter queue in `wrangler.toml`.
pub const DEAD_LETTERS: &str = "hatchrun-ci-launches-dlq";

/// How long a launch that found no capacity waits, in milliseconds.
const RETRY_AFTER_MS: u64 = 30_000;
/// How long a message waits before it is delivered again, in seconds.
const REDELIVER_AFTER_SECS: u32 = 15;
/// Launch attempts a job gets.
const MAX_LAUNCH_ATTEMPTS: u32 = 3;
/// How long the broker keeps a boot document for its instance, in
/// seconds.
const TICKET_EXPIRES_IN: u64 = token::BOOT_SECS;
/// The message that ends a job whose launch message ran out of retries.
const DEAD_LETTER_MESSAGE: &str = "the launch did not finish";
/// The message that ends a job whose row names no runner template.
const NO_TEMPLATE: &str = "job has no runner template";
/// The message that ends a job that used all its launch attempts.
const OUT_OF_ATTEMPTS: &str = "the launch did not finish in 3 attempts";

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
            batch.retry_all_with_options(&redeliver_delay());
            return;
        }
    };
    let (settings, backend) = match start(&env).await {
        Ok(started) => started,
        Err(err) => {
            console_error!("launch queue: {err}");
            batch.retry_all_with_options(&redeliver_delay());
            return;
        }
    };
    let results = join_all(
        messages
            .iter()
            .map(|message| launch(&env, &settings, &backend, *message.body())),
    )
    .await;
    let delay = redeliver_delay();
    for (message, result) in messages.iter().zip(results) {
        match result {
            Ok(()) => message.ack(),
            Err(err) => {
                console_error!("launch queue: job {}: {err}", message.body().job_id);
                message.retry_with_options(&delay);
            }
        }
    }
}

fn redeliver_delay() -> QueueRetryOptions {
    QueueRetryOptionsBuilder::new()
        .with_delay_seconds(REDELIVER_AFTER_SECS)
        .build()
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
    let delivery = delivery(&job, message.attempt);
    match delivery {
        Delivery::Skip => return Ok(()),
        Delivery::Wait => {
            return Err("the job is still launching or its end is not recorded".to_owned());
        }
        Delivery::Launch | Delivery::GiveUp => {}
    }
    let now = now_ms();
    if delivery == Delivery::Launch && job.environment.is_some() {
        let Some(found) = store::job_environment(&db, job.id)
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        let asked = job.environment.as_deref().unwrap_or_default();
        match environments::gate(
            asked,
            &found,
            environments::decision(&job),
            &settings.app_url,
        ) {
            Gate::Pass => {}
            Gate::Fail(message) => return end_job(env, &db, &job, &message).await,
            Gate::Approval(message) => {
                return hold_job(env, &db, &job, "approval", &message, now).await;
            }
        }
    }
    let standing = match store::standing(
        &db,
        message.job_id,
        billing::month_start(now),
        DEFAULT_SPEND_CAP_MICROS,
    )
    .await
    {
        Ok(Some(standing)) => standing,
        Ok(None) => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    // A job that is out of launch attempts is ended, not launched, so no
    // cap applies to it.
    let max_running = if delivery == Delivery::GiveUp {
        u32::MAX
    } else {
        if let Some(reason) = billing::admission(&standing) {
            let message =
                reason.message(&standing.org_login, &settings.app_url, standing.cap_micros);
            return hold_job(env, &db, &job, reason.as_str(), &message, now).await;
        }
        MAX_RUNNING_JOBS
    };
    let job = match store::claim_launch(&db, message.job_id, &standing.tenant_id, max_running, now)
        .await
        .map_err(|err| err.to_string())?
    {
        Claim::Claimed(job) => job,
        Claim::Busy => {
            let reason = Hold::Concurrency;
            let message =
                reason.message(&standing.org_login, &settings.app_url, standing.cap_micros);
            return hold_job(env, &db, &job, reason.as_str(), &message, now).await;
        }
        Claim::Gone => return Ok(()),
    };
    if delivery == Delivery::GiveUp {
        return end_job(env, &db, &job, OUT_OF_ATTEMPTS).await;
    }
    let Some(template) = runner_template(&job) else {
        return end_job(env, &db, &job, NO_TEMPLATE).await;
    };
    match launch_claimed(&db, settings, backend, &job, template).await {
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

/// Keeps the queued job held for `reason` (the word `ci_jobs.held`
/// stores), to be looked at again after [`HOLD_RECHECK_MS`], and tells
/// the run's object `message` every time, changed or not: a post that
/// fails is made again when the reconciler requeues the job. The message
/// is acked either way, so an error is only a database one.
async fn hold_job(
    env: &Env,
    db: &Db,
    job: &JobRow,
    reason: &str,
    message: &str,
    now: u64,
) -> Result<(), String> {
    let changed = store::hold(db, job.id, job.attempt, reason, now + HOLD_RECHECK_MS, now)
        .await
        .map_err(|err| err.to_string())?;
    if changed {
        console_log!("job {}: held, {reason}", job.id);
    }
    let answer = post_to_run(env, db, job, "held", message).await;
    if !matches!(answer, EndAnswer::Status(200..=299 | 409)) {
        console_error!("job {}: post held: {answer:?}", job.id);
    }
    Ok(())
}

/// Sends a launch message for each of `woken`: the consumer checks the
/// caps again.
///
/// # Errors
///
/// The queue did not take a message; the ones before it were sent.
pub(crate) async fn send_wakes(env: &Env, woken: &[Woken]) -> Result<(), String> {
    let queue = env.queue(LAUNCHES).map_err(|e| e.to_string())?;
    for job in woken {
        queue
            .send(LaunchMessage {
                job_id: job.job_id,
                attempt: job.attempt,
            })
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A job of `tenant_id` ended: wakes its oldest job held for the
/// concurrency cap, which the freed place is for. A failure is logged
/// and left to the reconciler's backstop, since the job's end is
/// recorded already.
pub(crate) async fn wake_oldest(env: &Env, db: &Db, tenant_id: &str) {
    let woken = match ci_wake::held(db, tenant_id, Hold::Concurrency.as_str(), 1).await {
        Ok(woken) => woken,
        Err(err) => {
            console_error!("tenant {tenant_id}: find the held job: {err}");
            return;
        }
    };
    if let Err(err) = send_wakes(env, &woken).await {
        console_error!("tenant {tenant_id}: wake the held job: {err}");
    }
}

/// The runner template the job's row names, if it names a known one.
fn runner_template(job: &JobRow) -> Option<&'static Template> {
    job.template.as_deref().and_then(template)
}

/// What a delivered message asks of the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// Nothing: the job moved on, was re-run, or is gone.
    Skip,
    /// Claim and start an instance.
    Launch,
    /// Claim and end the job: it used its launch attempts.
    GiveUp,
    /// The job is launching, or its failure is not recorded yet. The
    /// message is retried; the dead-letter consumer ends the job when
    /// nothing else did.
    Wait,
}

/// Decides what the message for `attempt` does to `job`.
fn delivery(job: &JobRow, attempt: u32) -> Delivery {
    if job.attempt != attempt {
        return Delivery::Skip;
    }
    match job.status.as_str() {
        "queued" if job.launch_attempts >= MAX_LAUNCH_ATTEMPTS => Delivery::GiveUp,
        "queued" => Delivery::Launch,
        "launching" => Delivery::Wait,
        _ => Delivery::Skip,
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
    template: &'static Template,
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
        name: crate::reconcile::runner_name(job.id, job.attempt, job.launch_attempts),
        user_data: user_data(&settings.broker_url, &ticket),
        template,
    };
    // The ticket is stored before any instance starts, so a pool instance
    // that boots at once finds it, and a broker that is down launches
    // nothing.
    store_ticket(settings, &ticket, &document).await?;
    let instance_id = match from_pool(db, backend, &spec).await {
        Some(instance_id) => instance_id,
        None => backend.launch(db, &spec).await?,
    };
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

/// Hands the oldest ready instance of the job's template from the warm
/// pool to the job and starts it, or returns `None` for a fresh launch:
/// the pool has none, could not be read, or the instance did not take
/// the job's user data (it is terminated, as an instance in an unknown
/// state must not run a job).
async fn from_pool(db: &Db, backend: &Backend, spec: &LaunchSpec) -> Option<String> {
    let template = spec.template.labels[0];
    let instance_id = match store::take_pool(db, template).await {
        Ok(Some(instance_id)) => instance_id,
        Ok(None) => return None,
        Err(err) => {
            console_error!(
                "job {}: take a {template} pool instance: {err}",
                spec.job_id
            );
            return None;
        }
    };
    match backend.start_pooled(&instance_id, spec).await {
        Ok(()) => {
            console_log!("job {}: started pool instance {instance_id}", spec.job_id);
            Some(instance_id)
        }
        Err(failure) => {
            console_error!(
                "job {}: start pool instance {instance_id}: {}",
                spec.job_id,
                failure.message
            );
            backend.terminate(&instance_id).await;
            None
        }
    }
}

/// Stores the boot document in the broker under `ticket`.
async fn store_ticket(
    settings: &Settings,
    ticket: &str,
    document: &BootDocument,
) -> Result<(), Failure> {
    let request = Request::new(
        "PUT",
        format!("{}/v1/tickets/{ticket}", settings.broker_url),
    )
    .header("Authorization", format!("Bearer {}", settings.broker_token))
    .header("Content-Type", "application/json")
    .body(broker_body(document).to_string());
    let response = send(&request)
        .await
        .map_err(|err| Failure::new(Cause::Unreachable, format!("store boot document: {err}")))?;
    if response.ok() {
        Ok(())
    } else {
        Err(Failure::new(
            broker_cause(response.status),
            format!(
                "store boot document: broker answered {}: {}",
                response.status,
                response.text()
            ),
        ))
    }
}

/// A broker that is throttling or failing is worth another launch; a
/// 4xx answer means the Worker is misconfigured.
pub(crate) fn broker_cause(status: u16) -> Cause {
    if status == 429 || status >= 500 {
        Cause::Unreachable
    } else {
        Cause::Broker
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
        Next::Fail => end_job(env, db, job, &failure.message).await,
    }
}

/// What the run object, or its absence, answered to ending a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EndAnswer {
    /// The object answered with this status.
    Status(u16),
    /// The job's run has no row.
    RunMissing,
    /// The object could not be asked.
    Error,
}

/// What to do about the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndAction {
    /// The object ended the job, or it was already over.
    Done,
    /// Nobody owns the job: complete its row here.
    GiveUp,
    /// Try again: retry the message.
    Retry,
}

/// Decides what follows `answer`. A `409` means the attempt ended or
/// was replaced; a `404` or a missing run means no object holds the job.
pub(crate) fn end_action(answer: &EndAnswer) -> EndAction {
    match answer {
        EndAnswer::Status(200..=299 | 409) => EndAction::Done,
        EndAnswer::Status(404) | EndAnswer::RunMissing => EndAction::GiveUp,
        EndAnswer::Status(_) | EndAnswer::Error => EndAction::Retry,
    }
}

/// Asks the run's object to end attempt `job.attempt` as failed.
pub(crate) async fn post_failed(env: &Env, db: &Db, job: &JobRow, message: &str) -> EndAnswer {
    post_to_run(env, db, job, "failed", message).await
}

/// Posts `{"message": message}` to route `route` of the object of the
/// job's run, for the job's current attempt.
async fn post_to_run(env: &Env, db: &Db, job: &JobRow, route: &str, message: &str) -> EndAnswer {
    let run = match store::run(db, job.run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return EndAnswer::RunMissing,
        Err(err) => {
            console_error!("job {}: read run: {err}", job.id);
            return EndAnswer::Error;
        }
    };
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(json!({ "message": message }).to_string().into()));
    let url = format!(
        "https://run/{route}?job={}&attempt={}&run={}&tenant={}&days={}",
        job.id, job.attempt, run.id, run.tenant_id, run.retention_days
    );
    let sent = async {
        let request = worker::Request::new_with_init(&url, &init)?;
        run_stub(env, run.id)?.fetch_with_request(request).await
    };
    match sent.await {
        Ok(response) => EndAnswer::Status(response.status_code()),
        Err(err) => {
            console_error!("job {}: post {route}: {err}", job.id);
            EndAnswer::Error
        }
    }
}

/// Ends the job through the run's object. When nobody owns it, its row
/// is completed here, whether it was claimed (launching) or still
/// queued. An error asks for the message to be retried.
pub(crate) async fn end_job(env: &Env, db: &Db, job: &JobRow, message: &str) -> Result<(), String> {
    let answer = post_failed(env, db, job, message).await;
    match end_action(&answer) {
        EndAction::Done => Ok(()),
        EndAction::GiveUp => {
            console_error!("job {}: no run object holds it, failing the row", job.id);
            let now = now_ms();
            let claimed = store::launch_failed(db, job.id, message, None, now)
                .await
                .map_err(|err| err.to_string())?;
            if claimed {
                return Ok(());
            }
            // Not claimed: the job is still queued (an environment check
            // ended it), and launch_failed only changes a launching one.
            reconcile::fail_row(env, db, job, now).await
        }
        EndAction::Retry => Err(format!("could not end job {}: {answer:?}", job.id)),
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
    match dead_letter_action(&job, message.attempt, now_ms()) {
        DeadLetter::Leave => Ok(()),
        DeadLetter::End => end_job(env, &db, &job, DEAD_LETTER_MESSAGE).await,
    }
}

/// What the dead-letter consumer does with a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadLetter {
    /// Nothing: the job moved on, or the reconciler decides.
    Leave,
    /// End the job as failed.
    End,
}

/// A `queued` job is left to the reconciler, which sends it again (a
/// database blip is the usual reason the message ran out of retries). A
/// `launching` job is ended only once it has been quiet for the
/// reconciler's [`LAUNCH_QUIET_MS`]; before that another consumer may
/// still be inside its launch.
pub(crate) fn dead_letter_action(job: &JobRow, message_attempt: u32, now: u64) -> DeadLetter {
    if job.attempt == message_attempt
        && job.status == "launching"
        && job.updated_at < now.saturating_sub(LAUNCH_QUIET_MS)
    {
        DeadLetter::End
    } else {
        DeadLetter::Leave
    }
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

    fn quiet_row(status: &str, attempt: u32, updated_at: u64) -> JobRow {
        JobRow {
            updated_at,
            ..row(status, attempt, 1)
        }
    }

    #[test]
    fn the_dead_letter_consumer_leaves_queued_and_fresh_launching_jobs() {
        let now = 10_000_000;
        assert_eq!(
            dead_letter_action(&quiet_row("queued", 1, 0), 1, now),
            DeadLetter::Leave
        );
        assert_eq!(
            dead_letter_action(&quiet_row("launching", 1, now - 1_000), 1, now),
            DeadLetter::Leave
        );
        assert_eq!(
            dead_letter_action(
                &quiet_row("launching", 1, now - LAUNCH_QUIET_MS - 1),
                1,
                now
            ),
            DeadLetter::End
        );
        assert_eq!(
            dead_letter_action(&quiet_row("launching", 2, 0), 1, now),
            DeadLetter::Leave,
            "another attempt"
        );
        assert_eq!(
            dead_letter_action(&quiet_row("running", 1, 0), 1, now),
            DeadLetter::Leave
        );
    }

    #[test]
    fn a_broker_that_throttles_or_fails_is_retried() {
        for status in [429, 500, 502, 503, 504] {
            assert_eq!(broker_cause(status), Cause::Unreachable, "{status}");
            assert_eq!(
                next_step(&broker_cause(status), 1, 0),
                Next::Retry { at_ms: 30_000 }
            );
        }
        for status in [400, 401, 403, 404, 409] {
            assert_eq!(broker_cause(status), Cause::Broker, "{status}");
            assert_eq!(next_step(&broker_cause(status), 1, 0), Next::Fail);
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
    fn ending_a_job_follows_the_answer() {
        for status in [200, 204, 409] {
            assert_eq!(end_action(&EndAnswer::Status(status)), EndAction::Done);
        }
        assert_eq!(end_action(&EndAnswer::Status(404)), EndAction::GiveUp);
        assert_eq!(end_action(&EndAnswer::RunMissing), EndAction::GiveUp);
        for status in [400, 500, 503] {
            assert_eq!(end_action(&EndAnswer::Status(status)), EndAction::Retry);
        }
        assert_eq!(end_action(&EndAnswer::Error), EndAction::Retry);
    }

    fn row(status: &str, attempt: u32, launch_attempts: u32) -> JobRow {
        serde_json::from_value(json!({
            "id": 1, "run_id": 2, "plan_id": "a", "name": "a", "status": status,
            "conclusion": null, "instance_id": null, "attempt": attempt,
            "launch_attempts": launch_attempts, "next_launch_at": null,
            "timeout_minutes": 10, "updated_at": 0, "heartbeat_at": null,
            "started_at": null, "completed_at": null, "steps": "[]", "log_key": null,
            "template": "ubuntu-latest", "token_permissions": "{}",
            "secret_refs": "{\"all\":false,\"names\":[]}",
        }))
        .unwrap()
    }

    #[test]
    fn a_row_without_a_known_template_has_no_runner_template() {
        let mut job = row("queued", 1, 0);
        assert_eq!(
            runner_template(&job).map(|t| t.vcpu),
            Some(2),
            "ubuntu-latest"
        );
        job.template = Some("ubuntu-26.04-16core-arm".into());
        assert_eq!(runner_template(&job).map(|t| t.vcpu), Some(16));
        job.template = Some("ubuntu-24.04".into());
        assert_eq!(runner_template(&job), None);
        job.template = None;
        assert_eq!(runner_template(&job), None);
    }

    #[test]
    fn a_delivery_launches_only_a_queued_job_with_attempts_left() {
        assert_eq!(delivery(&row("queued", 1, 0), 1), Delivery::Launch);
        assert_eq!(delivery(&row("queued", 1, 2), 1), Delivery::Launch);
        assert_eq!(delivery(&row("queued", 1, 3), 1), Delivery::GiveUp);
        assert_eq!(delivery(&row("queued", 1, 5), 1), Delivery::GiveUp);
        assert_eq!(delivery(&row("queued", 2, 0), 1), Delivery::Skip);
        assert_eq!(delivery(&row("launching", 1, 1), 1), Delivery::Wait);
        for status in ["launched", "running", "completed", "waiting"] {
            assert_eq!(delivery(&row(status, 1, 1), 1), Delivery::Skip, "{status}");
        }
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
