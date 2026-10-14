// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Where GitHub says a job stands, for jobs whose webhooks went
//! missing: an open job in D1 that GitHub finished keeps a runner's
//! place under `max_runners` and its instance alive.

use serde::Deserialize;

use crate::github;
use crate::http::{Request, Response};
use crate::webhook::Status;

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
}

#[derive(Deserialize)]
struct Job {
    status: String,
    runner_name: Option<String>,
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
    Ok(Seen {
        status,
        runner_name: job.runner_name.filter(|name| !name.is_empty()),
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
            }
        );
        assert_eq!(
            parse(&answer(
                200,
                r#"{"status":"completed","conclusion":"cancelled","runner_name":""}"#
            ))
            .unwrap(),
            Seen {
                status: Status::Completed,
                runner_name: None,
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
