// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Billing through Polar, the merchant of record, kept apart from the
//! Workers runtime so it tests natively.
//!
//! An organization pays per job minute through a Polar subscription to
//! the metered product: a metered price on the job minutes meter, with
//! no cap, and a meter credit benefit for the free minutes (see
//! [`crate::usage`]). The reconciler sends each job's minutes as an
//! event (see [`ingest`]). The app opens a Polar checkout for the
//! product, with the tenant id as Polar's
//! external customer id and in the checkout's metadata, so every
//! subscription Polar reports names its tenant. Polar's webhooks, signed
//! as Standard Webhooks, keep the `subscriptions` table current; the
//! customer portal, reached through a customer session, is where the
//! customer changes payment details or cancels.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::http::{Request, Response};

/// Polar's API.
pub const API: &str = "https://api.polar.sh";
/// Seconds a webhook's timestamp may be off from the Worker's clock,
/// the Standard Webhooks default.
pub const TOLERANCE: u64 = 5 * 60;
/// Subscription statuses that hold a licence. A subscription cancelled
/// at the end of its period stays `active` until then.
pub const LICENSED: [&str; 2] = ["active", "trialing"];

fn call(method: &'static str, url: String, token: &str) -> Request {
    Request::new(method, url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "hatchrun.io")
}

/// The licence's product ids in `list`, separated by commas.
#[must_use]
pub fn product_ids(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Asks for the product `id`.
#[must_use]
pub fn product(api: &str, token: &str, id: &str) -> Request {
    call(
        "GET",
        format!("{api}/v1/products/{}", crate::http::encode(id)),
        token,
    )
}

/// One of the licence's products, as the app shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Polar's product id.
    pub id: String,
    /// The product's name.
    pub name: String,
    /// The product's description.
    pub description: Option<String>,
    /// The fixed price in the smallest currency unit, such as cents.
    pub amount: Option<u64>,
    /// The price's currency, such as `usd`.
    pub currency: Option<String>,
    /// `month`, `year`, ...
    pub interval: Option<String>,
    /// The metered price per unit in the smallest currency unit, as
    /// Polar writes it: a decimal string such as `"0.1"`.
    pub unit_amount: Option<String>,
    /// The most the metered price charges per period, in the smallest
    /// currency unit.
    pub cap_amount: Option<u64>,
}

#[derive(Deserialize)]
struct Product {
    id: String,
    name: String,
    description: Option<String>,
    recurring_interval: Option<String>,
    #[serde(default)]
    prices: Vec<Price>,
}

#[derive(Deserialize)]
struct Price {
    amount_type: Option<String>,
    #[serde(rename = "price_amount")]
    amount: Option<u64>,
    #[serde(rename = "price_currency")]
    currency: Option<String>,
    recurring_interval: Option<String>,
    unit_amount: Option<serde_json::Value>,
    cap_amount: Option<u64>,
}

/// The plan of the product Polar answered with.
///
/// # Errors
///
/// Polar's answer when it is not a product.
pub fn plan(response: &Response) -> Result<Plan, String> {
    let product: Product = decode(response, "product")?;
    let mut prices = product.prices;
    let metered = prices
        .iter()
        .position(|price| price.amount_type.as_deref() == Some("metered_unit"))
        .map(|at| prices.swap_remove(at));
    let fixed = prices
        .into_iter()
        .find(|price| price.amount_type.as_deref() == Some("fixed"));
    let (amount, mut currency, price_interval) = fixed.map_or((None, None, None), |price| {
        (price.amount, price.currency, price.recurring_interval)
    });
    let (unit_amount, cap_amount) = metered.map_or((None, None), |price| {
        if currency.is_none() {
            currency = price.currency;
        }
        let unit = price.unit_amount.and_then(|value| match value {
            serde_json::Value::String(text) => Some(text),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        });
        (unit, price.cap_amount)
    });
    Ok(Plan {
        id: product.id,
        name: product.name,
        description: product.description,
        amount,
        currency,
        interval: product.recurring_interval.or(price_interval),
        unit_amount,
        cap_amount,
    })
}

