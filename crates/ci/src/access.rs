// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What the person behind a request to `/api/ci/` may do in a
//! repository.
//!
//! Anyone may read a public repository, signed in or not. A private
//! repository needs a GitHub sign-in whose user can read it, and
//! starting or stopping runs needs a user who can push. GitHub decides:
//! the session's own token asks for the repository, so a user whose
//! access ends loses it on the next request, with nothing cached here.

use std::cell::RefCell;
use std::collections::HashMap;

use hatchrun_cloud::Session;
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::send;
use worker::Request;

use crate::env::Settings;
use crate::github::{self, Permissions};
use crate::store::RepoRow;

/// How much of a repository a request may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Nothing: the repository is not shown.
    None,
    /// Runs, jobs and logs.
    Read,
    /// Also dispatch, cancel and re-run.
    Write,
    /// Also the repository's secrets and variables.
    Admin,
}

/// The access GitHub's answer gives. `p` is `None` when nobody is
/// signed in. A public repository is at least `Read` either way; a
/// private one needs the permissions to say so.
#[must_use]
pub fn from_permissions(private: bool, p: Option<Permissions>) -> Access {
    let public = if private { Access::None } else { Access::Read };
    match p {
        Some(p) if p.admin => Access::Admin,
        Some(p) if p.write => Access::Write,
        Some(p) if p.read => Access::Read,
        _ => public,
    }
}

/// Why access could not be decided.
#[derive(Debug)]
pub enum Failure {
    /// GitHub is rate limiting the user's token: ask again in this many
    /// seconds. Never "no access".
    Retry(u64),
    /// The session lookup or the call failed.
    Worker(worker::Error),
}

impl From<worker::Error> for Failure {
    fn from(err: worker::Error) -> Self {
        Self::Worker(err)
    }
}

/// What GitHub's answer to the permissions call means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The user's permissions.
    Known(Permissions),
    /// Rate limited: wait this many seconds.
    Retry(u64),
    /// GitHub failed (5xx).
    Broken(u16),
}

/// Classifies the answer of [`github::user_repo`] at `now_secs`.
#[must_use]
pub fn classify(response: &hatchrun_cloud::http::Response, now_secs: u64) -> Verdict {
    if let Some(wait) = github::throttle(response, now_secs, 0) {
        return Verdict::Retry(wait);
    }
    if response.status >= 500 {
        return Verdict::Broken(response.status);
    }
    Verdict::Known(github::permissions(response))
}

/// How long a user's permissions on a repository are kept, in
/// milliseconds. Polling readers would otherwise spend the user's
/// GitHub quota.
pub const CACHE_MS: u64 = 60_000;

/// Entries kept before the cache is emptied.
const CACHE_CAP: usize = 2048;

type Key = (u64, u64);

thread_local! {
    static CACHE: RefCell<HashMap<Key, (u64, Permissions)>> = RefCell::new(HashMap::new());
}

fn cached(key: Key, now: u64) -> Option<Permissions> {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        cache
            .get(&key)
            .filter(|(until, _)| *until > now)
            .map(|(_, p)| *p)
    })
}

fn remember(key: Key, p: Permissions, now: u64) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_CAP {
            cache.retain(|_, (until, _)| *until > now);
            if cache.len() >= CACHE_CAP {
                cache.clear();
            }
        }
        cache.insert(key, (now + CACHE_MS, p));
    });
}

/// The access the request has to `repo`.
///
/// # Errors
///
/// The session lookup or the call to GitHub failed, or GitHub asks to
/// wait. The caller answers 500 or 503, never a guess in either
/// direction.
pub async fn check(
    req: &Request,
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
) -> Result<Access, Failure> {
    let session = Session::find(req, db).await?;
    resolve(session.as_ref(), settings, repo).await
}

