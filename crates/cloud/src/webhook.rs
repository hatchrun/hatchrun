// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! GitHub App webhooks: the signature check and the `workflow_job` events
//! the Worker acts on, kept apart from the Workers runtime so they test
//! natively.

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::labels;

/// Checks `X-Hub-Signature-256`, GitHub's HMAC-SHA256 of the body keyed
/// with the App's webhook secret. The comparison is constant time.
#[must_use]
pub fn verify(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(given) = header
        .and_then(|value| value.strip_prefix("sha256="))
        .and_then(decode_hex)
    else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(value.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Where a job stands, as the `jobs` table records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting for a runner.
    Queued,
    /// A runner took it.
    InProgress,
    /// It finished, was cancelled, or never started.
    Completed,
}

impl Status {
    /// The value stored in `jobs.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// A queued job that asked for a hatchrun runner.
#[derive(Debug, PartialEq, Eq)]
pub struct QueuedJob {
    /// Workflow job id, unique across GitHub.
    pub id: u64,
    /// Workflow run the job belongs to.
    pub run_id: u64,
    /// The App installation that delivered it, which names the tenant.
    pub installation_id: u64,
    /// `owner/name` of the repository.
    pub repository: String,
    /// The job's hatchrun label, as written in the workflow.
    pub label: String,
    /// The runner the label asks for.
    pub spec: labels::Spec,
}

/// What a webhook asks the Worker to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Record a job that needs a runner.
    Queued(QueuedJob),
    /// Move a recorded job to a new status.
    Progress {
        /// Workflow job id.
        id: u64,
        /// The new status.
        status: Status,
    },
    /// Nothing to do, and why. GitHub shows the reason in the App's
    /// delivery log, which is where a user looks when a job waits.
    Ignored(String),
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    workflow_job: Job,
    repository: Repository,
    installation: Option<Installation>,
}

#[derive(Deserialize)]
struct Job {
    id: u64,
    run_id: u64,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

/// Reads a webhook delivery. `event` is the `X-GitHub-Event` header.
///
/// # Errors
///
/// When a `workflow_job` body does not parse.
pub fn parse(event: &str, body: &[u8]) -> Result<Event, serde_json::Error> {
    if event != "workflow_job" {
        return Ok(Event::Ignored(format!("{event} events are not used")));
    }
    let payload: Payload = serde_json::from_slice(body)?;
    let id = payload.workflow_job.id;
    let status = match payload.action.as_str() {
        "queued" => Status::Queued,
        "in_progress" => Status::InProgress,
        "completed" => Status::Completed,
        other => return Ok(Event::Ignored(format!("workflow_job {other} is not used"))),
    };
    if status != Status::Queued {
        return Ok(Event::Progress { id, status });
    }

    let label = match labels::job_label(&payload.workflow_job.labels) {
        Ok(label) => label.to_owned(),
        Err(error) => return Ok(Event::Ignored(error.to_string())),
    };
    let spec = match labels::parse(&label) {
        Ok(spec) => spec,
        Err(error) => return Ok(Event::Ignored(format!("{label}: {error}"))),
    };
    let Some(installation) = payload.installation else {
        return Ok(Event::Ignored("no App installation".to_owned()));
    };
    Ok(Event::Queued(QueuedJob {
        id,
        run_id: payload.workflow_job.run_id,
        installation_id: installation.id,
        repository: payload.repository.full_name,
        label,
        spec,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::{Arch, Spec};

    // The example in GitHub's "Validating webhook deliveries".
    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const SIGNATURE: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_githubs_example() {
        assert!(verify(SECRET, BODY, Some(SIGNATURE)));
        assert!(verify(
            SECRET,
            BODY,
            Some(&SIGNATURE.to_uppercase().replace("SHA256", "sha256"))
        ));
    }

    #[test]
    fn refuses_bad_signatures() {
        assert!(!verify(SECRET, b"Hello, World?", Some(SIGNATURE)));
        assert!(!verify(b"another secret", BODY, Some(SIGNATURE)));
        assert!(!verify(b"", BODY, Some(SIGNATURE)));
        assert!(!verify(SECRET, BODY, None));
        assert!(!verify(SECRET, BODY, Some(&SIGNATURE[7..])));
        assert!(!verify(SECRET, BODY, Some(&SIGNATURE[..70])));
        assert!(!verify(
            SECRET,
            BODY,
            Some(&SIGNATURE.replace("sha256=", "sha1="))
        ));
        let not_hex = format!("sha256={}", "zz".repeat(32));
        assert!(!verify(SECRET, BODY, Some(&not_hex)));
    }

    fn job(action: &str, labels: &[&str]) -> Vec<u8> {
        serde_json::json!({
            "action": action,
            "workflow_job": { "id": 29_679_449, "run_id": 5, "labels": labels, "status": action },
            "repository": { "full_name": "acme/app" },
            "installation": { "id": 42 },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn records_a_queued_hatchrun_job() {
        assert_eq!(
            parse("workflow_job", &job("queued", &["hatchrun/arm64"])).unwrap(),
            Event::Queued(QueuedJob {
                id: 29_679_449,
                run_id: 5,
                installation_id: 42,
                repository: "acme/app".to_owned(),
                label: "hatchrun/arm64".to_owned(),
                spec: Spec {
                    arch: Arch::Arm64,
                    ..Spec::default()
                },
            })
        );
    }

    #[test]
    fn says_why_a_job_is_skipped() {
        assert_eq!(
            parse("workflow_job", &job("queued", &["ubuntu-latest"])).unwrap(),
            Event::Ignored("no hatchrun label".to_owned())
        );
        assert_eq!(
            parse("workflow_job", &job("queued", &["hatchrun/gpu"])).unwrap(),
            Event::Ignored("hatchrun/gpu: unknown option \"gpu\"".to_owned())
        );
        assert_eq!(
            parse("workflow_job", &job("waiting", &["hatchrun"])).unwrap(),
            Event::Ignored("workflow_job waiting is not used".to_owned())
        );
        assert_eq!(
            parse("ping", b"{}").unwrap(),
            Event::Ignored("ping events are not used".to_owned())
        );
    }

    #[test]
    fn tracks_progress_whatever_the_labels() {
        assert_eq!(
            parse("workflow_job", &job("completed", &["ubuntu-latest"])).unwrap(),
            Event::Progress {
                id: 29_679_449,
                status: Status::Completed
            }
        );
    }

    #[test]
    fn rejects_malformed_job_events() {
        assert!(parse("workflow_job", b"{\"action\":\"queued\"}").is_err());
    }
}