/// Orders plans by billing interval, shortest first, so monthly comes
/// before yearly. Plans with no interval go last.
pub fn sort(plans: &mut [Plan]) {
    let rank = |plan: &Plan| match plan.interval.as_deref() {
        Some("day") => 0,
        Some("week") => 1,
        Some("month") => 2,
        Some("year") => 3,
        _ => 4,
    };
    plans.sort_by_key(rank);
}

/// A checkout for the licence of `tenant`, the GitHub organization
/// `org`, that offers every product in `product_ids`. Polar sends the
/// buyer to `success_url` afterwards.
#[must_use]
pub fn checkout(
    api: &str,
    token: &str,
    product_ids: &[String],
    tenant: &str,
    org: &str,
    success_url: &str,
    return_url: &str,
) -> Request {
    call("POST", format!("{api}/v1/checkouts/"), token).body(
        serde_json::json!({
            "products": product_ids,
            "external_customer_id": tenant,
            "metadata": { "tenant_id": tenant, "github_org": org },
            "customer_metadata": { "github_org": org },
            "success_url": success_url,
            "return_url": return_url,
        })
        .to_string(),
    )
}

/// A customer portal session for the customer of `tenant`.
#[must_use]
pub fn customer_session(api: &str, token: &str, tenant: &str, return_url: &str) -> Request {
    call("POST", format!("{api}/v1/customer-sessions/"), token).body(
        serde_json::json!({
            "external_customer_id": tenant,
            "return_url": return_url,
        })
        .to_string(),
    )
}

#[derive(Deserialize)]
struct CheckoutAnswer {
    url: String,
}

#[derive(Deserialize)]
struct SessionAnswer {
    customer_portal_url: String,
}

fn decode<T: for<'de> Deserialize<'de>>(response: &Response, what: &str) -> Result<T, String> {
    if !response.ok() {
        return Err(format!(
            "{what}: Polar answered {}: {}",
            response.status,
            response.text()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|err| format!("{what}: {err}"))
}

/// Name of the events that carry a job's minutes. The job minutes
/// meter sums their `minutes`.
pub const MINUTES_EVENT: &str = "job.minutes";

/// Most events in one ingest call.
pub const INGEST_BATCH: usize = 500;

/// One job's minutes, as Polar's meter takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minutes<'a> {
    /// The job, which keeps Polar from counting it twice.
    pub job_id: u64,
    /// The tenant, Polar's external customer id.
    pub tenant: &'a str,
    /// Its minutes.
    pub minutes: u32,
    /// When it completed, ISO 8601.
    pub at: String,
}

/// Sends job minutes to Polar. Each event's `external_id` is the job,
/// so an event sent again after a lost answer is skipped as a
/// duplicate.
#[must_use]
pub fn ingest(api: &str, token: &str, events: &[Minutes<'_>]) -> Request {
    let events: Vec<_> = events
        .iter()
        .map(|event| {
            serde_json::json!({
                "name": MINUTES_EVENT,
                "external_customer_id": event.tenant,
                "external_id": format!("job:{}", event.job_id),
                "timestamp": event.at,
                "metadata": { "minutes": event.minutes, "job_id": event.job_id.to_string() },
            })
        })
        .collect();
    call("POST", format!("{api}/v1/events/ingest"), token)
        .body(serde_json::json!({ "events": events }).to_string())
}

#[derive(Deserialize)]
struct Ingested {
    inserted: u32,
    #[serde(default)]
    duplicates: u32,
}

/// How many events Polar took, new and duplicate.
///
/// # Errors
///
/// Polar's answer when it did not take the events.
pub fn ingested(response: &Response) -> Result<u32, String> {
    decode::<Ingested>(response, "events").map(|answer| answer.inserted + answer.duplicates)
}

/// The URL of the checkout Polar made.
///
/// # Errors
///
/// Polar's answer when it is not a checkout.
pub fn checkout_url(response: &Response) -> Result<String, String> {
    decode::<CheckoutAnswer>(response, "checkout").map(|answer| answer.url)
}

/// The URL of the customer portal session Polar made.
///
/// # Errors
///
/// Polar's answer when it is not a customer session.
pub fn portal_url(response: &Response) -> Result<String, String> {
    decode::<SessionAnswer>(response, "customer session").map(|answer| answer.customer_portal_url)
}

/// The keys a webhook secret may sign with. Standard Webhooks secrets
/// are `whsec_` and base64 of the key; Polar signed with the UTF-8
/// bytes of the whole string until 8 September 2026, as endpoints made
/// before then may still do. Both are tried, as Polar's SDKs do.
fn keys(secret: &str) -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    if let Some(encoded) = secret.strip_prefix("whsec_")
        && let Ok(key) = STANDARD.decode(encoded)
    {
        keys.push(key);
    }
    keys.push(secret.as_bytes().to_vec());
    keys
}

/// The Standard Webhooks headers of a delivery.
#[derive(Debug, Clone, Copy)]
pub struct Signed<'a> {
    /// `webhook-id`.
    pub id: &'a str,
    /// `webhook-timestamp`, Unix seconds.
    pub timestamp: &'a str,
    /// `webhook-signature`: `v1,<base64>` entries separated by spaces.
    pub signature: &'a str,
}

