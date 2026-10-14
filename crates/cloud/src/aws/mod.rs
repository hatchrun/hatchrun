// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The few AWS calls the Worker makes, over the Query API.
//!
//! aws-sdk does not build for wasm, so requests are built here as
//! [`crate::http::Request`]s, signed with `SigV4` by `aws-sigv4`, and the
//! XML answers are read with `roxmltree`. Nothing here sends anything.

pub mod describe;
pub mod ec2;
pub mod instances;
pub mod sts;

use std::time::{Duration, SystemTime};

use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;

use crate::http::{Request, Response};

/// Temporary credentials from STS.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// `ASIA...`
    pub access_key_id: String,
    /// The secret half.
    pub secret_access_key: String,
    /// Session token, sent with every request.
    pub session_token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// An error AWS answered with, or an answer that did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// HTTP status, 0 when the answer was a success that did not parse.
    pub status: u16,
    /// AWS error code, such as `InsufficientInstanceCapacity`.
    pub code: String,
    /// AWS's message.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl Error {
    fn malformed(what: &str) -> Self {
        Self {
            status: 0,
            code: "MalformedResponse".to_owned(),
            message: what.to_owned(),
        }
    }
}

/// The endpoint of `service` in `region`, or `override_url` when set,
/// which points the Worker at a local stub in development.
#[must_use]
pub fn endpoint(service: &str, region: &str, override_url: Option<&str>) -> String {
    match override_url {
        Some(url) if !url.is_empty() => format!("{}/", url.trim_end_matches('/')),
        _ => format!("https://{service}.{region}.amazonaws.com/"),
    }
}

/// A Query API call: a form-encoded POST of `Action`, `Version` and the
/// parameters.
#[must_use]
pub fn query(endpoint: &str, action: &str, version: &str, params: &[(String, String)]) -> Request {
    let mut all = vec![
        ("Action".to_owned(), action.to_owned()),
        ("Version".to_owned(), version.to_owned()),
    ];
    all.extend_from_slice(params);
    Request::new("POST", endpoint)
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .body(crate::http::form(&all))
}

/// Signs `request` with `SigV4` for `service` in `region` at `now_ms`
/// (Unix milliseconds), adding `X-Amz-Date`, `X-Amz-Security-Token` and
/// `Authorization`.
///
/// # Errors
///
/// When the URL or a header cannot be signed.
pub fn sign_v4(
    request: &mut Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
    now_ms: u64,
) -> Result<(), String> {
    let identity = aws_credential_types::Credentials::new(
        &credentials.access_key_id,
        &credentials.secret_access_key,
        Some(credentials.session_token.clone()).filter(|token| !token.is_empty()),
        None,
        "sts",
    )
    .into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(SystemTime::UNIX_EPOCH + Duration::from_millis(now_ms))
        .settings(SigningSettings::default())
        .build()
        .map_err(|err| err.to_string())?
        .into();
    let signable = SignableRequest::new(
        request.method,
        request.url.as_str(),
        request
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
        SignableBody::Bytes(&request.body),
    )
    .map_err(|err| err.to_string())?;
    let (instructions, _) = sign(signable, &params)
        .map_err(|err| err.to_string())?
        .into_parts();
    let (headers, _) = instructions.into_parts();
    for header in headers {
        request
            .headers
            .push((header.name().to_owned(), header.value().to_owned()));
    }
    Ok(())
}

/// Parses a Query API answer, returning AWS's error for anything but a
/// 2xx.
fn document(response: &Response) -> Result<roxmltree::Document<'_>, Error> {
    let text = std::str::from_utf8(&response.body).map_err(|_| Error::malformed("not UTF-8"))?;
    let parsed = roxmltree::Document::parse(text);
    if response.ok() {
        return parsed.map_err(|err| Error::malformed(&format!("not XML: {err}")));
    }
    // EC2 answers `<Response><Errors><Error>`, STS `<ErrorResponse><Error>`.
    let found = parsed.ok().and_then(|doc| {
        let error = doc.descendants().find(|node| node.has_tag_name("Error"))?;
        Some((
            child_text(error, "Code").unwrap_or_default().to_owned(),
            child_text(error, "Message").unwrap_or_default().to_owned(),
        ))
    });
    let (code, message) =
        found.unwrap_or_else(|| (format!("HTTP{}", response.status), response.text()));
    Err(Error {
        status: response.status,
        code,
        message,
    })
}

