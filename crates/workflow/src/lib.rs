// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads and plans hatchrun workflow files (`.hatchrun/config.yml`).
//!
//! The crate does no I/O and never reads the clock, so the same code runs
//! in a Cloudflare Worker, the runner agent and the CLI.

pub mod config;
pub mod convert;
pub mod diagnostic;
pub mod expr;
pub mod plan;
pub mod templates;
pub mod trigger;

pub use config::{
    CacheStep, Command, Config, Executor, Job, Parameter, ParameterKind, RunStep, Step, UsesStep,
    Workflow, WorkflowJob, WorkspaceStep, parse,
};
pub use convert::{Conversion, convert, to_yaml};
pub use diagnostic::Diagnostic;
pub use expr::{ExprError, Functions, JobStatus, evaluate, interpolate, truthy};
pub use plan::{JobKind, Plan, PlannedJob, PlannedStep, compile, validate};
pub use templates::{Arch, TEMPLATES, Template, template, template_labels};
pub use trigger::{
    Dispatch, Event, Filter, PullRequestTrigger, PushTrigger, Schedule, Triggers, triggered,
};
