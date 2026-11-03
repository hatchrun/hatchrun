// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `permissions:` for a job's token: what the workflow asks for and the
//! GitHub App permissions it turns into.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

/// How much of a scope the token may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// No access.
    None,
    /// Read access.
    Read,
    /// Read and write access.
    Write,
}

impl Level {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(Self::None),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// `permissions:` as written: `read-all`, `write-all` or a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permissions {
    /// `read-all`: every scope at read.
    ReadAll,
    /// `write-all`: every scope at write.
    WriteAll,
    /// A map of workflow keys (see [`SCOPES`]) to levels.
    Scopes(BTreeMap<String, Level>),
}

/// Workflow keys and the GitHub App permission each one asks for.
pub const SCOPES: &[(&str, &str)] = &[
    ("actions", "actions"),
    ("attestations", "attestations"),
    ("checks", "checks"),
    ("contents", "contents"),
    ("deployments", "deployments"),
    ("discussions", "discussions"),
    ("issues", "issues"),
    ("packages", "packages"),
    ("pages", "pages"),
    ("pull-requests", "pull_requests"),
    ("repository-projects", "repository_projects"),
    ("security-events", "security_events"),
    ("statuses", "statuses"),
];

/// The workflow key of the OIDC permission. It is not a GitHub App
/// permission, so it is not in [`SCOPES`].
pub const ID_TOKEN: &str = "id-token";

/// The key [`token_scopes`] uses for `id-token: write`. It is kept out
/// of the permissions asked of GitHub.
pub const ID_TOKEN_SCOPE: &str = "id_token";

/// The token without any `permissions:`: Plan 3's read set.
pub const DEFAULT_SCOPES: &[(&str, Level)] = &[
    ("checks", Level::Read),
    ("contents", Level::Read),
    ("metadata", Level::Read),
    ("pull_requests", Level::Read),
];

/// App permission names and levels for a job's token. Always holds
/// `metadata: read`; `none` levels are left out. `id-token: write`
/// (and `write-all`) adds [`ID_TOKEN_SCOPE`], which is not an App
/// permission: it says the job may ask for OIDC tokens.
#[must_use]
pub fn token_scopes(p: Option<&Permissions>) -> BTreeMap<String, Level> {
    let mut out = BTreeMap::new();
    match p {
        None => {
            out.extend(DEFAULT_SCOPES.iter().map(|(k, l)| ((*k).to_owned(), *l)));
        }
        Some(Permissions::ReadAll) => {
            out.extend(
                SCOPES
                    .iter()
                    .map(|(_, app)| ((*app).to_owned(), Level::Read)),
            );
        }
        Some(Permissions::WriteAll) => {
            out.extend(
                SCOPES
                    .iter()
                    .map(|(_, app)| ((*app).to_owned(), Level::Write)),
            );
            out.insert(ID_TOKEN_SCOPE.to_owned(), Level::Write);
        }
        Some(Permissions::Scopes(map)) => {
            for (key, level) in map {
                if *level == Level::None {
                    continue;
                }
                if key == ID_TOKEN {
                    out.insert(ID_TOKEN_SCOPE.to_owned(), *level);
                    continue;
                }
                if let Some((_, app)) = SCOPES.iter().find(|(k, _)| k == key) {
                    out.insert((*app).to_owned(), *level);
                }
            }
        }
    }
    out.insert("metadata".to_owned(), Level::Read);
    out
}

