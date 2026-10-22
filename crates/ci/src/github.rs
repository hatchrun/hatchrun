// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The GitHub requests the control plane sends, as data (see
//! `hatchrun_cloud::github`), and readers for the answers.

use hatchrun_cloud::http::{Request, Response, encode};
use hatchrun_workflow::MAX_SOURCE;
use serde::{Deserialize, Serialize};

/// Where a repository keeps its config.
pub const CONFIG_PATH: &str = ".hatchrun/config.yml";

/// The most seconds a throttled call waits between attempts.
const MAX_BACKOFF_SECS: u64 = 900;

fn call(method: &'static str, url: String, token: &str, accept: &str) -> Request {
    Request::new(method, url)
        .header("Accept", accept)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "hatchrun-ci")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

fn rest(method: &'static str, url: String, token: &str) -> Request {
    call(method, url, token, "application/vnd.github+json")
}

fn json_body(request: Request, body: &impl Serialize) -> Request {
    let body = serde_json::to_vec(body).expect("a request body serializes");
    request
        .header("Content-Type", "application/json")
        .body(body)
}

fn decode<T: for<'de> Deserialize<'de>>(response: &Response, what: &str) -> Result<T, String> {
    if !response.ok() {
        return Err(format!(
            "{what}: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|err| format!("{what}: {err}"))
}

/// The raw config file at `sha`.
#[must_use]
pub fn config_file(api: &str, repo: &str, sha: &str, token: &str) -> Request {
    call(
        "GET",
        format!(
            "{api}/repos/{repo}/contents/{CONFIG_PATH}?ref={}",
            encode(sha)
        ),
        token,
        "application/vnd.github.raw+json",
    )
}

/// The config text, `None` when the repository has no config file.
///
/// # Errors
///
/// GitHub's answer when it is not the file, or a file over
/// [`MAX_SOURCE`] bytes or not UTF-8.
pub fn config_text(response: &Response) -> Result<Option<String>, String> {
    if response.status == 404 {
        return Ok(None);
    }
    if !response.ok() {
        return Err(format!(
            "config: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    if response.body.len() > MAX_SOURCE {
        return Err(format!(
            "{CONFIG_PATH} is {} bytes; the limit is {MAX_SOURCE}",
            response.body.len()
        ));
    }
    String::from_utf8(response.body.clone())
        .map(Some)
        .map_err(|_| format!("{CONFIG_PATH} is not UTF-8"))
}

/// One page of the files a pull request changes, 100 to a page.
#[must_use]
pub fn pull_files(api: &str, repo: &str, number: u64, page: u32, token: &str) -> Request {
    rest(
        "GET",
        format!("{api}/repos/{repo}/pulls/{number}/files?per_page=100&page={page}"),
        token,
    )
}

/// The file names on one page of [`pull_files`].
///
/// # Errors
///
/// GitHub's answer when it is not a file list.
pub fn pull_file_names(response: &Response) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct File {
        filename: String,
    }
    decode::<Vec<File>>(response, "pull request files")
        .map(|files| files.into_iter().map(|f| f.filename).collect())
}

/// A commit by sha, branch or tag (`heads/main` also works).
#[must_use]
pub fn commit_sha(api: &str, repo: &str, git_ref: &str, token: &str) -> Request {
    let git_ref = git_ref.split('/').map(encode).collect::<Vec<_>>().join("/");
    rest(
        "GET",
        format!("{api}/repos/{repo}/commits/{git_ref}"),
        token,
    )
}

/// The sha of the commit [`commit_sha`] asked for.
///
/// # Errors
///
/// GitHub's answer when it is not a commit.
pub fn sha_of(response: &Response) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    decode::<Commit>(response, "commit").map(|c| c.sha)
}

/// Trades the App's JWT for a token of one installation limited to one
/// repository and to read access.
#[must_use]
pub fn scoped_token(api: &str, installation_id: u64, app_jwt: &str, repo_id: u64) -> Request {
    json_body(
        rest(
            "POST",
            format!("{api}/app/installations/{installation_id}/access_tokens"),
            app_jwt,
        ),
        &serde_json::json!({
            "repository_ids": [repo_id],
            "permissions": {
                "contents": "read",
                "metadata": "read",
                "checks": "read",
                "pull_requests": "read",
            },
        }),
    )
}

/// Revokes the installation token it is sent with.
#[must_use]
pub fn revoke_token(api: &str, token: &str) -> Request {
    rest("DELETE", format!("{api}/installation/token"), token)
}

/// A check run, as created or updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckRun {
    /// The job's name.
    pub name: String,
    /// The commit the check run is on. Left out of the body when empty,
    /// which [`update_check_run`] does.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub head_sha: String,
    /// Names the job again when GitHub sends a re-run request. Left out
    /// of the body when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub external_id: String,
    /// Where the app shows the job. Left out of the body when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub details_url: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// RFC 3339 UTC, see [`rfc3339`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Set with `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// RFC 3339 UTC, see [`rfc3339`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// What the check run shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Output>,
}

/// What a check run shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Output {
    /// The heading.
    pub title: String,
    /// The summary, Markdown.
    pub summary: String,
    /// The details, Markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Notes on lines of files.
    pub annotations: Vec<GhAnnotation>,
}

/// A note on lines of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GhAnnotation {
    /// The file.
    pub path: String,
    /// First line.
    pub start_line: u32,
    /// Last line.
    pub end_line: u32,
    /// First column, on one-line notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    /// Last column, on one-line notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    /// `notice`, `warning` or `failure`.
    pub annotation_level: String,
    /// The note.
    pub message: String,
    /// The note's heading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Creates a check run.
#[must_use]
pub fn create_check_run(api: &str, repo: &str, token: &str, run: &CheckRun) -> Request {
    json_body(
        rest("POST", format!("{api}/repos/{repo}/check-runs"), token),
        run,
    )
}

/// Updates check run `id`. GitHub does not take `head_sha` here.
#[must_use]
pub fn update_check_run(api: &str, repo: &str, id: u64, token: &str, run: &CheckRun) -> Request {
    let run = CheckRun {
        head_sha: String::new(),
        ..run.clone()
    };
    json_body(
        rest(
            "PATCH",
            format!("{api}/repos/{repo}/check-runs/{id}"),
            token,
        ),
        &run,
    )
}

/// The id of the check run [`create_check_run`] made.
///
/// # Errors
///
/// GitHub's answer when it is not a check run.
pub fn check_run_id(response: &Response) -> Result<u64, String> {
    #[derive(Deserialize)]
    struct Created {
        id: u64,
    }
    decode::<Created>(response, "check run").map(|c| c.id)
}

/// A repository as a signed-in user sees it.
#[must_use]
pub fn user_repo(api: &str, repo: &str, user_token: &str) -> Request {
    rest("GET", format!("{api}/repos/{repo}"), user_token)
}

/// What a user may do in a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// May read the repository and its logs.
    pub read: bool,
    /// May push to it.
    pub write: bool,
}

