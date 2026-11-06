// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `docker` command lines the agent builds, and the output it parses.
//! Pure functions, so tests need no Docker.

use std::collections::BTreeMap;
use std::path::Path;

/// What `docker create` or `docker run` gets, besides the user's
/// `options`.
#[derive(Debug, Default, Clone)]
pub struct Create<'a> {
    pub name: &'a str,
    pub label: &'a str,
    pub network: Option<&'a str>,
    /// The hostname other containers on the network reach it by.
    pub alias: Option<&'a str>,
    pub workdir: Option<&'a str>,
    /// `-e` flags, from `env_flags`: values travel in the docker
    /// client's environment, not on its command line.
    pub env: Vec<String>,
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    /// The user's `options`, already split.
    pub options: Vec<String>,
    pub entrypoint: Option<&'a str>,
    pub image: &'a str,
    /// The command and arguments after the image.
    pub command: Vec<String>,
}

impl Create<'_> {
    /// The arguments after `docker <verb>`. The agent's options come
    /// first and the user's `options` after them, so `--cpus` and its
    /// kind apply and a repeated flag takes the user's value; the
    /// entrypoint and image come last.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        let mut out = vec![
            "--name".to_owned(),
            self.name.to_owned(),
            "--label".to_owned(),
            self.label.to_owned(),
        ];
        if let Some(workdir) = self.workdir {
            out.extend(["--workdir".to_owned(), workdir.to_owned()]);
        }
        if let Some(network) = self.network {
            out.extend(["--network".to_owned(), network.to_owned()]);
        }
        if let Some(alias) = self.alias {
            out.extend(["--network-alias".to_owned(), alias.to_owned()]);
        }
        out.extend(self.env.iter().cloned());
        for port in &self.ports {
            out.extend(["-p".to_owned(), port.clone()]);
        }
        for volume in &self.volumes {
            out.extend(["-v".to_owned(), volume.clone()]);
        }
        out.extend(self.options.iter().cloned());
        if let Some(entrypoint) = self.entrypoint {
            out.extend(["--entrypoint".to_owned(), entrypoint.to_owned()]);
        }
        out.push(self.image.to_owned());
        out.extend(self.command.iter().cloned());
        out
    }
}

/// The registry an image is pulled from: its first path part when that
/// looks like a host (a dot, a port or `localhost`), otherwise Docker
/// Hub, which is `None`.
#[must_use]
pub fn registry_of(image: &str) -> Option<&str> {
    let (first, _) = image.split_once('/')?;
    (first.contains('.') || first.contains(':') || first == "localhost").then_some(first)
}

/// What `docker login` is given for a registry. The password goes on
/// standard input, never the command line.
#[must_use]
pub fn login_args(registry: Option<&str>, username: &str) -> Vec<String> {
    let mut out = vec!["login".to_owned()];
    out.extend(registry.map(str::to_owned));
    out.extend([
        "--username".to_owned(),
        username.to_owned(),
        "--password-stdin".to_owned(),
    ]);
    out
}

/// The host ports `docker port` printed, keyed by container port:
/// `5432` for a TCP port and `5432/udp` otherwise. Lines look like
/// `5432/tcp -> 0.0.0.0:49153`; a port that is published on IPv4 and
/// IPv6 has two, and the first one counts.
#[must_use]
pub fn parse_ports(output: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in output.lines() {
        let Some((container, host)) = line.split_once("->") else {
            continue;
        };
        let container = container.trim();
        let key = container.strip_suffix("/tcp").unwrap_or(container);
        let Some(port) = host.trim().rsplit(':').next().filter(|p| !p.is_empty()) else {
            continue;
        };
        out.entry(key.to_owned()).or_insert_with(|| port.to_owned());
    }
    out
}

/// What `docker inspect` says of a container's health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The image and the options define no health check.
    None,
    /// The check has not passed yet.
    Starting,
    /// The last checks passed.
    Healthy,
    /// The check ran out of retries.
    Unhealthy,
}

/// Reads the output of `docker inspect --format` with
/// [`HEALTH_FORMAT`].
#[must_use]
pub fn parse_health(output: &str) -> Health {
    match output.trim() {
        "healthy" => Health::Healthy,
        "unhealthy" => Health::Unhealthy,
        "" | "<no value>" => Health::None,
        _ => Health::Starting,
    }
}

/// The `--format` that prints the health status, or nothing for a
/// container with no health state. That includes an image whose check is
/// `NONE`, which `--no-healthcheck` sets and which has no `State.Health`.
pub const HEALTH_FORMAT: &str = "{{if .State.Health}}{{print .State.Health.Status}}{{end}}";

/// Whether a volume's source names a Docker volume rather than a path:
/// `mydata:/data` does, `/data:/data` and `./data:/data` do not.
#[must_use]
pub fn named_volume(volume: &str) -> Option<&str> {
    let source = volume.split(':').next()?;
    let named = !source.is_empty()
        && !source.starts_with('/')
        && !source.starts_with('.')
        && !source.starts_with('~')
        && volume.contains(':');
    named.then_some(source)
}

