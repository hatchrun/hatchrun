// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! An organization's licence: Polar checkout, the customer portal,
//! Polar's webhooks, and the beta, which needs no payment.
//!
//! - `GET /api/plan` describes the licence's Polar products.
//! - `POST /api/tenants/<org>/checkout` opens a Polar checkout.
//! - `POST /api/tenants/<org>/portal` opens Polar's customer portal.
//! - `POST /api/tenants/<org>/beta` with `{"code": ...}` redeems a beta
//!   invite code.
//! - `POST /api/polar/webhook` takes Polar's deliveries.

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use worker::{D1Database, Env, Request, Response, Result, console_log, console_warn};

use super::tenants::authorize;
use super::{Answer, Ctx, Failure, get, json_error, need};
use crate::fetch::send;
use crate::polar::{self, Event, Signed};
use crate::{int, now_ms, session};

/// Variable that replaces Polar's API URL, such as Polar's sandbox.
const POLAR_API_URL: &str = "POLAR_API_URL";
/// Variable with the Polar products of the licence, separated by
/// commas: one per billing interval.
const PRODUCT_ID: &str = "POLAR_PRODUCT_ID";
/// Secret with Polar's organization access token.
const ACCESS_TOKEN: &str = "POLAR_ACCESS_TOKEN";
/// Secret with the webhook endpoint's secret.
const WEBHOOK_SECRET: &str = "POLAR_WEBHOOK_SECRET";

/// Where Polar is, and the licence's products, when billing is set up.
struct Polar {
    api: String,
    product_ids: Vec<String>,
    token: String,
}

