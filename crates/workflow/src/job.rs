// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The document the control plane gives the agent for one job, and the
//! result the agent reports back. Plain data, so it builds for wasm.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plan::PlannedJob;

/// Everything the agent needs to run one job.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JobDocument {
    /// The run's id.
    pub run_id: String,
    /// The run's number within the repository.
    pub run_number: u64,
    /// Which attempt of the run this is, from 1.
    pub run_attempt: u32,
    /// The workflow's name.
    pub workflow: String,
    /// The job to run.
    pub job: PlannedJob,
    /// The event that started the run, as `push` or `pull_request`.
    pub event_name: String,
    /// The webhook payload.
    pub event: Value,
    /// The `github` context.
    pub github: Github,
    /// Results of the jobs this one requires, keyed by `PlannedJob::name`.
    pub needs: BTreeMap<String, Need>,
    /// Secret values, by name.
    pub secrets: BTreeMap<String, String>,
    /// Repository variables, by name.
    pub vars: BTreeMap<String, String>,
    /// Seconds each test took in earlier runs, by test name.
    pub timings: BTreeMap<String, f64>,
    /// The cache service, when the repository has one.
    pub cache: Option<CacheService>,
    /// Where the job asks for OIDC tokens. `None` unless the job has
    /// `id-token: write`.
    pub id_token: Option<IdTokenRequest>,
    /// The artifact service, when the control plane has one. The agent
    /// serves `actions/upload-artifact` and `download-artifact` from it.
    pub artifacts: Option<ArtifactService>,
    /// The `inputs` context of a job of a called workflow: what the
    /// calls passed. `None` for a job of the workflow that was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Value>,
}

impl fmt::Debug for JobDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secrets: BTreeMap<&String, &str> =
            self.secrets.keys().map(|k| (k, "[redacted]")).collect();
        f.debug_struct("JobDocument")
            .field("run_id", &self.run_id)
            .field("run_number", &self.run_number)
            .field("run_attempt", &self.run_attempt)
            .field("workflow", &self.workflow)
            .field("job", &self.job)
            .field("event_name", &self.event_name)
            .field("event", &"[redacted]")
            .field("github", &self.github)
            .field("needs", &self.needs)
            .field("secrets", &secrets)
            .field("vars", &self.vars)
            .field("timings", &self.timings)
            .field("cache", &self.cache)
            .field("id_token", &self.id_token)
            .field("artifacts", &self.artifacts)
            .field("inputs", &self.inputs)
            .finish()
    }
}

/// The `github` context of a run.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Github {
    /// `owner/name`.
    pub repository: String,
    /// The owner.
    pub repository_owner: String,
    /// The commit.
    pub sha: String,
    /// The full ref, such as `refs/heads/main`.
    #[serde(rename = "ref")]
    pub ref_: String,
    /// The short ref name.
    pub ref_name: String,
    /// `branch` or `tag`.
    pub ref_type: String,
    /// The pull request's head branch, empty otherwise.
    pub head_ref: String,
    /// The pull request's base branch, empty otherwise.
    pub base_ref: String,
    /// The user that triggered the run.
    pub actor: String,
    /// The GitHub server, such as `https://github.com`.
    pub server_url: String,
    /// The REST API root.
    pub api_url: String,
    /// The GraphQL endpoint.
    pub graphql_url: String,
    /// The installation token for the repository.
    pub token: String,
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github")
            .field("repository", &self.repository)
            .field("repository_owner", &self.repository_owner)
            .field("sha", &self.sha)
            .field("ref", &self.ref_)
            .field("ref_name", &self.ref_name)
            .field("ref_type", &self.ref_type)
            .field("head_ref", &self.head_ref)
            .field("base_ref", &self.base_ref)
            .field("actor", &self.actor)
            .field("server_url", &self.server_url)
            .field("api_url", &self.api_url)
            .field("graphql_url", &self.graphql_url)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// What a required job produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Need {
    /// How the job ended.
    pub result: Conclusion,
    /// The job's outputs.
    pub outputs: BTreeMap<String, String>,
}

/// Where and how to reach the cache service.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheService {
    /// The service's base URL.
    pub url: String,
    /// The bearer token.
    pub token: String,
    /// The refs a job restores from, in order. The first is the ref the
    /// job saves to. The agent names them in its runtime token, which
    /// `BuildKit` reads to build its index keys.
    pub scopes: Vec<String>,
}

impl fmt::Debug for CacheService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheService")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Where a job with `id-token: write` asks for OIDC tokens: the agent
/// sets `ACTIONS_ID_TOKEN_REQUEST_URL` and `ACTIONS_ID_TOKEN_REQUEST_TOKEN`
/// from it, only for this job.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IdTokenRequest {
    /// The token endpoint. It has a query string, because the toolkit
    /// appends `&audience=` to it.
    pub url: String,
    /// The bearer token for the endpoint, bound to this job's attempt.
    pub token: String,
}

impl fmt::Debug for IdTokenRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdTokenRequest")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Where and how to reach the artifact service of the control plane.
/// The token is the job's own; the service answers only for the job's
/// run.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArtifactService {
    /// The service's base URL.
    pub url: String,
    /// The bearer token.
    pub token: String,
}

impl fmt::Debug for ArtifactService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactService")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// How a job or step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// It finished without error.
    Success,
    /// It failed.
    Failure,
    /// It was cancelled.
    Cancelled,
    /// It did not run.
    Skipped,
}

/// How serious an annotation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLevel {
    /// An error.
    Error,
    /// A warning.
    Warning,
    /// A note.
    Notice,
}

/// A message a step attached to the run, such as `::error::`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Annotation {
    /// How serious it is.
    pub level: AnnotationLevel,
    /// The text.
    pub message: String,
    /// A short heading.
    pub title: Option<String>,
    /// The file it points at.
    pub file: Option<String>,
    /// The first line.
    pub line: Option<u32>,
    /// The last line.
    pub end_line: Option<u32>,
    /// The first column.
    pub col: Option<u32>,
    /// The last column.
    pub end_column: Option<u32>,
}

/// How one step went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StepSummary {
    /// The step's name.
    pub name: String,
    /// The result before `continue-on-error` applies.
    pub outcome: Conclusion,
    /// The result after `continue-on-error` applies.
    pub conclusion: Conclusion,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
}

/// What the agent reports when a job ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JobResult {
    /// How the job ended.
    pub conclusion: Conclusion,
    /// The job's outputs.
    pub outputs: BTreeMap<String, String>,
    /// One entry per step.
    pub steps: Vec<StepSummary>,
    /// Annotations from all steps.
    pub annotations: Vec<Annotation>,
    /// The job's `environment-url`, evaluated after its steps ran. `None`
    /// when the job names none, did not succeed, or the address is not a
    /// usable link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_url: Option<String>,
}