/// The access of a session found already, `None` when nobody is signed
/// in.
///
/// # Errors
///
/// As [`check`].
pub async fn resolve(
    session: Option<&Session>,
    settings: &Settings,
    repo: &RepoRow,
) -> Result<Access, Failure> {
    let Some(session) = session else {
        return Ok(from_permissions(repo.private, None));
    };
    let now = worker::Date::now().as_millis();
    let key = (session.user.id, repo.id);
    if let Some(p) = cached(key, now) {
        return Ok(from_permissions(repo.private, Some(p)));
    }
    let request = github::user_repo(&settings.github_api, &repo.full_name, &session.token);
    let response = send(&request).await.map_err(|err| {
        Failure::Worker(worker::Error::RustError(format!(
            "{}: {err}",
            repo.full_name
        )))
    })?;
    match classify(&response, now / 1000) {
        Verdict::Known(p) => {
            remember(key, p, now);
            Ok(from_permissions(repo.private, Some(p)))
        }
        Verdict::Retry(wait) => Err(Failure::Retry(wait)),
        Verdict::Broken(status) => Err(Failure::Worker(worker::Error::RustError(format!(
            "{}: GitHub answered {status}",
            repo.full_name
        )))),
    }
}

/// What GitHub's answer to the membership call means at `now_secs`:
/// the id of the organization when the user is an active admin of it.
///
/// # Errors
///
/// GitHub rate limits the user's token, or failed.
pub fn admin_verdict(
    response: &hatchrun_cloud::http::Response,
    now_secs: u64,
) -> Result<Option<u64>, Failure> {
    if let Some(wait) = github::throttle(response, now_secs, 0) {
        return Err(Failure::Retry(wait));
    }
    // 403: the token or the App cannot see the organization's members.
    // Neither says the user is an admin.
    if response.status == 403 {
        return Ok(None);
    }
    hatchrun_cloud::github::parse_membership(response)
        .map(|membership| {
            membership
                .filter(hatchrun_cloud::github::Membership::is_admin)
                .map(|m| m.organization.id)
        })
        .map_err(|err| Failure::Worker(worker::Error::RustError(format!("membership: {err}"))))
}

