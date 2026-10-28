// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The URL that names where runners register.

use std::fmt;
use std::str::FromStr;

use url::Url;

use crate::Error;

/// Where runners register: an enterprise, an organization or a
/// repository, on github.com, GHE.com or GitHub Enterprise Server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigUrl {
    url: Url,
    scope: Scope,
}

/// What a [`ConfigUrl`] names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `https://github.com/enterprises/{enterprise}`
    Enterprise(String),
    /// `https://github.com/{organization}`
    Organization(String),
    /// `https://github.com/{owner}/{repository}`
    Repository {
        /// The user or organization that owns the repository.
        owner: String,
        /// The repository name.
        repository: String,
    },
}

impl ConfigUrl {
    /// Parses a URL such as `https://github.com/hatchrun`.
    ///
    /// # Errors
    ///
    /// [`Error::ConfigUrl`] when the URL does not parse or its path does
    /// not name an enterprise, organization or repository.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let invalid = |reason| Error::ConfigUrl {
            url: input.to_owned(),
            reason,
        };
        // The URL goes into the registration request as given, and the
        // service does not match it with a trailing slash.
        let url =
            Url::parse(input.trim().trim_end_matches('/')).map_err(|_| invalid("not a URL"))?;
        let Some(host) = url.host_str() else {
            return Err(invalid("no host"));
        };
        // Tokens and signed JWTs go to this host, so plain HTTP is only
        // for a local test server.
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(invalid("must use https"));
        }
        let parts: Vec<&str> = url
            .path_segments()
            .map(|segments| segments.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let scope = match parts.as_slice() {
            [organization] => Scope::Organization((*organization).to_owned()),
            [first, enterprise] if first.eq_ignore_ascii_case("enterprises") => {
                Scope::Enterprise((*enterprise).to_owned())
            }
            [owner, repository] => Scope::Repository {
                owner: (*owner).to_owned(),
                repository: (*repository).to_owned(),
            },
            _ => {
                return Err(invalid(
                    "path must be /{org}, /{owner}/{repo} or /enterprises/{enterprise}",
                ));
            }
        };
        Ok(Self { url, scope })
    }

    /// Returns the URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// Returns what the URL names.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Returns the REST API URL for `path`, which starts with `/`.
    pub(crate) fn api_url(&self, path: &str) -> Result<Url, Error> {
        let host = self.url.host_str().unwrap_or_default();
        let mut api = self.url.clone();
        api.set_query(None);
        if host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com") {
            api.set_host(Some("api.github.com"))?;
            api.set_path(path);
        } else if self.is_hosted() {
            api.set_host(Some(&format!("api.{host}")))?;
            api.set_path(path);
        } else {
            api.set_path(&format!("/api/v3{path}"));
        }
        Ok(api)
    }

    /// GitHub Enterprise Server serves the API under `/api/v3` on its own
    /// host. github.com and GHE.com serve it on an `api.` subdomain.
    fn is_hosted(&self) -> bool {
        let host = self.url.host_str().unwrap_or_default().to_ascii_lowercase();
        host == "github.com"
            || host == "www.github.com"
            || host == "github.localhost"
            || host.ends_with(".ghe.com")
    }
}

impl FromStr for ConfigUrl {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for ConfigUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(config: &str) -> String {
        ConfigUrl::parse(config)
            .unwrap()
            .api_url("/orgs/x")
            .unwrap()
            .to_string()
    }

    #[test]
    fn requires_https_except_loopback() {
        assert!(ConfigUrl::parse("http://github.com/hatchrun").is_err());
        assert!(ConfigUrl::parse("ftp://github.com/hatchrun").is_err());
        ConfigUrl::parse("http://127.0.0.1:8080/hatchrun").unwrap();
        ConfigUrl::parse("http://localhost/hatchrun").unwrap();
    }

    #[test]
    fn parses_scopes() {
        assert_eq!(
            ConfigUrl::parse("https://github.com/hatchrun/")
                .unwrap()
                .scope(),
            &Scope::Organization("hatchrun".into())
        );
        assert_eq!(
            ConfigUrl::parse("https://github.com/hatchrun/hatchrun")
                .unwrap()
                .scope(),
            &Scope::Repository {
                owner: "hatchrun".into(),
                repository: "hatchrun".into()
            }
        );
        assert_eq!(
            ConfigUrl::parse("https://github.com/Enterprises/acme")
                .unwrap()
                .scope(),
            &Scope::Enterprise("acme".into())
        );
    }

    #[test]
    fn drops_trailing_slash() {
        assert_eq!(
            ConfigUrl::parse("https://github.com/hatchrun/")
                .unwrap()
                .as_str(),
            "https://github.com/hatchrun"
        );
    }

    #[test]
    fn rejects_bad_paths() {
        for url in [
            "https://github.com",
            "https://github.com/a/b/c",
            "not a url",
        ] {
            assert!(
                matches!(ConfigUrl::parse(url), Err(Error::ConfigUrl { .. })),
                "{url}"
            );
        }
    }

    #[test]
    fn api_urls() {
        assert_eq!(
            api("https://github.com/hatchrun"),
            "https://api.github.com/orgs/x"
        );
        assert_eq!(
            api("https://www.github.com/hatchrun"),
            "https://api.github.com/orgs/x"
        );
        assert_eq!(
            api("https://acme.ghe.com/hatchrun"),
            "https://api.acme.ghe.com/orgs/x"
        );
        assert_eq!(
            api("https://git.example.com:8443/hatchrun"),
            "https://git.example.com:8443/api/v3/orgs/x"
        );
    }
}
