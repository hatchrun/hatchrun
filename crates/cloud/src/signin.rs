// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Sign-in with GitHub: the hatchrun App's user authorization, GitHub's
//! OAuth web flow with a `state` and PKCE.
//!
//! `/api/auth/login` sends the browser to GitHub with a random state and
//! the S256 challenge of a random verifier, and keeps both in a short
//! cookie ([`Pending`]). GitHub sends the browser back to
//! `/api/auth/callback` with a code and the state; the Worker checks the
//! state against the cookie and trades the code, the verifier and the
//! App's client secret for a user token.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::http::{Request, Response, form};
use crate::jwt::b64url;

/// GitHub's web host, where users authorize the App.
pub const WEB: &str = "https://github.com";
/// Path GitHub sends users back to.
pub const CALLBACK_PATH: &str = "/api/auth/callback";
/// Seconds the user has to finish signing in on GitHub.
pub const PENDING_MAX_AGE: u64 = 10 * 60;

/// The S256 PKCE challenge of `verifier`.
#[must_use]
pub fn challenge(verifier: &str) -> String {
    b64url(Sha256::digest(verifier.as_bytes()))
}

/// Where to send the browser to sign in.
#[must_use]
pub fn authorize_url(
    web: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
) -> String {
    format!(
        "{web}/login/oauth/authorize?{}",
        form(&[
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("code_challenge", &challenge(verifier)),
            ("code_challenge_method", "S256"),
        ])
    )
}

/// A sign-in on its way through GitHub, as its cookie holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The `state` GitHub must hand back.
    pub state: String,
    /// The PKCE verifier.
    pub verifier: String,
    /// The app path to land on afterwards.
    pub return_to: String,
}

impl Pending {
    /// The cookie value: the three parts, base64url, joined by dots.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}.{}.{}",
            self.state,
            self.verifier,
            b64url(&self.return_to)
        )
    }

    /// Reads a cookie value [`Pending::encode`] wrote.
    #[must_use]
    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        let state = parts.next().filter(|part| !part.is_empty())?;
        let verifier = parts.next().filter(|part| !part.is_empty())?;
        let return_to = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            state: state.to_owned(),
            verifier: verifier.to_owned(),
            return_to: return_to_path(&String::from_utf8(return_to).ok()?),
        })
    }
}

/// `raw` if it is a path on this site, `/` otherwise, so sign-in cannot
/// be used to send someone elsewhere. `//host` and `/\host` are other
/// sites to a browser.
#[must_use]
pub fn return_to_path(raw: &str) -> String {
    let local = raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.starts_with("/\\")
        && !raw.chars().any(char::is_control);
    if local {
        raw.to_owned()
    } else {
        "/".to_owned()
    }
}

