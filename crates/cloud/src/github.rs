// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The GitHub App's REST calls: an installation token, and, with a
//! signed-in user's token, who the user is and what they can see.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::http::{Request, Response};
use crate::jwt;

/// The REST API.
pub const API: &str = "https://api.github.com";

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

/// A REST call with `token`, App JWT or installation or user token.
pub(crate) fn call(method: &'static str, url: String, token: &str) -> Request {
    Request::new(method, url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "hatchrun.io")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// The header with the requests an installation has left this hour.
pub const RATE_LIMIT_REMAINING: &str = "x-ratelimit-remaining";
/// Fewer requests left than this is low: the reconciler, launches and
/// the app share an installation's 5,000 an hour.
pub const RATE_LIMIT_LOW: u32 = 2000;

/// What one or more answers said about an installation's rate limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    /// The fewest requests left, when an answer said.
    pub remaining: Option<u32>,
    /// Whether GitHub refused a request for the rate limit.
    pub limited: bool,
}

impl RateLimit {
    /// What both say together: the fewer requests left, and limited if
    /// either was.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        let remaining = match (self.remaining, other.remaining) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Self {
            remaining,
            limited: self.limited || other.limited,
        }
    }

    /// Whether it is worth an alert.
    #[must_use]
    pub fn low(self) -> bool {
        self.limited || self.remaining.is_some_and(|left| left < RATE_LIMIT_LOW)
    }
}

/// The rate limit an answer shows, with its `x-ratelimit-remaining`
/// header. GitHub refuses with 429, or with 403 when nothing is left or
/// a secondary limit applies, which only its message tells apart from a
/// missing permission.
#[must_use]
pub fn rate_limit(response: &Response, remaining: Option<&str>) -> RateLimit {
    let remaining = remaining.and_then(|value| value.trim().parse().ok());
    let limited = response.status == 429
        || (response.status == 403
            && (remaining == Some(0)
                || response.text().to_ascii_lowercase().contains("rate limit")));
    RateLimit { remaining, limited }
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

/// An installation token.
#[derive(Deserialize)]
pub struct Token {
    /// The token.
    pub token: String,
    /// When it stops working, such as `2026-10-13T13:00:00Z`: an hour
    /// after it was made.
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Reads the token out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not a token.
pub fn token(response: &Response) -> Result<Token, String> {
    decode(response, "installation token")
}

/// The owner of an `owner/name` repository.
#[must_use]
pub fn owner(repository: &str) -> &str {
    repository.split('/').next().unwrap_or(repository)
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

/// The user's membership of one organization. With a user token from
/// the App this needs the App's organization permission Members (read)
/// and the App installed on that organization.
#[must_use]
pub fn membership(api: &str, token: &str, org: &str) -> Request {
    call("GET", format!("{api}/user/memberships/orgs/{org}"), token)
}

/// An organization.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Org {
    /// Numeric id, which outlives renames.
    pub id: u64,
    /// Handle.
    pub login: String,
    /// Avatar image URL.
    pub avatar_url: String,
}

/// A user's membership of an organization.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Membership {
    /// `admin` for owners, `member` otherwise.
    pub role: String,
    /// `active`, or `pending` until an invitation is accepted.
    pub state: String,
    /// The organization.
    pub organization: Org,
}

impl Membership {
    /// Whether the user owns the organization.
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.role == "admin" && self.state == "active"
    }
}

/// Reads one membership out of the answer. `None` when GitHub says the
/// user is not in the organization.
///
/// # Errors
///
/// GitHub's answer when it is neither a membership nor a 404.
pub fn parse_membership(response: &Response) -> Result<Option<Membership>, String> {
    if response.status == 404 {
        return Ok(None);
    }
    decode(response, "membership").map(Some)
}

/// One installation of the App, asked with the App's JWT.
#[must_use]
pub fn installation(api: &str, app_jwt: &str, installation_id: u64) -> Request {
    call(
        "GET",
        format!("{api}/app/installations/{installation_id}"),
        app_jwt,
    )
}

/// An installation of the App.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Installation {
    /// Installation id.
    pub id: u64,
    /// The account the App is installed on.
    pub account: Account,
    /// When an owner suspended it; GitHub refuses its tokens until
    /// they lift the suspension.
    #[serde(default)]
    pub suspended_at: Option<String>,
    /// What the App holds on this installation, by permission name
    /// (`contents`) to level (`read` or `write`).
    pub permissions: BTreeMap<String, String>,
}

/// The user or organization an installation belongs to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// Numeric id.
    pub id: u64,
    /// Handle.
    pub login: String,
    /// `Organization` or `User`.
    #[serde(rename = "type")]
    pub kind: String,
}

/// Reads the installation out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not an installation.
pub fn parse_installation(response: &Response) -> Result<Installation, String> {
    decode(response, "installation")
}

/// The installations of the App the signed-in user can see, asked with
/// their user token: those on accounts where they own or can reach a
/// repository the App covers. The first hundred.
#[must_use]
pub fn user_installations(api: &str, token: &str) -> Request {
    call(
        "GET",
        format!("{api}/user/installations?per_page=100"),
        token,
    )
}

#[derive(Deserialize)]
struct Installations {
    installations: Vec<Installation>,
}

