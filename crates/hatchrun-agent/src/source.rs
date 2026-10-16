// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Finds the instance's just-in-time runner config, and where to report
//! metrics when the launch says.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tracing::warn;

/// Tag naming the SSM parameter that holds the JIT config.
pub const TAG_JIT_PARAMETER: &str = "hatchrun:jit-parameter";

/// Tag set by github-aws-runners/terraform-aws-github-runner. Its
/// `token_path` parameter holds the prefix of the per-instance JIT config.
pub const TAG_GHR_CONFIG_PATH: &str = "ghr:ssm_config_path";

/// The part of the EC2 instance metadata service the agent uses.
pub trait Metadata {
    /// Returns the EC2 instance id.
    async fn instance_id(&self) -> Result<String>;
    /// Returns `None` when the instance has no user data.
    async fn user_data(&self) -> Result<Option<Vec<u8>>>;
    /// Returns `None` when the tag is not set.
    async fn tag(&self, key: &str) -> Result<Option<String>>;
}

/// Claims boot documents from hatchrun-broker.
pub trait Broker {
    /// Claims the boot document stored under `ticket`. Returns `None`
    /// while there is nothing to claim yet.
    async fn claim(&self, broker: &str, ticket: &str) -> Result<Option<String>>;
}

/// Reads and deletes SSM parameters.
pub trait Parameters {
    /// Returns `None` when the parameter does not exist.
    async fn get(&self, name: &str) -> Result<Option<String>>;
    /// Deletes the parameter.
    async fn delete(&self, name: &str) -> Result<()>;
}

/// Resolves the JIT config for this instance.
pub struct Resolver<M, P, B> {
    /// Instance metadata.
    pub metadata: M,
    /// SSM parameters.
    pub parameters: P,
    /// The broker.
    pub broker: B,
    /// How often to check for a parameter that does not exist yet.
    pub poll_interval: Duration,
}

/// What the runner starts with.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Boot {
    /// The encoded JIT config.
    pub jit_config: String,
    /// Where to report metrics. hatchrun.io sets it; the self-hosted
    /// scaler and configs from user data or SSM do not.
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
    /// Where the job cache lives. hatchrun.io sets it for profiles with
    /// the `cache` extra on a stack that has a cache bucket.
    #[serde(default)]
    pub cache: Option<Cache>,
}

/// Where a hosted runner reports its metrics.
#[derive(PartialEq, Eq, Deserialize)]
pub struct Telemetry {
    /// hatchrun-telemetry's base URL.
    pub url: String,
    /// The bearer token that names the runner's job. It stays in
    /// memory and out of the log.
    pub token: String,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The job cache API of a hosted runner.
#[derive(PartialEq, Eq, Deserialize)]
pub struct Cache {
    /// `/v1/cache` on hatchrun-cloud.
    pub url: String,
    /// The bearer token that names the runner's job. It stays in
    /// memory and out of the log.
    pub token: String,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Boot {
    fn config(jit_config: String) -> Self {
        Self {
            jit_config,
            telemetry: None,
            cache: None,
        }
    }
}

/// The JSON document the hatchrun scaler passes as user data.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserData {
    #[serde(default)]
    jit_config: String,
    /// hatchrun-broker base URL.
    #[serde(default)]
    broker: String,
    /// Ticket to claim the config with.
    #[serde(default)]
    ticket: String,
}

/// Where user data says the config is.
#[derive(Debug, PartialEq, Eq)]
enum FromUserData {
    Config(String),
    Ticket { broker: String, ticket: String },
}

impl<M: Metadata, P: Parameters, B: Broker> Resolver<M, P, B> {
    /// Returns the JIT config, in this order of precedence: a `jitConfig`
    /// field in user data, the boot document a `broker` and `ticket` in
    /// user data claim, the parameter named by the
    /// `hatchrun:jit-parameter` tag, or the github-aws-runners token
    /// parameter. A ticket is claimed and a parameter read and deleted,
    /// both polled until the config is there. Callers bound the wait with
    /// a timeout.
    pub async fn boot(&self) -> Result<Boot> {
        let user_data = self.metadata.user_data().await.context("read user data")?;
        match from_user_data(user_data.as_deref().unwrap_or_default())? {
            Some(FromUserData::Config(config)) => return Ok(Boot::config(config)),
            Some(FromUserData::Ticket { broker, ticket }) => {
                return self.claim(&broker, &ticket).await;
            }
            None => {}
        }
        let name = self.parameter_name().await?;
        Ok(Boot::config(self.take_parameter(&name).await?))
    }

