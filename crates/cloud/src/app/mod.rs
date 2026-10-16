// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The API of app.hatchrun.io, under `/api`. The app itself is static
//! assets from `web/app`, served by the same Worker, so the session
//! cookie stays on one origin.
//!
//! - `GET /api/auth/login` starts sign-in with GitHub (see
//!   [`crate::signin`]); `GET /api/auth/callback` finishes it.
//! - `POST /api/auth/logout` ends the session.
//! - `GET /api/session` returns the signed-in user and the CSRF token
//!   the app sends back on every request that changes something.
//! - `/api/installations`, `/api/github/setup` and `/api/tenants` set
//!   up an organization from an installation of the App (see
//!   [`tenants`]).
//! - `/api/tenants/<org>/aws` connects the AWS account (see [`aws`]).
//! - `/api/tenants/<org>/{jobs,settings}` serve the dashboard (see
//!   [`dashboard`]); `/api/tenants/<org>/jobs/<id>/metrics` serves one
//!   job's CPU and memory (see [`metrics`]).
//! - `/api/tenants/<org>/runners` manages runner profiles (see
//!   [`runners`]).
//! - `/api/plan`, `/api/tenants/<org>/{checkout,portal,beta}` and
//!   `/api/polar/webhook` handle the licence (see [`billing`]).
//! - `POST /api/internal/images` takes runner image releases from the
//!   images build (see [`images`]).
//!
//! Everything but sign-in, the webhooks and image releases needs a
//! session, and everything but `GET` needs the session's CSRF token.

mod aws;
mod billing;
mod dashboard;
mod images;
mod metrics;
mod runners;
mod tenants;

use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, Env, Headers, Method, Request, Response, Result, console_error};

use crate::fetch::send;
use crate::github::{self, User};
use crate::session;
use crate::signin::{self, Pending};
use crate::{DB, int, jwt, now_ms, webcrypto};

/// Variable with the app's origin, such as `https://app.hatchrun.io`.
const APP_URL: &str = "APP_URL";
/// Variable with the App's client id.
const CLIENT_ID: &str = "GITHUB_APP_ID";
/// Secret with the App's client secret, for user authorization.
const CLIENT_SECRET: &str = "GITHUB_CLIENT_SECRET";
/// Variable that replaces `https://github.com`, for development.
const GITHUB_WEB_URL: &str = "GITHUB_WEB_URL";
/// Variable that replaces the GitHub API URL, for development.
const GITHUB_API_URL: &str = "GITHUB_API_URL";
/// Variable with the App's slug, as in `github.com/apps/<slug>`.
const APP_SLUG: &str = "GITHUB_APP_SLUG";

/// Longest a session lasts. A session also ends when the user's GitHub
/// token does, since the app acts with it.
const SESSION_MAX_AGE: u64 = 7 * 24 * 60 * 60;

/// The Worker's settings for the app.
pub(crate) struct Config {
    pub app_url: String,
    pub client_id: String,
    pub github_web: String,
    pub github_api: String,
    pub app_slug: String,
}

