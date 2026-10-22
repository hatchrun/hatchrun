// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads and plans hatchrun workflow files (`.hatchrun/config.yml`).
//!
//! The crate does no I/O and never reads the clock, so the same code runs
//! in a Cloudflare Worker, the runner agent and the CLI.

pub mod agent;
pub mod config;
pub mod convert;
pub mod cron;
pub mod diagnostic;
pub mod expr;
pub mod job;
pub mod permissions;
pub mod plan;
pub mod secrets;
pub mod split;
pub mod templates;
pub mod trigger;

pub use config::{
    CacheStep, Command, Config, Executor, Job, MAX_SOURCE, Parameter, ParameterKind, RunStep, Step,
    UsesStep, Workflow, WorkflowJob, WorkspaceStep, parse,
};
pub use convert::{Conversion, convert, to_yaml};
pub use cron::{Cron, UtcMinute};
pub use diagnostic::Diagnostic;
pub use expr::{
    ExprError, Functions, JobStatus, checks_status, evaluate, interpolate, job_condition,
    secret_refs, truthy,
};
pub use job::{
    Annotation, AnnotationLevel, CacheService, Conclusion, Github, JobDocument, JobResult, Need,
    StepSummary,
};
pub use permissions::{DEFAULT_SCOPES, Level, Permissions, SCOPES, token_scopes};
pub use plan::{JobKind, Plan, PlannedJob, PlannedStep, compile, config_errors, validate};
pub use secrets::{DEBUG_SECRETS, SecretRefs};
pub use split::split;
pub use templates::{
    Arch, DEFAULT_SPEND_CAP_MICROS, FREE_UNITS, MAX_RUNNING_JOBS, MAX_SPEND_CAP_MICROS,
    MIN_SPEND_CAP_MICROS, TEMPLATES, Template, UNIT_PRICE_MICROS, dollars, spend_micros, template,
    template_labels,
};
pub use trigger::{
    Dispatch, Event, Filter, MAX_CHANGED, MAX_GLOBS, MAX_PATTERNS, PullRequestTrigger, PushTrigger,
    Schedule, Triggers, triggered,
};
