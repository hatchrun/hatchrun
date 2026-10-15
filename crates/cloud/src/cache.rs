// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The job cache in the customer's S3 bucket (#53): what a runner may
//! read and write, and how entries are named. The endpoints are in
//! [`serve`].
//!
//! The agent on a runner calls `/v1/cache/*` with a token from its boot
//! document. The token names the tenant and the job; the repository and
//! the refs the job may use come from the job's own record and its
//! workflow run on GitHub, never from the request.
//!
//! Entries live at `<repository id>/<scope>/<version>/<key>`. A job
//! writes only to its own scope and reads its own, then the default
//! branch's. Branches pushed to the repository itself get `b/<hash of
//! branch>`. Pull requests, and runs whose code comes from a fork, get
//! `p/<head repository id>/<hash of head branch>`, which no other run
//! reads, so a pull request cannot put an entry where the default
//! branch would restore it.

pub mod serve;

use std::fmt::Write;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::aws::s3::Object;
use crate::jwt;

/// The `aud` of a cache token, so a token for hatchrun-telemetry, signed
/// by the same key, is refused here.
pub const AUDIENCE: &str = "cache";
/// Variable holding the URL the agent calls, ending in `/v1/cache`.
pub const URL: &str = "CACHE_URL";
/// The runner profile extra that turns the cache on.
pub const EXTRA: &str = "cache";
/// How long a presigned URL holds.
pub const URL_EXPIRES: Duration = Duration::from_mins(15);
/// How long the Worker reuses a tenant's role credentials. STS sessions
/// last an hour, so a URL presigned with them at the end of this still
/// holds for [`URL_EXPIRES`].
pub const CREDENTIALS_TTL: Duration = Duration::from_mins(30);
/// GitHub's limit on a cache key.
pub const MAX_KEY: usize = 512;
/// GitHub's limit on restore keys.
pub const MAX_RESTORE_KEYS: usize = 10;
/// Part URLs one request may ask for.
pub const MAX_PARTS_PER_REQUEST: usize = 100;
/// S3's part numbers run from 1 to this.
pub const MAX_PART: u16 = 10_000;
/// Listing pages one lookup reads per prefix before giving up.
pub const MAX_PAGES: usize = 5;

/// Events whose code may come from outside the repository's branches.
const PULL_REQUEST_EVENTS: [&str; 4] = [
    "pull_request",
    "pull_request_target",
    "pull_request_review",
    "pull_request_review_comment",
];

/// Who a cache token is for.
#[derive(Debug, Clone, Copy)]
pub struct Job<'a> {
    /// The tenant id.
    pub tenant: &'a str,
    /// The App installation.
    pub installation_id: u64,
    /// The workflow job.
    pub job_id: u64,
}

/// The signing input of a cache token for `job`, issued at `now` (Unix
/// seconds). It lives as long as a telemetry token: past the longest a
/// runner instance runs.
#[must_use]
pub fn signing_input(job: Job<'_>, now: u64) -> String {
    jwt::eddsa_signing_input(&serde_json::json!({
        "aud": AUDIENCE,
        "tenant": job.tenant,
        "installation_id": job.installation_id,
        "job_id": job.job_id,
        "exp": now + crate::telemetry::LIFETIME,
    }))
}

/// What a valid token says.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Claims {
    /// Always [`AUDIENCE`].
    pub aud: String,
    /// The tenant id.
    pub tenant: String,
    /// The App installation.
    pub installation_id: u64,
    /// The workflow job.
    pub job_id: u64,
    /// Expiry, Unix seconds.
    pub exp: u64,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

