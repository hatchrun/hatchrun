// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `POST /api/internal/images`: the images build posts each release's
//! manifest here (see [`crate::images`]). It carries no session; the
//! caller proves who it is with a GitHub Actions token (see
//! [`crate::actions`]).

use std::cell::RefCell;

use serde::Deserialize;
use worker::{
    D1Database, Env, Request, Response, Result, console_error, console_log, console_warn,
};

use super::{get, json_error};
use crate::actions::{self, Key, Policy, Rejection};
use crate::fetch::send;
use crate::http;
use crate::images::{self, MAX_BODY, ROOT_DEVICE, Release, UPSERT};
use crate::{DB, int, now_ms};

/// Variable that replaces GitHub's Actions issuer, for development.
/// Production refuses it (see [`images::issuer`]).
const ISSUER_URL: &str = "ACTIONS_OIDC_ISSUER";
/// Variable with the app's origin.
const APP_URL: &str = "APP_URL";

/// The issuer's keys, kept between requests the isolate serves.
struct Cached {
    issuer: String,
    fetched_at: u64,
    keys: Vec<Key>,
}

/// A fetch of the keys that failed, remembered for
/// [`actions::KEYS_FAILURE_MS`].
struct Failed {
    issuer: String,
    at: u64,
    error: String,
}

thread_local! {
    static KEYS: RefCell<Option<Cached>> = const { RefCell::new(None) };
    static FAILED: RefCell<Option<Failed>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
struct Stored {
    version: Option<String>,
}

pub(crate) async fn post(mut req: Request, env: &Env) -> Result<Response> {
    let token = req
        .headers()
        .get("Authorization")?
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned));
    let Some(token) = token else {
        return json_error("no bearer token", 401);
    };
    let length = req.headers().get("Content-Length")?;
    if length.is_some_and(|length| length.parse::<usize>().map_or(true, |n| n > MAX_BODY)) {
        return json_error(&format!("manifest is larger than {MAX_BODY} bytes"), 413);
    }
    let issuer = match trusted_issuer(&req, env)? {
        Ok(issuer) => issuer,
        Err(response) => return Ok(response),
    };
    let policy = Policy {
        issuer: &issuer,
        ..images::POLICY
    };
    let claims = match check(&token, &policy).await {
        Ok(claims) => claims,
        Err(Refused::Token(rejection)) => {
            console_warn!("images: {rejection}");
            let status = if matches!(rejection, Rejection::Claim { .. }) {
                403
            } else {
                401
            };
            return json_error(&rejection.to_string(), status);
        }
        Err(Refused::Keys(err)) => {
            console_error!("images: {err}");
            return json_error("cannot read the issuer's keys", 502);
        }
    };

    let body = req.bytes().await?;
    let release = match images::parse(&body) {
        Ok(release) => release,
        Err(err) => return json_error(&err, 400),
    };
    if let Err(err) = images::check_version_time(&release.version, claims.iat) {
        return json_error(&err, 400);
    }
    let db = env.d1(DB)?;
    let used = db
        .prepare("SELECT 1 FROM used_tokens WHERE jti = ?1")
        .bind(&[claims.jti.as_str().into()])?
        .first::<serde_json::Value>(None)
        .await?;
    if used.is_some() {
        return json_error("token already used", 401);
    }
    let stored: Option<Stored> = db
        .prepare("SELECT MAX(version) AS version FROM images")
        .first(None)
        .await?;
    let stored = stored.and_then(|row| row.version);
    if !images::is_current(&release.version, stored.as_deref()) {
        return json_error(
            &format!(
                "{} is older than {}",
                release.version,
                stored.unwrap_or_default()
            ),
            409,
        );
    }

    store(&db, &release, &claims).await?;
    console_log!(
        "images: {} from run {} of {}, {} images",
        release.version,
        claims.run_id,
        claims.repository,
        release.images.len()
    );
    Response::from_json(&serde_json::json!({
        "version": release.version,
        "images": release.images.len(),
    }))
}

enum Refused {
    Token(Rejection),
    Keys(String),
}