/// What the answer of [`user_repo`] says. Any answer but 200 gives no
/// permissions; a public repository can be read by anyone.
#[must_use]
pub fn permissions(response: &Response) -> Permissions {
    #[derive(Deserialize)]
    struct Repo {
        #[serde(default = "yes")]
        private: bool,
        permissions: Option<std::collections::BTreeMap<String, bool>>,
    }
    fn yes() -> bool {
        true
    }
    let none = Permissions {
        read: false,
        write: false,
    };
    if response.status != 200 {
        return none;
    }
    let Ok(repo) = serde_json::from_slice::<Repo>(&response.body) else {
        return none;
    };
    let has = |names: &[&str]| {
        repo.permissions
            .as_ref()
            .is_some_and(|rights| names.iter().any(|n| rights.get(*n) == Some(&true)))
    };
    Permissions {
        read: !repo.private || has(&["pull", "push", "maintain", "admin"]),
        write: has(&["push", "maintain", "admin"]),
    }
}

/// Seconds to wait before sending the request again, when `response`
/// is GitHub throttling it: a `429`, or a `403` with `retry-after`,
/// with `x-ratelimit-remaining: 0` or whose body mentions the rate
/// limit. The wait, kept within 1 to 900 s, is `retry-after`, else the time until
/// `x-ratelimit-reset` when nothing is left, else 60 s doubling per
/// `attempt` up to 900 s. Any other answer is `None`.
#[must_use]
pub fn throttle(response: &Response, now_secs: u64, attempt: u32) -> Option<u64> {
    let number = |name: &str| response.header(name)?.trim().parse::<u64>().ok();
    let retry_after = number("retry-after");
    let remaining = number("x-ratelimit-remaining");
    let throttled = response.status == 429
        || (response.status == 403
            && (retry_after.is_some()
                || remaining == Some(0)
                || response.text().to_ascii_lowercase().contains("rate limit")));
    if !throttled {
        return None;
    }
    let wait = if let Some(seconds) = retry_after {
        seconds
    } else if remaining == Some(0)
        && let Some(reset) = number("x-ratelimit-reset")
    {
        reset.saturating_sub(now_secs)
    } else {
        60_u64 << attempt.min(10)
    };
    Some(wait.clamp(1, MAX_BACKOFF_SECS))
}