impl Config {
    pub(crate) fn read(env: &Env) -> Result<Self> {
        Ok(Self {
            app_url: need(env, APP_URL)?.trim_end_matches('/').to_owned(),
            client_id: need(env, CLIENT_ID)?,
            github_web: get(env, GITHUB_WEB_URL)
                .unwrap_or_else(|| signin::WEB.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            github_api: get(env, GITHUB_API_URL)
                .unwrap_or_else(|| github::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            app_slug: need(env, APP_SLUG)?,
        })
    }
}

/// A variable or secret, if set and not empty.
pub(crate) fn get(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|value| value.to_string())
        .ok()
        .filter(|value| !value.is_empty())
}

pub(crate) fn need(env: &Env, name: &str) -> Result<String> {
    get(env, name).ok_or_else(|| worker::Error::RustError(format!("{name} is not set")))
}

/// A signed-in user, as the `users` table has them.
#[derive(Debug, Deserialize)]
pub(crate) struct UserRow {
    pub id: u64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
}

#[derive(Deserialize)]
struct SessionRow {
    id: u64,
    login: String,
    name: Option<String>,
    avatar_url: String,
    github_token: String,
}

/// The user of a session that has not expired. Binds the session key
/// and the time in milliseconds.
const FIND_SESSION: &str = "SELECT users.id, users.login, users.name, users.avatar_url, \
    sessions.github_token FROM sessions JOIN users ON users.id = sessions.user_id \
    WHERE sessions.key = ?1 AND sessions.expires_at > ?2";

/// Ends a session, by its key.
const END_SESSION: &str = "DELETE FROM sessions WHERE key = ?1";

/// Writes a user who signed in. Binds the GitHub user id, login, name
/// or NULL, avatar URL and the time in milliseconds.
const UPSERT_USER: &str = "INSERT INTO users (id, login, name, avatar_url, created_at, updated_at) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
    ON CONFLICT (id) DO UPDATE SET login = ?2, name = ?3, avatar_url = ?4, updated_at = ?5";

/// Starts a session. Binds the key, user id, sealed token, the time and
/// when it expires, both in milliseconds.
const START_SESSION: &str = "INSERT INTO sessions (key, user_id, github_token, created_at, \
    expires_at) VALUES (?1, ?2, ?3, ?4, ?5)";

/// Sweeps sessions that expired before the time in milliseconds.
const SWEEP_SESSIONS: &str = "DELETE FROM sessions WHERE expires_at < ?1";

/// The session a request's cookie names, while it lasts.
pub(crate) struct Session {
    /// The session id from the cookie.
    pub id: Vec<u8>,
    pub user: UserRow,
    /// The user's GitHub token.
    pub token: String,
}

impl Session {
    pub(crate) async fn find(req: &Request, db: &D1Database) -> Result<Option<Self>> {
        let cookies = req.headers().get("Cookie")?;
        let Some(id) =
            session::cookie(cookies.as_deref(), session::COOKIE).and_then(session::decode_id)
        else {
            return Ok(None);
        };
        let row: Option<SessionRow> = db
            .prepare(FIND_SESSION)
            .bind(&[session::key(&id).into(), int(now_ms())?])?
            .first(None)
            .await?;
        Ok(row.and_then(|row| {
            let token = session::open(&id, &row.github_token)?;
            Some(Self {
                id,
                user: UserRow {
                    id: row.id,
                    login: row.login,
                    name: row.name,
                    avatar_url: row.avatar_url,
                },
                token,
            })
        }))
    }

    /// Whether the request carries this session's CSRF token.
    pub(crate) fn csrf_ok(&self, req: &Request) -> Result<bool> {
        let given = req.headers().get(session::CSRF_HEADER)?;
        Ok(session::check_csrf(&self.id, given.as_deref()))
    }
}

/// Answers a request under `/api`.
#[allow(clippy::too_many_lines)] // One match arm per route.
pub async fn handle(mut req: Request, env: Env) -> Result<Response> {
    let config = Config::read(&env)?;
    let db = env.d1(DB)?;
    let path = req.path();
    match (req.method(), path.as_str()) {
        (Method::Get, "/api/auth/login") => return login(&req, &config),
        (Method::Get, "/api/auth/callback") => {
            return match callback(&req, &env, &config, &db).await {
                Ok(response) => Ok(response),
                Err(err) => {
                    console_error!("sign-in: {err}");
                    redirect(
                        &config,
                        "/signin?error=github",
                        &[&session::clear_cookie(session::OAUTH_COOKIE)],
                    )
                }
            };
        }
        (Method::Post, "/api/auth/logout") => {
            let Some(current) = Session::find(&req, &db).await? else {
                return json_error("not signed in", 401);
            };
            if !current.csrf_ok(&req)? {
                return json_error("bad CSRF token", 403);
            }
            db.prepare(END_SESSION)
                .bind(&[session::key(&current.id).into()])?
                .run()
                .await?;
            let mut response = Response::empty()?.with_status(204);
            response
                .headers_mut()
                .append("Set-Cookie", &session::clear_cookie(session::COOKIE))?;
            return Ok(response);
        }
        (Method::Post, "/api/polar/webhook") => return billing::webhook(req, &env).await,
        (Method::Post, crate::images::PATH) => return images::post(req, &env).await,
        _ => {}
    }

    let Some(current) = Session::find(&req, &db).await? else {
        if path == SETUP_PATH {
            // Back from installing the App with the session gone: sign
            // in again and come back here.
            let back = req.url()?;
            let back = format!("{}?{}", back.path(), back.query().unwrap_or_default());
            return found(
                &format!(
                    "{}/api/auth/login?return_to={}",
                    config.app_url,
                    crate::http::encode(&back)
                ),
                &[],
            );
        }
        return json_error("not signed in", 401);
    };
    let method = req.method();
    if method != Method::Get && !current.csrf_ok(&req)? {
        return json_error("bad CSRF token", 403);
    }
    let ctx = Ctx {
        env: &env,
        config: &config,
        db: &db,
        session: &current,
    };
    let segments: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    let answer = match (method, segments.as_slice()) {
        (Method::Get, ["session"]) => session_json(&current),
        (Method::Get, ["installations"]) => tenants::installations(&ctx).await,
        (Method::Get, ["tenants"]) => tenants::mine(&ctx).await,
        (Method::Post, ["tenants"]) => tenants::create(&ctx, req.json().await?).await,
        (Method::Get, ["tenants", org]) => tenants::show(&ctx, org).await,
        (Method::Get, ["tenants", org, "aws"]) => aws::instructions(&ctx, org).await,
        (Method::Post, ["tenants", org, "aws"]) => aws::verify(&ctx, org, req.json().await?).await,
        (Method::Get, ["tenants", org, "jobs"]) => dashboard::jobs(&ctx, org, &req).await,
        (Method::Get, ["tenants", org, "jobs", id, "metrics"]) => metrics::job(&ctx, org, id).await,
        (Method::Put, ["tenants", org, "settings"]) => {
            dashboard::settings(&ctx, org, req.json().await?).await
        }
        (method, ["tenants", org, "runners", rest @ ..]) => {
            runners::route(&ctx, org, method, rest, &mut req).await
        }
        (Method::Get, ["plan"]) => billing::plan(&ctx).await,
        (Method::Post, ["tenants", org, "checkout"]) => billing::checkout(&ctx, org).await,
        (Method::Post, ["tenants", org, "portal"]) => billing::portal(&ctx, org).await,
        (Method::Post, ["tenants", org, "beta"]) => {
            billing::redeem(&ctx, org, req.json().await?).await
        }
        (Method::Get, ["github", "setup"]) => tenants::setup(&ctx, &req).await,
        _ => return json_error("not found", 404),
    };
    match answer {
        Ok(response) => Ok(response),
        Err(Failure { status, message }) => {
            if status >= 500 {
                console_error!("{} {path}: {message}", req.method().as_ref());
            }
            json_error(&message, status)
        }
    }
}

/// `GET /api/session`
fn session_json(current: &Session) -> Answer {
    Ok(Response::from_json(&serde_json::json!({
        "user": {
            "id": current.user.id,
            "login": current.user.login,
            "name": current.user.name,
            "avatarUrl": current.user.avatar_url,
        },
        "csrfToken": session::csrf_token(&current.id),
    }))?)
}

/// The app's callback after the App is installed.
const SETUP_PATH: &str = "/api/github/setup";

/// What a request handler has to work with.
pub(crate) struct Ctx<'a> {
    pub env: &'a Env,
    pub config: &'a Config,
    pub db: &'a D1Database,
    pub session: &'a Session,
}