fn scope_list() -> String {
    SCOPES
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Serialize for Permissions {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::ReadAll => s.serialize_str("read-all"),
            Self::WriteAll => s.serialize_str("write-all"),
            Self::Scopes(map) => {
                let mut out = s.serialize_map(Some(map.len()))?;
                for (key, level) in map {
                    out.serialize_entry(key, level.word())?;
                }
                out.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Permissions;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`read-all`, `write-all` or a map of scopes to levels")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Permissions, E> {
                match v {
                    "read-all" => Ok(Permissions::ReadAll),
                    "write-all" => Ok(Permissions::WriteAll),
                    other => Err(E::custom(format!(
                        "`permissions` is `{other}`; use read-all, write-all or a map of scopes"
                    ))),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Permissions, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, level)) = map.next_entry::<String, String>()? {
                    if key == ID_TOKEN {
                        let level = match Level::parse(&level) {
                            Some(Level::Read) => {
                                return Err(de::Error::custom(
                                    "`permissions.id-token` is `read`; use write or none",
                                ));
                            }
                            Some(parsed) => parsed,
                            None => {
                                return Err(de::Error::custom(format!(
                                    "`permissions.id-token` is `{level}`; use write or none"
                                )));
                            }
                        };
                        out.insert(key, level);
                        continue;
                    }
                    if !SCOPES.iter().any(|(k, _)| *k == key) {
                        return Err(de::Error::custom(format!(
                            "`permissions` names unknown scope `{key}`; use one of {}",
                            scope_list()
                        )));
                    }
                    let Some(parsed) = Level::parse(&level) else {
                        return Err(de::Error::custom(format!(
                            "`permissions.{key}` is `{level}`; use read, write or none"
                        )));
                    };
                    out.insert(key, parsed);
                }
                Ok(Permissions::Scopes(out))
            }
        }

        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Permissions, String> {
        serde_saphyr::from_str::<Permissions>(yaml).map_err(|e| e.to_string())
    }

    fn scopes(yaml: &str) -> BTreeMap<String, Level> {
        token_scopes(Some(&parse(yaml).unwrap()))
    }

    fn map(entries: &[(&str, Level)]) -> BTreeMap<String, Level> {
        entries.iter().map(|(k, l)| ((*k).to_owned(), *l)).collect()
    }

    #[test]
    fn no_permissions_is_the_read_default() {
        assert_eq!(
            token_scopes(None),
            map(&[
                ("checks", Level::Read),
                ("contents", Level::Read),
                ("metadata", Level::Read),
                ("pull_requests", Level::Read),
            ])
        );
    }

    #[test]
    fn read_all_reads_every_scope() {
        let got = scopes("read-all");
        assert_eq!(got.len(), 14);
        assert!(got.values().all(|l| *l == Level::Read));
    }

    #[test]
    fn write_all_writes_every_scope() {
        let got = scopes("write-all");
        assert_eq!(got.len(), 15);
        assert_eq!(got["metadata"], Level::Read);
        assert_eq!(got["id_token"], Level::Write);
        assert_eq!(got.values().filter(|l| **l == Level::Write).count(), 14);
    }

    #[test]
    fn empty_map_leaves_metadata() {
        assert_eq!(scopes("{}"), map(&[("metadata", Level::Read)]));
    }

    #[test]
    fn none_level_drops_the_scope() {
        assert_eq!(scopes("contents: none"), map(&[("metadata", Level::Read)]));
    }

    #[test]
    fn scope_names_become_app_names() {
        assert_eq!(
            scopes("pull-requests: write\nsecurity-events: read"),
            map(&[
                ("metadata", Level::Read),
                ("pull_requests", Level::Write),
                ("security_events", Level::Read),
            ])
        );
    }

    #[test]
    fn unknown_scope_is_an_error() {
        let err = parse("foo: read").unwrap_err();
        assert!(
            err.contains("`permissions` names unknown scope `foo`; use one of actions, attestations, checks, contents, deployments, discussions, issues, packages, pages, pull-requests, repository-projects, security-events, statuses"),
            "{err}"
        );
    }

    #[test]
    fn unknown_level_is_an_error() {
        let err = parse("contents: admin").unwrap_err();
        assert!(
            err.contains("`permissions.contents` is `admin`; use read, write or none"),
            "{err}"
        );
    }

    #[test]
    fn id_token_write_grants_oidc_tokens() {
        let got = scopes("id-token: write\ncontents: read");
        assert_eq!(got.get("id_token"), Some(&Level::Write));
        assert_eq!(got.get("contents"), Some(&Level::Read));
        assert!(!scopes("id-token: none").contains_key("id_token"));
        assert!(!scopes("contents: read").contains_key("id_token"));
    }

    #[test]
    fn write_all_grants_oidc_tokens_and_read_all_does_not() {
        assert_eq!(
            token_scopes(Some(&Permissions::WriteAll)).get("id_token"),
            Some(&Level::Write)
        );
        assert!(!token_scopes(Some(&Permissions::ReadAll)).contains_key("id_token"));
        assert!(!token_scopes(None).contains_key("id_token"));
    }

    #[test]
    fn id_token_read_is_refused() {
        let err = parse("id-token: read").unwrap_err();
        assert!(
            err.contains("`permissions.id-token` is `read`; use write or none"),
            "{err}"
        );
        let err = parse("id-token: admin").unwrap_err();
        assert!(err.contains("`permissions.id-token` is `admin`"), "{err}");
    }

    #[test]
    fn permissions_round_trip() {
        for text in [
            "read-all",
            "write-all",
            "contents: read\npull-requests: none",
        ] {
            let parsed = parse(text).unwrap();
            let yaml = serde_saphyr::to_string(&parsed).unwrap();
            assert_eq!(parse(&yaml).unwrap(), parsed, "{text}");
        }
    }
}
