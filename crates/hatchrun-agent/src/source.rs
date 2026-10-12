//! Finds the instance's just-in-time runner config.

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

/// Reads and deletes SSM parameters.
pub trait Parameters {
    /// Returns `None` when the parameter does not exist.
    async fn get(&self, name: &str) -> Result<Option<String>>;
    /// Deletes the parameter.
    async fn delete(&self, name: &str) -> Result<()>;
}

/// Resolves the JIT config for this instance.
pub struct Resolver<M, P> {
    /// Instance metadata.
    pub metadata: M,
    /// SSM parameters.
    pub parameters: P,
    /// How often to check for a parameter that does not exist yet.
    pub poll_interval: Duration,
}

/// The JSON document the hatchrun scaler passes as user data.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserData {
    #[serde(default)]
    jit_config: String,
}

impl<M: Metadata, P: Parameters> Resolver<M, P> {
    /// Returns the JIT config, in this order of precedence: a `jitConfig`
    /// field in user data, the parameter named by the `hatchrun:jit-parameter`
    /// tag, or the github-aws-runners token parameter. A parameter is polled
    /// until it exists, then deleted. Callers bound the wait with a timeout.
    pub async fn jit_config(&self) -> Result<String> {
        let user_data = self.metadata.user_data().await.context("read user data")?;
        if let Some(config) = jit_config_from_user_data(user_data.as_deref().unwrap_or_default())? {
            return Ok(config);
        }
        let name = self.parameter_name().await?;
        self.take_parameter(&name).await
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

/// Reads `jitConfig` from JSON user data, plain or gzip. Anything that is not
/// a JSON object, such as a shell script meant for another boot tool, is
/// ignored.
fn jit_config_from_user_data(raw: &[u8]) -> Result<Option<String>> {
    let raw = decompress(raw)?;
    let raw = raw.trim_ascii();
    if raw.first() != Some(&b'{') {
        return Ok(None);
    }
    let doc: UserData = serde_json::from_slice(raw).context("parse user data")?;
    Ok(Some(doc.jit_config).filter(|config| !config.is_empty()))
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

    fn resolver(
        metadata: FakeMetadata,
        parameters: FakeParameters,
    ) -> Resolver<FakeMetadata, FakeParameters> {
        Resolver {
            metadata,
            parameters,
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
            .jit_config()
            .await
            .unwrap();
        assert_eq!(config, "jit-from-user-data");
    }

    #[tokio::test]
    async fn reads_gzip_user_data() {
        let metadata = FakeMetadata {
            user_data: Some(gzip(br#" {"jitConfig":"jit-gzip"} "#)),
            ..Default::default()
        };
        let config = resolver(metadata, FakeParameters::default())
            .jit_config()
            .await
            .unwrap();
        assert_eq!(config, "jit-gzip");
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

        assert_eq!(resolver.jit_config().await.unwrap(), "jit-from-ssm");
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

        assert_eq!(resolver.jit_config().await.unwrap(), "jit-from-module");
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
            resolver(metadata, parameters).jit_config().await.unwrap(),
            "jit-from-ssm"
        );
    }

    #[tokio::test]
    async fn no_source_is_an_error() {
        let err = resolver(FakeMetadata::default(), FakeParameters::default())
            .jit_config()
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
                .jit_config()
                .await
                .is_err()
        );
    }
}