/// Why a request failed, as the app shows it.
#[derive(Debug)]
pub(crate) struct Failure {
    pub status: u16,
    pub message: String,
}

impl Failure {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<worker::Error> for Failure {
    fn from(err: worker::Error) -> Self {
        Self::new(500, err.to_string())
    }
}

/// Errors from GitHub, AWS or Polar, which the Worker only relays.
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(502, message)
    }
}

pub(crate) type Answer = std::result::Result<Response, Failure>;

/// Starts sign-in: a fresh state and verifier in a cookie, and off to
/// GitHub.
fn login(req: &Request, config: &Config) -> Result<Response> {
    let return_to = req
        .url()?
        .query()
        .and_then(|query| signin::query_param(query, "return_to"))
        .map_or_else(|| "/".to_owned(), |raw| signin::return_to_path(&raw));
    let pending = Pending {
        state: jwt::b64url(webcrypto::random_bytes(16)?),
        verifier: jwt::b64url(webcrypto::random_bytes(32)?),
        return_to,
    };
    let url = signin::authorize_url(
        &config.github_web,
        &config.client_id,
        &signin::redirect_uri(&config.app_url),
        &pending.state,
        &pending.verifier,
    );
    found(
        &url,
        &[&session::set_cookie(
            session::OAUTH_COOKIE,
            &pending.encode(),
            signin::PENDING_MAX_AGE,
        )],
    )
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// Finishes sign-in: checks the state, trades the code for a user
/// token, records the user and starts a session.
async fn callback(
    req: &Request,
    env: &Env,
    config: &Config,
    db: &D1Database,
) -> std::result::Result<Response, String> {
    let query: Callback = req.query().map_err(|err| err.to_string())?;
    let cookies = req.headers().get("Cookie").map_err(|err| err.to_string())?;
    let pending = session::cookie(cookies.as_deref(), session::OAUTH_COOKIE)
        .and_then(Pending::decode)
        .ok_or("no sign-in in progress")?;
    if query.state.as_deref() != Some(pending.state.as_str()) {
        return Err("state does not match".to_owned());
    }
    let code = query.code.ok_or("no code")?;
    let secret = need(env, CLIENT_SECRET).map_err(|err| err.to_string())?;

    let token = signin::user_token(
        &send(&signin::exchange(
            &config.github_web,
            &config.client_id,
            &secret,
            &code,
            &signin::redirect_uri(&config.app_url),
            &pending.verifier,
        ))
        .await?,
    )?;
    let user =
        github::parse_user(&send(&github::user(&config.github_api, &token.access_token)).await?)?;

    let now = now_ms();
    let max_age = token
        .expires_in
        .map_or(SESSION_MAX_AGE, |lifetime| lifetime.min(SESSION_MAX_AGE));
    let id = webcrypto::random_bytes(session::ID_BYTES).map_err(|err| err.to_string())?;
    let nonce: [u8; session::NONCE_BYTES] = webcrypto::random_bytes(session::NONCE_BYTES)
        .map_err(|err| err.to_string())?
        .try_into()
        .map_err(|_| "short nonce")?;
    let sealed = session::seal(&id, &nonce, &token.access_token);
    store_session(db, &user, &id, &sealed, now, max_age)
        .await
        .map_err(|err| err.to_string())?;

    redirect(
        config,
        &pending.return_to,
        &[
            &session::set_cookie(session::COOKIE, &session::encode_id(&id), max_age),
            &session::clear_cookie(session::OAUTH_COOKIE),
        ],
    )
    .map_err(|err| err.to_string())
}

async fn store_session(
    db: &D1Database,
    user: &User,
    id: &[u8],
    sealed: &str,
    now: u64,
    max_age: u64,
) -> Result<()> {
    let now_value = int(now)?;
    db.batch(vec![
        db.prepare(UPSERT_USER).bind(&[
            int(user.id)?,
            user.login.as_str().into(),
            user.name.as_deref().map_or(JsValue::NULL, Into::into),
            user.avatar_url.as_str().into(),
            now_value.clone(),
        ])?,
        db.prepare(START_SESSION).bind(&[
            session::key(id).into(),
            int(user.id)?,
            sealed.into(),
            now_value.clone(),
            int(now + max_age * 1000)?,
        ])?,
        // Sign-ins are rare enough to sweep expired sessions here.
        db.prepare(SWEEP_SESSIONS).bind(&[now_value])?,
    ])
    .await?;
    Ok(())
}

/// A 302 to a path of the app, setting `cookies`.
pub(crate) fn redirect(config: &Config, path: &str, cookies: &[&str]) -> Result<Response> {
    found(&format!("{}{path}", config.app_url), cookies)
}

pub(crate) fn found(url: &str, cookies: &[&str]) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Location", url)?;
    headers.set("Cache-Control", "no-store")?;
    for cookie in cookies {
        headers.append("Set-Cookie", cookie)?;
    }
    Ok(Response::empty()?.with_status(302).with_headers(headers))
}

