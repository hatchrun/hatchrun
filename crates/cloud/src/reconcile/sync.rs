// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Where GitHub says a job stands, for jobs whose webhooks went
//! missing: an open job in D1 that GitHub finished keeps a runner's
//! place under `max_runners` and its instance alive.

use serde::Deserialize;

use crate::github;
use crate::http::{Request, Response};
use crate::reconcile::time;
use crate::webhook::{Outcome, Status};

/// One job, with the installation's token.
#[must_use]
pub fn job(api: &str, token: &str, repository: &str, id: u64) -> Request {
    github::call(
        "GET",
        format!("{api}/repos/{repository}/actions/jobs/{id}"),
        token,
    )
}

/// What GitHub knows about a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// Its status, mapped onto the `jobs` table's.
    pub status: Status,
    /// The runner that took it, once one did.
    pub runner_name: Option<String>,
    /// Its conclusion and times, once GitHub has them.
    pub outcome: Outcome,
}

#[derive(Deserialize)]
struct Job {
    status: String,
    runner_name: Option<String>,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

/// Reads the job out of the answer. A job GitHub no longer has (its
/// repository is gone or out of the installation's reach) counts as
/// completed: no runner will ever take it.
///
/// # Errors
///
/// GitHub's answer when it is neither a job nor a 404.
pub fn parse(response: &Response) -> Result<Seen, String> {
    if response.status == 404 {
        return Ok(Seen {
            status: Status::Completed,
            runner_name: None,
            outcome: Outcome::default(),
        });
    }
    if !response.ok() {
        return Err(format!(
            "job: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    let job: Job = serde_json::from_slice(&response.body).map_err(|err| format!("job: {err}"))?;
    let status = match job.status.as_str() {
        "in_progress" => Status::InProgress,
        "completed" => Status::Completed,
        // queued, waiting, requested, pending.
        _ => Status::Queued,
    };
    // As the webhook reads it: a start only once a runner took the job.
    let time = |value: Option<String>| value.as_deref().and_then(time::parse);
    let completed = status == Status::Completed;
    let runner_name = job.runner_name.filter(|name| !name.is_empty());
    Ok(Seen {
        status,
        outcome: Outcome {
            conclusion: job.conclusion.filter(|_| completed),
            started_at: time(job.started_at).filter(|_| runner_name.is_some()),
            completed_at: time(job.completed_at).filter(|_| completed),
        },
        runner_name,
    })
}

/// The status to store: GitHub's, unless D1 already has a later one,
/// since a job never goes back.
#[must_use]
pub fn advance(stored: Status, seen: Status) -> Status {
    let rank = |status| match status {
        Status::Queued => 0,
        Status::InProgress => 1,
        Status::Completed => 2,
    };
    if rank(seen) > rank(stored) {
        seen
    } else {
        stored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: &str) -> Response {
        Response {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn asks_for_one_job() {
        assert_eq!(
            job("https://api.github.com", "ghs_x", "acme/app", 7).url,
            "https://api.github.com/repos/acme/app/actions/jobs/7"
        );
    }

    #[test]
    fn reads_the_status_and_runner() {
        assert_eq!(
            parse(&answer(
                200,
                r#"{"id":7,"status":"in_progress","conclusion":null,"runner_name":"hatchrun-7-ab"}"#
            ))
            .unwrap(),
            Seen {
                status: Status::InProgress,
                runner_name: Some("hatchrun-7-ab".to_owned()),
                outcome: Outcome::default(),
            }
        );
        assert_eq!(
            parse(&answer(
                200,
                r#"{"status":"completed","conclusion":"cancelled","runner_name":"",
                    "started_at":"2026-10-14T10:00:00Z","completed_at":"2026-10-14T10:00:05Z"}"#
            ))
            .unwrap(),
            Seen {
                status: Status::Completed,
                runner_name: None,
                outcome: Outcome {
                    conclusion: Some("cancelled".to_owned()),
                    started_at: None,
                    completed_at: Some(1_791_972_005_000),
                },
            }
        );
        assert_eq!(
            parse(&answer(200, r#"{"status":"waiting","runner_name":null}"#))
                .unwrap()
                .status,
            Status::Queued
        );
    }

    #[test]
    fn a_job_github_lost_is_over() {
        assert_eq!(
            parse(&answer(404, r#"{"message":"Not Found"}"#))
                .unwrap()
                .status,
            Status::Completed
        );
        assert!(parse(&answer(403, "{}")).is_err());
    }

    #[test]
    fn jobs_never_go_back() {
        assert_eq!(
            advance(Status::InProgress, Status::Queued),
            Status::InProgress
        );
        assert_eq!(
            advance(Status::Queued, Status::InProgress),
            Status::InProgress
        );
        assert_eq!(
            advance(Status::Queued, Status::Completed),
            Status::Completed
        );
        assert_eq!(advance(Status::Queued, Status::Queued), Status::Queued);
    }
}
