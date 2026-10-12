// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Client for the GitHub Actions runner scale set API.
//!
//! The crate does no I/O. Every API call is an [`Operation`]: the HTTP
//! request to send and a [`Parser`] for the response. Callers send the
//! request with whatever HTTP client their runtime has, so the same code
//! runs on tokio, in a Cloudflare Worker and in tests. [`call`] drives an
//! operation through a [`Transport`] for callers that implement one.
//!
//! Talking to a scale set takes three tokens:
//!
//! 1. A GitHub API token: a personal access token, or an installation
//!    token from [`github::installation_token`] for a GitHub App. Signing
//!    the App JWT is left to the caller.
//! 2. A runner registration token from [`github::registration_token`].
//! 3. An [`Actions`] connection from [`github::admin_connection`]. Get a
//!    new one when [`Actions::expires_soon`] says so.
//!
//! With the connection, find or create the scale set, open a [`Session`]
//! and long-poll [`Session::get_message`]. A message queue call that fails
//! with [`ErrorKind::Unauthorized`] means the session token expired:
//! refresh it with [`Actions::refresh_session`] and retry.

mod actions;
mod config;
mod error;
pub mod github;
mod secret;
mod session;
mod types;

use std::fmt;
use std::future::Future;

use http::{Method, Request, Response, StatusCode, Uri};
use serde::de::DeserializeOwned;

pub use actions::Actions;
pub use config::{ConfigUrl, Scope};
pub use error::{ApiError, Error, ErrorKind};
pub use secret::Secret;
pub use session::Session;
pub use types::{
    JitConfig, JobMessage, JobMessageKind, Label, Message, NewScaleSet, Runner, RunnerGroup,
    RunnerSetting, ScaleSet, Statistics,
};

/// The `User-Agent` sent with every request.
pub const USER_AGENT: &str = concat!("hatchrun-scaleset/", env!("CARGO_PKG_VERSION"));

/// One API call: the request to send and the parser for its response.
pub struct Operation<T> {
    /// The request. Callers may add headers, such as a proxy's, before
    /// sending it.
    pub request: Request<Vec<u8>>,
    /// Turns the response into the call's result.
    pub parser: Parser<T>,
}

impl<T> Operation<T> {
    fn new(request: Request<Vec<u8>>, parse: fn(&Response<Vec<u8>>) -> Result<T, Error>) -> Self {
        let parser = Parser {
            method: request.method().clone(),
            uri: request.uri().clone(),
            parse,
        };
        Self { request, parser }
    }
}

/// Parses the response to an [`Operation`]'s request.
pub struct Parser<T> {
    method: Method,
    uri: Uri,
    parse: fn(&Response<Vec<u8>>) -> Result<T, Error>,
}

impl<T> Parser<T> {
    /// Parses the response.
    ///
    /// # Errors
    ///
    /// [`Error::Api`] for a status the call does not expect, and
    /// [`Error::Decode`] for a body that does not match the API.
    pub fn parse(&self, response: &Response<Vec<u8>>) -> Result<T, Error> {
        (self.parse)(response).map_err(|err| match err {
            Error::Api(mut api) => {
                api.method = Some(self.method.clone());
                self.uri.path().clone_into(&mut api.path);
                Error::Api(api)
            }
            err => err,
        })
    }
}

impl<T> fmt::Debug for Parser<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parser")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .finish_non_exhaustive()
    }
}

/// Sends HTTP requests. Implement it on top of the runtime's HTTP client to
/// use [`call`].
pub trait Transport {
    /// The error for a request that got no response.
    type Error;

    /// Sends the request and reads the whole response body.
    fn send(
        &self,
        request: Request<Vec<u8>>,
    ) -> impl Future<Output = Result<Response<Vec<u8>>, Self::Error>>;
}

/// The error from [`call`].
#[derive(Debug)]
pub enum CallError<E> {
    /// The request got no response.
    Transport(E),
    /// The response was an error or could not be parsed.
    Api(Error),
}

impl<E: fmt::Display> fmt::Display for CallError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "send request: {err}"),
            Self::Api(err) => err.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for CallError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(err) => Some(err),
            Self::Api(err) => err.source(),
        }
    }
}

/// Sends the operation's request and parses the response.
///
/// # Errors
///
/// [`CallError::Transport`] when the request fails to send, otherwise the
/// error from [`Parser::parse`].
pub async fn call<Tr: Transport, T>(
    transport: &Tr,
    operation: Operation<T>,
) -> Result<T, CallError<Tr::Error>> {
    let response = transport
        .send(operation.request)
        .await
        .map_err(CallError::Transport)?;
    operation.parser.parse(&response).map_err(CallError::Api)
}

/// Returns the body when the response has the expected status.
fn expect(response: &Response<Vec<u8>>, status: StatusCode) -> Result<&[u8], Error> {
    if response.status() == status {
        Ok(trim_bom(response.body()))
    } else {
        Err(ApiError::from_response(response).into())
    }
}

fn decode<T: DeserializeOwned>(body: &[u8], what: &'static str) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|source| Error::Decode { what, source })
}

/// The Actions service starts some bodies with a UTF-8 byte order mark.
fn trim_bom(body: &[u8]) -> &[u8] {
    body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Builds a response for parser tests.
    pub(crate) fn response(status: u16, body: &str) -> Response<Vec<u8>> {
        Response::builder()
            .status(status)
            .body(body.as_bytes().to_vec())
            .unwrap()
    }

    struct FakeTransport {
        response: RefCell<Option<Response<Vec<u8>>>>,
        sent: RefCell<Vec<Request<Vec<u8>>>>,
    }

    impl Transport for FakeTransport {
        type Error = std::io::Error;

        fn send(
            &self,
            request: Request<Vec<u8>>,
        ) -> impl Future<Output = Result<Response<Vec<u8>>, Self::Error>> {
            self.sent.borrow_mut().push(request);
            std::future::ready(
                self.response
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| std::io::Error::other("connection refused")),
            )
        }
    }

    fn operation() -> Operation<String> {
        let request = Request::post("https://example.com/things?x=1")
            .body(Vec::new())
            .unwrap();
        Operation::new(request, |response| {
            let body = expect(response, StatusCode::CREATED)?;
            decode(body, "thing")
        })
    }

    #[tokio::test]
    async fn call_sends_and_parses() {
        let transport = FakeTransport {
            response: RefCell::new(Some(response(201, "\u{feff}\"made\""))),
            sent: RefCell::default(),
        };
        assert_eq!(call(&transport, operation()).await.unwrap(), "made");
        assert_eq!(transport.sent.borrow()[0].uri().path(), "/things");
    }

    #[tokio::test]
    async fn call_reports_transport_errors() {
        let transport = FakeTransport {
            response: RefCell::default(),
            sent: RefCell::default(),
        };
        let err = call(&transport, operation()).await.unwrap_err();
        assert!(matches!(err, CallError::Transport(_)), "{err}");
    }

    #[test]
    fn parse_errors_name_the_request() {
        let err = operation()
            .parser
            .parse(&response(404, r#"{"typeName":"X","message":"gone"}"#))
            .unwrap_err();
        let Error::Api(api) = &err else {
            panic!("{err}")
        };
        assert_eq!(api.kind, ErrorKind::NotFound);
        assert_eq!(api.method, Some(Method::POST));
        assert_eq!(api.path, "/things");
        assert_eq!(err.to_string(), "POST /things: 404 Not Found: X: gone");
    }
}
