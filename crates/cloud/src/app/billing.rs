// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! An organization's billing: its job minutes this month, Polar
//! checkout, the customer portal, Polar's webhooks, and the beta, which
//! needs no payment. See [`crate::usage`] for the free minutes and who
//! may launch.
//!
//! - `GET /api/plan` describes the metered product and the free
//!   minutes.
//! - `POST /api/tenants/<org>/checkout` opens a Polar checkout, which
//!   adds the card that pays per minute past the free ones.
//! - `POST /api/tenants/<org>/portal` opens Polar's customer portal.
//! - `POST /api/tenants/<org>/beta` with `{"code": ...}` redeems a beta
//!   invite code.
//! - `POST /api/polar/webhook` takes Polar's deliveries.

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use worker::{Env, Request, Response, Result, console_error, console_log, console_warn};

use super::tenants::authorize;
use super::{Answer, Ctx, Failure, get, json_error, need};
use crate::db::{Db, params};
use crate::fetch::send;
use crate::polar::{self, Event, Signed, Subscription};
use crate::reconcile::time;
use crate::{ci_wake, now_ms, session, usage};

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

/// A tenant's billing, as the app shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Billing {
    pub beta: bool,
    /// In the beta or paying, so there is nothing to set up.
    pub licensed: bool,
    pub subscription: Option<SubscriptionView>,
    pub usage: UsageView,
}

/// A tenant's job minutes this month.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageView {
    /// The month's first and the next month's first instant, ISO 8601.
    month_start: String,
    month_end: String,
    minutes: u64,
    free_minutes: u32,
    /// When it used up the free minutes this month, ISO 8601.
    free_used_up_at: Option<String>,
    /// When runners stop launching without a card, ISO 8601.
    grace_ends_at: Option<String>,
    /// Whether runners launch now.
    pub may_launch: bool,
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
    cancel_at_period_end: bool,
}

/// The tenant's licensed subscription if there is one, else its latest.
const SUBSCRIPTION: &str = "SELECT status, current_period_end, cancel_at_period_end \
    FROM subscriptions WHERE tenant_id = $1 \
    ORDER BY status IN ('active', 'trialing') DESC, modified_at DESC LIMIT 1";

/// The licence of tenant `id`, which is in the beta when `beta`.
pub(crate) async fn billing(db: &Db, id: &str, beta: bool) -> Result<Billing> {
    let row: Option<SubscriptionRow> = db.first(SUBSCRIPTION, &params![id]).await?;
    let subscription = row.map(|row| SubscriptionView {
        cancel_at_period_end: row.cancel_at_period_end,
        current_period_end: row.current_period_end,
        status: row.status,
    });
    let paid = subscription
        .as_ref()
        .is_some_and(|subscription| polar::LICENSED.contains(&subscription.status.as_str()));
    let month = usage::month(db, id).await?;
    let (start, end) = time::month(now_ms());
    let unpaid = !beta && !paid;
    Ok(Billing {
        beta,
        licensed: beta || paid,
        subscription,
        usage: UsageView {
            month_start: time::format(start),
            month_end: time::format(end),
            minutes: month.minutes,
            free_minutes: usage::FREE_MINUTES,
            free_used_up_at: month.free_used_up_at.map(time::format),
            grace_ends_at: month
                .free_used_up_at
                .filter(|_| unpaid)
                .map(|at| time::format(at + usage::GRACE_MS)),
            may_launch: month.may_launch,
        },
    })
}

/// Puts a tenant in the beta when its organization is in `beta_orgs`,
/// which holds logins in lower case. Binds the tenant and the time in
/// milliseconds.
const JOIN_LISTED_BETA: &str = "UPDATE tenants SET beta = true, updated_at = $2 \
    WHERE id = $1 AND NOT beta \
    AND EXISTS (SELECT 1 FROM beta_orgs WHERE beta_orgs.org_login = lower(tenants.org_login))";

