// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a run may be given.
//!
//! Every run gets one [`Trust`] at intake, stored on its row: `fork` for
//! a pull request whose head repository is not the base repository,
//! `trusted` for everything else. The rest of the control plane reads
//! `RunRow.trust` and never derives it from a payload, so a modified
//! agent or a modified workflow cannot ask for more than the row says.
//!
//! The code that hands out a secret, a token, an OIDC identity, an
//! environment or a cache scope asks [`Trust::allows`] first. A new
//! [`Capability`] needs a new arm in the match there, so the compiler
//! finds every place that has to decide.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::intake::PullRequest;

/// Who wrote the code a run executes, as far as intake can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// The repository's own code: a push, a pull request from the same
    /// repository, a schedule, a dispatch and the like.
    Trusted,
    /// A pull request from another repository: code the maintainers
    /// have not reviewed.
    Fork,
}

/// Something a run may be given or may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Organization, repository and environment secrets.
    Secrets,
    /// A GitHub token with any write permission, or a permission above
    /// the fork ceiling.
    WriteToken,
    /// OIDC identity tokens.
    IdToken,
    /// An environment and its rules, secrets and vars.
    Environments,
    /// Saving cache entries into a scope other than the run's own pull
    /// request.
    CacheWriteBase,
    /// Registry credentials for job containers and services.
    RegistryCredentials,
}

/// The App permissions a fork run's token may hold, and at most which
/// level. The workflow's `permissions:` can ask for less, never more.
/// `pull_requests: read` stays because `gh pr view` and paths filters
/// need it.
pub const FORK_CEILING: [(&str, &str); 3] = [
    ("contents", "read"),
    ("pull_requests", "read"),
    ("metadata", "read"),
];

impl Trust {
    /// The trust of a pull request delivery.
    #[must_use]
    pub fn of(pr: &PullRequest) -> Self {
        if pr.fork.is_some() {
            Self::Fork
        } else {
            Self::Trusted
        }
    }

    /// The word the `trust` columns store.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Fork => "fork",
        }
    }

    /// Whether a run of this trust may use `capability`.
    #[must_use]
    pub const fn allows(self, capability: Capability) -> bool {
        match self {
            Self::Trusted => true,
            Self::Fork => match capability {
                Capability::Secrets
                | Capability::WriteToken
                | Capability::IdToken
                | Capability::Environments
                | Capability::CacheWriteBase
                | Capability::RegistryCredentials => false,
            },
        }
    }
}

fn rank(level: &str) -> u8 {
    match level {
        "read" => 1,
        "write" => 2,
        "admin" => 3,
        _ => 0,
    }
}

/// The permissions `asked` gives a run of `trust`: all of them for a
/// trusted run, and for a fork run only the scopes of [`FORK_CEILING`],
/// each at the lower of the asked level and the ceiling's. `id_token`
/// and every write level are gone.
#[must_use]
pub fn limit_permissions(
    trust: Trust,
    asked: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    if trust == Trust::Trusted {
        return asked;
    }
    FORK_CEILING
        .iter()
        .filter_map(|(scope, ceiling)| {
            let level = asked.get(*scope)?;
            let capped = if rank(level) <= rank(ceiling) {
                level.as_str()
            } else {
                ceiling
            };
            (rank(capped) > 0).then(|| ((*scope).to_owned(), capped.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_trusted_run_may_do_everything() {
        for capability in [
            Capability::Secrets,
            Capability::WriteToken,
            Capability::IdToken,
            Capability::Environments,
            Capability::CacheWriteBase,
            Capability::RegistryCredentials,
        ] {
            assert!(Trust::Trusted.allows(capability), "{capability:?}");
            assert!(!Trust::Fork.allows(capability), "{capability:?}");
        }
    }

    #[test]
    fn the_words_are_the_ones_the_columns_store() {
        assert_eq!(Trust::Trusted.as_str(), "trusted");
        assert_eq!(Trust::Fork.as_str(), "fork");
        assert_eq!(
            serde_json::from_str::<Trust>("\"fork\"").unwrap(),
            Trust::Fork
        );
        assert_eq!(
            serde_json::to_string(&Trust::Trusted).unwrap(),
            "\"trusted\""
        );
    }

    #[test]
    fn a_fork_token_never_rises_above_the_ceiling() {
        let wide = asked(&[
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "write"),
            ("checks", "write"),
            ("statuses", "write"),
            ("actions", "write"),
            ("packages", "write"),
            ("id_token", "write"),
            ("metadata", "read"),
        ]);
        assert_eq!(
            limit_permissions(Trust::Fork, wide),
            asked(&[
                ("contents", "read"),
                ("metadata", "read"),
                ("pull_requests", "read")
            ])
        );
    }

    #[test]
    fn a_workflow_can_lower_a_fork_token_further() {
        let narrow = asked(&[("contents", "read")]);
        assert_eq!(
            limit_permissions(Trust::Fork, narrow),
            asked(&[("contents", "read")])
        );
        // A scope the workflow did not ask for is not added.
        assert!(limit_permissions(Trust::Fork, BTreeMap::new()).is_empty());
        assert!(
            limit_permissions(Trust::Fork, asked(&[("contents", "none")])).is_empty(),
            "none is no permission"
        );
    }

    #[test]
    fn a_trusted_token_keeps_what_it_asked() {
        let wide = asked(&[("contents", "write"), ("id_token", "write")]);
        assert_eq!(limit_permissions(Trust::Trusted, wide.clone()), wide);
    }
}