/// Checks `Authorization: Bearer {token}`: an `EdDSA` JWT for
/// [`AUDIENCE`], signed by `key`, that has not expired at `now` (Unix
/// seconds).
///
/// # Errors
///
/// What is wrong, for a 401.
pub fn verify(header: Option<&str>, key: &VerifyingKey, now: u64) -> Result<Claims, &'static str> {
    const BAD: &str = "bad token";
    let token = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() <= 4096)
        .ok_or("no token")?;
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(BAD);
    };
    let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| BAD);
    let header: Header = serde_json::from_slice(&decode(head)?).map_err(|_| BAD)?;
    if header.alg != "EdDSA" {
        return Err(BAD);
    }
    let signature: [u8; 64] = decode(signature)?.try_into().map_err(|_| BAD)?;
    let signed = &token.as_bytes()[..head.len() + 1 + body.len()];
    key.verify_strict(signed, &Signature::from_bytes(&signature))
        .map_err(|_| BAD)?;
    let claims: Claims = serde_json::from_slice(&decode(body)?).map_err(|_| BAD)?;
    if claims.aud != AUDIENCE {
        return Err(BAD);
    }
    if claims.exp <= now {
        return Err("token expired");
    }
    Ok(claims)
}

/// The fields of a workflow run the scopes follow from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Run {
    /// What started the run: `push`, `pull_request`, ...
    pub event: String,
    /// The branch or tag the run's code is on, in its head repository.
    #[serde(default)]
    pub head_branch: Option<String>,
    /// The repository the code comes from: a fork for pull requests
    /// from forks.
    #[serde(default)]
    pub head_repository: Option<RepositoryId>,
    /// The repository the run belongs to.
    pub repository: RepositoryId,
}

/// A repository's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RepositoryId {
    /// GitHub's id, which survives renames.
    pub id: u64,
}

/// Where a job reads and writes, as key prefixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scopes {
    /// The job's own scope.
    pub write: String,
    /// Scopes to look in, in order; the first is [`Scopes::write`].
    pub reads: Vec<String>,
}

/// The scopes of a job in `run`, whose repository's default branch is
/// `default_branch`.
#[must_use]
pub fn scopes(run: &Run, default_branch: &str) -> Scopes {
    let repository = run.repository.id;
    let head = run.head_repository.map_or(repository, |head| head.id);
    let branch = run.head_branch.as_deref().unwrap_or_default();
    let outside = head != repository || PULL_REQUEST_EVENTS.contains(&run.event.as_str());
    let write = if outside {
        format!("{repository}/p/{head}/{}", hash(branch))
    } else {
        format!("{repository}/b/{}", hash(branch))
    };
    let default = format!("{repository}/b/{}", hash(default_branch));
    let mut reads = vec![write.clone()];
    if default != write {
        reads.push(default);
    }
    Scopes { write, reads }
}

/// The first 16 hex digits of the SHA-256 of `text`: names a branch in
/// a key without spelling it out.
fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

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

/// The S3 key of `key` at `version` in `scope`.
#[must_use]
pub fn object_key(scope: &str, version: &str, key: &str) -> String {
    format!("{scope}/{version}/{key}")
}