pub(crate) fn json_error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;

    /// The statements [`store_session`] batches for a sign-in.
    fn sign_in(
        id: u64,
        login: &str,
        name: Option<&str>,
        key: &str,
        now: u64,
    ) -> Vec<(&'static str, Vec<Param>)> {
        vec![
            (
                UPSERT_USER,
                params![id, login, name, "https://avatars.example/u", now],
            ),
            (
                START_SESSION,
                params![key, id, "sealed", now, now + SESSION_MAX_AGE * 1000],
            ),
            (SWEEP_SESSIONS, params![now]),
        ]
    }

    fn find(db: &Db, key: &str, now: u64) -> Option<SessionRow> {
        db.first(FIND_SESSION, &params![key, now])
    }

    #[test]
    fn a_sign_in_starts_a_session() {
        let db = Db::new();
        db.batch(&sign_in(42, "octo", Some("Octo Cat"), "k1", NOW))
            .unwrap();
        let row = find(&db, "k1", NOW + 1).unwrap();
        assert_eq!(row.id, 42);
        assert_eq!(row.login, "octo");
        assert_eq!(row.name.as_deref(), Some("Octo Cat"));
        assert_eq!(row.avatar_url, "https://avatars.example/u");
        assert_eq!(row.github_token, "sealed");
        assert!(find(&db, "k2", NOW + 1).is_none());
        assert!(find(&db, "k1", NOW + SESSION_MAX_AGE * 1000).is_none());
    }

    #[test]
    fn signing_in_again_updates_the_user_and_sweeps_old_sessions() {
        let db = Db::new();
        db.batch(&sign_in(42, "octo", Some("Octo Cat"), "k1", NOW))
            .unwrap();
        let later = NOW + 8 * DAY;
        db.batch(&sign_in(42, "octocat", None, "k2", later))
            .unwrap();
        let row = find(&db, "k2", later).unwrap();
        assert_eq!(row.login, "octocat");
        assert_eq!(row.name, None);
        let keys = db.rows("SELECT key FROM sessions", &[]).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["key"], "k2");
    }

    #[test]
    fn signing_out_ends_the_session() {
        let db = Db::new();
        db.batch(&sign_in(42, "octo", None, "k1", NOW)).unwrap();
        assert_eq!(db.run(END_SESSION, &params!["k1"]), 1);
        assert!(find(&db, "k1", NOW).is_none());
    }
}
