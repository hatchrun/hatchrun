// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Tokens, reports, limits and the data point layout, kept apart from
//! the Workers runtime so they test natively.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;

/// Largest report body. Sixty windows take about 20 KiB.
pub const MAX_BODY: usize = 64 * 1024;
/// Most windows in one report.
pub const MAX_WINDOWS: usize = 60;
/// Longest token. A real one is about 300 bytes.
const MAX_TOKEN: usize = 2048;
/// How far a window's end may lie in the future, for clock drift, and
/// in the past, for a runner whose reports failed for a while.
const FUTURE_SECS: u64 = 5 * 60;
const PAST_SECS: u64 = 24 * 60 * 60;

/// What a token says about the runner. Everything a data point names
/// comes from here, never from the report.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    /// The tenant id.
    pub tenant: String,
    /// The App installation, which the app queries by.
    pub installation_id: u64,
    /// The workflow job.
    pub job_id: u64,
    /// The runner's name.
    pub runner_name: String,
    /// When the token expires, in Unix seconds.
    pub exp: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
}

/// Why a request was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The HTTP status.
    pub status: u16,
    /// A message for the agent's log.
    pub message: &'static str,
}

const UNAUTHORIZED: Refusal = Refusal {
    status: 401,
    message: "invalid token",
};

/// Reads the token public keys: unpadded base64url Ed25519 keys,
/// separated by commas. Two keys are listed while the signing key is
/// rotated.
///
/// # Errors
///
/// When a key does not decode, or there is none.
pub fn public_keys(value: &str) -> Result<Vec<VerifyingKey>, String> {
    let keys = value
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| {
            let bytes: [u8; 32] = URL_SAFE_NO_PAD
                .decode(key)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| format!("public key {key:?} is not 32 bytes of base64url"))?;
            VerifyingKey::from_bytes(&bytes).map_err(|err| format!("public key {key:?}: {err}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err("no public key".to_owned());
    }
    Ok(keys)
}

/// Checks `Authorization: Bearer {token}`: an `EdDSA` JWT signed by one
/// of `keys` that has not expired at `now` (Unix seconds).
///
/// # Errors
///
/// 401 for anything else.
pub fn verify(header: Option<&str>, keys: &[VerifyingKey], now: u64) -> Result<Claims, Refusal> {
    let token = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() <= MAX_TOKEN)
        .ok_or(UNAUTHORIZED)?;
    let mut parts = token.split('.');
    let (Some(head), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(UNAUTHORIZED);
    };
    let header: Header = decode_json(head)?;
    if header.alg != "EdDSA" || header.typ != "JWT" {
        return Err(UNAUTHORIZED);
    }
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(UNAUTHORIZED)?;
    let signature = Signature::from_bytes(&signature);
    let signed = &token.as_bytes()[..head.len() + 1 + claims.len()];
    if !keys
        .iter()
        .any(|key| key.verify_strict(signed, &signature).is_ok())
    {
        return Err(UNAUTHORIZED);
    }
    let claims: Claims = decode_json(claims)?;
    if claims.exp <= now {
        return Err(Refusal {
            status: 401,
            message: "token expired",
        });
    }
    Ok(claims)
}

fn decode_json<T: serde::de::DeserializeOwned>(part: &str) -> Result<T, Refusal> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| UNAUTHORIZED)?;
    serde_json::from_slice(&bytes).map_err(|_| UNAUTHORIZED)
}

/// A report from the agent.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    /// The EC2 instance type, such as `m7i.large`.
    pub instance_type: String,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    /// The agent's version.
    pub agent_version: String,
    /// What the agent summed up, oldest first.
    pub windows: Vec<Window>,
}

/// Ten seconds of samples, as the agent sums them up. Byte counts are
/// whole numbers; JSON carries them as numbers and they stay exact up
/// to 2^53.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Window {
    /// When the window ended, in Unix seconds.
    pub at: f64,
    /// Seconds the window covers.
    pub seconds: f64,
    /// Lowest one-second share of busy CPU, all cores, in percent.
    pub cpu_min: f64,
    /// Mean share of busy CPU, in percent.
    pub cpu_avg: f64,
    /// Highest one-second share of busy CPU, in percent.
    pub cpu_max: f64,
    /// Mean share of CPU time the hypervisor gave elsewhere, in percent.
    pub cpu_steal: f64,
    /// Highest memory in use, in bytes.
    pub mem_max: f64,
    /// The instance's memory, in bytes.
    pub mem_total: f64,
    /// The runner cgroup's peak memory, in bytes.
    pub cgroup_peak: f64,
    /// Processes the OOM killer ended during the window.
    pub oom_kills: f64,
    /// Bytes used on the root file system.
    pub disk_used: f64,
    /// Bytes received during the window.
    pub net_rx: f64,
    /// Bytes sent during the window.
    pub net_tx: f64,
}

/// The largest byte count a window may carry: 1 PiB.
const MAX_BYTES: f64 = 1_125_899_906_842_624.0;

