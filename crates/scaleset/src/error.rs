// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Errors.

use std::fmt;

use http::{Method, Response, StatusCode};
use serde::Deserialize;

use crate::trim_bom;

/// Longest response body kept in an error message.
const MAX_ERROR_BODY: usize = 1024;

/// An error building a request or reading a response.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with an error.
    #[error(transparent)]
    Api(Box<ApiError>),
    /// A response body did not match the API.
    #[error("decode {what}: {source}")]
    Decode {
        /// What the body should have held.
        what: &'static str,
        /// The JSON error.
        source: serde_json::Error,
    },
    /// The config URL does not name an enterprise, organization or
    /// repository.
    #[error("invalid config URL {url:?}: {reason}")]
    ConfigUrl {
        /// The URL as given.
        url: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A URL from the API or the caller does not parse.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
    /// A request could not be built, for example because a token holds
    /// bytes a header cannot carry.
    #[error("build request: {0}")]
    Request(#[from] http::Error),
    /// The Actions service token cannot be read.
    #[error("actions service token: {0}")]
    AdminToken(&'static str),
    /// A lookup by name matched more than one object.
    #[error("{0} matched more than one object")]
    Ambiguous(&'static str),
}

impl From<ApiError> for Error {
    fn from(err: ApiError) -> Self {
        Self::Api(Box::new(err))
    }
}

impl Error {
    /// Returns the kind of API error, or `None` for errors that are not
    /// from the API.
    #[must_use]
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Api(api) => Some(api.kind),
            _ => None,
        }
    }
}

/// What went wrong in an API call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 400.
    BadRequest,
    /// 401. On message queue calls the session token expired.
    Unauthorized,
    /// 404.
    NotFound,
    /// 409.
    Conflict,
    /// A runner with this name is already registered.
    RunnerExists,
    /// The runner is not registered.
    RunnerNotFound,
    /// The runner cannot be removed while it runs a job.
    JobStillRunning,
    /// Any other status.
    Other,
}

/// An error response from the API.
#[derive(Debug)]
pub struct ApiError {
    /// What went wrong.
    pub kind: ErrorKind,
    /// The response status.
    pub status: StatusCode,
    /// The exception the service reported, or the start of the body.
    pub message: String,
    /// The Actions service `ActivityId` header, for GitHub support.
    pub activity_id: Option<String>,
    /// The `X-GitHub-Request-Id` header, for GitHub support.
    pub github_request_id: Option<String>,
    /// The request method.
    pub method: Option<Method>,
    /// The request path, without the query.
    pub path: String,
}

impl ApiError {
    pub(crate) fn from_response(response: &Response<Vec<u8>>) -> Self {
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let body = trim_bom(response.body());

        let mut kind = match status {
            StatusCode::BAD_REQUEST => ErrorKind::BadRequest,
            StatusCode::UNAUTHORIZED => ErrorKind::Unauthorized,
            StatusCode::NOT_FOUND => ErrorKind::NotFound,
            StatusCode::CONFLICT => ErrorKind::Conflict,
            _ => ErrorKind::Other,
        };
        let message = match serde_json::from_slice::<Exception>(body) {
            Ok(exception) if !exception.type_name.is_empty() => {
                kind = exception_kind(&exception.type_name).unwrap_or(kind);
                format!("{}: {}", exception.type_name, exception.message)
            }
            _ => String::from_utf8_lossy(&body[..body.len().min(MAX_ERROR_BODY)])
                .trim()
                .to_owned(),
        };

        Self {
            kind,
            status,
            message,
            activity_id: header("ActivityId"),
            github_request_id: header("X-GitHub-Request-Id"),
            method: None,
            path: String::new(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(method) = &self.method {
            write!(f, "{method} {}: ", self.path)?;
        }
        write!(f, "{}", self.status)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        if let Some(id) = &self.activity_id {
            write!(f, " (activity id {id})")?;
        }
        if let Some(id) = &self.github_request_id {
            write!(f, " (github request id {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// The error body of the Actions service.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Exception {
    #[serde(default)]
    type_name: String,
    #[serde(default)]
    message: String,
}

fn exception_kind(type_name: &str) -> Option<ErrorKind> {
    [
        ("AgentExistsException", ErrorKind::RunnerExists),
        ("AgentNotFoundException", ErrorKind::RunnerNotFound),
        ("JobStillRunningException", ErrorKind::JobStillRunning),
    ]
    .into_iter()
    .find_map(|(name, kind)| type_name.contains(name).then_some(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::response;

    #[test]
    fn exception_sets_kind() {
        let err = ApiError::from_response(&response(
            409,
            r#"{"typeName":"GitHub.DistributedTask.WebApi.AgentExistsException, GitHub.DistributedTask.WebApi","message":"runner-1 exists"}"#,
        ));
        assert_eq!(err.kind, ErrorKind::RunnerExists);
        assert!(err.message.ends_with("runner-1 exists"), "{}", err.message);
    }

    #[test]
    fn status_sets_kind_without_exception() {
        let err = ApiError::from_response(&response(401, "Bad credentials"));
        assert_eq!(err.kind, ErrorKind::Unauthorized);
        assert_eq!(err.message, "Bad credentials");
    }

    #[test]
    fn unknown_exception_keeps_status_kind() {
        let err = ApiError::from_response(&response(
            400,
            r#"{"typeName":"ArgumentException","message":"bad"}"#,
        ));
        assert_eq!(err.kind, ErrorKind::BadRequest);
        assert_eq!(err.message, "ArgumentException: bad");
    }

    #[test]
    fn long_bodies_are_cut() {
        let body = "x".repeat(MAX_ERROR_BODY * 2);
        let err = ApiError::from_response(&response(502, &body));
        assert_eq!(err.kind, ErrorKind::Other);
        assert_eq!(err.message.len(), MAX_ERROR_BODY);
    }

    #[test]
    fn display_includes_request_ids() {
        let response = http::Response::builder()
            .status(500)
            .header("ActivityId", "a-1")
            .header("X-GitHub-Request-Id", "g-1")
            .body(b"boom".to_vec())
            .unwrap();
        let err = ApiError::from_response(&response);
        assert_eq!(
            err.to_string(),
            "500 Internal Server Error: boom (activity id a-1) (github request id g-1)"
        );
    }
}
