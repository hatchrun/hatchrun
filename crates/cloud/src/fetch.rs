// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Sends [`crate::http::Request`]s with the runtime's `fetch`.

use worker::{Fetch, Headers, Method, RequestInit};

use crate::http::{Request, Response};

/// Sends a request with the runtime's `fetch`.
pub async fn send(request: &Request) -> std::result::Result<Response, String> {
    send_reading(request, None)
        .await
        .map(|(response, _)| response)
}

/// Sends a request and returns the answer with the value of its
/// `header`, such as GitHub's `Link`.
pub async fn send_reading(
    request: &Request,
    header: Option<&str>,
) -> std::result::Result<(Response, Option<String>), String> {
    let sent = async {
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers.set(name, value)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::from(request.method.to_owned()))
            .with_headers(headers);
        if !request.body.is_empty() {
            init.with_body(Some(
                js_sys::Uint8Array::from(request.body.as_slice()).into(),
            ));
        }
        let mut response = Fetch::Request(worker::Request::new_with_init(&request.url, &init)?)
            .send()
            .await?;
        let value = match header {
            Some(name) => response.headers().get(name)?,
            None => None,
        };
        Ok::<_, worker::Error>((
            Response {
                status: response.status_code(),
                body: response.bytes().await?,
            },
            value,
        ))
    };
    sent.await
        .map_err(|err| format!("{} {}: {err}", request.method, host(&request.url)))
}

/// The scheme and host of a URL, for messages that must not carry a
/// path with a ticket in it.
fn host(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |at| at + 3);
    url[after_scheme..]
        .find('/')
        .map_or(url, |at| &url[..after_scheme + at])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_drop_the_path() {
        assert_eq!(
            host("https://broker.example/v1/tickets/secret"),
            "https://broker.example"
        );
        assert_eq!(host("http://127.0.0.1:8788"), "http://127.0.0.1:8788");
    }
}