/// The entry a lookup for `wanted` restores from `objects`, all listed
/// under the prefix `object_key(scope, version, wanted)`: the object at
/// exactly that key when `exact`, otherwise the newest whose key starts
/// with it.
#[must_use]
pub fn pick<'a>(objects: &'a [Object], prefix: &str, exact: bool) -> Option<&'a Object> {
    if exact {
        return objects.iter().find(|object| object.key == prefix);
    }
    objects
        .iter()
        .filter(|object| object.key.starts_with(prefix))
        .max_by(|a, b| a.last_modified.cmp(&b.last_modified))
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
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn run(event: &str, branch: &str, head: u64) -> Run {
        Run {
            event: event.to_owned(),
            head_branch: Some(branch.to_owned()),
            head_repository: Some(RepositoryId { id: head }),
            repository: RepositoryId { id: 1 },
        }
    }

    #[test]
    fn a_push_writes_its_branch_and_reads_the_default_branch() {
        let feature = scopes(&run("push", "feature", 1), "main");
        assert_eq!(feature.write, format!("1/b/{}", hash("feature")));
        assert_eq!(
            feature.reads,
            [feature.write.clone(), format!("1/b/{}", hash("main"))]
        );
        let main = scopes(&run("push", "main", 1), "main");
        assert_eq!(main.reads, std::slice::from_ref(&main.write));
    }

    #[test]
    fn pull_requests_never_write_where_a_branch_reads() {
        let main = format!("1/b/{}", hash("main"));
        // A pull request from a branch named main, in the repository and
        // from a fork, and a fork's run under another event.
        for run in [
            run("pull_request", "main", 1),
            run("pull_request_target", "main", 1),
            run("pull_request", "main", 2),
            run("workflow_dispatch", "main", 2),
        ] {
            let scopes = scopes(&run, "main");
            assert!(scopes.write.starts_with("1/p/"), "{scopes:?}");
            assert_eq!(scopes.reads, [scopes.write.clone(), main.clone()]);
        }
        assert_ne!(
            scopes(&run("pull_request", "main", 2), "main").write,
            scopes(&run("pull_request", "main", 3), "main").write
        );
    }

    #[test]
    fn reads_a_run_from_github() {
        let run: Run = serde_json::from_str(
            r#"{"id":9,"event":"pull_request","head_branch":"fix","head_sha":"abc",
                "head_repository":{"id":2,"full_name":"fork/repo"},
                "repository":{"id":1,"full_name":"acme/repo"}}"#,
        )
        .unwrap();
        assert_eq!(run, self::run("pull_request", "fix", 2));
    }

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
    fn picks_the_exact_key_or_the_newest_prefix_match() {
        let object = |key: &str, at: &str| Object {
            key: key.to_owned(),
            last_modified: at.to_owned(),
            size: 1,
        };
        let objects = [
            object("s/v/npm-a", "2026-10-01T00:00:00.000Z"),
            object("s/v/npm-ab", "2026-10-03T00:00:00.000Z"),
            object("s/v/npm-b", "2026-10-02T00:00:00.000Z"),
        ];
        assert_eq!(pick(&objects, "s/v/npm-a", true).unwrap().key, "s/v/npm-a");
        assert_eq!(pick(&objects, "s/v/npm-", false).unwrap().key, "s/v/npm-ab");
        assert!(pick(&objects, "s/v/npm-c", true).is_none());
        assert!(pick(&objects, "s/v/npm-", true).is_none());
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

    fn token(key: &SigningKey, claims: &serde_json::Value) -> String {
        let input = jwt::eddsa_signing_input(claims);
        let signature = key.sign(input.as_bytes());
        jwt::token(&input, &signature.to_bytes())
    }

    #[test]
    fn verifies_tokens() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key();
        let job = Job {
            tenant: "acme1234",
            installation_id: 42,
            job_id: 7,
        };
        let input = signing_input(job, 1_000);
        let good = jwt::token(&input, &key.sign(input.as_bytes()).to_bytes());
        let claims = verify(Some(&format!("Bearer {good}")), &public, 1_001).unwrap();
        assert_eq!((claims.tenant.as_str(), claims.job_id), ("acme1234", 7));

        let telemetry = token(
            &key,
            &serde_json::json!({ "tenant": "acme1234", "installation_id": 42, "job_id": 7,
                "runner_name": "r", "exp": 99_999 }),
        );
        let other = SigningKey::from_bytes(&[8; 32]);
        for refused in [
            None,
            Some(good.clone()),
            Some(format!("Bearer {telemetry}")),
            Some(format!(
                "Bearer {}",
                token(
                    &other,
                    &serde_json::json!({ "aud": "cache",
                "tenant": "acme1234", "installation_id": 42, "job_id": 7, "exp": 99_999 })
                )
            )),
            Some(format!("Bearer {good}x")),
        ] {
            assert!(
                verify(refused.as_deref(), &public, 1_001).is_err(),
                "{refused:?}"
            );
        }
        let expired = verify(
            Some(&format!("Bearer {good}")),
            &public,
            1_000 + crate::telemetry::LIFETIME,
        );
        assert_eq!(expired, Err("token expired"));
    }
}
