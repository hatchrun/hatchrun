// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Hands each runner instance its JIT config exactly once.
//!
//! The scaler launches an instance with a random ticket in its user data,
//! then stores the instance's JIT config here under that ticket. At boot
//! the agent claims the config with the ticket. The claim deletes it, so
//! the ticket in user data is useless once the runner has started, and a
//! config nobody claims is deleted when it expires.
//!
//! Each ticket gets its own Durable Object, named by the ticket's hash.
//! A Durable Object handles one request at a time, so two claims for the
//! same ticket cannot both succeed.
//!
//! - `PUT /v1/tickets/{ticket}` with `Authorization: Bearer {token}` and
//!   `{"config": "...", "expiresIn": 900}` stores a config. A ticket is
//!   stored once; storing it again answers 409.
//! - `POST /v1/tickets/{ticket}/claim` answers `{"config": "..."}` once,
//!   then 404. A ticket that was never stored, was claimed or expired
//!   answers the same 404, so the agent retries until the scaler stores
//!   its config.

pub mod protocol;
mod ticket;

use worker::{Env, Method, Request, RequestInit, Response, Result, event};

use crate::protocol::Route;

/// Durable Object binding in `wrangler.toml`.
const BINDING: &str = "TICKETS";
/// Secret holding the token the scaler sends.
const TOKEN_SECRET: &str = "BROKER_TOKEN";

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    let path = req.path();
    let (ticket, op, body) = match protocol::route(req.method().as_ref(), &path) {
        Route::Store(ticket) => {
            let token = env
                .secret(TOKEN_SECRET)
                .map(|secret| secret.to_string())
                .unwrap_or_default();
            if token.is_empty() {
                return error("broker has no BROKER_TOKEN", 500);
            }
            if !protocol::authorized(req.headers().get("Authorization")?.as_deref(), &token) {
                return error("unauthorized", 401);
            }
            // A body without a declared length would be read whole before
            // any size check.
            match req
                .headers()
                .get("Content-Length")?
                .and_then(|length| length.parse::<usize>().ok())
            {
                None => return error("content-length required", 411),
                Some(length) if length > protocol::MAX_BODY => {
                    return error("body too large", 413);
                }
                Some(_) => {}
            }
            (ticket, "store", Some(req.bytes().await?))
        }
        Route::Claim(ticket) => (ticket, "claim", None),
        Route::NotFound => return error("not found", 404),
    };

    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    if let Some(body) = body {
        init.with_body(Some(body.into()));
    }
    let request = Request::new_with_init(&format!("https://ticket/{op}"), &init)?;
    env.durable_object(BINDING)?
        .id_from_name(&protocol::object_name(ticket))?
        .get_stub()?
        .fetch_with_request(request)
        .await
}

pub(crate) fn error(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}

pub(crate) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}