/// `ms` (Unix milliseconds) as RFC 3339 UTC, such as
/// `2026-10-22T13:45:00Z`, in whole seconds.
#[must_use]
pub fn rfc3339(ms: u64) -> String {
    hatchrun_cloud::format_rfc3339(ms)
}

#[cfg(test)]
mod tests {
    use hatchrun_cloud::http::Response;

    use super::*;

    fn answer(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn json(request: &Request) -> serde_json::Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    fn run() -> CheckRun {
        CheckRun {
            name: "build".into(),
            head_sha: "abc".into(),
            external_id: "hatchrun-ci:42".into(),
            details_url: "https://ci.hatchrun.io/runs/1".into(),
            status: "in_progress".into(),
            started_at: Some(rfc3339(1_792_676_700_000)),
            conclusion: None,
            completed_at: None,
            output: None,
        }
    }

    #[test]
    fn config_file_asks_for_raw_content_at_the_sha() {
        let r = config_file("https://api.github.com", "octo/widgets", "abc", "tok");
        assert_eq!(r.method, "GET");
        assert_eq!(
            r.url,
            "https://api.github.com/repos/octo/widgets/contents/.hatchrun/config.yml?ref=abc"
        );
        assert_eq!(
            r.header_value("Accept"),
            Some("application/vnd.github.raw+json")
        );
        assert_eq!(r.header_value("Authorization"), Some("Bearer tok"));
        assert_eq!(r.header_value("User-Agent"), Some("hatchrun-ci"));
        assert_eq!(r.header_value("X-GitHub-Api-Version"), Some("2022-11-28"));
    }

    #[test]
    fn missing_config_is_none() {
        assert_eq!(config_text(&answer(404, &[], "{}")), Ok(None));
        assert_eq!(
            config_text(&answer(200, &[], "version: 1")),
            Ok(Some("version: 1".into()))
        );
        assert!(config_text(&answer(500, &[], "boom")).is_err());
    }

    #[test]
    fn oversized_config_is_an_error() {
        let big = "a".repeat(hatchrun_workflow::MAX_SOURCE + 1);
        assert!(config_text(&answer(200, &[], &big)).is_err());
    }

    #[test]
    fn pull_files_page_through_the_list() {
        let r = pull_files("https://api.github.com", "octo/widgets", 7, 2, "tok");
        assert_eq!(
            r.url,
            "https://api.github.com/repos/octo/widgets/pulls/7/files?per_page=100&page=2"
        );
        let names = pull_file_names(&answer(
            200,
            &[],
            r#"[{"filename":"a.rs"},{"filename":"b/c"}]"#,
        ));
        assert_eq!(names, Ok(vec!["a.rs".to_owned(), "b/c".to_owned()]));
    }

    #[test]
    fn commit_sha_reads_the_sha() {
        let r = commit_sha(
            "https://api.github.com",
            "octo/widgets",
            "heads/feat x",
            "tok",
        );
        assert_eq!(
            r.url,
            "https://api.github.com/repos/octo/widgets/commits/heads/feat%20x"
        );
        assert_eq!(
            sha_of(&answer(200, &[], r#"{"sha":"abc"}"#)),
            Ok("abc".into())
        );
        assert!(sha_of(&answer(404, &[], "{}")).is_err());
    }

    #[test]
    fn scoped_token_names_one_repository() {
        let r = scoped_token("https://api.github.com", 777, "jwt", 4242);
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.url,
            "https://api.github.com/app/installations/777/access_tokens"
        );
        assert_eq!(r.header_value("Authorization"), Some("Bearer jwt"));
        assert_eq!(
            json(&r),
            serde_json::json!({
                "repository_ids": [4242],
                "permissions": {
                    "contents": "read",
                    "metadata": "read",
                    "checks": "read",
                    "pull_requests": "read"
                }
            })
        );
    }

    #[test]
    fn revoke_deletes_the_installation_token() {
        let r = revoke_token("https://api.github.com", "tok");
        assert_eq!(r.method, "DELETE");
        assert_eq!(r.url, "https://api.github.com/installation/token");
        assert_eq!(r.header_value("Authorization"), Some("Bearer tok"));
    }

    #[test]
    fn create_carries_head_sha_and_leaves_out_unset_fields() {
        let r = create_check_run("https://api.github.com", "octo/widgets", "tok", &run());
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.url,
            "https://api.github.com/repos/octo/widgets/check-runs"
        );
        let body = json(&r);
        assert_eq!(body["head_sha"], "abc");
        assert_eq!(body["started_at"], "2026-10-22T13:45:00Z");
        assert!(body.get("conclusion").is_none());
        assert!(body.get("output").is_none());
    }

    #[test]
    fn update_leaves_out_head_sha() {
        let mut finished = run();
        finished.status = "completed".into();
        finished.conclusion = Some("success".into());
        finished.output = Some(Output {
            title: "t".into(),
            summary: "s".into(),
            text: None,
            annotations: vec![],
        });
        let r = update_check_run(
            "https://api.github.com",
            "octo/widgets",
            99,
            "tok",
            &finished,
        );
        assert_eq!(r.method, "PATCH");
        assert_eq!(
            r.url,
            "https://api.github.com/repos/octo/widgets/check-runs/99"
        );
        let body = json(&r);
        assert!(body.get("head_sha").is_none());
        assert_eq!(body["conclusion"], "success");
        assert_eq!(body["output"]["title"], "t");
    }

    #[test]
    fn check_run_id_reads_the_id() {
        assert_eq!(check_run_id(&answer(201, &[], r#"{"id":99}"#)), Ok(99));
        assert!(check_run_id(&answer(422, &[], "{}")).is_err());
    }

    #[test]
    fn throttle_prefers_retry_after() {
        let r = answer(
            403,
            &[
                ("retry-after", "30"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "5000"),
            ],
            "",
        );
        assert_eq!(throttle(&r, 1000, 0), Some(30));
    }

    #[test]
    fn throttle_waits_for_reset() {
        let r = answer(
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1090"),
            ],
            "",
        );
        assert_eq!(throttle(&r, 1000, 0), Some(90));
    }

    #[test]
    fn throttle_wait_stays_between_one_and_900_seconds() {
        let past = answer(
            403,
            &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "900")],
            "",
        );
        assert_eq!(throttle(&past, 1000, 0), Some(1));
        let huge = answer(429, &[("retry-after", "86400")], "");
        assert_eq!(throttle(&huge, 1000, 0), Some(900));
        let zero = answer(429, &[("retry-after", "0")], "");
        assert_eq!(throttle(&zero, 1000, 0), Some(1));
    }

    #[test]
    fn too_many_requests_backs_off_without_headers() {
        assert_eq!(throttle(&answer(429, &[], ""), 1000, 1), Some(120));
    }

    #[test]
    fn secondary_limit_without_headers_backs_off() {
        let r = answer(403, &[], "You have exceeded a secondary rate limit");
        assert_eq!(throttle(&r, 1000, 0), Some(60));
        assert_eq!(throttle(&r, 1000, 3), Some(480));
        assert_eq!(throttle(&r, 1000, 9), Some(900));
        assert_eq!(throttle(&r, 1000, u32::MAX), Some(900));
    }

    #[test]
    fn plain_forbidden_is_not_throttled() {
        let forbidden = answer(
            403,
            &[("x-ratelimit-remaining", "4000")],
            "Resource not accessible",
        );
        assert_eq!(throttle(&forbidden, 1000, 0), None);
        assert_eq!(throttle(&answer(500, &[], ""), 1000, 0), None);
        assert_eq!(throttle(&answer(200, &[], ""), 1000, 0), None);
    }

    #[test]
    fn public_repo_is_readable_without_permissions() {
        let public = answer(200, &[], r#"{"private":false}"#);
        assert_eq!(
            permissions(&public),
            Permissions {
                read: true,
                write: false
            }
        );
    }

    #[test]
    fn private_repo_follows_the_users_permissions() {
        let body = |p: &str| {
            answer(
                200,
                &[],
                &format!(r#"{{"private":true,"permissions":{p}}}"#),
            )
        };
        assert_eq!(
            permissions(&body(r#"{"pull":true,"push":false}"#)),
            Permissions {
                read: true,
                write: false
            }
        );
        assert_eq!(
            permissions(&body(r#"{"pull":true,"push":true}"#)),
            Permissions {
                read: true,
                write: true
            }
        );
        assert_eq!(
            permissions(&answer(200, &[], r#"{"private":true}"#)),
            Permissions {
                read: false,
                write: false
            }
        );
    }

    #[test]
    fn unreadable_repo_has_no_permissions() {
        let none = Permissions {
            read: false,
            write: false,
        };
        assert_eq!(permissions(&answer(404, &[], "{}")), none);
        assert_eq!(permissions(&answer(200, &[], "nope")), none);
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(1_792_676_700_000), "2026-10-22T13:45:00Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
