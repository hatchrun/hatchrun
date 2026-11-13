// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! An organization's billing: Polar checkout, the customer portal and
//! Polar's webhooks.
//!
//! - `POST /api/tenants/<org>/checkout` opens a Polar checkout, which
//!   adds the card that pays for hatchrun CI.
//! - `POST /api/tenants/<org>/portal` opens Polar's customer portal.
//! - `POST /api/polar/webhook` takes Polar's deliveries.

use serde::Deserialize;
use worker::{Env, Request, Response, Result, console_error, console_warn};

use super::tenants::authorize;
use super::{Answer, Ctx, Failure, get, json_error, need};
use crate::db::{Db, params};
use crate::fetch::send;
use crate::polar::{self, Event, Signed, Subscription};
use crate::{ci_wake, now_ms};

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

#[derive(Deserialize)]
struct SubscriptionRow {
    status: String,
}

/// The tenant's licensed subscription if there is one, else its latest.
const SUBSCRIPTION: &str = "SELECT status \
    FROM subscriptions WHERE tenant_id = $1 \
    ORDER BY status IN ('active', 'trialing') DESC, modified_at DESC LIMIT 1";

/// Puts a tenant in the beta when its organization is in `beta_orgs`,
/// which holds logins in lower case. Binds the tenant and the time in
/// milliseconds.
const JOIN_LISTED_BETA: &str = "UPDATE tenants SET beta = true, updated_at = $2 \
    WHERE id = $1 AND NOT beta \
    AND EXISTS (SELECT 1 FROM beta_orgs WHERE beta_orgs.org_login = lower(tenants.org_login))";

/// Puts the tenant in the beta when the operator listed its
/// organization.
pub(crate) async fn join_listed_beta(db: &Db, id: &str) -> Result<()> {
    db.run(JOIN_LISTED_BETA, &params![id, now_ms()]).await?;
    Ok(())
}

/// Where checkout and the portal send the customer back to.
fn billing_page(ctx: &Ctx<'_>, org: &str) -> String {
    format!("{}/o/{org}/settings/billing", ctx.config.app_url)
}