/// Checks a delivery: one of its `v1` signatures is the HMAC-SHA256 of
/// `id.timestamp.body` under the secret, and the timestamp is within
/// [`TOLERANCE`] of `now` (Unix seconds), so an old delivery cannot be
/// replayed. Comparisons are constant time.
#[must_use]
pub fn verify(secret: &str, signed: Signed<'_>, body: &[u8], now: u64) -> bool {
    if secret.is_empty() || signed.id.is_empty() {
        return false;
    }
    let Ok(timestamp) = signed.timestamp.parse::<u64>() else {
        return false;
    };
    if timestamp.abs_diff(now) > TOLERANCE {
        return false;
    }
    let given: Vec<Vec<u8>> = signed
        .signature
        .split(' ')
        .filter_map(|entry| entry.strip_prefix("v1,"))
        .filter_map(|encoded| STANDARD.decode(encoded).ok())
        .collect();
    keys(secret).iter().any(|key| {
        given.iter().any(|signature| {
            let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
                return false;
            };
            mac.update(signed.id.as_bytes());
            mac.update(b".");
            mac.update(signed.timestamp.as_bytes());
            mac.update(b".");
            mac.update(body);
            mac.verify_slice(signature).is_ok()
        })
    })
}

/// A subscription as a webhook reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Polar's subscription id.
    pub id: String,
    /// The tenant it pays for.
    pub tenant: String,
    /// `active`, `trialing`, `past_due`, `canceled`, ...
    pub status: String,
    /// Polar's customer id.
    pub customer_id: String,
    /// The product subscribed to.
    pub product_id: String,
    /// When the current period ends, ISO 8601.
    pub current_period_end: Option<String>,
    /// Whether it ends with the current period.
    pub cancel_at_period_end: bool,
    /// When Polar last changed it, ISO 8601, which orders deliveries
    /// that arrive out of order.
    pub modified_at: String,
}

impl Subscription {
    /// Whether it holds a licence.
    #[must_use]
    pub fn licensed(&self) -> bool {
        LICENSED.contains(&self.status.as_str())
    }
}

/// What a webhook means for the Worker.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// A subscription changed.
    Subscription(Subscription),
    /// Anything else, and why it is ignored.
    Ignored(String),
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct Data {
    id: String,
    status: String,
    customer_id: String,
    product_id: String,
    current_period_end: Option<String>,
    #[serde(default)]
    cancel_at_period_end: bool,
    created_at: String,
    modified_at: Option<String>,
    #[serde(default)]
    metadata: serde_json::Map<String, serde_json::Value>,
    customer: Option<Customer>,
}

