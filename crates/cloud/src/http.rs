// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Outgoing requests as plain data. The AWS and GitHub modules build
//! them and parse the answers; only the Worker sends them, so the
//! building and parsing test natively.

/// A request to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET`, `POST` or `DELETE`.
    pub method: &'static str,
    /// Full URL.
    pub url: String,
    /// Headers, in the order they are sent.
    pub headers: Vec<(String, String)>,
    /// Body, empty for none.
    pub body: Vec<u8>,
}

impl Request {
    /// A request with no headers and no body.
    #[must_use]
    pub fn new(method: &'static str, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// Sets the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The value of a header, by case-insensitive name.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// An answer: status, headers and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status.
    pub status: u16,
    /// Headers, names lowercased.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl Response {
    /// Whether the status is 2xx.
    #[must_use]
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The value of a header, by case-insensitive name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body as text, for error messages.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Percent-encodes `value` the way AWS signs it: everything but
/// `A-Z a-z 0-9 - _ . ~`.
#[must_use]
pub fn encode(value: &str) -> String {
    use std::fmt::Write;
    value.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

/// An `application/x-www-form-urlencoded` body.
#[must_use]
pub fn form<K: AsRef<str>, V: AsRef<str>>(params: &[(K, V)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key.as_ref()), encode(value.as_ref())))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_aws() {
        assert_eq!(encode("a-Z_0.~"), "a-Z_0.~");
        assert_eq!(encode("a b/c+d=e&é"), "a%20b%2Fc%2Bd%3De%26%C3%A9");
        assert_eq!(
            form(&[("Action", "RunInstances"), ("Tag.1.Value", "a b")]),
            "Action=RunInstances&Tag.1.Value=a%20b"
        );
    }

    #[test]
    fn header_lookup_ignores_case() {
        let response = Response {
            status: 429,
            headers: vec![("retry-after".to_owned(), "30".to_owned())],
            body: Vec::new(),
        };
        assert_eq!(response.header("Retry-After"), Some("30"));
        assert_eq!(response.header("etag"), None);
    }

    #[test]
    fn finds_headers_by_any_case() {
        let request = Request::new("GET", "https://x").header("Content-Type", "text/plain");
        assert_eq!(request.header_value("content-type"), Some("text/plain"));
        assert_eq!(request.header_value("accept"), None);
    }
}
