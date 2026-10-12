// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Calls to the GitHub REST API that lead to an [`Actions`] connection.

use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{HeaderValue, Method, Request, StatusCode};
use serde::Deserialize;

use crate::actions::bearer;
use crate::{Actions, ApiError, ConfigUrl, Error, Operation, Scope, Secret, decode, trim_bom};

const API_VERSION: &str = "2022-11-28";

/// A token with its expiry, as the REST API returns it.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct Token {
    /// The token.
    pub token: Secret,
    /// When it expires, in RFC 3339.
    pub expires_at: String,
}

/// Exchanges a GitHub App JWT for an installation token. The caller signs
/// the JWT (RS256, `iss` set to the App's client id, `iat` a minute in the
/// past, `exp` at most ten minutes after it).
///
/// # Errors
///
/// [`Error::Request`] when the request cannot be built.
pub fn installation_token(
    config: &ConfigUrl,
    installation_id: u64,
    app_jwt: &Secret,
) -> Result<Operation<Token>, Error> {
    let path = format!("/app/installations/{installation_id}/access_tokens");
    let request = request(config, &path, bearer(app_jwt.expose())?, Vec::new())?;
    Ok(Operation::new(request, created_token))
}

/// Gets a runner registration token for the config URL. `api_token` is an
/// installation token or a personal access token that can administer
/// runners there.
///
/// # Errors
///
/// [`Error::Request`] when the request cannot be built.
pub fn registration_token(
    config: &ConfigUrl,
    api_token: &Secret,
) -> Result<Operation<Token>, Error> {
    let path = match config.scope() {
        Scope::Enterprise(enterprise) => {
            format!("/enterprises/{enterprise}/actions/runners/registration-token")
        }
        Scope::Organization(organization) => {
            format!("/orgs/{organization}/actions/runners/registration-token")
        }
        Scope::Repository { owner, repository } => {
            format!("/repos/{owner}/{repository}/actions/runners/registration-token")
        }
    };
    let request = request(config, &path, bearer(api_token.expose())?, Vec::new())?;
    Ok(Operation::new(request, created_token))
}

/// Exchanges a registration token for an Actions service connection.
///
/// A registration token can take a few seconds to become valid here, so
/// callers should retry [`crate::ErrorKind::Unauthorized`] and 403 errors
/// with a backoff.
///
/// # Errors
///
/// [`Error::Request`] when the request cannot be built.
pub fn admin_connection(
    config: &ConfigUrl,
    registration_token: &Secret,
) -> Result<Operation<Actions>, Error> {
    let mut authorization =
        HeaderValue::try_from(format!("RemoteAuth {}", registration_token.expose()))
            .map_err(|err| Error::Request(err.into()))?;
    authorization.set_sensitive(true);
    let body = serde_json::json!({ "url": config.as_str(), "runner_event": "register" });
    let request = request(
        config,
        "/actions/runner-registration",
        authorization,
        body.to_string().into_bytes(),
    )?;
    Ok(Operation::new(request, |response| {
        #[derive(Deserialize)]
        struct Connection {
            #[serde(default)]
            url: String,
            #[serde(default)]
            token: String,
        }
        if !response.status().is_success() {
            return Err(ApiError::from_response(response).into());
        }
        let connection: Connection = decode(trim_bom(response.body()), "admin connection")?;
        if connection.url.is_empty() || connection.token.is_empty() {
            return Err(Error::AdminToken("connection has no url or token"));
        }
        Actions::new(&connection.url, Secret::new(connection.token))
    }))
}

fn request(
    config: &ConfigUrl,
    path: &str,
    authorization: HeaderValue,
    body: Vec<u8>,
) -> Result<Request<Vec<u8>>, Error> {
    Ok(Request::builder()
        .method(Method::POST)
        .uri(config.api_url(path)?.as_str())
        .header(ACCEPT, "application/vnd.github+json")
        .header(CONTENT_TYPE, "application/json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .header(AUTHORIZATION, authorization)
        .header(USER_AGENT, crate::USER_AGENT)
        .body(body)?)
}

fn created_token(response: &http::Response<Vec<u8>>) -> Result<Token, Error> {
    decode(crate::expect(response, StatusCode::CREATED)?, "token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::actions::tests::ADMIN_TOKEN;
    use crate::tests::response;

    fn config(url: &str) -> ConfigUrl {
        ConfigUrl::parse(url).unwrap()
    }

    #[test]
    fn registration_token_paths() {
        let token = Secret::new("ghs_x");
        let path = |url| {
            registration_token(&config(url), &token)
                .unwrap()
                .request
                .uri()
                .to_string()
        };
        assert_eq!(
            path("https://github.com/hatchrun"),
            "https://api.github.com/orgs/hatchrun/actions/runners/registration-token"
        );
        assert_eq!(
            path("https://github.com/hatchrun/hatchrun"),
            "https://api.github.com/repos/hatchrun/hatchrun/actions/runners/registration-token"
        );
        assert_eq!(
            path("https://ghes.example.com/enterprises/acme"),
            "https://ghes.example.com/api/v3/enterprises/acme/actions/runners/registration-token"
        );
    }

    #[test]
    fn registration_token_response() {
        let op =
            registration_token(&config("https://github.com/hatchrun"), &Secret::new("p")).unwrap();
        assert_eq!(op.request.headers()[AUTHORIZATION], "Bearer p");
        let token = op
            .parser
            .parse(&response(
                201,
                r#"{"token":"AABB","expires_at":"2026-10-12T12:00:00Z"}"#,
            ))
            .unwrap();
        assert_eq!(token.token.expose(), "AABB");
        assert_eq!(
            op.parser.parse(&response(403, "")).unwrap_err().kind(),
            Some(ErrorKind::Other)
        );
    }

    #[test]
    fn installation_token_request() {
        let op = installation_token(
            &config("https://github.com/hatchrun"),
            77,
            &Secret::new("jwt"),
        )
        .unwrap();
        assert_eq!(
            op.request.uri().to_string(),
            "https://api.github.com/app/installations/77/access_tokens"
        );
        assert_eq!(op.request.headers()[AUTHORIZATION], "Bearer jwt");
    }

    #[test]
    fn admin_connection_flow() {
        let config = config("https://github.com/hatchrun");
        let op = admin_connection(&config, &Secret::new("AABB")).unwrap();
        assert_eq!(
            op.request.uri().to_string(),
            "https://api.github.com/actions/runner-registration"
        );
        assert_eq!(op.request.headers()[AUTHORIZATION], "RemoteAuth AABB");
        assert!(op.request.headers()[AUTHORIZATION].is_sensitive());
        let body: serde_json::Value = serde_json::from_slice(op.request.body()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"url": "https://github.com/hatchrun", "runner_event": "register"})
        );

        let actions = op
            .parser
            .parse(&response(
                200,
                &format!(r#"{{"url":"https://pipelines.example/t/","token":"{ADMIN_TOKEN}"}}"#),
            ))
            .unwrap();
        assert_eq!(actions.expires_at(), 1_900_000_000);

        assert!(matches!(
            op.parser.parse(&response(200, r#"{"url":"","token":""}"#)),
            Err(Error::AdminToken(_))
        ));
        assert_eq!(
            op.parser.parse(&response(401, "")).unwrap_err().kind(),
            Some(ErrorKind::Unauthorized)
        );
    }
}
