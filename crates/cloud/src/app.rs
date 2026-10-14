// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The API of app.hatchrun.io, under `/api`. The app itself is static
//! assets from `web/app`, served by the same Worker, so the session
//! cookie stays on one origin.
//!
//! - `GET /api/auth/login` starts sign-in with GitHub (see
//!   [`crate::signin`]); `GET /api/auth/callback` finishes it.
//! - `POST /api/auth/logout` ends the session.
//! - `GET /api/session` returns the signed-in user and the CSRF token
//!   the app sends back on every request that changes something.

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

/// Longest a session lasts. A session also ends when the user's GitHub
/// token does, since the app acts with it.
const SESSION_MAX_AGE: u64 = 7 * 24 * 60 * 60;

/// The Worker's settings for the app.
pub(crate) struct Config {
    pub app_url: String,
    pub client_id: String,
    pub github_web: String,
    pub github_api: String,
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

/// The session a request's cookie names, while it lasts.
pub(crate) struct Session {
    /// The session id from the cookie.
    pub id: Vec<u8>,
    pub user: UserRow,
    /// The user's GitHub token.
    #[allow(dead_code)] // Org setup acts with it.
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
            .prepare(
                "SELECT users.id, users.login, users.name, users.avatar_url, sessions.github_token \
                 FROM sessions JOIN users ON users.id = sessions.user_id \
                 WHERE sessions.key = ?1 AND sessions.expires_at > ?2",
            )
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
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let config = Config::read(&env)?;
    let db = env.d1(DB)?;
    let path = req.path();
    match (req.method(), path.as_str()) {
        (Method::Get, "/api/auth/login") => login(&req, &config),
        (Method::Get, "/api/auth/callback") => match callback(&req, &env, &config, &db).await {
            Ok(response) => Ok(response),
            Err(err) => {
                console_error!("sign-in: {err}");
                redirect(
                    &config,
                    "/signin?error=github",
                    &[&session::clear_cookie(session::OAUTH_COOKIE)],
                )
            }
        },
        (Method::Post, "/api/auth/logout") => {
            let Some(current) = Session::find(&req, &db).await? else {
                return json_error("not signed in", 401);
            };
            if !current.csrf_ok(&req)? {
                return json_error("bad CSRF token", 403);
            }
            db.prepare("DELETE FROM sessions WHERE key = ?1")
                .bind(&[session::key(&current.id).into()])?
                .run()
                .await?;
            let mut response = Response::empty()?.with_status(204);
            response
                .headers_mut()
                .append("Set-Cookie", &session::clear_cookie(session::COOKIE))?;
            Ok(response)
        }
        (Method::Get, "/api/session") => match Session::find(&req, &db).await? {
            Some(current) => Response::from_json(&serde_json::json!({
                "user": {
                    "id": current.user.id,
                    "login": current.user.login,
                    "name": current.user.name,
                    "avatarUrl": current.user.avatar_url,
                },
                "csrfToken": session::csrf_token(&current.id),
            })),
            None => json_error("not signed in", 401),
        },
        _ => json_error("not found", 404),
    }
}

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
        db.prepare(
            "INSERT INTO users (id, login, name, avatar_url, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
             ON CONFLICT (id) DO UPDATE SET login = ?2, name = ?3, avatar_url = ?4, updated_at = ?5",
        )
        .bind(&[
            int(user.id)?,
            user.login.as_str().into(),
            user.name.as_deref().map_or(JsValue::NULL, Into::into),
            user.avatar_url.as_str().into(),
            now_value.clone(),
        ])?,
        db.prepare(
            "INSERT INTO sessions (key, user_id, github_token, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&[
            session::key(id).into(),
            int(user.id)?,
            sealed.into(),
            now_value.clone(),
            int(now + max_age * 1000)?,
        ])?,
        // Sign-ins are rare enough to sweep expired sessions here.
        db.prepare("DELETE FROM sessions WHERE expires_at < ?1")
            .bind(&[now_value])?,
    ])
    .await?;
    Ok(())
}

/// A 302 to a path of the app, setting `cookies`.
fn redirect(config: &Config, path: &str, cookies: &[&str]) -> Result<Response> {
    found(&format!("{}{path}", config.app_url), cookies)
}

fn found(url: &str, cookies: &[&str]) -> Result<Response> {
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