/// `POST /api/tenants/<org>/checkout`
pub(crate) async fn checkout(ctx: &Ctx<'_>, org: &str) -> Answer {
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
    let page = billing_page(ctx, &row.org_login);
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
    let page = billing_page(ctx, &row.org_login);
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

/// Writes a subscription of an existing tenant, unless the row holds a
/// newer delivery. Binds the subscription id, tenant, status, customer,
/// product, period start, period end or NULL, cancel at period end,
/// Polar's modified time and the time in milliseconds.
const UPSERT_SUBSCRIPTION: &str = "INSERT INTO subscriptions (id, tenant_id, status, \
    customer_id, product_id, current_period_start, current_period_end, \
    cancel_at_period_end, modified_at, updated_at) \
    SELECT $1::text, id, $3::text, $4::text, $5::text, $6::text, $7::text, $8::boolean, \
    $9::text, $10::bigint FROM tenants WHERE id = $2 \
    ON CONFLICT (id) DO UPDATE SET status = excluded.status, \
    customer_id = excluded.customer_id, product_id = excluded.product_id, \
    current_period_start = excluded.current_period_start, \
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
    let product_ids = get(env, PRODUCT_ID)
        .map(|list| polar::product_ids(&list))
        .unwrap_or_default();
    if !product_ids.contains(&subscription.product_id) {
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
                subscription.current_period_start.as_str(),
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

/// What a stored subscription changes. A licensed one returns the
/// tenant's oldest CI jobs held for the card, for the caller to send to
/// the CI launch queue. One that is no longer licensed closes the
/// tenant's open usage window at once so the next send bills it.
async fn settle(db: &Db, subscription: &Subscription, changed: u64) -> Result<Vec<ci_wake::Woken>> {
    if changed == 0 {
        console_warn!(
            "subscription {} for tenant {}: no such tenant, or an older delivery",
            subscription.id,
            subscription.tenant
        );
    } else if subscription.licensed() {
        return ci_wake::card_returned(db, &subscription.tenant).await;
    } else {
        crate::usage::close_windows(db, now_ms(), Some(&subscription.tenant)).await?;
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
        period_start: &str,
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
            period_start,
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
                "2026-10-16T00:00:00Z",
                Some("2026-11-16T00:00:00Z"),
                true,
                "2026-10-16T10:00:00Z",
            ),
        );
        assert_eq!(changed, 1);
        let row = subscription(&db, "t1").unwrap();
        assert_eq!(row.status, "active");
        assert!(subscription(&db, "t2").is_none());
    }

    fn subscription_of(id: &str, tenant: &str, status: &str) -> Subscription {
        Subscription {
            id: id.into(),
            tenant: tenant.into(),
            status: status.into(),
            customer_id: "cus_1".into(),
            product_id: "prod_1".into(),
            current_period_start: "2026-10-16T00:00:00Z".into(),
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

    fn held_jobs(db: &Db) -> Vec<u64> {
        db.run(
            "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
             private, updated_at) VALUES (7, 't1', 1, 'acme/app', 'main', false, 1)",
            &[],
        );
        db.run(
            "INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, event, \
             ref, sha, actor, status, created_at, updated_at, expires_at, trust, author_login, \
             author_association, gate, gate_status) \
             VALUES ('t1', 7, 'ci', 1, 'd1', 'push', 'refs/heads/main', 'abc', 'octocat', \
             'queued', 1, 1, 2, 'trusted', 'octocat', '', 'none', 'none')",
            &[],
        );
        for plan in ["a", "b"] {
            db.run(
                "INSERT INTO ci_jobs (run_id, plan_id, name, status, timeout_minutes, held, \
                 created_at, updated_at, template, token_permissions, secret_refs, needs) \
                 SELECT id, $1, $1, 'queued', 10, 'card', 1, 1, 'hatchrun', '{}', '{}', '[]' \
                 FROM ci_runs",
                &params![plan],
            );
        }
        db.rows("SELECT id FROM ci_jobs ORDER BY id", &[])
            .unwrap()
            .iter()
            .map(|row| row["id"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn a_licensed_subscription_wakes_jobs_held_for_the_card() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let ids = held_jobs(&db);
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
    fn a_licensed_subscription_set_to_cancel_still_wakes_held_jobs() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let ids = held_jobs(&db);
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
             ended_at) VALUES (1, 1, 't1', 'ubuntu-latest', 60, 60, $1)",
            &params![NOW - 1_000],
        );
        let canceling = Subscription {
            cancel_at_period_end: true,
            ..subscription_of("sub_1", "t1", "active")
        };
        let woken = db.wait(settle(db.db(), &canceling, 1)).unwrap();
        let jobs: Vec<u64> = woken.iter().map(|job| job.job_id).collect();
        assert_eq!(jobs, ids);
        assert_eq!(held(&db), ["concurrency", "concurrency"]);
        assert_eq!(
            db.rows("SELECT id FROM ci_usage_windows", &[])
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn an_ending_subscription_closes_the_open_window() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        db.run(
            "INSERT INTO ci_usage (job_id, attempt, tenant_id, template, seconds, units, \
             ended_at) VALUES (1, 1, 't1', 'ubuntu-latest', 60, 60, $1)",
            &params![NOW - 1_000],
        );
        // A licensed subscription sends no usage anywhere.
        let active = subscription_of("sub_1", "t1", "active");
        db.wait(settle(db.db(), &active, 1)).unwrap();
        assert_eq!(
            db.rows("SELECT id FROM ci_usage_windows", &[])
                .unwrap()
                .len(),
            0
        );
        // A canceled one closes the open window at once.
        let canceled = subscription_of("sub_1", "t1", "canceled");
        db.wait(settle(db.db(), &canceled, 1)).unwrap();
        let windows = db
            .rows(
                "SELECT tenant_id, units, seconds, jobs FROM ci_usage_windows",
                &[],
            )
            .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0]["tenant_id"].as_str(), Some("t1"));
        assert_eq!(windows[0]["units"].as_u64(), Some(60));
        assert_eq!(windows[0]["jobs"].as_i64(), Some(1));
        let tagged: u64 = db
            .column(
                "SELECT COUNT(*) AS n FROM ci_usage WHERE window_id IS NULL",
                &[],
                "n",
            )
            .unwrap();
        assert_eq!(tagged, 0);
    }

    #[test]
    fn an_older_delivery_changes_nothing() {
        let db = Db::new();
        tenant(&db, "t1", 11, "acme");
        let newer = delivery(
            "sub_1",
            "t1",
            "canceled",
            "2026-10-16T00:00:00Z",
            None,
            false,
            "2026-10-16T11:00:00Z",
        );
        let older = delivery(
            "sub_1",
            "t1",
            "active",
            "2026-10-01T00:00:00Z",
            None,
            false,
            "2026-10-16T10:00:00Z",
        );
        assert_eq!(db.run(UPSERT_SUBSCRIPTION, &newer), 1);
        assert_eq!(db.run(UPSERT_SUBSCRIPTION, &older), 0);
        let row = subscription(&db, "t1").unwrap();
        assert_eq!(row.status, "canceled");
        let kept = db
            .rows(
                "SELECT current_period_start, current_period_end, cancel_at_period_end \
                 FROM subscriptions",
                &[],
            )
            .unwrap();
        assert_eq!(
            kept[0]["current_period_start"].as_str(),
            Some("2026-10-16T00:00:00Z")
        );
        assert!(kept[0]["current_period_end"].is_null());
        assert_eq!(kept[0]["cancel_at_period_end"], false);
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
                "2026-10-16T00:00:00Z",
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
                &delivery(
                    id,
                    "t1",
                    status,
                    "2026-10-01T00:00:00Z",
                    None,
                    false,
                    modified_at,
                ),
            );
        }
        assert_eq!(subscription(&db, "t1").unwrap().status, "active");
        db.run(
            UPSERT_SUBSCRIPTION,
            &delivery(
                "sub_1",
                "t1",
                "revoked",
                "2026-10-01T00:00:00Z",
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
}
