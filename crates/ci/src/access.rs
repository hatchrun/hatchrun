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
}

/// The access GitHub's answer gives. `p` is `None` when nobody is
/// signed in. A public repository is at least `Read` either way; a
/// private one needs the permissions to say so.
#[must_use]
pub fn from_permissions(private: bool, p: Option<Permissions>) -> Access {
    let public = if private { Access::None } else { Access::Read };
    match p {
        Some(p) if p.write => Access::Write,
        Some(p) if p.read => Access::Read,
        _ => public,
    }
}

/// The access the request has to `repo`.
///
/// # Errors
///
/// The session lookup or the call to GitHub failed. The caller answers
/// 500, never a guess in either direction.
pub async fn check(
    req: &Request,
    db: &Db,
    settings: &Settings,
    repo: &RepoRow,
) -> worker::Result<Access> {
    let session = Session::find(req, db).await?;
    resolve(session.as_ref(), settings, repo).await
}

/// The access of a session found already, `None` when nobody is signed
/// in.
///
/// # Errors
///
/// The call to GitHub failed.
pub async fn resolve(
    session: Option<&Session>,
    settings: &Settings,
    repo: &RepoRow,
) -> worker::Result<Access> {
    let Some(session) = session else {
        return Ok(from_permissions(repo.private, None));
    };
    let request = github::user_repo(&settings.github_api, &repo.full_name, &session.token);
    let response = send(&request)
        .await
        .map_err(|err| worker::Error::RustError(format!("{}: {err}", repo.full_name)))?;
    // GitHub failing is not "no access": a private repository would flip
    // to hidden and a signed-in reader would see it vanish.
    if response.status >= 500 {
        return Err(worker::Error::RustError(format!(
            "{}: GitHub answered {}",
            repo.full_name, response.status
        )));
    }
    Ok(from_permissions(
        repo.private,
        Some(github::permissions(&response)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps)] // The argument of `from_permissions`.
    const fn p(read: bool, write: bool) -> Option<Permissions> {
        Some(Permissions { read, write })
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

    #[test]
    fn access_is_ordered() {
        assert!(Access::None < Access::Read && Access::Read < Access::Write);
    }
}
