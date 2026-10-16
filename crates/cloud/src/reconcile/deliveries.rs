// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Webhook deliveries that failed, and which of them to redeliver.
//!
//! GitHub does not retry a webhook it could not deliver; the App lists
//! its recent deliveries, newest first, and may ask for any of them
//! again. A delivery is worth asking for again when the Worker did not
//! answer, answered 408 or 429, or failed with a 5xx; a 4xx means the
//! payload or signature was refused and would be refused again.
//!
//! Every attempt at one event shares its `guid`. An event is
//! redelivered at most once: not when any attempt at it succeeded, not
//! when an attempt was already a redelivery, and not when the Worker
//! remembers asking for it (the `redeliveries` table).

use std::collections::HashMap;

use serde::Deserialize;

use super::time;
use crate::github;
use crate::http::{Request, Response};

/// Deliveries in one page, GitHub's most.
pub const PAGE_SIZE: u32 = 100;

/// One attempt at delivering an event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Delivery {
    /// The attempt's id, which a redelivery names.
    pub id: u64,
    /// The event's id, shared by every attempt at it.
    pub guid: String,
    /// When GitHub made the attempt, RFC 3339.
    pub delivered_at: String,
    /// Whether the attempt was a redelivery.
    pub redelivery: bool,
    /// The Worker's answer, 0 when there was none.
    pub status_code: u16,
    /// `X-GitHub-Event`, such as `workflow_job`.
    pub event: String,
}

impl Delivery {
    /// When GitHub made the attempt, in Unix milliseconds.
    #[must_use]
    pub fn at(&self) -> Option<u64> {
        time::parse(&self.delivered_at)
    }
}

/// The first page of the App's deliveries.
#[must_use]
pub fn first_page(api: &str) -> String {
    format!("{api}/app/hook/deliveries?per_page={PAGE_SIZE}")
}

/// Lists a page of the App's deliveries, with the App's JWT.
#[must_use]
pub fn list(url: &str, app_jwt: &str) -> Request {
    github::call("GET", url.to_owned(), app_jwt)
}

/// Asks GitHub to deliver the event of attempt `id` again.
#[must_use]
pub fn redeliver(api: &str, app_jwt: &str, id: u64) -> Request {
    github::call(
        "POST",
        format!("{api}/app/hook/deliveries/{id}/attempts"),
        app_jwt,
    )
}