impl Window {
    /// The window's numbers in data point order, `double1` first. The
    /// app's queries (crates/cloud/src/app/metrics.rs) name them by this
    /// order.
    #[must_use]
    pub fn doubles(&self) -> [f64; 13] {
        [
            self.at,
            self.seconds,
            self.cpu_min,
            self.cpu_avg,
            self.cpu_max,
            self.cpu_steal,
            self.mem_max,
            self.mem_total,
            self.cgroup_peak,
            self.oom_kills,
            self.disk_used,
            self.net_rx,
            self.net_tx,
        ]
    }

    fn check(&self, now: u64) -> Result<(), String> {
        #[allow(clippy::cast_precision_loss)] // Unix seconds stay far below 2^52.
        let (earliest, latest) = (
            now.saturating_sub(PAST_SECS) as f64,
            (now + FUTURE_SECS) as f64,
        );
        let checks: [(&str, f64, f64, f64); 13] = [
            ("at", self.at, earliest, latest),
            ("seconds", self.seconds, 1.0, 600.0),
            ("cpuMin", self.cpu_min, 0.0, 100.0),
            ("cpuAvg", self.cpu_avg, 0.0, 100.0),
            ("cpuMax", self.cpu_max, 0.0, 100.0),
            ("cpuSteal", self.cpu_steal, 0.0, 100.0),
            ("memMax", self.mem_max, 0.0, MAX_BYTES),
            ("memTotal", self.mem_total, 0.0, MAX_BYTES),
            ("cgroupPeak", self.cgroup_peak, 0.0, MAX_BYTES),
            ("oomKills", self.oom_kills, 0.0, 1_000_000.0),
            ("diskUsed", self.disk_used, 0.0, MAX_BYTES),
            ("netRx", self.net_rx, 0.0, MAX_BYTES),
            ("netTx", self.net_tx, 0.0, MAX_BYTES),
        ];
        for (name, value, low, high) in checks {
            if !value.is_finite() || value < low || value > high {
                return Err(format!("{name} must be {low} to {high}"));
            }
        }
        if self.cpu_min > self.cpu_avg || self.cpu_avg > self.cpu_max {
            return Err("cpuMin, cpuAvg and cpuMax must be in order".to_owned());
        }
        Ok(())
    }
}

impl Report {
    /// Parses and checks a report body received at `now` (Unix seconds).
    ///
    /// # Errors
    ///
    /// A message for the agent when the body is not acceptable.
    pub fn parse(body: &[u8], now: u64) -> Result<Self, String> {
        if body.len() > MAX_BODY {
            return Err("body too large".to_owned());
        }
        let report: Self =
            serde_json::from_slice(body).map_err(|err| format!("invalid body: {err}"))?;
        if report.windows.is_empty() || report.windows.len() > MAX_WINDOWS {
            return Err(format!("a report holds 1 to {MAX_WINDOWS} windows"));
        }
        for (name, value) in [
            ("instanceType", &report.instance_type),
            ("arch", &report.arch),
            ("agentVersion", &report.agent_version),
        ] {
            if !label(value) {
                return Err(format!(
                    "{name} must be 1 to 32 letters, digits, dots, dashes or underscores"
                ));
            }
        }
        for window in &report.windows {
            window.check(now)?;
        }
        Ok(report)
    }

    /// The blobs of every data point: the installation, then the
    /// instance type, the architecture and the agent version.
    #[must_use]
    pub fn blobs(&self, claims: &Claims) -> [String; 4] {
        [
            claims.installation_id.to_string(),
            self.instance_type.clone(),
            self.arch.clone(),
            self.agent_version.clone(),
        ]
    }
}

