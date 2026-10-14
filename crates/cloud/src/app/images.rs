// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `POST /api/internal/images`: the images build posts each release's
//! manifest here (see [`crate::images`]). It carries no session; the
//! caller proves who it is with a GitHub Actions token (see
//! [`crate::actions`]).

use std::cell::RefCell;

use serde::Deserialize;
use worker::{Env, Request, Response, Result, console_error, console_log, console_warn};

use super::{get, json_error};
use crate::actions::{self, Key, Policy, Rejection};
use crate::fetch::send;
use crate::http;
use crate::images::{self, ROOT_DEVICE, UPSERT};
use crate::{DB, int, now_ms};

/// Variable that replaces GitHub's Actions issuer, for development.
const ISSUER_URL: &str = "ACTIONS_OIDC_ISSUER";

/// The issuer's keys, kept between requests the isolate serves.
struct Cached {
    issuer: String,
    fetched_at: u64,
    keys: Vec<Key>,
}

thread_local! {
    static KEYS: RefCell<Option<Cached>> = const { RefCell::new(None) };
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
    let issuer = get(env, ISSUER_URL).unwrap_or_else(|| actions::ISSUER.to_owned());
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
    let db = env.d1(DB)?;
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

    let now = int(now_ms())?;
    let statements = release
        .images
        .iter()
        .map(|image| {
            db.prepare(UPSERT).bind(&[
                image.region.as_str().into(),
                image.arch.into(),
                image.image_id.as_str().into(),
                ROOT_DEVICE.into(),
                release.version.as_str().into(),
                now.clone(),
            ])
        })
        .collect::<Result<Vec<_>>>()?;
    db.batch(statements).await?;
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

/// Fetches the issuer's JWKS and caches its keys.
async fn fetch_keys(issuer: &str, now: u64) -> std::result::Result<Vec<Key>, String> {
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
    let keys = actions::parse_jwks(&response.body)?;
    KEYS.with_borrow_mut(|cached| {
        *cached = Some(Cached {
            issuer: issuer.to_owned(),
            fetched_at: now,
            keys: keys.clone(),
        });
    });
    Ok(keys)
}