/// Reads a page of deliveries.
///
/// # Errors
///
/// GitHub's answer when it is not a list of deliveries.
pub fn parse(response: &Response) -> Result<Vec<Delivery>, String> {
    if !response.ok() {
        return Err(format!(
            "deliveries: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|err| format!("deliveries: {err}"))
}

/// The next page's URL from a `Link` header.
#[must_use]
pub fn next_page(link: Option<&str>) -> Option<String> {
    link?.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|param| param.trim() == "rel=\"next\"")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

/// Whether an attempt that got `status_code` may succeed next time.
#[must_use]
pub fn retryable(status_code: u16) -> bool {
    matches!(status_code, 0 | 408 | 429 | 500..=599)
}

/// An event to deliver again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redelivery {
    /// The event's id.
    pub guid: String,
    /// The attempt to name in the request: the latest.
    pub id: u64,
    /// `X-GitHub-Event`.
    pub event: String,
}

/// The events among `deliveries` to deliver again: first attempted at
/// or after `since` (Unix milliseconds), never delivered, never
/// redelivered, and last refused in a way that may pass next time.
/// Oldest first, so events reach the Worker in the order they happened.
#[must_use]
pub fn choose(deliveries: &[Delivery], since: u64) -> Vec<Redelivery> {
    let mut events: HashMap<&str, Vec<&Delivery>> = HashMap::new();
    for delivery in deliveries {
        events.entry(&delivery.guid).or_default().push(delivery);
    }
    let mut chosen: Vec<(u64, Redelivery)> = events
        .into_values()
        .filter_map(|attempts| {
            let first = attempts.iter().filter_map(|attempt| attempt.at()).min()?;
            let latest = attempts
                .iter()
                .max_by_key(|attempt| (attempt.at().unwrap_or(0), attempt.id))?;
            let settled = attempts
                .iter()
                .any(|attempt| attempt.redelivery || (200..300).contains(&attempt.status_code));
            (first >= since && !settled && retryable(latest.status_code)).then(|| {
                (
                    first,
                    Redelivery {
                        guid: latest.guid.clone(),
                        id: latest.id,
                        event: latest.event.clone(),
                    },
                )
            })
        })
        .collect();
    chosen.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    chosen
        .into_iter()
        .map(|(_, redelivery)| redelivery)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(id: u64, guid: &str, at: &str, status_code: u16, redelivery: bool) -> Delivery {
        Delivery {
            id,
            guid: guid.to_owned(),
            delivered_at: at.to_owned(),
            redelivery,
            status_code,
            event: "workflow_job".to_owned(),
        }
    }

    #[test]
    fn reads_a_page() {
        let answer = Response {
            status: 200,
            body: br#"[{"id":12345678,"guid":"0b989ba4","delivered_at":"2026-10-14T10:00:00Z","redelivery":false,"duration":0.27,"status":"Invalid HTTP Response: 503","status_code":503,"event":"workflow_job","action":"queued","installation_id":7,"repository_id":9,"throttled_at":null}]"#.to_vec(),
        };
        assert_eq!(
            parse(&answer).unwrap(),
            [attempt(
                12_345_678,
                "0b989ba4",
                "2026-10-14T10:00:00Z",
                503,
                false
            )]
        );
        let denied = Response {
            status: 401,
            body: br#"{"message":"A JSON web token could not be decoded"}"#.to_vec(),
        };
        assert!(parse(&denied).unwrap_err().contains("401"));
    }

    #[test]
    fn builds_the_requests() {
        assert_eq!(
            first_page("https://api.github.com"),
            "https://api.github.com/app/hook/deliveries?per_page=100"
        );
        let request = redeliver("https://api.github.com", "jwt", 42);
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://api.github.com/app/hook/deliveries/42/attempts"
        );
        assert_eq!(request.header_value("authorization"), Some("Bearer jwt"));
    }

    #[test]
    fn follows_the_next_link() {
        let link = "<https://api.github.com/app/hook/deliveries?per_page=100&cursor=v1_9>; rel=\"next\", \
                    <https://api.github.com/app/hook/deliveries?per_page=100>; rel=\"first\"";
        assert_eq!(
            next_page(Some(link)).as_deref(),
            Some("https://api.github.com/app/hook/deliveries?per_page=100&cursor=v1_9")
        );
        assert_eq!(next_page(Some("<https://x/?page=1>; rel=\"first\"")), None);
        assert_eq!(next_page(None), None);
    }

    #[test]
    fn retries_what_may_pass() {
        for code in [0, 408, 429, 500, 502, 503, 504] {
            assert!(retryable(code), "{code}");
        }
        for code in [200, 202, 400, 401, 404, 422] {
            assert!(!retryable(code), "{code}");
        }
    }

    #[test]
    fn redelivers_each_failed_event_once() {
        let since = time::parse("2026-10-14T10:00:00Z").unwrap();
        let deliveries = [
            // Failed, never retried: redelivered.
            attempt(5, "late", "2026-10-14T10:05:00Z", 0, false),
            attempt(3, "failed", "2026-10-14T10:01:00Z", 502, false),
            // Delivered after all: settled.
            attempt(6, "ok-later", "2026-10-14T10:06:00Z", 200, true),
            attempt(4, "ok-later", "2026-10-14T10:02:00Z", 500, false),
            // Already redelivered once, and that failed too: left alone.
            attempt(8, "twice", "2026-10-14T10:08:00Z", 503, true),
            attempt(7, "twice", "2026-10-14T10:03:00Z", 503, false),
            // Refused for good.
            attempt(9, "bad-signature", "2026-10-14T10:04:00Z", 401, false),
            // Delivered.
            attempt(10, "fine", "2026-10-14T10:04:00Z", 202, false),
            // Before the window.
            attempt(2, "old", "2026-10-14T09:59:59Z", 503, false),
            // Unreadable time.
            attempt(1, "odd", "yesterday", 503, false),
        ];
        let chosen: Vec<(String, u64)> = choose(&deliveries, since)
            .into_iter()
            .map(|redelivery| (redelivery.guid, redelivery.id))
            .collect();
        assert_eq!(chosen, [("failed".to_owned(), 3), ("late".to_owned(), 5)]);
    }
}