/// A short label without anything that needs escaping.
fn label(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn b64(bytes: impl AsRef<[u8]>) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn token(key: &SigningKey, header: &str, claims: &serde_json::Value) -> String {
        let input = format!("{}.{}", b64(header), b64(claims.to_string()));
        let signature = key.sign(input.as_bytes());
        format!("Bearer {input}.{}", b64(signature.to_bytes()))
    }

    fn claims(exp: u64) -> serde_json::Value {
        serde_json::json!({
            "tenant": "acme1234",
            "installation_id": 42,
            "job_id": 7,
            "runner_name": "hatchrun-7-ab0100ff",
            "exp": exp,
        })
    }

    const HEADER: &str = r#"{"alg":"EdDSA","typ":"JWT"}"#;

    #[test]
    fn reads_public_keys() {
        let one = b64(key(1).verifying_key().as_bytes());
        let two = b64(key(2).verifying_key().as_bytes());
        assert_eq!(public_keys(&one).unwrap().len(), 1);
        assert_eq!(public_keys(&format!("{one}, {two}")).unwrap().len(), 2);
        assert!(public_keys("").is_err());
        assert!(public_keys("short").is_err());
        assert!(public_keys(&format!("{one},{}", b64([0; 31]))).is_err());
    }

    #[test]
    fn verifies_tokens() {
        let keys = [key(1).verifying_key(), key(2).verifying_key()];
        let good = token(&key(2), HEADER, &claims(NOW + 60));
        assert_eq!(
            verify(Some(&good), &keys, NOW).unwrap(),
            Claims {
                tenant: "acme1234".into(),
                installation_id: 42,
                job_id: 7,
                runner_name: "hatchrun-7-ab0100ff".into(),
                exp: NOW + 60,
            }
        );
    }

    #[test]
    fn refuses_bad_tokens() {
        let keys = [key(1).verifying_key()];
        let good = token(&key(1), HEADER, &claims(NOW + 60));
        let (input, _) = good.rsplit_once('.').unwrap();
        let other_claims = b64(claims(NOW + 7200).to_string());
        let (head, _) = input.trim_start_matches("Bearer ").split_once('.').unwrap();
        let mut extra = claims(NOW + 60);
        extra["admin"] = true.into();
        for header in [
            None,
            Some(good.trim_start_matches("Bearer ").to_owned()),
            Some(format!("Basic {}", good.trim_start_matches("Bearer "))),
            // Signed by another key.
            Some(token(&key(9), HEADER, &claims(NOW + 60))),
            // Claims swapped under a valid signature.
            Some(format!(
                "Bearer {head}.{other_claims}.{}",
                good.rsplit_once('.').unwrap().1
            )),
            // Another algorithm, unknown claims, no signature, too long.
            Some(token(
                &key(1),
                r#"{"alg":"none","typ":"JWT"}"#,
                &claims(NOW + 60),
            )),
            Some(token(&key(1), HEADER, &extra)),
            Some(format!("{input}.")),
            Some(format!("{good}.x")),
            Some(format!("Bearer {}", "a".repeat(MAX_TOKEN + 1))),
        ] {
            assert_eq!(
                verify(header.as_deref(), &keys, NOW).unwrap_err().status,
                401,
                "{header:?}"
            );
        }
        let expired = token(&key(1), HEADER, &claims(NOW));
        assert_eq!(
            verify(Some(&expired), &keys, NOW).unwrap_err().message,
            "token expired"
        );
    }

    fn window() -> serde_json::Value {
        serde_json::json!({
            "at": NOW, "seconds": 10, "cpuMin": 1.0, "cpuAvg": 2.5, "cpuMax": 4.0,
            "cpuSteal": 0.0, "memMax": 2048, "memTotal": 8192, "cgroupPeak": 1024,
            "oomKills": 0, "diskUsed": 4096, "netRx": 10, "netTx": 20,
        })
    }

    fn report(windows: &[serde_json::Value]) -> Vec<u8> {
        serde_json::json!({
            "instanceType": "m7i.large",
            "arch": "x86_64",
            "agentVersion": "0.34.0",
            "windows": windows,
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn parses_reports() {
        let report = Report::parse(&report(&[window()]), NOW).unwrap();
        #[allow(clippy::cast_precision_loss)]
        let at = NOW as f64;
        assert_eq!(
            report.windows[0].doubles(),
            [
                at, 10.0, 1.0, 2.5, 4.0, 0.0, 2048.0, 8192.0, 1024.0, 0.0, 4096.0, 10.0, 20.0
            ]
        );
        let claims = Claims {
            tenant: "acme1234".into(),
            installation_id: 42,
            job_id: 7,
            runner_name: "r".into(),
            exp: 0,
        };
        assert_eq!(
            report.blobs(&claims),
            ["42", "m7i.large", "x86_64", "0.34.0"]
        );
    }

    #[test]
    fn refuses_bad_reports() {
        let with = |field: &str, value: serde_json::Value| {
            let mut window = window();
            window[field] = value;
            report(&[window])
        };
        let mut extra = window();
        extra["jobId"] = 8.into();
        let mut no_type: serde_json::Value = serde_json::from_slice(&report(&[window()])).unwrap();
        no_type["instanceType"] = "m7i.large\"; DROP".into();
        let mut unknown: serde_json::Value = serde_json::from_slice(&report(&[window()])).unwrap();
        unknown["installationId"] = 1.into();
        for body in [
            report(&[]),
            report(&vec![window(); MAX_WINDOWS + 1]),
            report(&[extra]),
            with("cpuMax", 100.5.into()),
            with("cpuMin", (-1.0).into()),
            with("cpuMin", 3.0.into()),
            with("memMax", 1e300.into()),
            with("seconds", 0.into()),
            with("at", (NOW + 3600).into()),
            with("at", (NOW - 3 * 86_400).into()),
            with("netRx", "10".into()),
            with("netRx", serde_json::Value::Null),
            no_type.to_string().into_bytes(),
            unknown.to_string().into_bytes(),
            b"not json".to_vec(),
            vec![b' '; MAX_BODY + 1],
        ] {
            assert!(
                Report::parse(&body, NOW).is_err(),
                "{}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            );
        }
    }

    #[test]
    fn a_full_report_fits() {
        let mut big = window();
        big["cpuAvg"] = 33.333_333_333_333_336.into();
        big["cpuMax"] = 99.999_999_999_999_99.into();
        big["memMax"] = 1_099_511_627_776_u64.into();
        assert!(report(&vec![big; MAX_WINDOWS]).len() < MAX_BODY);
    }
}
