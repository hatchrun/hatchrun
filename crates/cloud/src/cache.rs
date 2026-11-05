// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The job cache's wire types and the checks on what a job asks for.
//! `hatchrun-ci` serves the endpoints (see its `cache` module) and keeps
//! the entries in the S3 bucket of its AWS account.

use serde::{Deserialize, Serialize};

/// The runner profile extra that turns the cache on.
pub const EXTRA: &str = "cache";
/// GitHub's limit on a cache key.
pub const MAX_KEY: usize = 512;
/// GitHub's limit on restore keys.
pub const MAX_RESTORE_KEYS: usize = 10;
/// Part URLs one request may ask for.
pub const MAX_PARTS_PER_REQUEST: usize = 100;
/// S3's part numbers run from 1 to this.
pub const MAX_PART: u16 = 10_000;

/// Checks a key the way GitHub does: present, at most [`MAX_KEY`]
/// bytes, no commas.
///
/// # Errors
///
/// What is wrong, for a 400.
pub fn check_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_KEY {
        return Err(format!("a key has 1 to {MAX_KEY} bytes"));
    }
    if key.contains(',') {
        return Err("a key cannot contain commas".to_owned());
    }
    Ok(())
}

/// Checks a version: the toolkit's hash of the paths and compression,
/// letters, digits, `-` and `_`.
///
/// # Errors
///
/// What is wrong, for a 400.
pub fn check_version(version: &str) -> Result<(), String> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("a version has 1 to 128 letters, digits, - or _".to_owned());
    }
    Ok(())
}

/// `POST /v1/cache/lookup`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupRequest {
    /// The primary key, matched exactly.
    pub key: String,
    /// Prefixes tried in order when the key misses.
    #[serde(default)]
    pub restore_keys: Vec<String>,
    /// The toolkit's version of the entry.
    pub version: String,
}

/// A hit.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupResponse {
    /// The key that matched, as the job named it.
    pub key: String,
    /// Presigned GET of the entry.
    pub url: String,
    /// Bytes.
    pub size: u64,
}

/// `POST /v1/cache/uploads`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadRequest {
    /// The key to save under.
    pub key: String,
    /// The toolkit's version of the entry.
    pub version: String,
}

/// A started upload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadResponse {
    /// Names the upload in later calls.
    pub upload_id: String,
}

/// `POST /v1/cache/uploads/{id}/parts`.
#[derive(Debug, Deserialize)]
pub struct PartsRequest {
    /// Part numbers, 1 to [`MAX_PART`].
    pub numbers: Vec<u16>,
}

/// Presigned `UploadPart` URLs.
#[derive(Debug, Serialize)]
pub struct PartsResponse {
    /// One per number asked for.
    pub parts: Vec<PartUrl>,
}

/// Where to PUT one part.
#[derive(Debug, Serialize)]
pub struct PartUrl {
    /// The part number.
    pub number: u16,
    /// Presigned PUT.
    pub url: String,
}

/// `POST /v1/cache/uploads/{id}/complete`.
#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    /// Every part, with the `ETag` S3 answered its upload with.
    pub parts: Vec<UploadedPart>,
}

/// One uploaded part.
#[derive(Debug, Deserialize)]
pub struct UploadedPart {
    /// The part number.
    pub number: u16,
    /// The `ETag` S3 answered with, quotes included.
    pub etag: String,
}

/// Checks part numbers for a parts request.
///
/// # Errors
///
/// What is wrong, for a 400.
pub fn check_part_numbers(numbers: &[u16]) -> Result<(), String> {
    if numbers.is_empty() || numbers.len() > MAX_PARTS_PER_REQUEST {
        return Err(format!("ask for 1 to {MAX_PARTS_PER_REQUEST} parts"));
    }
    if numbers
        .iter()
        .any(|number| !(1..=MAX_PART).contains(number))
    {
        return Err(format!("part numbers run from 1 to {MAX_PART}"));
    }
    Ok(())
}

/// The parts of a complete request, sorted, checked for gaps in
/// nothing but their range: S3 takes any increasing part numbers.
///
/// # Errors
///
/// What is wrong, for a 400.
pub fn complete_parts(mut parts: Vec<UploadedPart>) -> Result<Vec<UploadedPart>, String> {
    if parts.is_empty() || parts.len() > usize::from(MAX_PART) {
        return Err(format!("complete with 1 to {MAX_PART} parts"));
    }
    parts.sort_by_key(|part| part.number);
    if parts
        .windows(2)
        .any(|pair| pair[0].number == pair[1].number)
    {
        return Err("each part once".to_owned());
    }
    if parts
        .iter()
        .any(|part| !(1..=MAX_PART).contains(&part.number) || part.etag.is_empty())
    {
        return Err(format!(
            "parts need a number from 1 to {MAX_PART} and an etag"
        ));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_keys_and_versions() {
        assert!(check_key("npm-linux-abc").is_ok());
        assert!(check_key("").is_err());
        assert!(check_key("a,b").is_err());
        assert!(check_key(&"k".repeat(MAX_KEY + 1)).is_err());
        assert!(check_version("0123abcdef_-").is_ok());
        assert!(check_version("../x").is_err());
        assert!(check_version("").is_err());
    }

    #[test]
    fn checks_parts() {
        assert!(check_part_numbers(&[1, 2, 3]).is_ok());
        assert!(check_part_numbers(&[]).is_err());
        assert!(check_part_numbers(&[0]).is_err());
        assert!(check_part_numbers(&[MAX_PART + 1]).is_err());
        let part = |number: u16| UploadedPart {
            number,
            etag: "\"e\"".to_owned(),
        };
        let sorted = complete_parts(vec![part(3), part(1)]).unwrap();
        assert_eq!(sorted.iter().map(|p| p.number).collect::<Vec<_>>(), [1, 3]);
        assert!(complete_parts(vec![part(1), part(1)]).is_err());
        assert!(complete_parts(Vec::new()).is_err());
    }
}