/// The text of the first descendant of `node` named `name`, by local
/// name, so namespaced EC2 answers match too.
fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.descendants()
        .find(|child| child.tag_name().name() == name)
        .and_then(|child| child.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            endpoint("ec2", "eu-west-1", None),
            "https://ec2.eu-west-1.amazonaws.com/"
        );
        assert_eq!(
            endpoint("sts", "eu-west-1", Some("http://127.0.0.1:8788/")),
            "http://127.0.0.1:8788/"
        );
        assert_eq!(
            endpoint("sts", "eu-west-1", Some("")),
            "https://sts.eu-west-1.amazonaws.com/"
        );
    }

    /// The example request from AWS's `SigV4` documentation and test suite,
    /// IAM `ListUsers` signed at 2015-08-30T12:36:00Z.
    #[test]
    fn signs_the_documented_example() {
        let mut request = Request::new(
            "GET",
            "https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08",
        )
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        let credentials = Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: String::new(),
        };
        sign_v4(
            &mut request,
            &credentials,
            "us-east-1",
            "iam",
            1_440_938_160_000,
        )
        .unwrap();
        assert_eq!(request.header_value("x-amz-date"), Some("20150830T123600Z"));
        assert_eq!(
            request.header_value("authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date, \
                 Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
            )
        );
        assert_eq!(request.header_value("x-amz-security-token"), None);
    }

    #[test]
    fn sends_the_session_token() {
        let mut request = query("https://ec2.eu-west-1.amazonaws.com/", "X", "1", &[]);
        let credentials = Credentials {
            access_key_id: "ASIAEXAMPLE".to_owned(),
            secret_access_key: "secret".to_owned(),
            session_token: "session".to_owned(),
        };
        sign_v4(&mut request, &credentials, "eu-west-1", "ec2", 0).unwrap();
        assert_eq!(
            request.header_value("x-amz-security-token"),
            Some("session")
        );
        let authorization = request.header_value("authorization").unwrap();
        assert!(
            authorization.starts_with(
                "AWS4-HMAC-SHA256 Credential=ASIAEXAMPLE/19700101/eu-west-1/ec2/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, "
            ),
            "{authorization}"
        );
        assert!(!format!("{credentials:?}").contains("secret"));
    }

    #[test]
    fn reads_both_error_shapes() {
        let ec2 = Response {
            status: 400,
            body: b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Response><Errors><Error>\
                    <Code>InsufficientInstanceCapacity</Code><Message>none &amp; more</Message>\
                    </Error></Errors><RequestID>r</RequestID></Response>"
                .to_vec(),
        };
        assert_eq!(
            document(&ec2).unwrap_err(),
            Error {
                status: 400,
                code: "InsufficientInstanceCapacity".to_owned(),
                message: "none & more".to_owned(),
            }
        );
        let sts = Response {
            status: 403,
            body: br#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>Not authorized</Message></Error><RequestId>r</RequestId></ErrorResponse>"#.to_vec(),
        };
        assert_eq!(document(&sts).unwrap_err().code, "AccessDenied");
        let html = Response {
            status: 503,
            body: b"<html>busy".to_vec(),
        };
        assert_eq!(document(&html).unwrap_err().code, "HTTP503");
        let garbage = Response {
            status: 200,
            body: b"{}".to_vec(),
        };
        assert_eq!(document(&garbage).unwrap_err().code, "MalformedResponse");
    }
}
