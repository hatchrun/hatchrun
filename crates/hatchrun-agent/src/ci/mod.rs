// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-agent ci`: runs one job for the control plane.
//!
//! The boot ticket in user data is claimed from the broker, which hands
//! over the control plane's address and a job token. The agent claims the
//! job document, runs it with the same executor as `exec` and sends the
//! events as they happen, then reports the result. The control plane
//! decides the job-level `if:`; the agent runs only step-level
//! conditions.

pub mod client;
pub mod sink;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use hatchrun_workflow::agent::BootDocument;
use hatchrun_workflow::{Conclusion, JobKind, JobResult};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{debug, error, info};

use self::client::{Claim, Client};
use self::sink::HttpSink;
use crate::broker::HttpBroker;
use crate::exec::mask::Masker;
use crate::exec::sink::Sink;
use crate::exec::{self, run_job};
use crate::source::{Broker, ci_ticket};

/// How the CI agent runs.
pub struct CiOptions {
    /// How the executor runs the job.
    pub exec: exec::Options,
    /// How long to wait for the boot document and for the job to be
    /// ready, each.
    pub claim_timeout: Duration,
}

/// Claims the boot document, polling until the control plane has stored
/// it or `timeout` passes.
async fn boot(
    broker: &impl Broker,
    url: &str,
    ticket: &str,
    timeout: Duration,
) -> Result<BootDocument> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(doc) = broker.claim(url, ticket).await? {
            return serde_json::from_str(&doc).context("parse boot document");
        }
        if Instant::now() >= deadline {
            bail!("no boot document after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Runs the job that `user_data` names and returns the exit code: 0 when
/// the job succeeded or had already ended, 1 otherwise.
///
/// # Errors
///
/// When the boot document, the claim or the result cannot be had.
pub async fn run(user_data: Vec<u8>, opts: CiOptions) -> Result<i32> {
    let (broker_url, ticket) = ci_ticket(&user_data)?
        .context("user data does not name a CI launch (no \"mode\": \"ci\")")?;
    let broker = HttpBroker::new()?;
    let document = boot(&broker, &broker_url, &ticket, opts.claim_timeout).await?;
    let mut client = Client::new(&document.ci.url, document.ci.token)?;
    client.claim_timeout = opts.claim_timeout;
    run_claimed(&client, &opts.exec).await
}

async fn run_claimed(client: &Client, exec: &exec::Options) -> Result<i32> {
    let mut doc = match client.claim().await? {
        Claim::Job(doc) => *doc,
        Claim::Ended => {
            info!("the job already ended, nothing to run");
            return Ok(0);
        }
    };
    // The control plane alone decides the job-level `if:`. The agent sees
    // only the direct needs, not every ancestor, so it would decide wrong.
    doc.job.if_ = None;
    debug!(run = %doc.run_id, job = %doc.job.id, "claimed the job");
    let mut masker = Masker::default();
    for secret in doc.secrets.values() {
        masker.add(secret);
    }
    masker.add(&doc.github.token);

    let (cancel, cancelled) = watch::channel(0u32);
    let (mut sink, flusher) = HttpSink::start(client.clone(), cancel);
    let result = if matches!(doc.job.kind, JobKind::Approval) {
        sink.line("[hatchrun] approval jobs do not run on a runner");
        failure()
    } else {
        match run_job(doc, exec, &mut sink, cancelled).await {
            Ok(result) => result,
            Err(err) => {
                // The job could not start. The message can name a secret
                // that a path or a user name happens to hold.
                let raw = format!("{err:#}");
                let message = masker.mask(&raw);
                error!("{message}");
                sink.line(&format!("[hatchrun] the job could not start: {message}"));
                failure()
            }
        }
    };
    if let Err(err) = flusher.close().await {
        error!("{err:#}");
    }
    client.result(&result).await?;
    Ok(i32::from(result.conclusion != Conclusion::Success))
}

fn failure() -> JobResult {
    JobResult {
        conclusion: Conclusion::Failure,
        outputs: std::collections::BTreeMap::new(),
        steps: Vec::new(),
        annotations: Vec::new(),
    }
}