/// The id of the GitHub organization `org` when the session's user is
/// an active admin of it, by GitHub's word on every call. A login can
/// name another organization than a tenant once knew, so callers find
/// the tenant by this id and never by the login.
///
/// # Errors
///
/// The call to GitHub failed, or GitHub asks to wait. The caller never
/// reads that as "not an admin".
pub async fn org_admin(
    settings: &Settings,
    session: &Session,
    org: &str,
) -> Result<Option<u64>, Failure> {
    let request = hatchrun_cloud::github::membership(&settings.github_api, &session.token, org);
    let response = send(&request).await.map_err(|err| {
        Failure::Worker(worker::Error::RustError(format!(
            "membership of {org}: {err}"
        )))
    })?;
    admin_verdict(&response, worker::Date::now().as_millis() / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps)] // The argument of `from_permissions`.
    const fn p(read: bool, write: bool) -> Option<Permissions> {
        Some(Permissions {
            read,
            write,
            admin: false,
        })
    }

    #[test]
    fn public_repo_is_readable_signed_out() {
        assert_eq!(from_permissions(false, None), Access::Read);
    }

    #[test]
    fn private_repo_needs_pull() {
        assert_eq!(from_permissions(true, None), Access::None);
        assert_eq!(from_permissions(true, p(false, false)), Access::None);
        assert_eq!(from_permissions(true, p(true, false)), Access::Read);
    }

    #[test]
    fn push_permission_writes() {
        assert_eq!(from_permissions(true, p(true, true)), Access::Write);
        assert_eq!(from_permissions(false, p(true, true)), Access::Write);
    }

    #[test]
    fn a_public_repo_stays_readable_when_github_gives_the_user_nothing() {
        // An expired or revoked user token answers 401: no permissions.
        assert_eq!(from_permissions(false, p(false, false)), Access::Read);
    }

    fn answer(status: u16, headers: &[(&str, &str)], body: &str) -> hatchrun_cloud::http::Response {
        hatchrun_cloud::http::Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn rate_limits_are_retried_not_denied() {
        let limited = answer(
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1090"),
            ],
            "",
        );
        assert_eq!(classify(&limited, 1000), Verdict::Retry(90));
        assert_eq!(classify(&answer(429, &[], ""), 1000), Verdict::Retry(60));
        let secondary = answer(403, &[], "You have exceeded a secondary rate limit");
        assert_eq!(classify(&secondary, 1000), Verdict::Retry(60));
    }

    #[test]
    fn other_answers_are_permissions_or_a_failure() {
        assert_eq!(classify(&answer(502, &[], ""), 0), Verdict::Broken(502));
        let none = Verdict::Known(Permissions {
            read: false,
            write: false,
            admin: false,
        });
        assert_eq!(classify(&answer(404, &[], ""), 0), none);
        assert_eq!(
            classify(&answer(403, &[], "Resource not accessible"), 0),
            none
        );
        let ok = answer(
            200,
            &[],
            r#"{"private": true, "permissions": {"pull": true, "push": true}}"#,
        );
        assert_eq!(
            classify(&ok, 0),
            Verdict::Known(Permissions {
                read: true,
                write: true,
                admin: false
            })
        );
    }

    #[test]
    fn permissions_are_kept_for_a_minute() {
        let key = (900_001, 900_002);
        assert_eq!(cached(key, 1000), None);
        let p = Permissions {
            read: true,
            write: false,
            admin: false,
        };
        remember(key, p, 1000);
        assert_eq!(cached(key, 1000 + CACHE_MS - 1), Some(p));
        assert_eq!(cached(key, 1000 + CACHE_MS), None);
        assert_eq!(cached((900_001, 5), 1000), None);
    }

    #[test]
    fn access_is_ordered() {
        assert!(Access::None < Access::Read && Access::Read < Access::Write);
        assert!(Access::Write < Access::Admin);
    }

    #[test]
    fn admin_needs_the_admin_permission() {
        let admin = Some(Permissions {
            read: true,
            write: true,
            admin: true,
        });
        assert_eq!(from_permissions(true, admin), Access::Admin);
        assert_eq!(from_permissions(false, admin), Access::Admin);
        assert_eq!(from_permissions(true, p(true, true)), Access::Write);
        assert_eq!(from_permissions(false, None), Access::Read);
    }

    #[test]
    fn only_an_active_admin_passes_and_names_the_organization() {
        let ok = |role: &str, state: &str, id: u64| {
            answer(
                200,
                &[],
                &format!(
                    r#"{{"role":"{role}","state":"{state}","organization":{{"id":{id},"login":"acme","avatar_url":""}}}}"#
                ),
            )
        };
        assert_eq!(
            admin_verdict(&ok("admin", "active", 42), 0).unwrap(),
            Some(42)
        );
        // A new organization that reuses a login has another id, so it
        // cannot match the tenant of the old one.
        assert_ne!(
            admin_verdict(&ok("admin", "active", 43), 0).unwrap(),
            Some(42)
        );
        assert_eq!(admin_verdict(&ok("member", "active", 42), 0).unwrap(), None);
        assert_eq!(admin_verdict(&ok("admin", "pending", 42), 0).unwrap(), None);
        assert_eq!(admin_verdict(&answer(404, &[], ""), 0).unwrap(), None);
        assert_eq!(
            admin_verdict(&answer(403, &[], "Resource not accessible"), 0).unwrap(),
            None
        );
        assert!(matches!(
            admin_verdict(&answer(429, &[], ""), 1000),
            Err(Failure::Retry(60))
        ));
        assert!(matches!(
            admin_verdict(&answer(502, &[], ""), 0),
            Err(Failure::Worker(_))
        ));
    }
}
