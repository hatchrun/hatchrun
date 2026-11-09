// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads the CI launch out of the instance's user data.

use std::io::Read;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;

/// Claims boot documents from the control plane's broker.
pub trait Broker {
    /// Claims the boot document stored under `ticket`. Returns `None`
    /// while there is nothing to claim yet.
    async fn claim(&self, broker: &str, ticket: &str) -> Result<Option<String>>;
}

/// The job cache API of a CI job.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Cache {
    /// `/v1/cache` on hatchrun-cloud.
    pub url: String,
    /// The bearer token that names the job. It stays in memory and out
    /// of the log.
    pub token: String,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The JSON document the control plane passes as user data.
#[derive(Deserialize)]
struct UserData {
    /// hatchrun-broker base URL.
    #[serde(default)]
    broker: String,
    /// Ticket to claim the boot document with.
    #[serde(default)]
    ticket: String,
    /// `ci` for a CI launch. Empty otherwise.
    #[serde(default)]
    mode: String,
}

/// The broker and ticket of a CI launch, when user data says `"mode":
/// "ci"`. Any other user data gives `None`; so does anything that is not
/// a JSON object, such as a shell script meant for another boot tool.
///
/// # Errors
///
/// When user data names the CI mode but not both a broker and a ticket,
/// or cannot be parsed as JSON.
pub(crate) fn ci_ticket(raw: &[u8]) -> Result<Option<(String, String)>> {
    let raw = decompress(raw)?;
    let raw = raw.trim_ascii();
    if raw.first() != Some(&b'{') {
        return Ok(None);
    }
    let doc: UserData = serde_json::from_slice(raw).context("parse user data")?;
    if doc.mode != "ci" {
        return Ok(None);
    }
    if doc.broker.is_empty() || doc.ticket.is_empty() {
        bail!("CI user data must set both broker and ticket");
    }
    Ok(Some((doc.broker, doc.ticket)))
}

/// Tells whether the user data is a CI launch. Malformed CI user data is
/// an error rather than `false`, so a broken launch still powers off.
pub(crate) fn is_ci_launch(user_data: Option<&[u8]>) -> Result<bool> {
    Ok(ci_ticket(user_data.unwrap_or_default())?.is_some())
}

fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Ok(raw.to_vec());
    }
    let mut out = Vec::new();
    GzDecoder::new(raw)
        .read_to_end(&mut out)
        .context("decompress user data")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn ci_mode_user_data_names_the_ticket() {
        let raw = br#"{"mode":"ci","broker":"https://b","ticket":"t"}"#;
        assert_eq!(
            ci_ticket(raw).unwrap(),
            Some(("https://b".to_owned(), "t".to_owned()))
        );
        assert_eq!(ci_ticket(&gzip(raw)).unwrap(), ci_ticket(raw).unwrap());
        assert!(ci_ticket(br#"{"mode":"ci","broker":"https://b"}"#).is_err());
    }

    #[test]
    fn other_user_data_is_not_ci() {
        for raw in [
            &br#"{"broker":"https://b","ticket":"t"}"#[..],
            br#"{"jitConfig":"jit"}"#,
            b"#!/bin/sh\necho hi\n",
            b"",
        ] {
            assert_eq!(ci_ticket(raw).unwrap(), None);
        }
    }

    #[test]
    fn launch_detection() {
        let ci = br#"{"mode":"ci","broker":"https://b","ticket":"t"}"#;
        assert!(is_ci_launch(Some(ci)).unwrap());
        assert!(is_ci_launch(Some(&gzip(ci))).unwrap());
        assert!(!is_ci_launch(None).unwrap());
        assert!(!is_ci_launch(Some(b"#!/bin/sh\n")).unwrap());
        assert!(is_ci_launch(Some(br#"{"mode":"ci"}"#)).is_err());
        assert!(is_ci_launch(Some(b"{not json")).is_err());
    }
}
