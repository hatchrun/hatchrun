// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What the list routes of `/api/ci/` share: the query string, the
//! opaque cursor and the page answer.
//!
//! A list answers `{"items": [..], "total": N, "next": cursor | null}`.
//! `total` counts the items the filters hold, not those on the page.
//! `next` is passed back as `?cursor=` for the next page; callers do
//! not read it. A cursor that does not decode is a `400`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{Stop, stop};

/// Items in a page when the caller names no `limit`.
pub const DEFAULT_LIMIT: u32 = 50;
/// The most items a caller may ask for in a page.
pub const MAX_LIMIT: u32 = 100;

/// A query string, decoded.
#[derive(Debug, Default)]
pub struct Query(Vec<(String, String)>);

impl Query {
    /// The pairs of `query`, the part after `?`.
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        let Some(query) = query else {
            return Self::default();
        };
        let Ok(url) = worker::Url::parse(&format!("https://q/?{query}")) else {
            return Self::default();
        };
        Self(
            url.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// The first value of `key`, `None` when it is missing or empty.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }

    /// The comma separated values of every `key`, without empty ones.
    #[must_use]
    pub fn list(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .flat_map(|(_, v)| v.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The number in `key`.
    ///
    /// # Errors
    ///
    /// `400` when the value is not a whole number.
    pub fn number(&self, key: &str) -> Result<Option<u64>, Stop> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .parse()
                .map(Some)
                .or_else(|_| stop(400, format!("{key} is a number"))),
        }
    }

    /// The page size: `limit` within `1..=100`, 50 when it is not
    /// given.
    ///
    /// # Errors
    ///
    /// `400` when it is not a number in range.
    pub fn limit(&self) -> Result<u32, Stop> {
        match self.number("limit")? {
            None => Ok(DEFAULT_LIMIT),
            Some(n) if (1..=u64::from(MAX_LIMIT)).contains(&n) => {
                Ok(u32::try_from(n).unwrap_or(MAX_LIMIT))
            }
            Some(_) => stop(400, format!("limit is 1 to {MAX_LIMIT}")),
        }
    }

    /// The decoded `cursor`, `None` for the first page.
    ///
    /// # Errors
    ///
    /// `400` when the cursor is not one this API gave.
    pub fn cursor(&self) -> Result<Option<Vec<u64>>, Stop> {
        self.get("cursor")
            .map(|text| decode(text).map_or_else(|| stop(400, "the cursor is not valid"), Ok))
            .transpose()
    }
}

/// The cursor that holds `parts`.
#[must_use]
pub fn encode(parts: &[u64]) -> String {
    let text: Vec<String> = parts.iter().map(u64::to_string).collect();
    URL_SAFE_NO_PAD.encode(text.join(","))
}

/// The numbers in `cursor`, `None` when it is not one [`encode`] made.
#[must_use]
pub fn decode(cursor: &str) -> Option<Vec<u64>> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    text.split(',').map(|part| part.parse().ok()).collect()
}

/// A page of `items` out of `total`. `next` is the cursor of the page
/// after this one.
#[must_use]
pub fn page<T: Serialize>(items: &[T], total: u64, next: Option<&str>) -> Value {
    json!({ "items": items, "total": total, "next": next })
}

/// Slices `all` for a cursor that counts items: the page at the cursor
/// and the cursor of the next one. For lists small enough to hold whole
/// (secrets, variables, environments, repositories of one tenant).
///
/// # Errors
///
/// `400` for a cursor that is not one number.
pub fn slice<T: Clone>(all: &[T], query: &Query) -> Result<(Vec<T>, Option<String>), Stop> {
    let limit = query.limit()? as usize;
    let start = match query.cursor()?.as_deref() {
        None => 0,
        Some([at]) => usize::try_from(*at).unwrap_or(usize::MAX),
        Some(_) => return stop(400, "the cursor is not valid"),
    };
    let start = start.min(all.len());
    let end = (start + limit).min(all.len());
    let next = (end < all.len()).then(|| encode(&[end as u64]));
    Ok((all[start..end].to_vec(), next))
}

/// Whether `text` holds `needle`, ignoring case; always when there is
/// no needle.
#[must_use]
pub fn matches(text: &str, needle: Option<&str>) -> bool {
    needle.is_none_or(|n| text.to_lowercase().contains(&n.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_noise() {
        let cursor = encode(&[1_700_000_000_000, 42]);
        assert!(!cursor.contains('=') && !cursor.contains('+') && !cursor.contains('/'));
        assert_eq!(decode(&cursor), Some(vec![1_700_000_000_000, 42]));
        assert_eq!(decode("!!"), None);
        assert_eq!(decode(&URL_SAFE_NO_PAD.encode("a,b")), None);
    }

    #[test]
    fn query_values_are_decoded_and_listed() {
        let q = Query::parse(Some(
            "status=queued,running&status=failure&q=a%20b&limit=5&x=",
        ));
        assert_eq!(q.list("status"), ["queued", "running", "failure"]);
        assert_eq!(q.get("q"), Some("a b"));
        assert_eq!(q.get("x"), None);
        assert_eq!(q.limit().unwrap(), 5);
        assert_eq!(Query::parse(None).limit().unwrap(), DEFAULT_LIMIT);
        assert!(Query::parse(Some("limit=0")).limit().is_err());
        assert!(Query::parse(Some("limit=101")).limit().is_err());
        assert!(Query::parse(Some("limit=x")).limit().is_err());
    }

    #[test]
    fn slices_page_through_a_list() {
        let all: Vec<u32> = (0..5).collect();
        let (first, next) = slice(&all, &Query::parse(Some("limit=2"))).unwrap();
        assert_eq!(first, [0, 1]);
        let query = format!("limit=2&cursor={}", next.unwrap());
        let (second, next) = slice(&all, &Query::parse(Some(&query))).unwrap();
        assert_eq!(second, [2, 3]);
        let query = format!("limit=2&cursor={}", next.unwrap());
        let (last, next) = slice(&all, &Query::parse(Some(&query))).unwrap();
        assert_eq!(last, [4]);
        assert_eq!(next, None);
        assert!(slice(&all, &Query::parse(Some("cursor=bad!"))).is_err());
    }
}
