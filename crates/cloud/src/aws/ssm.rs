// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `GetParameter`, the one SSM call: the connect stack keeps what it
//! allows in a parameter (see [`crate::connect`]). SSM speaks JSON
//! instead of the Query API, so its answers are read with `serde_json`.

use serde::Deserialize;

use super::Error;
use crate::http::{Request, Response};

/// Reads the parameter `name`.
#[must_use]
pub fn get_parameter(endpoint: &str, name: &str) -> Request {
    Request::new("POST", endpoint)
        .header("Content-Type", "application/x-amz-json-1.1")
        .header("X-Amz-Target", "AmazonSSM.GetParameter")
        .body(serde_json::json!({ "Name": name }).to_string())
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "Parameter")]
    parameter: Parameter,
}

#[derive(Deserialize)]
struct Parameter {
    #[serde(rename = "Value")]
    value: String,
}

#[derive(Deserialize)]
struct Failure {
    #[serde(rename = "__type", default)]
    kind: String,
    #[serde(alias = "Message", default)]
    message: String,
}

/// The parameter's value.
///
/// # Errors
///
/// SSM's error, such as `ParameterNotFound`, or a malformed answer.
pub fn value(response: &Response) -> Result<String, Error> {
    if !response.ok() {
        let failure: Failure = serde_json::from_slice(&response.body).map_err(|_| {
            Error::malformed(&format!(
                "SSM answered {} without an error",
                response.status
            ))
        })?;
        // `__type` may carry a namespace: `com.amazonaws.ssm#ParameterNotFound`.
        let code = failure.kind.rsplit('#').next().unwrap_or_default();
        return Err(Error {
            status: response.status,
            code: code.to_owned(),
            message: failure.message,
        });
    }
    let answer: Answer = serde_json::from_slice(&response.body)
        .map_err(|err| Error::malformed(&format!("GetParameter: {err}")))?;
    Ok(answer.parameter.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_the_parameter_by_name() {
        let request = get_parameter(
            "https://ssm.eu-west-1.amazonaws.com/",
            "/hatchrun/connect/abc",
        );
        assert_eq!(
            request.header_value("X-Amz-Target"),
            Some("AmazonSSM.GetParameter")
        );
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            r#"{"Name":"/hatchrun/connect/abc"}"#
        );
    }

    #[test]
    fn reads_the_value_and_errors() {
        let found = Response {
            status: 200,
            body: br#"{"Parameter":{"Name":"/hatchrun/connect/abc","Type":"String","Value":"{\"version\":4}","Version":2}}"#.to_vec(),
        };
        assert_eq!(value(&found).unwrap(), r#"{"version":4}"#);

        let missing = Response {
            status: 400,
            body: br#"{"__type":"com.amazonaws.ssm#ParameterNotFound","message":""}"#.to_vec(),
        };
        assert_eq!(value(&missing).unwrap_err().code, "ParameterNotFound");

        let denied = Response {
            status: 400,
            body: br#"{"__type":"AccessDeniedException","Message":"not allowed"}"#.to_vec(),
        };
        let err = value(&denied).unwrap_err();
        assert_eq!(
            (err.code.as_str(), err.message.as_str()),
            ("AccessDeniedException", "not allowed")
        );
    }
}
