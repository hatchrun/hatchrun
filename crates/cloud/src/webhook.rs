// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! GitHub App webhooks: the signature check and the installation events
//! the Worker acts on, kept apart from the Workers runtime so they test
//! natively. The CI events are `hatchrun-ci`'s.

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Checks `X-Hub-Signature-256`, GitHub's HMAC-SHA256 of the body keyed
/// with the App's webhook secret. The comparison is constant time.
#[must_use]
pub fn verify(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(given) = header
        .and_then(|value| value.strip_prefix("sha256="))
        .and_then(decode_hex)
    else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(value.get(at..at + 2)?, 16).ok())
        .collect()
}

/// What a webhook asks the Worker to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The App was uninstalled or suspended; the tenant loses its
    /// installation and launches nothing until it is back.
    Uninstalled {
        /// The installation that is gone.
        installation_id: u64,
    },
    /// The App was installed, or unsuspended, on an account. A tenant
    /// for that account gets the installation back.
    Installed {
        /// The installation.
        installation_id: u64,
        /// The user or organization it is installed on.
        account_id: u64,
    },
    /// Nothing to do, and why. GitHub shows the reason in the App's
    /// delivery log.
    Ignored(String),
}

#[derive(Deserialize)]
struct InstalledOn {
    id: u64,
    account: Account,
}

#[derive(Deserialize)]
struct Account {
    id: u64,
}

/// Reads a webhook delivery. `event` is the `X-GitHub-Event` header.
///
/// # Errors
///
/// When an `installation` body does not parse.
pub fn parse(event: &str, body: &[u8]) -> Result<Event, serde_json::Error> {
    if event == "installation" {
        // GitHub sends these to every App; nobody subscribes to them.
        #[derive(Deserialize)]
        struct Changed {
            action: String,
            installation: InstalledOn,
        }
        let changed: Changed = serde_json::from_slice(body)?;
        let installation_id = changed.installation.id;
        return Ok(match changed.action.as_str() {
            "deleted" | "suspend" => Event::Uninstalled { installation_id },
            "created" | "unsuspend" => Event::Installed {
                installation_id,
                account_id: changed.installation.account.id,
            },
            other => Event::Ignored(format!("installation {other} is not used")),
        });
    }
    Ok(Event::Ignored(format!("{event} events are not used")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signature cases from `testdata/signatures.tsv`: secret, body,
    /// signature header and whether it verifies.
    fn signatures() -> Vec<(String, String, String, bool)> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/signatures.tsv");
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .enumerate()
            .map(|(case, line)| {
                let fields: Vec<&str> = line.split('\t').collect();
                let [secret, body, signature, valid] = fields[..] else {
                    panic!("case {case} in {path} needs four fields");
                };
                (
                    secret.to_owned(),
                    body.to_owned(),
                    signature.to_owned(),
                    valid == "true",
                )
            })
            .collect()
    }

    #[test]
    fn checks_every_case() {
        let cases = signatures();
        assert!(cases.len() >= 4);
        for (case, (secret, body, signature, valid)) in cases.into_iter().enumerate() {
            assert_eq!(
                verify(secret.as_bytes(), body.as_bytes(), Some(&signature)),
                valid,
                "case {case} in testdata/signatures.tsv"
            );
        }
    }

    #[test]
    fn accepts_upper_case_hex() {
        let (secret, body, signature, _) = signatures().remove(0);
        let upper = signature.to_uppercase().replace("SHA256", "sha256");
        assert!(verify(secret.as_bytes(), body.as_bytes(), Some(&upper)));
    }

    #[test]
    fn refuses_malformed_headers() {
        let (secret, body, signature, _) = signatures().remove(0);
        let (secret, body) = (secret.as_bytes(), body.as_bytes());
        assert!(!verify(secret, body, None));
        assert!(!verify(secret, body, Some(&signature[7..])));
        assert!(!verify(secret, body, Some(&signature[..70])));
        assert!(!verify(
            secret,
            body,
            Some(&signature.replace("sha256=", "sha1="))
        ));
        let not_hex = format!("sha256={}", "zz".repeat(32));
        assert!(!verify(secret, body, Some(&not_hex)));
    }

    #[test]
    fn notices_uninstalls() {
        let event = |action: &str| {
            let body = serde_json::json!({
                "action": action,
                "installation": {
                    "id": 77,
                    "account": { "login": "acme", "id": 9, "type": "Organization" },
                },
            });
            parse("installation", body.to_string().as_bytes()).unwrap()
        };
        for action in ["deleted", "suspend"] {
            assert_eq!(
                event(action),
                Event::Uninstalled {
                    installation_id: 77
                }
            );
        }
        for action in ["created", "unsuspend"] {
            assert_eq!(
                event(action),
                Event::Installed {
                    installation_id: 77,
                    account_id: 9,
                }
            );
        }
        assert_eq!(
            event("new_permissions_accepted"),
            Event::Ignored("installation new_permissions_accepted is not used".to_owned())
        );
    }
}