/// Reads the user's installations out of the answer.
///
/// # Errors
///
/// GitHub's answer when it is not a list of installations.
pub fn parse_user_installations(response: &Response) -> Result<Vec<Installation>, String> {
    decode::<Installations>(response, "installations").map(|list| list.installations)
}

/// Where an organization owner installs the App. GitHub hands `state`
/// back to the App's setup URL.
#[must_use]
pub fn install_url(web: &str, app_slug: &str, state: &str) -> String {
    format!(
        "{web}/apps/{app_slug}/installations/new?state={}",
        crate::http::encode(state)
    )
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
            headers: Vec::new(),
            status: 201,
            body: br#"{"token":"ghs_x","expires_at":"2026-10-13T13:00:00Z"}"#.to_vec(),
        };
        let token = token(&answer).unwrap();
        assert_eq!(token.token, "ghs_x");
        assert_eq!(token.expires_at.as_deref(), Some("2026-10-13T13:00:00Z"));
    }

    #[test]
    fn reads_the_rate_limit() {
        let answer = |status: u16, body: &str| Response {
            headers: Vec::new(),
            status,
            body: body.as_bytes().to_vec(),
        };
        let ok = rate_limit(&answer(200, "{}"), Some("4999"));
        assert_eq!(ok.remaining, Some(4999));
        assert!(!ok.limited && !ok.low());
        assert!(rate_limit(&answer(200, "{}"), Some("1999")).low());
        assert!(rate_limit(&answer(429, "{}"), None).limited);
        assert!(rate_limit(&answer(403, "{}"), Some("0")).limited);
        let secondary = r#"{"message":"You have exceeded a secondary rate limit."}"#;
        assert!(rate_limit(&answer(403, secondary), Some("4000")).limited);
        // A missing permission is no rate limit.
        let denied = r#"{"message":"Resource not accessible by integration"}"#;
        assert!(!rate_limit(&answer(403, denied), Some("4000")).low());
        assert_eq!(rate_limit(&answer(200, "{}"), Some("x")).remaining, None);

        let merged = ok.merge(RateLimit {
            remaining: Some(1500),
            limited: false,
        });
        assert_eq!(merged.remaining, Some(1500));
        assert!(merged.low());
        let limited = RateLimit {
            remaining: None,
            limited: true,
        };
        assert_eq!(
            ok.merge(limited),
            RateLimit {
                remaining: Some(4999),
                limited: true
            }
        );
    }

    #[test]
    fn names_the_owner() {
        assert_eq!(owner("acme/app"), "acme");
    }

    #[test]
    fn reads_the_signed_in_user() {
        assert_eq!(user(API, "ghu_x").url, "https://api.github.com/user");
        let answer = Response {
            headers: Vec::new(),
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

    #[test]
    fn reads_a_membership() {
        assert_eq!(
            membership(API, "t", "acme").url,
            "https://api.github.com/user/memberships/orgs/acme"
        );
        let owner = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"url":"u","state":"active","role":"admin","organization":{"login":"acme","id":9,"avatar_url":"https://a/9"}}"#.to_vec(),
        };
        assert!(parse_membership(&owner).unwrap().unwrap().is_admin());
        let pending = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"state":"pending","role":"admin","organization":{"login":"acme","id":9,"avatar_url":"https://a/9"}}"#.to_vec(),
        };
        assert!(!parse_membership(&pending).unwrap().unwrap().is_admin());
        let gone = Response {
            headers: Vec::new(),
            status: 404,
            body: Vec::new(),
        };
        assert_eq!(parse_membership(&gone).unwrap(), None);
    }

    #[test]
    fn reads_installations() {
        assert_eq!(
            installation(API, "jwt", 5).url,
            "https://api.github.com/app/installations/5"
        );
        let answer = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"id":5,"account":{"login":"acme","id":9,"type":"Organization"},"app_slug":"hatchrun","permissions":{"contents":"write","metadata":"read"}}"#.to_vec(),
        };
        assert_eq!(
            parse_installation(&answer).unwrap(),
            Installation {
                id: 5,
                account: Account {
                    id: 9,
                    login: "acme".to_owned(),
                    kind: "Organization".to_owned(),
                },
                suspended_at: None,
                permissions: BTreeMap::from([
                    ("contents".to_owned(), "write".to_owned()),
                    ("metadata".to_owned(), "read".to_owned()),
                ]),
            }
        );
        assert_eq!(
            user_installations(API, "ghu_x").url,
            "https://api.github.com/user/installations?per_page=100"
        );
        let mine = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"total_count":2,"installations":[{"id":5,"account":{"login":"acme","id":9,"type":"Organization"},"suspended_at":null,"permissions":{}},{"id":6,"account":{"login":"octo","id":1,"type":"User"},"suspended_at":"2026-10-01T00:00:00Z","permissions":{}}]}"#.to_vec(),
        };
        let list = parse_user_installations(&mine).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[1].suspended_at.as_deref(),
            Some("2026-10-01T00:00:00Z")
        );
        assert_eq!(
            install_url("https://github.com", "hatchrun", "t.s/x"),
            "https://github.com/apps/hatchrun/installations/new?state=t.s%2Fx"
        );
    }
}