/// Trades the code GitHub handed back for a user token.
#[must_use]
pub fn exchange(
    web: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Request {
    Request::new("POST", format!("{web}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "hatchrun.io")
        .body(form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ]))
}

/// A user token.
#[derive(Deserialize)]
pub struct UserToken {
    /// `ghu_...`
    pub access_token: String,
    /// Seconds it lives, when the App's tokens expire.
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for UserToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserToken")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenAnswer {
    Token(UserToken),
    Error {
        error: String,
        error_description: Option<String>,
    },
}

/// Reads the token out of GitHub's answer. GitHub answers 200 with an
/// `error` field when the code is bad or expired.
///
/// # Errors
///
/// GitHub's error.
pub fn user_token(response: &Response) -> Result<UserToken, String> {
    if !response.ok() {
        return Err(format!("GitHub answered {}", response.status));
    }
    match serde_json::from_slice(&response.body) {
        Ok(TokenAnswer::Token(token)) => Ok(token),
        Ok(TokenAnswer::Error {
            error,
            error_description,
        }) => Err(match error_description {
            Some(description) => format!("{error}: {description}"),
            None => error,
        }),
        Err(err) => Err(format!("token answer: {err}")),
    }
}

/// The URL-encoded `redirect_uri` the App must list as a callback URL.
#[must_use]
pub fn redirect_uri(app_url: &str) -> String {
    format!("{}{CALLBACK_PATH}", app_url.trim_end_matches('/'))
}

/// A query parameter of a URL's query string, decoded.
#[must_use]
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| decode(value))
    })
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let hex = |byte: u8| char::from(byte).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                (Some(high), Some(low)) => {
                    out.push(u8::try_from(high * 16 + low).unwrap_or(b'?'));
                    at += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn sends_users_to_github_with_state_and_challenge() {
        let url = authorize_url(
            WEB,
            "Iv23li",
            "https://app.hatchrun.io/api/auth/callback",
            "st",
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        );
        assert_eq!(
            url,
            "https://github.com/login/oauth/authorize?client_id=Iv23li\
             &redirect_uri=https%3A%2F%2Fapp.hatchrun.io%2Fapi%2Fauth%2Fcallback\
             &state=st&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &code_challenge_method=S256"
        );
    }

    #[test]
    fn pending_round_trips() {
        let pending = Pending {
            state: "s".to_owned(),
            verifier: "v".to_owned(),
            return_to: "/orgs/new?x=1".to_owned(),
        };
        assert_eq!(Pending::decode(&pending.encode()).unwrap(), pending);
        assert_eq!(Pending::decode("s.v"), None);
        assert_eq!(Pending::decode("s.v.Lw.extra"), None);
        assert_eq!(Pending::decode(".v.Lw"), None);
    }

    #[test]
    fn returns_only_to_this_site() {
        assert_eq!(return_to_path("/dashboard?a=b"), "/dashboard?a=b");
        assert_eq!(return_to_path("https://evil.example/"), "/");
        assert_eq!(return_to_path("//evil.example/"), "/");
        assert_eq!(return_to_path("/\\evil.example/"), "/");
        assert_eq!(return_to_path("/a\nb"), "/");
        assert_eq!(return_to_path(""), "/");
    }

    #[test]
    fn trades_the_code_with_the_verifier() {
        let request = exchange(WEB, "id", "secret", "c0de", "https://a/cb", "ver");
        assert_eq!(request.url, "https://github.com/login/oauth/access_token");
        assert_eq!(request.header_value("accept"), Some("application/json"));
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "client_id=id&client_secret=secret&code=c0de\
             &redirect_uri=https%3A%2F%2Fa%2Fcb&code_verifier=ver"
        );
    }

    #[test]
    fn reads_tokens_and_errors() {
        let ok = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"access_token":"ghu_x","expires_in":28800,"refresh_token":"ghr_y","token_type":"bearer","scope":""}"#.to_vec(),
        };
        let token = user_token(&ok).unwrap();
        assert_eq!(token.access_token, "ghu_x");
        assert_eq!(token.expires_in, Some(28800));
        assert!(!format!("{token:?}").contains("ghu_x"));
        let bad = Response { headers: Vec::new(),
            status: 200,
            body: br#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#.to_vec(),
        };
        assert_eq!(
            user_token(&bad).unwrap_err(),
            "bad_verification_code: The code passed is incorrect or expired."
        );
        let down = Response {
            headers: Vec::new(),
            status: 502,
            body: Vec::new(),
        };
        assert_eq!(user_token(&down).unwrap_err(), "GitHub answered 502");
    }

    #[test]
    fn reads_query_parameters() {
        let query = "code=a%2Fb&state=x+y&empty=&bare&bad=%zz&tail=%4";
        assert_eq!(query_param(query, "code").as_deref(), Some("a/b"));
        assert_eq!(query_param(query, "state").as_deref(), Some("x y"));
        assert_eq!(query_param(query, "empty").as_deref(), Some(""));
        assert_eq!(query_param(query, "bare").as_deref(), Some(""));
        assert_eq!(query_param(query, "bad").as_deref(), Some("%zz"));
        assert_eq!(query_param(query, "tail").as_deref(), Some("%4"));
        assert_eq!(query_param(query, "missing"), None);
        assert_eq!(
            redirect_uri("https://app.hatchrun.io/"),
            "https://app.hatchrun.io/api/auth/callback"
        );
    }
}
