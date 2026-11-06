// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The repositories of a tenant's App installation: read from GitHub
//! into `ci_installation_repos`, when the `installation_repositories`
//! webhook says they changed, and when a list finds the table empty or
//! old.

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use worker::{Env, Result};

use crate::alerts;
use crate::env::Settings;
use crate::github;
use crate::store;

/// How long the stored list is trusted without a webhook, in
/// milliseconds.
pub const STALE_MS: u64 = 60 * 60 * 1000;

/// Pages of repositories that are read, a hundred each.
const MAX_PAGES: u32 = 30;

/// Whether the list read at `synced_at` (never, for `None`) has to be
/// read again at `now`. `total` is the repositories it holds.
#[must_use]
pub fn due(synced_at: Option<u64>, total: u64, now: u64) -> bool {
    total == 0 || synced_at.is_none_or(|at| now.saturating_sub(at) >= STALE_MS)
}

/// Reads the repositories of `installation_id` from GitHub and stores
/// them as those of `tenant`. Returns how many there are.
///
/// # Errors
///
/// GitHub or Postgres failed.
pub async fn sync(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tenant: &str,
    installation_id: u64,
    now: u64,
) -> Result<usize> {
    let counted = Counted::default();
    let read = read(env, db, settings, tenant, installation_id, now, &counted).await;
    // The calls made are noted even when the read failed.
    alerts::note_rate_limits(db, &counted, now).await;
    read
}

async fn read(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tenant: &str,
    installation_id: u64,
    now: u64,
    counted: &Counted,
) -> Result<usize> {
    let tokens = Tokens::new(
        Kv::new(env),
        App::new(
            settings.github_api.clone(),
            settings.app_id.clone(),
            settings.app_key.clone(),
        ),
        counted,
        now,
    );
    let mut repos = Vec::new();
    for page in 1..=MAX_PAGES {
        let response = tokens
            .send(installation_id, |token| {
                github::installation_repos(&settings.github_api, page, token)
            })
            .await
            .map_err(|err| worker::Error::RustError(err.to_string()))?;
        let (found, more) = github::installed_repos(&response).map_err(worker::Error::RustError)?;
        repos.extend(found);
        if !more {
            break;
        }
    }
    store::sync_installation(db, tenant, &repos, now).await?;
    Ok(repos.len())
}

/// Reads the list when [`due`], for a tenant that has an installation.
/// A failure leaves the stored list as it is and is logged: a list
/// that is a little old beats no answer.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn ensure(env: &Env, db: &Db, settings: &Settings, tenant: &str, now: u64) -> Result<()> {
    let (synced_at, total) = store::installation_synced(db, tenant).await?;
    if !due(synced_at, total, now) {
        return Ok(());
    }
    let Some(installation_id) = store::tenant_installation(db, tenant).await? else {
        return Ok(());
    };
    if let Err(err) = sync(env, db, settings, tenant, installation_id, now).await {
        worker::console_warn!("tenant {tenant}: installation repositories: {err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_old_list_is_read_again() {
        let now = 10 * STALE_MS;
        assert!(due(None, 5, now));
        assert!(due(Some(now), 0, now));
        assert!(due(Some(now - STALE_MS), 5, now));
        assert!(!due(Some(now - STALE_MS + 1), 5, now));
    }
}
