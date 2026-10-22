// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Billing through Polar, the merchant of record, kept apart from the
//! Workers runtime so it tests natively.
//!
//! An organization pays per second of runner time through a Polar
//! subscription to the metered product: a metered price on the "Runner
//! seconds" meter, with no cap and no free credit (see
//! [`crate::usage`]). The reconciler sends each job's billed seconds as
//! an event (see [`ingest`]). The app opens a Polar checkout for the
//! product, with the tenant id as Polar's
//! external customer id and in the checkout's metadata, so every
//! subscription Polar reports names its tenant. Polar's webhooks, signed
//! as Standard Webhooks, keep the `subscriptions` table current; the
//! customer portal, reached through a customer session, is where the
//! customer changes payment details or cancels.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
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

/// Name of the events that carry a job's billed seconds. The "Runner
/// seconds" meter sums their `seconds`.
pub const SECONDS_EVENT: &str = "job.seconds";

/// Most events in one ingest call.
pub const INGEST_BATCH: usize = 500;

/// One job's billed seconds, as Polar's meter takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSeconds<'a> {
    /// The job, which keeps Polar from counting it twice.
    pub job_id: u64,
    /// The tenant, Polar's external customer id.
    pub tenant: &'a str,
    /// Its billed seconds.
    pub seconds: u64,
    /// When it completed, ISO 8601.
    pub at: String,
}

/// Sends job seconds to Polar. Each event's `external_id` is the job,
/// so an event sent again after a lost answer is skipped as a
/// duplicate.
#[must_use]
pub fn ingest(api: &str, token: &str, events: &[JobSeconds<'_>]) -> Request {
    let events: Vec<_> = events
        .iter()
        .map(|event| {
            serde_json::json!({
                "name": SECONDS_EVENT,
                "external_customer_id": event.tenant,
                "external_id": format!("job:{}", event.job_id),
                "timestamp": event.at,
                "metadata": { "seconds": event.seconds, "job_id": event.job_id.to_string() },
            })
        })
        .collect();
    call("POST", format!("{api}/v1/events/ingest"), token)
        .body(serde_json::json!({ "events": events }).to_string())
}

/// Name of the events that carry a CI job attempt's billing units. The
/// CI usage meter sums their `units`.
pub const CI_USAGE_EVENT: &str = "ci.usage";

/// Most CI usage events in one ingest call. Polar documents no limit,
/// so this stays small: a batch with one refused event splits down to
/// it in about 2 * log2(100) calls.
pub const CI_INGEST_BATCH: usize = 100;

/// One CI job attempt's usage, as Polar's meter takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiUsage<'a> {
    /// The job.
    pub job_id: u64,
    /// The attempt, so a re-run counts apart from the first run.
    pub attempt: u32,
    /// The tenant, Polar's external customer id.
    pub tenant: &'a str,
    /// The runner template's label.
    pub template: &'a str,
    /// The billed seconds of the attempt, at least 60.
    pub seconds: u64,
    /// The billing units: the seconds weighted by the template, one
    /// unit per 2 vCPU second.
    pub units: u64,
    /// When it completed, ISO 8601.
    pub at: String,
}

/// Sends CI usage to Polar. Each event's `external_id` names the job and
/// the attempt, so an event sent again after a lost answer is skipped
/// as a duplicate and never counted twice.
#[must_use]
pub fn ci_ingest(api: &str, token: &str, events: &[CiUsage<'_>]) -> Request {
    let events: Vec<_> = events
        .iter()
        .map(|event| {
            serde_json::json!({
                "name": CI_USAGE_EVENT,
                "external_customer_id": event.tenant,
                "external_id": format!("ci:{}:{}", event.job_id, event.attempt),
                "timestamp": event.at,
                "metadata": {
                    "units": event.units,
                    "seconds": event.seconds,
                    "template": event.template,
                },
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
    fn sends_job_seconds() {
        let request = ingest(
            API,
            "t",
            &[JobSeconds {
                job_id: 29_679_449,
                tenant: "abcdefgh12345678",
                seconds: 600,
                at: "2026-10-14T10:09:30Z".to_owned(),
            }],
        );
        assert_eq!(request.url, "https://api.polar.sh/v1/events/ingest");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "events": [{
                "name": "job.seconds",
                "external_customer_id": "abcdefgh12345678",
                "external_id": "job:29679449",
                "timestamp": "2026-10-14T10:09:30Z",
                "metadata": { "seconds": 600, "job_id": "29679449" },
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
    fn ci_ingest_names_job_and_attempt() {
        let request = ci_ingest(
            API,
            "t",
            &[CiUsage {
                job_id: 7,
                attempt: 2,
                tenant: "abcdefgh12345678",
                template: "ubuntu-26.04-8core",
                seconds: 90,
                units: 360,
                at: "2026-10-14T10:09:30Z".to_owned(),
            }],
        );
        assert_eq!(request.url, "https://api.polar.sh/v1/events/ingest");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "events": [{
                "name": "ci.usage",
                "external_customer_id": "abcdefgh12345678",
                "external_id": "ci:7:2",
                "timestamp": "2026-10-14T10:09:30Z",
                "metadata": { "units": 360, "seconds": 90, "template": "ubuntu-26.04-8core" },
            }] })
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
}
