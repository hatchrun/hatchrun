// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! GitHub credentials and the connection to the Actions service.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hatchrun_scaleset::{
    Actions, CallError, ConfigUrl, Error, ErrorKind, Operation, Secret, Transport,
};
use http::{Request, Response, StatusCode};
use tracing::{info, warn};

/// How GitHub knows the scaler.
pub enum Credentials {
    /// A personal access token.
    Token(Secret),
    /// A GitHub App installation.
    App {
        /// The App's client id, or its numeric App id.
        client_id: String,
        /// The installation on the org, enterprise or repository.
        installation_id: u64,
        /// The App's private key.
        key: RsaKeyPair,
    },
}

impl Credentials {
    /// Reads a GitHub App private key in PEM, as GitHub hands it out
    /// (PKCS#1) or converted to PKCS#8.
    pub fn app(client_id: String, installation_id: u64, pem: &str) -> Result<Self> {
        Ok(Self::App {
            client_id,
            installation_id,
            key: parse_private_key(pem)?,
        })
    }
}

fn parse_private_key(pem: &str) -> Result<RsaKeyPair> {
    let mut lines = pem
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    let begin = lines.next().unwrap_or_default();
    let pkcs1 = match begin {
        "-----BEGIN RSA PRIVATE KEY-----" => true,
        "-----BEGIN PRIVATE KEY-----" => false,
        _ => bail!("GitHub App private key is not an RSA private key in PEM"),
    };
    let body: String = lines
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    let der = STANDARD
        .decode(body)
        .context("GitHub App private key is not valid PEM")?;
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|err| anyhow::anyhow!("GitHub App private key rejected: {err}"))
}

/// Signs the JWT a GitHub App exchanges for an installation token. It is
/// backdated a minute for clock drift and lives nine minutes, under
/// GitHub's ten minute limit.
fn app_jwt(key: &RsaKeyPair, client_id: &str, now: u64) -> Result<Secret> {
    let issued_at = now.saturating_sub(60);
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::json!({ "iat": issued_at, "exp": issued_at + 9 * 60, "iss": client_id })
            .to_string(),
    );
    let message = format!("{header}.{claims}");
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow::anyhow!("sign GitHub App JWT"))?;
    Ok(Secret::new(format!(
        "{message}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    )))
}

/// Sends requests with reqwest.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    /// Builds a client with a timeout above the message queue's long poll,
    /// which holds a request for about 50 seconds.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("build HTTP client")?;
        Ok(Self { client })
    }
}

impl Transport for Http {
    type Error = reqwest::Error;

