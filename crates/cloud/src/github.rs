// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The GitHub App's REST calls: an installation token, then a
//! just-in-time runner for a job, and removing it again when its
//! instance does not start; and, with a signed-in user's token, who the
//! user is.

use serde::Deserialize;

use crate::http::{Request, Response};
use crate::jwt;

/// The REST API.
pub const API: &str = "https://api.github.com";
/// Runner group every JIT runner joins: the organization's default.
const RUNNER_GROUP: u64 = 1;
/// The runner's work directory, relative to its install directory.
const WORK_FOLDER: &str = "_work";

/// The claims of the App's JWT, which it trades for an installation
/// token. Backdated a minute for clock drift and valid for nine, under
/// GitHub's ten minute limit. `app` is the App's client id or numeric id.
#[must_use]
pub fn app_jwt_signing_input(app: &str, now: u64) -> String {
    let issued_at = now.saturating_sub(60);
    jwt::signing_input(
        None,
        &serde_json::json!({ "iat": issued_at, "exp": issued_at + 9 * 60, "iss": app }),
    )
}

fn call(method: &'static str, url: String, token: &str) -> Request {
    Request::new(method, url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "hatchrun.io")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// Trades the App's JWT for a token of one installation.
#[must_use]
pub fn installation_token(api: &str, installation_id: u64, app_jwt: &str) -> Request {
    call(
        "POST",
        format!("{api}/app/installations/{installation_id}/access_tokens"),
        app_jwt,
    )
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

/// Reads the token out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not a token.
pub fn token(response: &Response) -> Result<String, String> {
    decode::<Token>(response, "installation token").map(|token| token.token)
}

/// The owner of an `owner/name` repository.
#[must_use]
pub fn owner(repository: &str) -> &str {
    repository.split('/').next().unwrap_or(repository)
}

/// Registers a just-in-time runner in `org` that takes jobs with exactly
/// `label` (see [`crate::labels`] on why one label).
#[must_use]
pub fn generate_jit_config(api: &str, org: &str, token: &str, name: &str, label: &str) -> Request {
    call(
        "POST",
        format!("{api}/orgs/{org}/actions/runners/generate-jitconfig"),
        token,
    )
    .header("Content-Type", "application/json")
    .body(
        serde_json::json!({
            "name": name,
            "runner_group_id": RUNNER_GROUP,
            "labels": [label],
            "work_folder": WORK_FOLDER,
        })
        .to_string(),
    )
}

/// A registered JIT runner.
#[derive(Deserialize)]
pub struct JitConfig {
    /// The runner.
    pub runner: Runner,
    /// What the agent hands to the runner. Anyone holding it can run
    /// jobs as this runner, so it travels only through the broker.
    pub encoded_jit_config: String,
}

impl std::fmt::Debug for JitConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitConfig")
            .field("runner", &self.runner)
            .finish_non_exhaustive()
    }
}

/// A runner GitHub knows.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Runner {
    /// Runner id.
    pub id: u64,
}

/// Reads the JIT config out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not a config.
pub fn jit_config(response: &Response) -> Result<JitConfig, String> {
    decode(response, "JIT config")
}

/// Removes a runner that never got an instance.
#[must_use]
pub fn delete_runner(api: &str, org: &str, token: &str, runner_id: u64) -> Request {
    call(
        "DELETE",
        format!("{api}/orgs/{org}/actions/runners/{runner_id}"),
        token,
    )
}

/// The signed-in user, asked with their user token.
#[must_use]
pub fn user(api: &str, token: &str) -> Request {
    call("GET", format!("{api}/user"), token)
}

/// A GitHub user.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct User {
    /// Numeric id, which outlives renames.
    pub id: u64,
    /// Handle.
    pub login: String,
    /// Display name, when set.
    pub name: Option<String>,
    /// Avatar image URL.
    pub avatar_url: String,
}

/// Reads the user out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not a user.
pub fn parse_user(response: &Response) -> Result<User, String> {
    decode(response, "user")
}

fn decode<T: for<'de> Deserialize<'de>>(response: &Response, what: &str) -> Result<T, String> {
    if !response.ok() {
        return Err(format!(
            "{what}: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|err| format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    #[test]
    fn app_jwt_is_backdated_and_short() {
        let input = app_jwt_signing_input("Iv23li", 1_000_000);
        let claims = input.split('.').nth(1).unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({ "iat": 999_940, "exp": 1_000_480, "iss": "Iv23li" })
        );
    }

    #[test]
    fn asks_for_an_installation_token() {
        let request = installation_token(API, 42, "app.jwt.sig");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://api.github.com/app/installations/42/access_tokens"
        );
        assert_eq!(
            request.header_value("authorization"),
            Some("Bearer app.jwt.sig")
        );
        assert!(request.header_value("user-agent").is_some());
        let answer = Response {
            status: 201,
            body: br#"{"token":"ghs_x","expires_at":"2026-10-13T13:00:00Z"}"#.to_vec(),
        };
        assert_eq!(token(&answer).unwrap(), "ghs_x");
    }

    #[test]
    fn registers_a_runner_with_the_job_label_only() {
        let request =
            generate_jit_config(API, "acme", "ghs_x", "hatchrun-7-ab12", "hatchrun/arm64");
        assert_eq!(
            request.url,
            "https://api.github.com/orgs/acme/actions/runners/generate-jitconfig"
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "name": "hatchrun-7-ab12",
                "runner_group_id": 1,
                "labels": ["hatchrun/arm64"],
                "work_folder": "_work",
            })
        );
        let answer = Response {
            status: 201,
            body: br#"{"runner":{"id":23,"name":"hatchrun-7-ab12","os":"unknown","status":"offline","busy":false,"labels":[]},"encoded_jit_config":"abc"}"#.to_vec(),
        };
        let config = jit_config(&answer).unwrap();
        assert_eq!(config.runner, Runner { id: 23 });
        assert_eq!(config.encoded_jit_config, "abc");
        assert!(!format!("{config:?}").contains("abc"));
    }

    #[test]
    fn says_what_github_answered() {
        let answer = Response {
            status: 403,
            body: br#"{"message":"Resource not accessible by integration"}"#.to_vec(),
        };
        assert_eq!(
            jit_config(&answer).unwrap_err(),
            "JIT config: GitHub answered 403: {\"message\":\"Resource not accessible by integration\"}"
        );
        assert_eq!(owner("acme/app"), "acme");
        assert_eq!(
            delete_runner(API, "acme", "t", 23).url,
            "https://api.github.com/orgs/acme/actions/runners/23"
        );
    }

    #[test]
    fn reads_the_signed_in_user() {
        assert_eq!(user(API, "ghu_x").url, "https://api.github.com/user");
        let answer = Response {
            status: 200,
            body:
                br#"{"login":"octo","id":1,"avatar_url":"https://a/1","name":null,"type":"User"}"#
                    .to_vec(),
        };
        assert_eq!(
            parse_user(&answer).unwrap(),
            User {
                id: 1,
                login: "octo".to_owned(),
                name: None,
                avatar_url: "https://a/1".to_owned(),
            }
        );
    }
}
