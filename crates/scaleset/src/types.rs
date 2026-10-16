// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Objects the API sends and receives.
//!
//! Timestamps stay as the RFC 3339 strings the service sends, which keeps
//! the crate free of a date library.

use serde::{Deserialize, Serialize};

use crate::Secret;

/// A runner group.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunnerGroup {
    /// The group id.
    pub id: u64,
    /// The group name.
    pub name: String,
    /// Whether this is the `Default` group.
    #[serde(rename = "isDefaultGroup")]
    pub is_default: bool,
}

/// A runner scale set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[non_exhaustive]
pub struct ScaleSet {
    /// The scale set id.
    pub id: u64,
    /// The name, which is also what `runs-on` matches.
    pub name: String,
    /// The runner group the scale set belongs to.
    pub runner_group_id: u64,
    /// The runner group name.
    pub runner_group_name: String,
    /// Labels jobs can target with `runs-on`.
    pub labels: Vec<Label>,
    /// Runner settings.
    #[serde(alias = "RunnerSetting")]
    pub runner_setting: RunnerSetting,
    /// Job and runner counts, when the service includes them.
    pub statistics: Option<Statistics>,
}

/// A scale set label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Label {
    /// The label name.
    pub name: String,
    /// `System` for labels the scale set owns.
    #[serde(rename = "type")]
    pub kind: String,
}

/// Settings for the runners of a scale set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunnerSetting {
    /// Keeps runners from updating themselves. Images that ship a pinned
    /// runner set this.
    pub disable_update: bool,
}

/// The scale set to create or update.
#[derive(Clone, Debug)]
pub struct NewScaleSet {
    /// The name, unique within the runner group.
    pub name: String,
    /// The runner group id, from [`crate::Actions::runner_group`].
    pub runner_group_id: u64,
    /// Labels jobs can target. Empty means the name is the only label.
    pub labels: Vec<String>,
    /// Keeps runners from updating themselves.
    pub disable_update: bool,
}

impl NewScaleSet {
    /// The body the service expects. Labels need an explicit `System` type.
    pub(crate) fn body(&self) -> serde_json::Value {
        let labels: Vec<Label> = if self.labels.is_empty() {
            vec![self.name.clone()]
        } else {
            self.labels.clone()
        }
        .into_iter()
        .map(|name| Label {
            name,
            kind: "System".into(),
        })
        .collect();
        serde_json::json!({
            "name": self.name,
            "runnerGroupId": self.runner_group_id,
            "labels": labels,
            "RunnerSetting": { "disableUpdate": self.disable_update },
        })
    }
}

/// Job and runner counts of a scale set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[non_exhaustive]
pub struct Statistics {
    /// Jobs waiting for a scale set to acquire them.
    pub total_available_jobs: u32,
    /// Jobs this scale set acquired.
    pub total_acquired_jobs: u32,
    /// Jobs assigned to this scale set. Scale on this number.
    pub total_assigned_jobs: u32,
    /// Jobs running on a runner.
    pub total_running_jobs: u32,
    /// Runners registered to the scale set.
    pub total_registered_runners: u32,
    /// Runners running a job.
    pub total_busy_runners: u32,
    /// Runners waiting for a job.
    pub total_idle_runners: u32,
}

/// A registered runner.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[non_exhaustive]
pub struct Runner {
    /// The runner id.
    pub id: u64,
    /// The runner name.
    pub name: String,
    /// The scale set the runner belongs to.
    pub runner_scale_set_id: u64,
}

/// A just-in-time runner config.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct JitConfig {
    /// The runner the config registers.
    pub runner: Runner,
    /// The value for `ACTIONS_RUNNER_INPUT_JITCONFIG`. Single use.
    #[serde(rename = "encodedJITConfig")]
    pub encoded_jit_config: Secret,
}

/// A message from the scale set message queue.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Message {
    /// The message id. Delete the message by this id once it is handled,
    /// and pass it as the last message id to the next poll.
    pub id: u64,
    /// Counts at the time of the message.
    pub statistics: Option<Statistics>,
    /// Job events in the order the service sent them.
    pub jobs: Vec<JobMessage>,
}

/// What happened to a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub enum JobMessageKind {
    /// The job can be acquired with [`crate::Actions::acquire_jobs`].
    JobAvailable,
    /// The job is assigned to this scale set and waits for a runner.
    JobAssigned,
    /// A runner started the job.
    JobStarted,
    /// The job finished.
    JobCompleted,
    /// A message type this crate does not know.
    #[serde(other)]
    Unknown,
}

/// A job event.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct JobMessage {
    /// What happened.
    #[serde(rename = "messageType")]
    pub kind: JobMessageKind,
    /// The id to acquire the job by.
    #[serde(default)]
    pub runner_request_id: i64,
    /// `owner/repo`.
    #[serde(default)]
    pub repository_name: String,
    /// The repository owner.
    #[serde(default)]
    pub owner_name: String,
    /// The job id.
    #[serde(default)]
    pub job_id: String,
    /// The workflow file and ref.
    #[serde(default)]
    pub job_workflow_ref: String,
    /// The job name shown in the UI.
    #[serde(default)]
    pub job_display_name: String,
    /// The workflow run id.
    #[serde(default)]
    pub workflow_run_id: i64,
    /// The event that started the workflow.
    #[serde(default)]
    pub event_name: String,
    /// The job's `runs-on` labels.
    #[serde(default)]
    pub request_labels: Vec<String>,
    /// When the job was queued.
    #[serde(default)]
    pub queue_time: Option<String>,
    /// When the job was assigned to the scale set.
    #[serde(default)]
    pub scale_set_assign_time: Option<String>,
    /// When a runner got the job.
    #[serde(default)]
    pub runner_assign_time: Option<String>,
    /// When the job finished.
    #[serde(default)]
    pub finish_time: Option<String>,
    /// Where to acquire the job, for available jobs.
    #[serde(default)]
    pub acquire_job_url: Option<String>,
    /// The runner, for started and completed jobs.
    #[serde(default)]
    pub runner_id: Option<u64>,
    /// The runner name, for started and completed jobs.
    #[serde(default)]
    pub runner_name: Option<String>,
    /// The job result, such as `succeeded`, for completed jobs.
    #[serde(default)]
    pub result: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_scale_set_defaults_label_to_name() {
        let body = NewScaleSet {
            name: "linux-x64".into(),
            runner_group_id: 1,
            labels: Vec::new(),
            disable_update: true,
        }
        .body();
        assert_eq!(
            body,
            serde_json::json!({
                "name": "linux-x64",
                "runnerGroupId": 1,
                "labels": [{"name": "linux-x64", "type": "System"}],
                "RunnerSetting": {"disableUpdate": true},
            })
        );
    }

    #[test]
    fn scale_set_reads_either_setting_casing() {
        let set: ScaleSet =
            serde_json::from_str(r#"{"id":7,"RunnerSetting":{"disableUpdate":true}}"#).unwrap();
        assert!(set.runner_setting.disable_update);
        let set: ScaleSet =
            serde_json::from_str(r#"{"id":7,"runnerSetting":{"disableUpdate":true}}"#).unwrap();
        assert!(set.runner_setting.disable_update);
    }
}