impl Polar {
    fn read(env: &Env) -> std::result::Result<Self, Failure> {
        let unset = || Failure::new(503, "billing is not set up yet");
        Ok(Self {
            api: get(env, POLAR_API_URL)
                .unwrap_or_else(|| polar::API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            product_ids: get(env, PRODUCT_ID)
                .map(|list| polar::product_ids(&list))
                .filter(|ids| !ids.is_empty())
                .ok_or_else(unset)?,
            token: get(env, ACCESS_TOKEN).ok_or_else(unset)?,
        })
    }
}

/// A tenant's licence, as the app shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Billing {
    pub beta: bool,
    pub licensed: bool,
    pub subscription: Option<SubscriptionView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubscriptionView {
    status: String,
    current_period_end: Option<String>,
    cancel_at_period_end: bool,
}

#[derive(Deserialize)]
struct SubscriptionRow {
    status: String,
    current_period_end: Option<String>,
    cancel_at_period_end: u8,
}

/// The licence of tenant `id`, which is in the beta when `beta`.
pub(crate) async fn billing(db: &D1Database, id: &str, beta: bool) -> Result<Billing> {
    // The licensed subscription if there is one, else the latest.
    let row: Option<SubscriptionRow> = db
        .prepare(
            "SELECT status, current_period_end, cancel_at_period_end FROM subscriptions \
             WHERE tenant_id = ?1 \
             ORDER BY status IN ('active', 'trialing') DESC, modified_at DESC LIMIT 1",
        )
        .bind(&[id.into()])?
        .first(None)
        .await?;
    let subscription = row.map(|row| SubscriptionView {
        cancel_at_period_end: row.cancel_at_period_end == 1,
        current_period_end: row.current_period_end,
        status: row.status,
    });
    let paid = subscription
        .as_ref()
        .is_some_and(|subscription| polar::LICENSED.contains(&subscription.status.as_str()));
    Ok(Billing {
        beta,
        licensed: beta || paid,
        subscription,
    })
}

/// Puts the tenant in the beta when the operator listed its
/// organization.
pub(crate) async fn join_listed_beta(db: &D1Database, id: &str) -> Result<()> {
    let joined = db
        .prepare(
            "UPDATE tenants SET beta = 1, updated_at = ?2 WHERE id = ?1 AND beta = 0 \
             AND EXISTS (SELECT 1 FROM beta_orgs WHERE beta_orgs.org_login = tenants.org_login)",
        )
        .bind(&[id.into(), int(now_ms())?])?
        .run()
        .await?;
    if joined.meta()?.and_then(|meta| meta.changes) == Some(1) {
        crate::launch::recheck(db, id).await?;
    }
    Ok(())
}

/// `GET /api/plan`: every product of the licence, shortest billing
/// interval first.
pub(crate) async fn plan(ctx: &Ctx<'_>) -> Answer {
    let polar = Polar::read(ctx.env)?;
    let requests: Vec<_> = polar
        .product_ids
        .iter()
        .map(|id| polar::product(&polar.api, &polar.token, id))
        .collect();
    let answers = join_all(requests.iter().map(send)).await;
    let mut plans = Vec::with_capacity(answers.len());
    for answer in answers {
        plans.push(polar::plan(&answer?).map_err(|err| Failure::new(502, err))?);
    }
    polar::sort(&mut plans);
    Ok(Response::from_json(&serde_json::json!({ "plans": plans }))?)
}

/// `POST /api/tenants/<org>/checkout`
pub(crate) async fn checkout(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let polar = Polar::read(ctx.env)?;
    if billing(ctx.db, &row.id, row.beta == 1).await?.licensed {
        return Err(Failure::new(
            409,
            format!("{} already has a licence", row.org_login),
        ));
    }
    let page = format!("{}/o/{}", ctx.config.app_url, row.org_login);
    let url = polar::checkout_url(
        &send(&polar::checkout(
            &polar.api,
            &polar.token,
            &polar.product_ids,
            &row.id,
            &row.org_login,
            &format!("{page}?checkout={{CHECKOUT_ID}}"),
            &page,
        ))
        .await?,
    )?;
    Ok(Response::from_json(&serde_json::json!({ "url": url }))?)
}

/// `POST /api/tenants/<org>/portal`
pub(crate) async fn portal(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let polar = Polar::read(ctx.env)?;
    let page = format!("{}/o/{}", ctx.config.app_url, row.org_login);
    let url = polar::portal_url(
        &send(&polar::customer_session(
            &polar.api,
            &polar.token,
            &row.id,
            &page,
        ))
        .await?,
    )?;
    Ok(Response::from_json(&serde_json::json!({ "url": url }))?)
}

#[derive(Debug, Deserialize)]
pub(crate) struct Redeem {
    code: String,
}

/// `POST /api/tenants/<org>/beta`
pub(crate) async fn redeem(ctx: &Ctx<'_>, org: &str, body: Redeem) -> Answer {
    let row = authorize(ctx, org).await?;
    if row.beta == 1 {
        return super::tenants::show(ctx, &row.org_login).await;
    }
    let code = body.code.trim();
    if code.is_empty() || code.len() > 200 {
        return Err(Failure::new(400, "that invite code is not valid"));
    }
    let hash = session::hex(&sha256(code.as_bytes()));
    let used = ctx
        .db
        .prepare(
            "UPDATE invites SET uses_left = uses_left - 1 WHERE code_hash = ?1 AND uses_left > 0",
        )
        .bind(&[hash.into()])?
        .run()
        .await?;
    if used.meta()?.and_then(|meta| meta.changes) != Some(1) {
        return Err(Failure::new(
            400,
            "that invite code is not valid or used up",
        ));
    }
    ctx.db
        .prepare("UPDATE tenants SET beta = 1, updated_at = ?2 WHERE id = ?1")
        .bind(&[row.id.as_str().into(), int(now_ms())?])?
        .run()
        .await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    console_log!("tenant {} joined the beta with an invite code", row.id);
    super::tenants::show(ctx, &row.org_login).await
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

/// `POST /api/polar/webhook`: no session, the signature is the
/// authentication.
pub(crate) async fn webhook(mut req: Request, env: &Env) -> Result<Response> {
    let secret = need(env, WEBHOOK_SECRET)?;
    let headers = req.headers();
    let id = headers.get("webhook-id")?.unwrap_or_default();
    let timestamp = headers.get("webhook-timestamp")?.unwrap_or_default();
    let signature = headers.get("webhook-signature")?.unwrap_or_default();
    let body = req.bytes().await?;
    let signed = Signed {
        id: &id,
        timestamp: &timestamp,
        signature: &signature,
    };
    if !polar::verify(&secret, signed, &body, now_ms() / 1000) {
        return json_error("bad signature", 403);
    }
    let event = match polar::parse(&body) {
        Ok(event) => event,
        Err(err) => return json_error(&format!("bad payload: {err}"), 400),
    };
    let subscription = match event {
        Event::Subscription(subscription) => subscription,
        Event::Ignored(reason) => {
            return Response::from_json(&serde_json::json!({ "ignored": reason }));
        }
    };
    let db = env.d1(crate::DB)?;
    let result = db
        .prepare(
            "INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id, \
             current_period_end, cancel_at_period_end, modified_at, updated_at) \
             SELECT ?1, id, ?3, ?4, ?5, ?6, ?7, ?8, ?9 FROM tenants WHERE id = ?2 \
             ON CONFLICT (id) DO UPDATE SET status = excluded.status, \
             customer_id = excluded.customer_id, product_id = excluded.product_id, \
             current_period_end = excluded.current_period_end, \
             cancel_at_period_end = excluded.cancel_at_period_end, \
             modified_at = excluded.modified_at, updated_at = excluded.updated_at \
             WHERE excluded.modified_at >= subscriptions.modified_at",
        )
        .bind(&[
            subscription.id.as_str().into(),
            subscription.tenant.as_str().into(),
            subscription.status.as_str().into(),
            subscription.customer_id.as_str().into(),
            subscription.product_id.as_str().into(),
            subscription
                .current_period_end
                .as_deref()
                .map_or(wasm_bindgen::JsValue::NULL, Into::into),
            u8::from(subscription.cancel_at_period_end).into(),
            subscription.modified_at.as_str().into(),
            int(now_ms())?,
        ])?
        .run()
        .await?;
    if result.meta()?.and_then(|meta| meta.changes) == Some(0) {
        console_warn!(
            "subscription {} for tenant {}: no such tenant, or an older delivery",
            subscription.id,
            subscription.tenant
        );
    } else if polar::LICENSED.contains(&subscription.status.as_str()) {
        crate::launch::recheck(&db, &subscription.tenant).await?;
    }
    Ok(Response::empty()?.with_status(202))
}
