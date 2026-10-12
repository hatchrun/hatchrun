// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The scale set message queue.

use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{Method, Request, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::actions::bearer;
use crate::{
    ApiError, Error, JobMessage, Message, Operation, ScaleSet, Secret, Statistics, decode, expect,
    trim_bom,
};

/// Tells the service how many runners the scale set can run, so it does
/// not hand out more jobs than that.
const MAX_CAPACITY_HEADER: &str = "X-ScaleSetMaxCapacity";

const MESSAGE_TYPE: &str = "RunnerScaleSetJobMessages";

/// A message session on a scale set, from
/// [`crate::Actions::create_session`].
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Session {
    /// The session id.
    #[serde(rename = "sessionId")]
    pub id: String,
    /// The owner name given when the session was created.
    #[serde(default)]
    pub owner_name: String,
    /// The scale set, when the service includes it.
    #[serde(default)]
    pub runner_scale_set: Option<ScaleSet>,
    /// Where messages are read from.
    pub message_queue_url: String,
    /// The token for the message queue and for acquiring jobs.
    pub message_queue_access_token: Secret,
    /// Counts when the session opened. Use them to scale before the first
    /// message arrives.
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

impl Session {
    /// Long-polls for the next message. The response is `None` when the
    /// poll timed out with no message. Pass the id of the last message
    /// handled, or 0 for none.
    ///
    /// # Errors
    ///
    /// [`Error::Url`] for a bad queue URL and [`Error::Request`] when the
    /// request cannot be built.
    pub fn get_message(
        &self,
        last_message_id: u64,
        max_capacity: u32,
    ) -> Result<Operation<Option<Message>>, Error> {
        let mut url = Url::parse(&self.message_queue_url)?;
        if last_message_id > 0 {
            url.query_pairs_mut()
                .append_pair("lastMessageId", &last_message_id.to_string());
        }
        let request = Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(ACCEPT, "application/json; api-version=6.0-preview")
            .header(
                AUTHORIZATION,
                bearer(self.message_queue_access_token.expose())?,
            )
            .header(USER_AGENT, crate::USER_AGENT)
            .header(MAX_CAPACITY_HEADER, max_capacity)
            .body(Vec::new())?;
        Ok(Operation::new(request, |response| {
            match response.status() {
                StatusCode::ACCEPTED => Ok(None),
                StatusCode::OK => parse_message(trim_bom(response.body())).map(Some),
                _ => Err(ApiError::from_response(response).into()),
            }
        }))
    }

    /// Deletes a handled message, so the next poll does not return it.
    ///
    /// # Errors
    ///
    /// [`Error::Url`] for a bad queue URL and [`Error::Request`] when the
    /// request cannot be built.
    pub fn delete_message(&self, message_id: u64) -> Result<Operation<()>, Error> {
        let mut url = Url::parse(&self.message_queue_url)?;
        url.path_segments_mut()
            .map_err(|()| Error::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .push(&message_id.to_string());
        let request = Request::builder()
            .method(Method::DELETE)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                bearer(self.message_queue_access_token.expose())?,
            )
            .header(USER_AGENT, crate::USER_AGENT)
            .body(Vec::new())?;
        Ok(Operation::new(request, |response| {
            expect(response, StatusCode::NO_CONTENT).map(|_| ())
        }))
    }
}

/// The queue's envelope. `body` is a JSON string holding the job messages.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    message_id: u64,
    message_type: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    statistics: Option<Statistics>,
}

fn parse_message(body: &[u8]) -> Result<Message, Error> {
    let envelope: Envelope = decode(body, "message")?;
    if envelope.message_type != MESSAGE_TYPE {
        return Err(Error::Decode {
            what: "message",
            source: serde::de::Error::custom(format!(
                "unknown message type {:?}",
                envelope.message_type
            )),
        });
    }
    let jobs: Vec<JobMessage> = if envelope.body.is_empty() {
        Vec::new()
    } else {
        decode(envelope.body.as_bytes(), "job messages")?
    };
    Ok(Message {
        id: envelope.message_id,
        statistics: envelope.statistics,
        jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::tests::session;
    use crate::tests::response;
    use crate::{ErrorKind, JobMessageKind};

    #[test]
    fn get_message_request() {
        let session = session();
        let first = session.get_message(0, 10).unwrap().request;
        assert_eq!(
            first.uri().to_string(),
            "https://broker.actions.githubusercontent.com/message"
        );
        assert_eq!(first.headers()[MAX_CAPACITY_HEADER], "10");
        assert_eq!(first.headers()[AUTHORIZATION], "Bearer queue-token");

        let next = session.get_message(41, 10).unwrap().request;
        assert_eq!(next.uri().query(), Some("lastMessageId=41"));
    }

    #[test]
    fn no_message() {
        let parser = session().get_message(0, 1).unwrap().parser;
        assert!(parser.parse(&response(202, "")).unwrap().is_none());
    }

    #[test]
    fn expired_token() {
        let parser = session().get_message(0, 1).unwrap().parser;
        assert_eq!(
            parser.parse(&response(401, "")).unwrap_err().kind(),
            Some(ErrorKind::Unauthorized)
        );
    }

    #[test]
    fn parses_job_messages() {
        let jobs = serde_json::to_string(&serde_json::json!([
            {"messageType": "JobAvailable", "runnerRequestId": 11, "acquireJobUrl": "https://x",
             "requestLabels": ["linux"], "repositoryName": "hatchrun", "ownerName": "hatchrun"},
            {"messageType": "JobAssigned", "runnerRequestId": 11},
            {"messageType": "JobStarted", "runnerRequestId": 11, "runnerId": 4, "runnerName": "r-4"},
            {"messageType": "JobCompleted", "runnerRequestId": 11, "runnerId": 4, "result": "succeeded"},
            {"messageType": "SomethingNew"},
        ]))
        .unwrap();
        let body = serde_json::json!({
            "messageId": 42,
            "messageType": "RunnerScaleSetJobMessages",
            "body": jobs,
            "statistics": {"totalAssignedJobs": 1, "totalRunningJobs": 1},
        })
        .to_string();

        let message = session()
            .get_message(0, 1)
            .unwrap()
            .parser
            .parse(&response(200, &body))
            .unwrap()
            .unwrap();

        assert_eq!(message.id, 42);
        assert_eq!(message.statistics.unwrap().total_assigned_jobs, 1);
        let kinds: Vec<_> = message.jobs.iter().map(|job| job.kind).collect();
        assert_eq!(
            kinds,
            [
                JobMessageKind::JobAvailable,
                JobMessageKind::JobAssigned,
                JobMessageKind::JobStarted,
                JobMessageKind::JobCompleted,
                JobMessageKind::Unknown,
            ]
        );
        assert_eq!(message.jobs[0].request_labels, ["linux"]);
        assert_eq!(message.jobs[2].runner_name.as_deref(), Some("r-4"));
        assert_eq!(message.jobs[3].result.as_deref(), Some("succeeded"));
    }

    #[test]
    fn rejects_other_message_types() {
        let body = r#"{"messageId":1,"messageType":"Other","body":""}"#;
        let err = parse_message(body.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("unknown message type"), "{err}");
    }

    #[test]
    fn delete_message_request() {
        let op = session().delete_message(42).unwrap();
        assert_eq!(op.request.method(), Method::DELETE);
        assert_eq!(
            op.request.uri().to_string(),
            "https://broker.actions.githubusercontent.com/message/42"
        );
        op.parser.parse(&response(204, "")).unwrap();
    }
}