#[derive(Deserialize)]
struct Customer {
    external_id: Option<String>,
}

/// Reads a verified webhook body.
///
/// # Errors
///
/// When a subscription event does not parse.
pub fn parse(body: &[u8]) -> Result<Event, serde_json::Error> {
    let payload: Payload = serde_json::from_slice(body)?;
    if !payload.kind.starts_with("subscription.") {
        return Ok(Event::Ignored(format!("{} is not used", payload.kind)));
    }
    let data: Data = serde_json::from_value(payload.data)?;
    let tenant = data
        .metadata
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| data.customer.and_then(|customer| customer.external_id));
    let Some(tenant) = tenant else {
        return Ok(Event::Ignored(format!(
            "subscription {} names no tenant",
            data.id
        )));
    };
    Ok(Event::Subscription(Subscription {
        id: data.id,
        tenant,
        status: data.status,
        customer_id: data.customer_id,
        product_id: data.product_id,
        current_period_end: data.current_period_end,
        cancel_at_period_end: data.cancel_at_period_end,
        modified_at: data.modified_at.unwrap_or(data.created_at),
    }))
}

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher, RandomState};

    use super::*;

    /// A fresh `whsec_` secret per run.
    fn secret() -> (String, Vec<u8>) {
        let state = RandomState::new();
        let key: Vec<u8> = (0..24)
            .map(|at| state.hash_one(at).to_le_bytes()[0])
            .collect();
        (format!("whsec_{}", STANDARD.encode(&key)), key)
    }

    fn sign(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).unwrap();
        mac.update(format!("{id}.{timestamp}.").as_bytes());
        mac.update(body);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }

    fn signed(signature: &str) -> Signed<'_> {
        Signed {
            id: "msg_1",
            timestamp: "1700000000",
            signature,
        }
    }

    #[test]
    fn verifies_standard_webhooks() {
        let (secret, key) = secret();
        let body = br#"{"type":"subscription.updated"}"#;
        let good = sign(&key, "msg_1", "1700000000", body);
        assert!(verify(&secret, signed(&good), body, 1_700_000_100));
        let several = format!("v1,bm9wZQ== {good}");
        assert!(verify(&secret, signed(&several), body, 1_700_000_100));
        // Replayed later, tampered with, or signed for another message.
        assert!(!verify(
            &secret,
            signed(&good),
            body,
            1_700_000_000 + TOLERANCE + 1
        ));
        assert!(!verify(&secret, signed(&good), b"{}", 1_700_000_000));
        let other = Signed {
            id: "msg_2",
            ..signed(&good)
        };
        assert!(!verify(&secret, other, body, 1_700_000_000));
        assert!(!verify("", signed(&good), body, 1_700_000_000));
        assert!(!verify(&secret, signed("v1,"), body, 1_700_000_000));
    }

    #[test]
    fn accepts_polars_earlier_key() {
        let (secret, _) = secret();
        let body = b"{}";
        let signature = sign(secret.as_bytes(), "m", "10", body);
        assert!(verify(
            &secret,
            Signed {
                id: "m",
                timestamp: "10",
                signature: &signature,
            },
            body,
            10
        ));
    }

    #[test]
    fn reads_subscriptions() {
        let body = br#"{"type":"subscription.updated","timestamp":"2026-10-14T10:00:00Z","data":{
            "id":"sub_1","status":"active","customer_id":"cus_1","product_id":"prod_1",
            "current_period_end":"2026-11-14T10:00:00Z","cancel_at_period_end":true,
            "created_at":"2026-10-14T09:00:00Z","modified_at":"2026-10-14T10:00:00Z",
            "metadata":{"tenant_id":"abcdefgh12345678"},"customer":{"external_id":"other"}}}"#;
        let Event::Subscription(subscription) = parse(body).unwrap() else {
            panic!("not a subscription");
        };
        assert_eq!(
            subscription,
            Subscription {
                id: "sub_1".to_owned(),
                tenant: "abcdefgh12345678".to_owned(),
                status: "active".to_owned(),
                customer_id: "cus_1".to_owned(),
                product_id: "prod_1".to_owned(),
                current_period_end: Some("2026-11-14T10:00:00Z".to_owned()),
                cancel_at_period_end: true,
                modified_at: "2026-10-14T10:00:00Z".to_owned(),
            }
        );
        assert!(subscription.licensed());

        let by_customer =
            br#"{"type":"subscription.revoked","data":{"id":"sub_1","status":"canceled",
            "customer_id":"cus_1","product_id":"prod_1","current_period_end":null,
            "created_at":"2026-10-14T09:00:00Z","modified_at":null,"metadata":{},
            "customer":{"external_id":"abcdefgh12345678"}}}"#;
        let Event::Subscription(revoked) = parse(by_customer).unwrap() else {
            panic!("not a subscription");
        };
        assert_eq!(revoked.tenant, "abcdefgh12345678");
        assert_eq!(revoked.modified_at, "2026-10-14T09:00:00Z");
        assert!(!revoked.licensed());

        assert_eq!(
            parse(br#"{"type":"order.paid","data":{}}"#).unwrap(),
            Event::Ignored("order.paid is not used".to_owned())
        );
        let orphan = br#"{"type":"subscription.created","data":{"id":"sub_2","status":"incomplete",
            "customer_id":"c","product_id":"p","created_at":"t","metadata":{}}}"#;
        assert_eq!(
            parse(orphan).unwrap(),
            Event::Ignored("subscription sub_2 names no tenant".to_owned())
        );
    }

    #[test]
    fn builds_checkouts_and_portal_sessions() {
        let request = checkout(
            API,
            "polar_oat",
            &["prod_month".to_owned(), "prod_year".to_owned()],
            "abcdefgh12345678",
            "acme",
            "https://app/o/acme?checkout={CHECKOUT_ID}",
            "https://app/o/acme",
        );
        assert_eq!(request.url, "https://api.polar.sh/v1/checkouts/");
        assert_eq!(
            request.header_value("authorization"),
            Some("Bearer polar_oat")
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["products"],
            serde_json::json!(["prod_month", "prod_year"])
        );
        assert_eq!(body["external_customer_id"], "abcdefgh12345678");
        assert_eq!(body["metadata"]["tenant_id"], "abcdefgh12345678");
        assert_eq!(
            checkout_url(&Response {
                headers: Vec::new(),
                status: 201,
                body: br#"{"id":"c","url":"https://buy.polar.sh/c"}"#.to_vec(),
            })
            .unwrap(),
            "https://buy.polar.sh/c"
        );

        let request = customer_session(API, "t", "abcdefgh12345678", "https://app/o/acme");
        assert_eq!(request.url, "https://api.polar.sh/v1/customer-sessions/");
        assert_eq!(
            portal_url(&Response {
                headers: Vec::new(),
                status: 201,
                body: br#"{"token":"x","customer_portal_url":"https://polar.sh/portal"}"#.to_vec(),
            })
            .unwrap(),
            "https://polar.sh/portal"
        );
        assert_eq!(
            portal_url(&Response {
                headers: Vec::new(),
                status: 404,
                body: b"{}".to_vec(),
            })
            .unwrap_err(),
            "customer session: Polar answered 404: {}"
        );
    }

    #[test]
    fn sends_job_minutes() {
        let request = ingest(
            API,
            "t",
            &[Minutes {
                job_id: 29_679_449,
                tenant: "abcdefgh12345678",
                minutes: 10,
                at: "2026-10-14T10:09:30Z".to_owned(),
            }],
        );
        assert_eq!(request.url, "https://api.polar.sh/v1/events/ingest");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "events": [{
                "name": "job.minutes",
                "external_customer_id": "abcdefgh12345678",
                "external_id": "job:29679449",
                "timestamp": "2026-10-14T10:09:30Z",
                "metadata": { "minutes": 10, "job_id": "29679449" },
            }] })
        );
        assert_eq!(
            ingested(&Response {
                headers: Vec::new(),
                status: 200,
                body: br#"{"inserted":2,"duplicates":1}"#.to_vec(),
            }),
            Ok(3)
        );
        assert!(
            ingested(&Response {
                headers: Vec::new(),
                status: 422,
                body: br#"{"detail":[]}"#.to_vec(),
            })
            .is_err()
        );
    }

    #[test]
    fn reads_product_ids() {
        assert_eq!(product_ids(""), Vec::<String>::new());
        assert_eq!(product_ids("prod_1"), ["prod_1"]);
        assert_eq!(
            product_ids(" prod_month , prod_year,"),
            ["prod_month", "prod_year"]
        );
    }

    #[test]
    fn reads_plans() {
        let request = product(API, "t", "prod_1");
        assert_eq!(request.method, "GET");
        assert_eq!(request.url, "https://api.polar.sh/v1/products/prod_1");

        let yearly = plan(&Response {
            headers: Vec::new(),
            status: 200,
            body: br#"{"id":"prod_year","name":"hatchrun yearly","description":null,
                "recurring_interval":"year","prices":[
                {"amount_type":"free"},
                {"amount_type":"fixed","price_amount":49000,"price_currency":"usd",
                 "recurring_interval":"year"}]}"#
                .to_vec(),
        })
        .unwrap();
        assert_eq!(
            yearly,
            Plan {
                id: "prod_year".to_owned(),
                name: "hatchrun yearly".to_owned(),
                description: None,
                amount: Some(49000),
                currency: Some("usd".to_owned()),
                interval: Some("year".to_owned()),
                unit_amount: None,
                cap_amount: None,
            }
        );
        // The interval may be on the price only, and a product may
        // have no fixed price.
        let monthly = plan(&Response {
            headers: Vec::new(),
            status: 200,
            body: br#"{"id":"prod_month","name":"hatchrun monthly","description":"d",
                "prices":[{"amount_type":"fixed","price_amount":4900,
                "price_currency":"usd","recurring_interval":"month"}]}"#
                .to_vec(),
        })
        .unwrap();
        assert_eq!(monthly.interval.as_deref(), Some("month"));
        let custom = plan(&Response {
            headers: Vec::new(),
            status: 200,
            body: br#"{"id":"prod_c","name":"c","description":null,"prices":[]}"#.to_vec(),
        })
        .unwrap();
        assert_eq!((custom.amount, custom.interval.as_deref()), (None, None));
        assert_eq!(
            plan(&Response {
                headers: Vec::new(),
                status: 404,
                body: b"{}".to_vec(),
            })
            .unwrap_err(),
            "product: Polar answered 404: {}"
        );

        assert_eq!(
            (yearly.unit_amount.as_deref(), yearly.cap_amount),
            (None, None)
        );
        let metered = plan(&Response {
            headers: Vec::new(),
            status: 200,
            body: br#"{"id":"prod_m","name":"hatchrun","description":null,
                "recurring_interval":"month","prices":[
                {"amount_type":"metered_unit","price_currency":"usd","unit_amount":"0.1",
                 "cap_amount":19900,"meter_id":"m"}]}"#
                .to_vec(),
        })
        .unwrap();
        assert_eq!(
            (
                metered.amount,
                metered.currency.as_deref(),
                metered.unit_amount.as_deref(),
                metered.cap_amount
            ),
            (None, Some("usd"), Some("0.1"), Some(19900))
        );

        let mut plans = vec![custom, yearly, monthly];
        sort(&mut plans);
        let ids: Vec<&str> = plans.iter().map(|plan| plan.id.as_str()).collect();
        assert_eq!(ids, ["prod_month", "prod_year", "prod_c"]);
    }
}