/// Replaces each host directory in `text` with where the container
/// sees it. `maps` is tried longest host path first by the caller. A
/// path matches only whole (it ends at a `/`, a `:` or the end) and only
/// where a path starts: at the start of the text or after `:`, `=`, a
/// quote or whitespace, so `/usr/w/x` stays when `/w` is mapped.
#[must_use]
pub fn translate(text: &str, maps: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        let starts_path = out
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ':' | '=' | '"' | '\'' | ';'));
        for (host, container) in maps {
            if host.is_empty() || !starts_path {
                continue;
            }
            if let Some(after) = rest.strip_prefix(host.as_str())
                && (after.is_empty() || after.starts_with('/') || after.starts_with(':'))
            {
                out.push_str(container);
                rest = after;
                continue 'outer;
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    out
}

/// The same for a path, as a path.
#[must_use]
pub fn translate_path(path: &Path, maps: &[(String, String)]) -> String {
    translate(&path.to_string_lossy(), maps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_orders_agent_options_then_the_users_then_the_image() {
        let create = Create {
            name: "c1",
            label: "hatchrun-1",
            network: Some("net"),
            alias: Some("db"),
            workdir: Some("/__w/repo/repo"),
            env: vec!["-e".into(), "A".into()],
            ports: vec!["5432:5432".into()],
            volumes: vec!["/a:/b".into()],
            options: vec!["--cpus".into(), "1".into()],
            entrypoint: Some("tail"),
            image: "postgres:17",
            command: vec!["-f".into(), "/dev/null".into()],
        };
        assert_eq!(
            create.args().join(" "),
            "--name c1 --label hatchrun-1 --workdir /__w/repo/repo --network net \
             --network-alias db -e A -p 5432:5432 -v /a:/b --cpus 1 \
             --entrypoint tail postgres:17 -f /dev/null"
        );
    }

    #[test]
    fn registries() {
        assert_eq!(registry_of("postgres:17"), None);
        assert_eq!(registry_of("library/redis"), None);
        assert_eq!(registry_of("acme/build:1"), None);
        assert_eq!(registry_of("ghcr.io/acme/build:1"), Some("ghcr.io"));
        assert_eq!(registry_of("localhost:5000/x"), Some("localhost:5000"));
        assert_eq!(registry_of("localhost/x"), Some("localhost"));
        assert_eq!(
            registry_of("123.dkr.ecr.eu-west-1.amazonaws.com/p"),
            Some("123.dkr.ecr.eu-west-1.amazonaws.com")
        );
    }

    #[test]
    fn login_reads_the_password_from_stdin() {
        assert_eq!(
            login_args(Some("ghcr.io"), "bot"),
            ["login", "ghcr.io", "--username", "bot", "--password-stdin"]
        );
        assert_eq!(
            login_args(None, "bot"),
            ["login", "--username", "bot", "--password-stdin"]
        );
    }

    #[test]
    fn ports_take_the_first_host_mapping() {
        let out = "5432/tcp -> 0.0.0.0:49153\n5432/tcp -> [::]:49153\n53/udp -> 0.0.0.0:5353\n";
        let ports = parse_ports(out);
        assert_eq!(ports["5432"], "49153");
        assert_eq!(ports["53/udp"], "5353");
        assert_eq!(ports.len(), 2);
        assert!(parse_ports("").is_empty());
    }

    #[test]
    fn health_statuses() {
        assert_eq!(parse_health(""), Health::None);
        assert_eq!(parse_health("\n"), Health::None);
        assert_eq!(parse_health("starting\n"), Health::Starting);
        assert_eq!(parse_health("healthy\n"), Health::Healthy);
        assert_eq!(parse_health("unhealthy\n"), Health::Unhealthy);
    }

    #[test]
    fn volumes_that_are_named() {
        assert_eq!(named_volume("data:/var/lib"), Some("data"));
        assert_eq!(named_volume("/host:/c"), None);
        assert_eq!(named_volume("./rel:/c"), None);
        assert_eq!(named_volume("/c"), None);
    }

    #[test]
    fn translation_follows_whole_path_parts() {
        let maps = vec![
            (
                "/w/_temp/_github_workflow".to_owned(),
                "/github/workflow".to_owned(),
            ),
            ("/w".to_owned(), "/__w".to_owned()),
        ];
        assert_eq!(
            translate("/w/_temp/_github_workflow/event.json", &maps),
            "/github/workflow/event.json"
        );
        assert_eq!(translate("/w/repo/repo", &maps), "/__w/repo/repo");
        assert_eq!(translate("/wx/repo", &maps), "/wx/repo");
        assert_eq!(translate("a:/w/b:/usr/bin", &maps), "a:/__w/b:/usr/bin");
        assert_eq!(translate("/w", &maps), "/__w");
        assert_eq!(translate("/usr/bin:/w/bin", &maps), "/usr/bin:/__w/bin");
        assert_eq!(translate("/usr/w/x", &maps), "/usr/w/x");
        assert_eq!(translate("cd '/w/repo'", &maps), "cd '/__w/repo'");
    }
}
