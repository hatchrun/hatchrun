// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The protocol between the control plane and the runner agent on a CI
//! instance: the boot document, the claim, event batches and the result.
//! Plain data, so it builds for wasm.
//!
//! The agent claims its job with `POST` to [`CLAIM_PATH`] and gets a
//! [`JobDocument`](crate::job::JobDocument) back. A `503` means retry
//! after a second, and a `409` means the job ended and the agent exits.
//! It sends [`Batch`]es to [`EVENTS_PATH`] about every
//! [`FLUSH_INTERVAL_MS`] and reports a [`JobResult`](crate::job::JobResult)
//! to [`RESULT_PATH`]. Any `401` ends the agent.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::job::{Annotation, StepSummary};

/// Where the agent claims its job.
pub const CLAIM_PATH: &str = "/v1/agent/claim";
/// Where the agent sends event batches.
pub const EVENTS_PATH: &str = "/v1/agent/events";
/// Where the agent reports the job's result.
pub const RESULT_PATH: &str = "/v1/agent/result";
/// How often the agent sends a batch of events, in milliseconds.
pub const FLUSH_INTERVAL_MS: u64 = 1_000;
/// The longest the agent waits between heartbeats, in milliseconds.
pub const HEARTBEAT_INTERVAL_MS: u64 = 10_000;
/// How long a job may stay silent before it fails, in milliseconds.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 120_000;
/// The largest batch the agent sends, in bytes.
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

/// What the broker hands a CI runner: `{"ci": {"url": ..., "token": ...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootDocument {
    /// The control plane's address and the job token.
    pub ci: CiBoot,
}

/// The control plane's address and the job token the agent calls with.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CiBoot {
    /// The control plane's base URL.
    pub url: String,
    /// The job token, sent as a bearer token.
    pub token: String,
}

impl fmt::Debug for CiBoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CiBoot")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// One thing that happened while the job ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A step began.
    StepStarted {
        /// The step's position in the job, from 0.
        index: usize,
        /// The step's name.
        name: String,
    },
    /// One line of output. The agent has already masked it.
    Line {
        /// The line's text.
        text: String,
    },
    /// A workflow command annotation.
    Annotation {
        /// The annotation.
        annotation: Annotation,
    },
    /// A step ended.
    StepFinished {
        /// The step's position in the job, from 0.
        index: usize,
        /// How the step ended.
        summary: StepSummary,
    },
}

/// A numbered group of events, sent in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    /// The batch's sequence number, from 1 and increasing by one.
    pub seq: u64,
    /// The events, in the order they happened.
    pub events: Vec<AgentEvent>,
}

/// The control plane's answer to a [`Batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReply {
    /// Set to 1 when the run was cancelled. Otherwise 0.
    pub cancel: u32,
}