/// Puts the tenant in the beta when the operator listed its
/// organization.
pub(crate) async fn join_listed_beta(db: &Db, id: &str) -> Result<()> {
    let joined = db.run(JOIN_LISTED_BETA, &params![id, now_ms()]).await?;
    if joined == 1 {
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

/// Whether a checkout or portal request asked to come back to the CI
/// page: its query holds `return=ci`.
pub(crate) fn back_to_ci(query: Option<&str>) -> bool {
    query.is_some_and(|query| query.split('&').any(|pair| pair == "return=ci"))
}

/// `POST /api/tenants/<org>/checkout`
pub(crate) async fn checkout(ctx: &Ctx<'_>, org: &str, back_to_ci: bool) -> Answer {
    let row = authorize(ctx, org).await?;
    let polar = Polar::read(ctx.env)?;
    let card = ctx
        .db
        .first::<SubscriptionRow>(SUBSCRIPTION, &params![row.id.as_str()])
        .await?
        .is_some_and(|row| polar::LICENSED.contains(&row.status.as_str()));
    if card {
        return Err(Failure::new(
            409,
            format!("{} already has a card on file", row.org_login),
        ));
    }
    let page = if back_to_ci {
        format!("{}/o/{}/ci", ctx.config.app_url, row.org_login)
    } else {
        format!("{}/o/{}/settings", ctx.config.app_url, row.org_login)
    };
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
pub(crate) async fn portal(ctx: &Ctx<'_>, org: &str, back_to_ci: bool) -> Answer {
    let row = authorize(ctx, org).await?;
    let polar = Polar::read(ctx.env)?;
    let page = if back_to_ci {
        format!("{}/o/{}/ci", ctx.config.app_url, row.org_login)
    } else {
        format!("{}/o/{}", ctx.config.app_url, row.org_login)
    };
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

/// Uses up one use of an invite code, by its hash.
const USE_INVITE: &str =
    "UPDATE invites SET uses_left = uses_left - 1 WHERE code_hash = $1 AND uses_left > 0";

/// Puts a tenant in the beta. Binds the tenant and the time in
/// milliseconds.
const JOIN_BETA: &str = "UPDATE tenants SET beta = true, updated_at = $2 WHERE id = $1";

#[derive(Debug, Deserialize)]
pub(crate) struct Redeem {
    code: String,
}

/// `POST /api/tenants/<org>/beta`
pub(crate) async fn redeem(ctx: &Ctx<'_>, org: &str, body: Redeem) -> Answer {
    let row = authorize(ctx, org).await?;
    if row.beta {
        return super::tenants::show(ctx, &row.org_login).await;
    }
    let code = body.code.trim();
    if code.is_empty() || code.len() > 200 {
        return Err(Failure::new(400, "that invite code is not valid"));
    }
    let hash = session::hex(&sha256(code.as_bytes()));
    let used = ctx.db.run(USE_INVITE, &params![hash]).await?;
    if used != 1 {
        return Err(Failure::new(
            400,
            "that invite code is not valid or used up",
        ));
    }
    ctx.db
        .run(JOIN_BETA, &params![row.id.as_str(), now_ms()])
        .await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    console_log!("tenant {} joined the beta with an invite code", row.id);
    super::tenants::show(ctx, &row.org_login).await
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

/// Writes a subscription of an existing tenant, unless the row holds a
/// newer delivery. Binds the subscription id, tenant, status, customer,
/// product, period end or NULL, cancel at period end, Polar's modified
/// time and the time in milliseconds.
const UPSERT_SUBSCRIPTION: &str = "INSERT INTO subscriptions (id, tenant_id, status, \
    customer_id, product_id, current_period_end, cancel_at_period_end, modified_at, updated_at) \
    SELECT $1::text, id, $3::text, $4::text, $5::text, $6::text, $7::boolean, $8::text, \
    $9::bigint FROM tenants WHERE id = $2 \
    ON CONFLICT (id) DO UPDATE SET status = excluded.status, \
    customer_id = excluded.customer_id, product_id = excluded.product_id, \
    current_period_end = excluded.current_period_end, \
    cancel_at_period_end = excluded.cancel_at_period_end, \
    modified_at = excluded.modified_at, updated_at = excluded.updated_at \
    WHERE excluded.modified_at >= subscriptions.modified_at";

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
    let db = Db::connect(env).await?;
    let runner_ids = get(env, PRODUCT_ID)
        .map(|list| polar::product_ids(&list))
        .unwrap_or_default();
    if !runner_ids.contains(&subscription.product_id) {
        // A product nobody configured must not license anything, so the
        // row is not written.
        console_error!(
            "polar webhook: subscription {} is for product {}, not in POLAR_PRODUCT_ID",
            subscription.id,
            subscription.product_id
        );
        return Ok(Response::empty()?.with_status(202));
    }
    let changed = db
        .run(
            UPSERT_SUBSCRIPTION,
            &params![
                subscription.id.as_str(),
                subscription.tenant.as_str(),
                subscription.status.as_str(),
                subscription.customer_id.as_str(),
                subscription.product_id.as_str(),
                subscription.current_period_end.as_deref(),
                subscription.cancel_at_period_end,
                subscription.modified_at.as_str(),
                now_ms(),
            ],
        )
        .await?;
    let woken = settle(&db, &subscription, changed).await?;
    wake(env, &woken).await;
    Ok(Response::empty()?.with_status(202))
}

/// Sends a launch message to the CI Worker's queue for each woken job.
/// A message that does not go is left to the CI reconciler's backstop,
/// so it is logged and not retried here: Polar must get its answer.
async fn wake(env: &Env, woken: &[ci_wake::Woken]) {
    if woken.is_empty() {
        return;
    }
    let queue = match env.queue(ci_wake::QUEUE) {
        Ok(queue) => queue,
        Err(err) => {
            console_error!("ci wake: no queue: {err}");
            return;
        }
    };
    for job in woken {
        if let Err(err) = queue.send(*job).await {
            console_error!("ci wake: job {}: {err}", job.job_id);
        }
    }
}

/// What a stored subscription changes. A licensed one clears the
/// tenant's grace period, rechecks waiting runner launches and returns
/// the tenant's oldest CI jobs held for the card, for the caller to send
/// to the CI launch queue. One that ended past the free minutes starts
/// the grace period.
async fn settle(db: &Db, subscription: &Subscription, changed: u64) -> Result<Vec<ci_wake::Woken>> {
    if changed == 0 {
        console_warn!(
            "subscription {} for tenant {}: no such tenant, or an older delivery",
            subscription.id,
            subscription.tenant
        );
    } else if polar::LICENSED.contains(&subscription.status.as_str()) {
        usage::clear_grace(db, &subscription.tenant).await?;
        crate::launch::recheck(db, &subscription.tenant).await?;
        return ci_wake::card_returned(db, &subscription.tenant).await;
    } else if usage::start_grace(db, &subscription.tenant, now_ms()).await? {
        console_log!(
            "tenant {}: subscription {} ended past the free minutes, grace period started",
            subscription.tenant,
            subscription.id
        );
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;

    fn tenant(db: &Db, id: &str, org_id: u64, login: &str) {
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $4)",
            &params![id, org_id, login, NOW],
        );
    }

    /// The values the webhook binds for one delivery.
    fn delivery(
        id: &str,
        tenant: &str,
        status: &str,
        period_end: Option<&str>,
        cancel: bool,
        modified_at: &str,
    ) -> Vec<Param> {
        params![
            id,
            tenant,
            status,
            "cus_1",
            "prod_1",
            period_end,
            cancel,
            modified_at,
            NOW,
        ]
    }

    fn subscription(db: &Db, tenant: &str) -> Option<SubscriptionRow> {
        db.first(SUBSCRIPTION, &params![tenant])
    }

    #[test]
    fn a_delivery_writes_the_subscription() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let changed = db.run(
            UPSERT_SUBSCRIPTION,
            &delivery(
                "sub_1",
                "t1",
                "active",
                Some("2026-11-16T00:00:00Z"),
                true,
                "2026-10-16T10:00:00Z",
            ),
        );
        assert_eq!(changed, 1);
        let row = subscription(&db, "t1").unwrap();
        assert_eq!(row.status, "active");
        assert_eq!(
            row.current_period_end.as_deref(),
            Some("2026-11-16T00:00:00Z")
        );
        assert!(row.cancel_at_period_end);
        assert!(subscription(&db, "t2").is_none());
    }

    #[test]
    fn back_to_ci_reads_the_query() {
        assert!(back_to_ci(Some("return=ci")));
        assert!(back_to_ci(Some("a=1&return=ci")));
        assert!(!back_to_ci(Some("return=settings")));
        assert!(!back_to_ci(None));
    }

    fn subscription_of(id: &str, tenant: &str, status: &str) -> Subscription {
        Subscription {
            id: id.into(),
            tenant: tenant.into(),
            status: status.into(),
            customer_id: "cus_1".into(),
            product_id: "prod_1".into(),
            current_period_end: None,
            cancel_at_period_end: false,
            modified_at: "2026-10-16T10:00:00Z".into(),
        }
    }

    fn held(db: &Db) -> Vec<serde_json::Value> {
        db.rows("SELECT held FROM ci_jobs ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|row| row["held"].clone())
            .collect()
    }

    #[test]
    fn a_licensed_subscription_wakes_jobs_held_for_the_card() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        db.run(
            "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
             private, updated_at) VALUES (7, 't1', 1, 'acme/app', 'main', false, 1)",
            &[],
        );
        db.run(
            "INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, event, \
             ref, sha, actor, status, created_at, updated_at, expires_at) \
             VALUES ('t1', 7, 'ci', 1, 'd1', 'push', 'refs/heads/main', 'abc', 'octocat', \
             'queued', 1, 1, 2)",
            &[],
        );
        for plan in ["a", "b"] {
            db.run(
                "INSERT INTO ci_jobs (run_id, plan_id, name, status, timeout_minutes, held, \
                 created_at, updated_at, template, token_permissions, secret_refs) \
                 SELECT id, $1, $1, 'queued', 10, 'card', 1, 1, 'hatchrun', '{}', '{}' \
                 FROM ci_runs",
                &params![plan],
            );
        }
        let ids: Vec<u64> = db
            .rows("SELECT id FROM ci_jobs ORDER BY id", &[])
            .unwrap()
            .iter()
            .map(|row| row["id"].as_u64().unwrap())
            .collect();
        let canceled = subscription_of("sub_1", "t1", "canceled");
        assert_eq!(db.wait(settle(db.db(), &canceled, 1)).unwrap(), []);
        assert_eq!(held(&db), ["card", "card"]);
        let active = subscription_of("sub_1", "t1", "active");
        let woken = db.wait(settle(db.db(), &active, 1)).unwrap();
        let jobs: Vec<u64> = woken.iter().map(|job| job.job_id).collect();
        assert_eq!(jobs, ids);
        assert_eq!(held(&db), ["concurrency", "concurrency"]);
    }

    #[test]
    fn an_older_delivery_changes_nothing() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let newer = delivery(
            "sub_1",
            "t1",
            "canceled",
            None,
            false,
            "2026-10-16T11:00:00Z",
        );
        let older = delivery("sub_1", "t1", "active", None, false, "2026-10-16T10:00:00Z");
        assert_eq!(db.run(UPSERT_SUBSCRIPTION, &newer), 1);
        assert_eq!(db.run(UPSERT_SUBSCRIPTION, &older), 0);
        let row = subscription(&db, "t1").unwrap();
        assert_eq!(row.status, "canceled");
        assert_eq!(row.current_period_end, None);
        assert!(!row.cancel_at_period_end);
    }

    #[test]
    fn a_delivery_for_no_tenant_changes_nothing() {
        let db = Db::new();
        let changed = db.run(
            UPSERT_SUBSCRIPTION,
            &delivery(
                "sub_1",
                "gone",
                "active",
                None,
                false,
                "2026-10-16T10:00:00Z",
            ),
        );
        assert_eq!(changed, 0);
    }

    #[test]
    fn the_licensed_subscription_comes_first() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        for (id, status, modified_at) in [
            ("sub_1", "active", "2026-10-01T00:00:00Z"),
            ("sub_2", "canceled", "2026-10-02T00:00:00Z"),
        ] {
            db.run(
                UPSERT_SUBSCRIPTION,
                &delivery(id, "t1", status, None, false, modified_at),
            );
        }
        assert_eq!(subscription(&db, "t1").unwrap().status, "active");
        db.run(
            UPSERT_SUBSCRIPTION,
            &delivery(
                "sub_1",
                "t1",
                "revoked",
                None,
                false,
                "2026-10-03T00:00:00Z",
            ),
        );
        assert_eq!(subscription(&db, "t1").unwrap().status, "revoked");
    }

    fn beta(db: &Db, id: &str) -> serde_json::Value {
        db.rows("SELECT beta FROM tenants WHERE id = $1", &params![id])
            .unwrap()[0]["beta"]
            .clone()
    }

    #[test]
    fn a_listed_organization_joins_the_beta_once() {
        let db = Db::new();
        tenant(&db, "t1", 11, "Acme");
        tenant(&db, "t2", 12, "other");
        db.run(
            "INSERT INTO beta_orgs (org_login, created_at) VALUES ($1, $2)",
            &params!["acme", NOW],
        );
        assert_eq!(db.run(JOIN_LISTED_BETA, &params!["t1", NOW]), 1);
        assert_eq!(db.run(JOIN_LISTED_BETA, &params!["t1", NOW]), 0);
        assert_eq!(db.run(JOIN_LISTED_BETA, &params!["t2", NOW]), 0);
        assert_eq!(beta(&db, "t1"), true);
        assert_eq!(beta(&db, "t2"), false);
    }

    #[test]
    fn an_invite_code_runs_out() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let hash = session::hex(&sha256(b"code"));
        db.run(
            "INSERT INTO invites (code_hash, uses_left, created_at) VALUES ($1, $2, $3)",
            &params![hash.as_str(), 1u64, NOW],
        );
        assert_eq!(db.run(USE_INVITE, &params![hash.clone()]), 1);
        assert_eq!(db.run(USE_INVITE, &params![hash]), 0);
        assert_eq!(db.run(USE_INVITE, &params!["unknown"]), 0);
        assert_eq!(db.run(JOIN_BETA, &params!["t1", NOW]), 1);
        assert_eq!(beta(&db, "t1"), true);
    }
}
