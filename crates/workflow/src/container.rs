// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What the planner and the agent both know about `container:` and
//! `services:`: how `options` splits into words and which options a
//! job may not set.

use crate::config::Container;

/// The `docker create` options the agent sets itself, so a job cannot.
/// GitHub refuses the same: the job's network and the container's name
/// belong to the runner.
const FORBIDDEN: &[&str] = &["--network", "--net", "--name"];

/// Splits `options` into words the way a shell does: whitespace
/// separates, single quotes keep everything, double quotes keep
/// everything but `\"` and `\\`, and a backslash outside quotes escapes
/// the next character.
///
/// # Errors
///
/// When a quote is not closed or a backslash ends the text.
pub fn split_options(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a single quote is not closed".to_owned()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("a double quote is not closed".to_owned()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("a double quote is not closed".to_owned()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("the text ends with a backslash".to_owned()),
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// The words of a container's `options` that the job may not set:
/// the network and the name. An option is forbidden in
/// both `--name x` and `--name=x` forms.
#[must_use]
pub fn forbidden_options(words: &[String]) -> Vec<&str> {
    words
        .iter()
        .filter_map(|word| {
            let name = word.split('=').next().unwrap_or(word);
            FORBIDDEN.iter().copied().find(|f| *f == name)
        })
        .collect()
}

/// The problems with the job container's `--network-alias` options: an
/// alias equal to a service id would make the hostname name two
/// containers. `what` starts each message; options that hold `${{ }}`
/// are not judged.
#[must_use]
pub fn alias_problems<'a>(
    what: &str,
    container: &Container,
    service_ids: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let Some(options) = container.options.as_deref().filter(|o| !o.contains("${{")) else {
        return Vec::new();
    };
    let Ok(words) = split_options(options) else {
        return Vec::new();
    };
    aliases_of_services(what, &words, service_ids)
}

/// [`alias_problems`] for options that are already split and evaluated.
#[must_use]
pub fn aliases_of_services<'a>(
    what: &str,
    words: &[String],
    service_ids: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let ids: Vec<&String> = service_ids.into_iter().collect();
    let mut out = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let alias = match word.split_once('=') {
            Some(("--network-alias" | "--net-alias", value)) => Some(value),
            None if matches!(word.as_str(), "--network-alias" | "--net-alias") => {
                words.get(i + 1).map(String::as_str)
            }
            _ => None,
        };
        if let Some(alias) = alias
            && ids.iter().any(|id| id.as_str() == alias)
        {
            out.push(format!(
                "{what} `options` sets `--network-alias {alias}`, which is the id of a service"
            ));
        }
    }
    out
}

/// Whether `id` can be a service id: it is the container's hostname on
/// the job's network and a key of the `job.services` context.
#[must_use]
pub fn valid_service_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && id.len() <= 63
}

/// The problems with a container as written, for messages that start
/// with its `what`. Texts that hold `${{ }}` are not judged, since
/// their value is known only when the job starts.
#[must_use]
pub fn problems(what: &str, container: &Container) -> Vec<String> {
    let mut out = Vec::new();
    let plain = |text: &str| !text.contains("${{");
    if container.image.trim().is_empty() {
        out.push(format!("{what} has an empty `image`"));
    } else if plain(&container.image) && container.image.contains(char::is_whitespace) {
        out.push(format!(
            "{what} has `image: {}`, which has whitespace",
            container.image
        ));
    }
    if let Some(credentials) = &container.credentials
        && (credentials.username.is_empty() || credentials.password.is_empty())
    {
        out.push(format!(
            "{what} `credentials` needs a `username` and a `password`"
        ));
    }
    for port in &container.ports {
        if plain(port) && port.trim().is_empty() {
            out.push(format!("{what} has an empty entry in `ports`"));
        }
    }
    for volume in &container.volumes {
        if plain(volume) && volume.trim().is_empty() {
            out.push(format!("{what} has an empty entry in `volumes`"));
        }
    }
    if let Some(options) = container.options.as_deref().filter(|o| plain(o)) {
        match split_options(options) {
            Ok(words) => {
                for name in forbidden_options(&words) {
                    out.push(format!(
                        "{what} `options` sets `{name}`, which hatchrun sets itself"
                    ));
                }
            }
            Err(e) => out.push(format!("{what} `options`: {e}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(
            split_options(r#"--health-cmd "pg_isready -U postgres" --cpus 1"#).unwrap(),
            ["--health-cmd", "pg_isready -U postgres", "--cpus", "1"]
        );
        assert_eq!(
            split_options(r"--env 'A B' a\ b").unwrap(),
            ["--env", "A B", "a b"]
        );
        assert_eq!(split_options("").unwrap(), Vec::<String>::new());
        assert_eq!(split_options("''").unwrap(), [""]);
        assert_eq!(split_options(r#"a"b c"d"#).unwrap(), ["ab cd"]);
        assert!(split_options("'open").is_err());
        assert!(split_options("\"open").is_err());
        assert!(split_options("end\\").is_err());
    }

    #[test]
    fn forbids_the_options_the_agent_sets() {
        let words = split_options("--network=host --cpus 1 --name x").unwrap();
        assert_eq!(forbidden_options(&words), ["--network", "--name"]);
        let none = split_options("--cpus 1").unwrap();
        assert_eq!(forbidden_options(&none).len(), 0);
    }

    #[test]
    fn a_job_alias_may_not_be_a_service_id() {
        let container = |options: &str| Container {
            image: "x".to_owned(),
            options: Some(options.to_owned()),
            ..Container::default()
        };
        let ids = [String::from("db")];
        assert_eq!(
            alias_problems("c", &container("--network-alias db"), &ids).len(),
            1
        );
        assert_eq!(
            alias_problems("c", &container("--net-alias=db --cpus 1"), &ids).len(),
            1
        );
        assert_eq!(
            alias_problems("c", &container("--network-alias web"), &ids).len(),
            0
        );
        assert_eq!(
            alias_problems("c", &container("--network-alias ${{ x }}"), &ids).len(),
            0
        );
        assert_eq!(alias_problems("c", &Container::default(), &ids).len(), 0);
    }

    #[test]
    fn service_ids_are_hostnames() {
        for ok in ["db", "pg_1", "Redis-cache", "_x"] {
            assert!(valid_service_id(ok), "{ok}");
        }
        for bad in ["", "1db", "a.b", "a b", "-x", &"x".repeat(64)] {
            assert!(!valid_service_id(bad), "{bad}");
        }
    }
}