    /// The scaler stores the boot document after the instance launched,
    /// so the first claims often find nothing.
    async fn claim(&self, broker: &str, ticket: &str) -> Result<Boot> {
        loop {
            if let Some(doc) = self.broker.claim(broker, ticket).await? {
                return serde_json::from_str(&doc).context("parse boot document");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn parameter_name(&self) -> Result<String> {
        if let Some(name) = self.metadata.tag(TAG_JIT_PARAMETER).await? {
            return Ok(name);
        }
        let Some(config_path) = self.metadata.tag(TAG_GHR_CONFIG_PATH).await? else {
            bail!(
                "no runner config: user data has no jitConfig and the instance has neither \
                 {TAG_JIT_PARAMETER} nor {TAG_GHR_CONFIG_PATH} tag"
            );
        };
        let token_path = self
            .parameters
            .get(&format!("{config_path}/token_path"))
            .await
            .with_context(|| format!("read {config_path}/token_path"))?
            .with_context(|| format!("parameter {config_path}/token_path does not exist"))?;
        let instance_id = self.metadata.instance_id().await?;
        Ok(format!("{token_path}/{instance_id}"))
    }

    /// The scaler writes the parameter after it launches the instance, so it
    /// is often missing for the first seconds of boot.
    async fn take_parameter(&self, name: &str) -> Result<String> {
        loop {
            if let Some(value) = self
                .parameters
                .get(name)
                .await
                .with_context(|| format!("read {name}"))?
            {
                // The JIT config is single use: a copy left behind after a
                // failed delete cannot register a second runner.
                if let Err(err) = self.parameters.delete(name).await {
                    warn!(name, "delete runner config parameter: {err:#}");
                }
                return Ok(value);
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

/// Reads `jitConfig`, or `broker` and `ticket`, from JSON user data, plain
/// or gzip. Anything that is not a JSON object, such as a shell script
/// meant for another boot tool, is ignored.
/// Tells whether the instance was launched as a runner: its user data
/// holds a runner config or a broker ticket, or it has a tag naming the
/// parameter that will hold the config. Malformed runner user data is an
/// error rather than `false`, so a broken launch still powers off.
pub async fn is_runner_launch(metadata: &impl Metadata) -> Result<bool> {
    let user_data = metadata.user_data().await.context("read user data")?;
    if from_user_data(user_data.as_deref().unwrap_or_default())?.is_some() {
        return Ok(true);
    }
    for key in [TAG_JIT_PARAMETER, TAG_GHR_CONFIG_PATH] {
        if metadata
            .tag(key)
            .await
            .with_context(|| format!("read tag {key}"))?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn from_user_data(raw: &[u8]) -> Result<Option<FromUserData>> {
    let raw = decompress(raw)?;
    let raw = raw.trim_ascii();
    if raw.first() != Some(&b'{') {
        return Ok(None);
    }
    let doc: UserData = serde_json::from_slice(raw).context("parse user data")?;
    if !doc.jit_config.is_empty() {
        return Ok(Some(FromUserData::Config(doc.jit_config)));
    }
    match (doc.broker.is_empty(), doc.ticket.is_empty()) {
        (false, false) => Ok(Some(FromUserData::Ticket {
            broker: doc.broker,
            ticket: doc.ticket,
        })),
        (true, true) => Ok(None),
        _ => bail!("user data must set both broker and ticket"),
    }
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
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::io::Write;

    use anyhow::anyhow;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    #[derive(Default)]
    struct FakeMetadata {
        user_data: Option<Vec<u8>>,
        tags: HashMap<&'static str, &'static str>,
    }

    // The fakes answer immediately; the trait is async for the real clients.
    #[allow(clippy::unused_async_trait_impl)]
    impl Metadata for FakeMetadata {
        async fn instance_id(&self) -> Result<String> {
            Ok("i-0123".into())
        }
        async fn user_data(&self) -> Result<Option<Vec<u8>>> {
            Ok(self.user_data.clone())
        }
        async fn tag(&self, key: &str) -> Result<Option<String>> {
            Ok(self.tags.get(key).map(ToString::to_string))
        }
    }

    /// Parameters that appear after `missing_reads` lookups.
    #[derive(Default)]
    struct FakeParameters {
        values: HashMap<&'static str, &'static str>,
        missing_reads: Cell<u32>,
        fail_delete: bool,
        deleted: RefCell<Vec<String>>,
    }

    // The fakes answer immediately; the trait is async for the real clients.
    #[allow(clippy::unused_async_trait_impl)]
    impl Parameters for FakeParameters {
        async fn get(&self, name: &str) -> Result<Option<String>> {
            if self.missing_reads.get() > 0 {
                self.missing_reads.set(self.missing_reads.get() - 1);
                return Ok(None);
            }
            Ok(self.values.get(name).map(ToString::to_string))
        }
        async fn delete(&self, name: &str) -> Result<()> {
            if self.fail_delete {
                return Err(anyhow!("access denied"));
            }
            self.deleted.borrow_mut().push(name.into());
            Ok(())
        }
    }

    /// A broker that has the config after `missing_claims` claims.
    #[derive(Default)]
    struct FakeBroker {
        config: Option<&'static str>,
        missing_claims: Cell<u32>,
        claims: RefCell<Vec<(String, String)>>,
    }

    // The fakes answer immediately; the trait is async for the real clients.
    #[allow(clippy::unused_async_trait_impl)]
    impl Broker for FakeBroker {
        async fn claim(&self, broker: &str, ticket: &str) -> Result<Option<String>> {
            self.claims
                .borrow_mut()
                .push((broker.into(), ticket.into()));
            if self.missing_claims.get() > 0 {
                self.missing_claims.set(self.missing_claims.get() - 1);
                return Ok(None);
            }
            Ok(self.config.map(ToString::to_string))
        }
    }

    fn resolver(
        metadata: FakeMetadata,
        parameters: FakeParameters,
    ) -> Resolver<FakeMetadata, FakeParameters, FakeBroker> {
        with_broker(metadata, parameters, FakeBroker::default())
    }

    fn with_broker(
        metadata: FakeMetadata,
        parameters: FakeParameters,
        broker: FakeBroker,
    ) -> Resolver<FakeMetadata, FakeParameters, FakeBroker> {
        Resolver {
            metadata,
            parameters,
            broker,
            poll_interval: Duration::from_millis(1),
        }
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn reads_user_data() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"jitConfig":"jit-from-user-data"}"#.to_vec()),
            ..Default::default()
        };
        let config = resolver(metadata, FakeParameters::default())
            .boot()
            .await
            .unwrap();
        assert_eq!(config, Boot::config("jit-from-user-data".into()));
    }

    #[tokio::test]
    async fn reads_gzip_user_data() {
        let metadata = FakeMetadata {
            user_data: Some(gzip(br#" {"jitConfig":"jit-gzip"} "#)),
            ..Default::default()
        };
        let config = resolver(metadata, FakeParameters::default())
            .boot()
            .await
            .unwrap();
        assert_eq!(config, Boot::config("jit-gzip".into()));
    }

    #[tokio::test]
    async fn claims_ticket_from_broker() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"broker":"https://broker.example.com","ticket":"t-1"}"#.to_vec()),
            ..Default::default()
        };
        let broker = FakeBroker {
            config: Some(
                r#"{"jitConfig":"jit-from-broker","telemetry":{"url":"https://telemetry.example.com","token":"a.b.c"},"cache":{"url":"https://app.example.com/v1/cache","token":"d.e.f"}}"#,
            ),
            missing_claims: Cell::new(2),
            ..Default::default()
        };
        let resolver = with_broker(metadata, FakeParameters::default(), broker);

        assert_eq!(
            resolver.boot().await.unwrap(),
            Boot {
                jit_config: "jit-from-broker".into(),
                telemetry: Some(Telemetry {
                    url: "https://telemetry.example.com".into(),
                    token: "a.b.c".into(),
                }),
                cache: Some(Cache {
                    url: "https://app.example.com/v1/cache".into(),
                    token: "d.e.f".into(),
                }),
            }
        );
        let claims = resolver.broker.claims.borrow();
        assert_eq!(claims.len(), 3);
        assert_eq!(
            claims[0],
            ("https://broker.example.com".into(), "t-1".into())
        );
    }

    #[tokio::test]
    async fn scaler_boot_documents_have_no_telemetry() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"broker":"https://broker.example.com","ticket":"t-1"}"#.to_vec()),
            ..Default::default()
        };
        let broker = FakeBroker {
            config: Some(r#"{"jitConfig":"jit-from-scaler"}"#),
            ..Default::default()
        };
        let resolver = with_broker(metadata, FakeParameters::default(), broker);
        assert_eq!(
            resolver.boot().await.unwrap(),
            Boot::config("jit-from-scaler".into())
        );
    }

    #[tokio::test]
    async fn a_bare_config_from_the_broker_is_an_error() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"broker":"https://broker.example.com","ticket":"t-1"}"#.to_vec()),
            ..Default::default()
        };
        let broker = FakeBroker {
            config: Some("jit-1"),
            ..Default::default()
        };
        let resolver = with_broker(metadata, FakeParameters::default(), broker);
        let err = resolver.boot().await.unwrap_err();
        assert!(err.to_string().contains("boot document"), "{err:#}");
    }

    #[tokio::test]
    async fn half_a_ticket_is_an_error() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"broker":"https://broker.example.com"}"#.to_vec()),
            ..Default::default()
        };
        let err = resolver(metadata, FakeParameters::default())
            .boot()
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("both broker and ticket"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn waits_for_tagged_parameter_and_deletes_it() {
        let metadata = FakeMetadata {
            user_data: Some(b"#!/bin/bash\necho not for the agent\n".to_vec()),
            tags: HashMap::from([(TAG_JIT_PARAMETER, "/hatchrun/jit/i-0123")]),
        };
        let parameters = FakeParameters {
            values: HashMap::from([("/hatchrun/jit/i-0123", "jit-from-ssm")]),
            missing_reads: Cell::new(3),
            ..Default::default()
        };
        let resolver = resolver(metadata, parameters);

        assert_eq!(resolver.boot().await.unwrap().jit_config, "jit-from-ssm");
        assert_eq!(
            *resolver.parameters.deleted.borrow(),
            ["/hatchrun/jit/i-0123"]
        );
    }

    #[tokio::test]
    async fn reads_github_aws_runners_token() {
        let metadata = FakeMetadata {
            tags: HashMap::from([(TAG_GHR_CONFIG_PATH, "/ghr/spot/runners/config")]),
            ..Default::default()
        };
        let parameters = FakeParameters {
            values: HashMap::from([
                (
                    "/ghr/spot/runners/config/token_path",
                    "/ghr/spot/runners/tokens",
                ),
                ("/ghr/spot/runners/tokens/i-0123", "jit-from-module"),
            ]),
            ..Default::default()
        };
        let resolver = resolver(metadata, parameters);

        assert_eq!(resolver.boot().await.unwrap().jit_config, "jit-from-module");
        assert_eq!(
            *resolver.parameters.deleted.borrow(),
            ["/ghr/spot/runners/tokens/i-0123"]
        );
    }

    #[tokio::test]
    async fn failed_delete_still_returns_config() {
        let metadata = FakeMetadata {
            tags: HashMap::from([(TAG_JIT_PARAMETER, "/hatchrun/jit/i-0123")]),
            ..Default::default()
        };
        let parameters = FakeParameters {
            values: HashMap::from([("/hatchrun/jit/i-0123", "jit-from-ssm")]),
            fail_delete: true,
            ..Default::default()
        };
        assert_eq!(
            resolver(metadata, parameters).boot().await.unwrap(),
            Boot::config("jit-from-ssm".into())
        );
    }

    #[tokio::test]
    async fn no_source_is_an_error() {
        let err = resolver(FakeMetadata::default(), FakeParameters::default())
            .boot()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no runner config"), "{err:#}");
    }

    #[tokio::test]
    async fn invalid_json_user_data_is_an_error() {
        let metadata = FakeMetadata {
            user_data: Some(b"{not json".to_vec()),
            ..Default::default()
        };
        assert!(
            resolver(metadata, FakeParameters::default())
                .boot()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn runner_launches() {
        let user_data: [&[u8]; 3] = [
            br#"{"jitConfig":"jit"}"#,
            br#"{"broker":"https://broker.example.com","ticket":"t"}"#,
            &gzip(br#"{"broker":"https://broker.example.com","ticket":"t"}"#),
        ];
        for data in user_data {
            let metadata = FakeMetadata {
                user_data: Some(data.to_vec()),
                ..Default::default()
            };
            assert!(is_runner_launch(&metadata).await.unwrap());
        }
        for key in [TAG_JIT_PARAMETER, TAG_GHR_CONFIG_PATH] {
            let metadata = FakeMetadata {
                tags: HashMap::from([(key, "/runners/config")]),
                ..Default::default()
            };
            assert!(is_runner_launch(&metadata).await.unwrap(), "{key}");
        }
    }

    #[tokio::test]
    async fn other_launches() {
        // Launched by hand or by the AWS Marketplace scan: no user data, a
        // script, or JSON that is not for hatchrun.
        for data in [None, Some(&b"#!/bin/sh\necho hi\n"[..]), Some(&b"{}"[..])] {
            let metadata = FakeMetadata {
                user_data: data.map(<[u8]>::to_vec),
                ..Default::default()
            };
            assert!(!is_runner_launch(&metadata).await.unwrap());
        }
    }

    #[tokio::test]
    async fn malformed_runner_user_data_is_an_error() {
        let metadata = FakeMetadata {
            user_data: Some(br#"{"broker":"https://broker.example.com"}"#.to_vec()),
            ..Default::default()
        };
        assert!(is_runner_launch(&metadata).await.is_err());
    }
}
