// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads and plans hatchrun workflow files (`.hatchrun/config.yml`).
//!
//! The crate does no I/O and never reads the clock, so the same code runs
//! in a Cloudflare Worker, the runner agent and the CLI.

pub mod agent;
pub mod artifacts;
pub mod call;
pub mod concurrency;
pub mod config;
pub mod container;
pub mod convert;
pub mod cron;
pub mod diagnostic;
pub mod dynamic;
pub mod environment;
pub mod expr;
pub mod job;
pub mod permissions;
pub mod plan;
pub mod secrets;
pub mod split;
pub mod templates;
pub mod trigger;

pub use call::{
    CallFrame, CallInput, CallSecret, CallTarget, CallTrigger, Callee, InputType, Library,
    MAX_CALL_DEPTH, MAX_UNIQUE_CALLEES, RemoteCall, SecretsPassed, entry_in, read_value,
    remote_calls, remote_calls_from, resolve_frame, resolve_inputs, shared_depth,
};
pub use concurrency::{Cancel, Concurrency, Resolved};
pub use config::{
    CacheStep, CheckoutStep, Command, Config, Container, ContinueOnError, Credentials, Executor,
    Job, MAX_SOURCE, Matrix, Outcome, Parameter, ParameterKind, Require, RunStep, Statuses, Step,
    Strategy, Submodules, UsesStep, Workflow, WorkflowJob, WorkspaceStep, parse,
};
pub use convert::{Conversion, convert, to_yaml};
pub use cron::{Cron, UtcMinute};
pub use diagnostic::Diagnostic;
pub use dynamic::{Leg, Siblings, expand};
pub use environment::{
    ENVIRONMENT_NAME_MAX, ENVIRONMENT_URL_MAX, is_expression, valid_environment_name,
    valid_environment_url,
};
pub use expr::{
    ExprError, Functions, HATCHRUN_KEYS, JobStatus, checks_status, continue_on_error, evaluate,
    hatchrun_context, interpolate, job_condition, may_hold_without, reads_context, secret_refs,
    split_job_outputs, truthy,
};
pub use job::{
    Annotation, AnnotationLevel, ArtifactService, CacheService, Conclusion, Github, IdTokenRequest,
    JobDocument, JobResult, Need, StepSummary,
};
pub use permissions::{
    DEFAULT_SCOPES, ID_TOKEN, ID_TOKEN_SCOPE, Level, Permissions, SCOPES, token_scopes,
};
pub use plan::{
    Deferred, JobKind, Plan, PlannedJob, PlannedStep, PlannedStrategy, Ready, compile,
    compile_with, config_errors, ready, run_title, validate,
};
pub use secrets::{DEBUG_SECRETS, SecretRefs};
pub use split::split;
pub use templates::{
    Arch, DEFAULT_SPEND_CAP_MICROS, MAX_RUNNING_JOBS, MAX_SPEND_CAP_MICROS, MIN_SPEND_CAP_MICROS,
    TEMPLATES, Template, UNIT_PRICE_MICROS_PER_MINUTE, dollars, spend_micros, template,
    template_labels,
};
pub use trigger::{
    Dispatch, Event, Filter, MAX_CHANGED, MAX_GLOBS, MAX_PATTERNS, MERGE_GROUP_TYPES,
    MergeGroupTrigger, PullRequestTrigger, PushTrigger, RELEASE_TYPES, RefTrigger, ReleaseTrigger,
    RepositoryDispatchTrigger, Schedule, Triggers, WORKFLOW_RUN_TYPES, WorkflowRunTrigger,
    triggered,
};