/// Verifies the token against the issuer's keys, fetching them when the
/// cache is old or lacks the token's `kid`.
async fn check(token: &str, policy: &Policy<'_>) -> std::result::Result<actions::Claims, Refused> {
    let kid = actions::key_id(token).map_err(Refused::Token)?;
    let now = now_ms();
    let cached = KEYS.with_borrow(|cached| {
        cached
            .as_ref()
            .filter(|cached| cached.issuer == policy.issuer)
            .filter(|cached| {
                let have = cached.keys.iter().any(|key| key.kid == kid);
                actions::reuse_keys(now.saturating_sub(cached.fetched_at), have)
            })
            .map(|cached| cached.keys.clone())
    });
    let keys = match cached {
        Some(keys) => keys,
        None => fetch_keys(policy.issuer, now)
            .await
            .map_err(Refused::Keys)?,
    };
    actions::verify(token, &keys, policy, now / 1000).map_err(Refused::Token)
}

/// Fetches the issuer's JWKS and caches its keys. A failure is
/// remembered for [`actions::KEYS_FAILURE_MS`] and answered from memory
/// until then.
async fn fetch_keys(issuer: &str, now: u64) -> std::result::Result<Vec<Key>, String> {
    let failed = FAILED.with_borrow(|failed| {
        failed
            .as_ref()
            .filter(|failed| failed.issuer == issuer)
            .filter(|failed| now.saturating_sub(failed.at) < actions::KEYS_FAILURE_MS)
            .map(|failed| failed.error.clone())
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let fetched = fetch_jwks(issuer).await;
    match &fetched {
        Ok(keys) => KEYS.with_borrow_mut(|cached| {
            *cached = Some(Cached {
                issuer: issuer.to_owned(),
                fetched_at: now,
                keys: keys.clone(),
            });
        }),
        Err(error) => FAILED.with_borrow_mut(|failed| {
            *failed = Some(Failed {
                issuer: issuer.to_owned(),
                at: now,
                error: error.clone(),
            });
        }),
    }
    fetched
}

async fn fetch_jwks(issuer: &str) -> std::result::Result<Vec<Key>, String> {
    let url = format!("{}{}", issuer.trim_end_matches('/'), actions::JWKS_PATH);
    let response =
        send(&http::Request::new("GET", url).header("Accept", "application/json")).await?;
    if !response.ok() {
        return Err(format!(
            "JWKS answered {}: {}",
            response.status,
            response.text()
        ));
    }
    actions::parse_jwks(&response.body)
}

/// The issuer to trust, or the answer when production has another one
/// configured. Warns on every request while an override is active.
fn trusted_issuer(req: &Request, env: &Env) -> Result<std::result::Result<String, Response>> {
    let configured = get(env, ISSUER_URL);
    let host = req.url()?.host_str().map(str::to_owned);
    let app_url = get(env, APP_URL).unwrap_or_default();
    match images::issuer(configured.as_deref(), host.as_deref(), &app_url) {
        Ok(issuer) => {
            if issuer != actions::ISSUER {
                console_warn!("images: trusting {issuer} instead of GitHub ({ISSUER_URL})");
            }
            Ok(Ok(issuer.to_owned()))
        }
        Err(err) => {
            console_error!("images: {ISSUER_URL}: {err}");
            Ok(Err(json_error("the issuer is misconfigured", 500)?))
        }
    }
}

/// Spends the token and writes the release's images in one batch, so a
/// second post with the token fails on its primary key and writes
/// nothing.
async fn store(db: &D1Database, release: &Release, claims: &actions::Claims) -> Result<()> {
    let now = int(now_ms())?;
    let mut statements = vec![
        db.prepare("DELETE FROM used_tokens WHERE expires_at < ?1")
            .bind(std::slice::from_ref(&now))?,
        db.prepare("INSERT INTO used_tokens (jti, expires_at) VALUES (?1, ?2)")
            .bind(&[
                claims.jti.as_str().into(),
                int(claims
                    .exp
                    .saturating_add(actions::LEEWAY)
                    .saturating_mul(1000))?,
            ])?,
    ];
    for image in &release.images {
        statements.push(db.prepare(UPSERT).bind(&[
            image.region.as_str().into(),
            image.arch.into(),
            image.image_id.as_str().into(),
            ROOT_DEVICE.into(),
            release.version.as_str().into(),
            now.clone(),
        ])?);
    }
    db.batch(statements).await?;
    Ok(())
}