    fn send(
        &self,
        request: Request<Vec<u8>>,
    ) -> impl Future<Output = Result<Response<Vec<u8>>, Self::Error>> {
        let client = self.client.clone();
        async move {
            let request = reqwest::Request::try_from(request)?;
            let response = client.execute(request).await?;
            let mut builder = Response::builder().status(response.status());
            if let Some(headers) = builder.headers_mut() {
                headers.extend(
                    response
                        .headers()
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
            }
            let body = response.bytes().await?.to_vec();
            Ok(builder
                .body(body)
                .expect("status and headers come from a valid response"))
        }
    }
}

/// Sends operations and gets Actions service connections.
pub struct GitHub<T> {
    transport: T,
    config: ConfigUrl,
    credentials: Credentials,
}

/// Attempts at the admin connection, which rejects a registration token
/// for a few seconds after it was issued.
const ADMIN_ATTEMPTS: u32 = 5;

impl<T: Transport> GitHub<T>
where
    T::Error: std::error::Error + Send + Sync + 'static,
{
    /// Talks to GitHub for the scale sets under `config`.
    pub fn new(transport: T, config: ConfigUrl, credentials: Credentials) -> Self {
        Self {
            transport,
            config,
            credentials,
        }
    }

    /// Sends an operation.
    pub async fn call<R>(
        &self,
        operation: Result<Operation<R>, Error>,
    ) -> Result<R, CallError<T::Error>> {
        let operation = operation.map_err(CallError::Api)?;
        hatchrun_scaleset::call(&self.transport, operation).await
    }

    /// Gets a new Actions service connection.
    pub async fn connect(&self, now: u64) -> Result<Actions> {
        let api_token = match &self.credentials {
            Credentials::Token(token) => token.clone(),
            Credentials::App {
                client_id,
                installation_id,
                key,
            } => {
                let jwt = app_jwt(key, client_id, now)?;
                self.call(hatchrun_scaleset::github::installation_token(
                    &self.config,
                    *installation_id,
                    &jwt,
                ))
                .await
                .context("get GitHub App installation token")?
                .token
            }
        };
        let registration = self
            .call(hatchrun_scaleset::github::registration_token(
                &self.config,
                &api_token,
            ))
            .await
            .with_context(|| format!("get runner registration token for {}", self.config))?
            .token;

        for attempt in 1..=ADMIN_ATTEMPTS {
            let result = self
                .call(hatchrun_scaleset::github::admin_connection(
                    &self.config,
                    &registration,
                ))
                .await;
            match result {
                Ok(actions) => {
                    info!(
                        expires_at = actions.expires_at(),
                        "connected to the Actions service"
                    );
                    return Ok(actions);
                }
                Err(CallError::Api(Error::Api(err)))
                    if attempt < ADMIN_ATTEMPTS
                        && (err.kind == ErrorKind::Unauthorized
                            || err.status == StatusCode::FORBIDDEN) =>
                {
                    warn!(attempt, "registration token not accepted yet: {err}");
                    tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
                }
                Err(err) => return Err(err).context("connect to the Actions service"),
            }
        }
        unreachable!("the last attempt returns")
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    fn pem(label: &str, der: &[u8]) -> String {
        let body = STANDARD.encode(der);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        format!(
            "\n-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    }

    /// Reads one DER element and returns its tag, contents and the rest.
    fn der(input: &[u8]) -> (u8, &[u8], &[u8]) {
        let (tag, first) = (input[0], input[1]);
        let (len, header) = if first < 0x80 {
            (usize::from(first), 2)
        } else {
            let octets = usize::from(first & 0x7f);
            let len = input[2..2 + octets]
                .iter()
                .fold(0, |len, byte| len << 8 | usize::from(*byte));
            (len, 2 + octets)
        };
        (tag, &input[header..header + len], &input[header + len..])
    }

    /// Unwraps the PKCS#1 `RSAPrivateKey` from a PKCS#8 `PrivateKeyInfo`,
    /// which is what GitHub hands out.
    fn pkcs1(pkcs8: &[u8]) -> &[u8] {
        let (_, info, _) = der(pkcs8);
        let (_, _, rest) = der(info); // version
        let (_, _, rest) = der(rest); // algorithm
        let (tag, key, _) = der(rest);
        assert_eq!(tag, 0x04);
        key
    }

    fn generated_key() -> RsaKeyPair {
        RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap()
    }

    #[test]
    fn reads_pkcs8_and_pkcs1_keys() {
        let key = generated_key();
        let pkcs8 = key.as_der().unwrap();
        let parsed = parse_private_key(&pem("PRIVATE KEY", pkcs8.as_ref())).unwrap();
        assert_eq!(parsed.public_key().as_ref(), key.public_key().as_ref());

        let parsed = parse_private_key(&pem("RSA PRIVATE KEY", pkcs1(pkcs8.as_ref()))).unwrap();
        assert_eq!(parsed.public_key().as_ref(), key.public_key().as_ref());
    }

    #[test]
    fn rejects_other_keys() {
        assert!(parse_private_key("-----BEGIN EC PRIVATE KEY-----\nAAAA\n").is_err());
        assert!(parse_private_key("-----BEGIN PRIVATE KEY-----\n!!!\n").is_err());
        assert!(parse_private_key("-----BEGIN PRIVATE KEY-----\nAAAA\n").is_err());
    }

    #[test]
    fn signs_app_jwt() {
        let key = generated_key();
        let jwt = app_jwt(&key, "Iv23li", 1_000_000).unwrap();
        let parts: Vec<&str> = jwt.expose().split('.').collect();
        let [header, claims, signature] = parts[..] else {
            panic!("{}", jwt.expose())
        };

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({"iat": 999_940, "exp": 1_000_480, "iss": "Iv23li"})
        );
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key().as_ref())
            .verify(
                format!("{header}.{}", parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .unwrap();
    }
}
