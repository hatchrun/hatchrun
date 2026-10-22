// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Installation tokens, kept in KV and reused until shortly before they
//! expire.
//!
//! GitHub holds each App installation to 12,500 REST requests an hour,
//! and a token lasts an hour. Minting one for every launch, cache call
//! and reconciler pass spent that budget on tokens. Now the Worker keeps
//! each installation's token in the `TOKENS` namespace with its expiry
//! and mints a new one [`REFRESH_BEFORE`] it expires, on a miss, or when
//! GitHub answers a call with 401. KV is eventually consistent; a stale
//! read only hands out a token that still works.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use worker::{Env, KvStore, console_warn};

use crate::fetch::Sender;
use crate::http::{Request, Response};
use crate::launch::{APP_ID, APP_KEY, GITHUB_API_URL};
use crate::reconcile::time;
use crate::start::app_jwt;
use crate::{github, jwt};

/// The KV namespace that holds the tokens.
pub(crate) const BINDING: &str = "TOKENS";
/// How long before it expires a token is replaced, in ms. A launch, or
/// a cache call, never holds a token that long.
pub(crate) const REFRESH_BEFORE: u64 = 10 * 60 * 1000;
/// KV's shortest expiration, in seconds.
const MIN_TTL: u64 = 60;

/// A token and when it expires, Unix ms.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    /// The token's value.
    pub token: String,
    /// Unix ms.
    pub expires_at: u64,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Token {
    /// Whether the token is good for use at `now`.
    #[must_use]
    pub fn fresh(&self, now: u64) -> bool {
        now.saturating_add(REFRESH_BEFORE) < self.expires_at
    }
}

/// Why there is no token, or no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The App's id or key is missing or will not sign.
    Settings(String),
    /// GitHub could not be reached.
    Unreachable(String),
    /// GitHub refused to mint a token.
    GitHub {
        /// GitHub's status.
        status: u16,
        /// What GitHub said.
        message: String,
    },
    /// GitHub's answer was not a token.
    Answer(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings(message)
            | Self::Unreachable(message)
            | Self::Answer(message)
            | Self::GitHub { message, .. } => f.write_str(message),
        }
    }
}

/// Where tokens are kept between requests.
#[allow(async_fn_in_trait)] // Workers run one thread; no `Send` is needed.
pub trait Store {
    /// The installation's token, if one is kept.
    async fn read(&self, installation_id: u64) -> Option<Token>;
    /// Keeps `token` for the installation, replacing any other.
    async fn write(&self, installation_id: u64, token: &Token, now: u64);
}

impl<S: Store> Store for &S {
    async fn read(&self, installation_id: u64) -> Option<Token> {
        (**self).read(installation_id).await
    }

    async fn write(&self, installation_id: u64, token: &Token, now: u64) {
        (**self).write(installation_id, token, now).await;
    }
}

/// Makes new tokens.
#[allow(async_fn_in_trait)] // Workers run one thread; no `Send` is needed.
pub trait Mint {
    /// A new token of the installation, asked for through `github`.
    async fn mint<G: Sender>(&self, github: &G, installation_id: u64) -> Result<Token, Error>;
}

/// The `TOKENS` namespace. Without the binding nothing is kept and
/// every call mints.
pub struct Kv(Option<KvStore>);

impl Kv {
    /// The namespace bound in `env`.
    #[must_use]
    pub fn new(env: &Env) -> Self {
        match env.kv(BINDING) {
            Ok(store) => Self(Some(store)),
            Err(err) => {
                console_warn!("tokens: {err}; minting a token for every call");
                Self(None)
            }
        }
    }
}

fn key(installation_id: u64) -> String {
    format!("installation/{installation_id}")
}

impl Store for Kv {
    async fn read(&self, installation_id: u64) -> Option<Token> {
        let store = self.0.as_ref()?;
        match store.get(&key(installation_id)).json::<Token>().await {
            Ok(token) => token,
            Err(err) => {
                console_warn!("tokens: read installation {installation_id}: {err}");
                None
            }
        }
    }

    async fn write(&self, installation_id: u64, token: &Token, now: u64) {
        let Some(store) = &self.0 else {
            return;
        };
        let ttl = token.expires_at.saturating_sub(now) / 1000;
        if ttl < MIN_TTL {
            return;
        }
        let written = match serde_json::to_string(token) {
            Ok(value) => match store.put(&key(installation_id), value) {
                Ok(put) => put
                    .expiration_ttl(ttl)
                    .execute()
                    .await
                    .map_err(|err| err.to_string()),
                Err(err) => Err(err.to_string()),
            },
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = written {
            console_warn!("tokens: write installation {installation_id}: {err}");
        }
    }
}

/// The App, which trades a JWT signed with its key for a token.
pub struct App {
    /// The REST API.
    pub(crate) api: String,
    id: String,
    key: Vec<u8>,
}

impl App {
    /// The App at `api`, with client id `app_id` and its private key in
    /// PKCS#8 DER.
    #[must_use]
    pub fn new(api: String, id: String, key: Vec<u8>) -> Self {
        Self { api, id, key }
    }

    /// The App from the Worker's variables and secrets.
    ///
    /// # Errors
    ///
    /// A setting is missing or the key does not parse.
    pub fn read(env: &Env) -> Result<Self, String> {
        let get = |name: &str| {
            env.secret(name)
                .map(|value| value.to_string())
                .ok()
                .filter(|value| !value.is_empty())
        };
        let app_id = get(APP_ID).ok_or_else(|| format!("{APP_ID} is not set"))?;
        let pem = get(APP_KEY).ok_or_else(|| format!("{APP_KEY} is not set"))?;
        let app_key = jwt::pkcs8_der(&pem).map_err(|err| format!("{APP_KEY}: {err}"))?;
        let api = get(GITHUB_API_URL)
            .unwrap_or_else(|| github::API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Ok(Self::new(api, app_id, app_key))
    }
}

impl Mint for App {
    async fn mint<G: Sender>(&self, github: &G, installation_id: u64) -> Result<Token, Error> {
        let jwt = app_jwt(&self.id, &self.key)
            .await
            .map_err(Error::Settings)?;
        let response = github
            .send(&github::installation_token(
                &self.api,
                installation_id,
                &jwt,
            ))
            .await
            .map_err(Error::Unreachable)?;
        if !response.ok() {
            return Err(Error::GitHub {
                status: response.status,
                message: format!(
                    "installation token: GitHub answered {}: {}",
                    response.status,
                    response.text()
                ),
            });
        }
        let token = github::token(&response).map_err(Error::Answer)?;
        Ok(Token {
            token: token.token,
            // Without a readable expiry the token is used once and not
            // kept.
            expires_at: token
                .expires_at
                .as_deref()
                .and_then(time::parse)
                .unwrap_or(0),
        })
    }
}

/// Installation tokens for one request or pass: from what it already
/// holds, then the store, then minted.
pub struct Tokens<'a, S, M, G> {
    store: S,
    mint: M,
    github: &'a G,
    now: u64,
    held: RefCell<HashMap<u64, Token>>,
}

impl<'a, S: Store, M: Mint, G: Sender> Tokens<'a, S, M, G> {
    /// Tokens kept in `store`, minted by `mint`, for calls through
    /// `github` made around `now`.
    pub fn new(store: S, mint: M, github: &'a G, now: u64) -> Self {
        Self {
            store,
            mint,
            github,
            now,
            held: RefCell::new(HashMap::new()),
        }
    }

    /// A token of the installation.
    ///
    /// # Errors
    ///
    /// Why none could be minted.
    pub async fn token(&self, installation_id: u64) -> Result<String, Error> {
        self.find(installation_id).await.map(|(token, _)| token)
    }

    /// The token, and whether it was minted just now.
    async fn find(&self, installation_id: u64) -> Result<(String, bool), Error> {
        let held = self.held.borrow().get(&installation_id).cloned();
        if let Some(token) = held.filter(|token| token.fresh(self.now)) {
            return Ok((token.token, false));
        }
        if let Some(token) = self
            .store
            .read(installation_id)
            .await
            .filter(|token| token.fresh(self.now))
        {
            self.held
                .borrow_mut()
                .insert(installation_id, token.clone());
            return Ok((token.token, false));
        }
        self.replace(installation_id)
            .await
            .map(|token| (token, true))
    }

    /// Mints a new token and keeps it in place of the old one.
    async fn replace(&self, installation_id: u64) -> Result<String, Error> {
        let token = self.mint.mint(self.github, installation_id).await?;
        self.store.write(installation_id, &token, self.now).await;
        self.held
            .borrow_mut()
            .insert(installation_id, token.clone());
        Ok(token.token)
    }

    /// Sends the request `build` makes with a token of the installation.
    /// A 401 to a kept token means GitHub no longer takes it, so a new
    /// one is minted and the request sent once more.
    ///
    /// # Errors
    ///
    /// Why there is no token, or GitHub could not be reached. GitHub's
    /// own answer, error or not, is the `Ok` value.
    pub async fn send(
        &self,
        installation_id: u64,
        build: impl Fn(&str) -> Request,
    ) -> Result<Response, Error> {
        let (token, minted) = self.find(installation_id).await?;
        let response = self
            .github
            .send_as(installation_id, &build(&token))
            .await
            .map_err(Error::Unreachable)?;
        if response.status != 401 || minted {
            return Ok(response);
        }
        let token = self.replace(installation_id).await?;
        self.github
            .send_as(installation_id, &build(&token))
            .await
            .map_err(Error::Unreachable)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::testdb::block_on;

    pub(crate) const NOW: u64 = 1_792_140_000_000;
    const HOUR: u64 = 60 * 60 * 1000;

    /// Tokens kept in memory, shared by the requests of a test.
    #[derive(Default)]
    pub(crate) struct Memory(pub(crate) RefCell<HashMap<u64, Token>>);

    // Stand-ins answer at once.
    #[allow(clippy::unused_async_trait_impl)]
    impl Store for Memory {
        async fn read(&self, installation_id: u64) -> Option<Token> {
            self.0.borrow().get(&installation_id).cloned()
        }

        async fn write(&self, installation_id: u64, token: &Token, _now: u64) {
            self.0.borrow_mut().insert(installation_id, token.clone());
        }
    }

    /// Mints `ghs_1`, `ghs_2`, ... valid for an hour from [`NOW`].
    #[derive(Default)]
    pub(crate) struct Minted(pub(crate) Cell<u32>);

    // Stand-ins answer at once.
    #[allow(clippy::unused_async_trait_impl)]
    impl Mint for &Minted {
        async fn mint<G: Sender>(&self, _: &G, _: u64) -> Result<Token, Error> {
            self.0.set(self.0.get() + 1);
            Ok(Token {
                token: format!("ghs_{}", self.0.get()),
                expires_at: NOW + HOUR,
            })
        }
    }

    /// Answers GitHub calls with `answer` and counts them.
    pub(crate) struct GitHub<F> {
        pub(crate) answer: F,
        pub(crate) calls: Cell<u32>,
    }

    impl<F: Fn(&Request) -> Response> GitHub<F> {
        pub(crate) fn new(answer: F) -> Self {
            Self {
                answer,
                calls: Cell::new(0),
            }
        }
    }

    // Stand-ins answer at once.
    #[allow(clippy::unused_async_trait_impl)]
    impl<F: Fn(&Request) -> Response> Sender for GitHub<F> {
        async fn send(&self, request: &Request) -> Result<Response, String> {
            self.calls.set(self.calls.get() + 1);
            Ok((self.answer)(request))
        }
    }

    pub(crate) fn answer(status: u16, body: &str) -> Response {
        Response {
            headers: Vec::new(),
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    /// GitHub that takes only `token`.
    fn takes(token: &'static str) -> GitHub<impl Fn(&Request) -> Response> {
        GitHub::new(move |request: &Request| {
            if request.header_value("authorization") == Some(&format!("Bearer {token}")) {
                answer(201, "{}")
            } else {
                answer(401, r#"{"message":"Bad credentials"}"#)
            }
        })
    }

    fn register(token: &str) -> Request {
        github::generate_jit_config(github::API, "acme", token, "r", "hatchrun")
    }

    #[test]
    fn a_second_launch_reuses_the_token() {
        let store = Memory::default();
        let minted = Minted::default();
        let github = takes("ghs_1");
        for _ in 0..2 {
            let tokens = Tokens::new(&store, &minted, &github, NOW);
            let response = block_on(tokens.send(42, register)).unwrap();
            assert_eq!(response.status, 201);
        }
        assert_eq!(minted.0.get(), 1);
        assert_eq!(github.calls.get(), 2);
        // Another installation gets its own.
        let tokens = Tokens::new(&store, &minted, &github, NOW);
        assert_eq!(block_on(tokens.token(43)).unwrap(), "ghs_2");
    }

    #[test]
    fn a_token_about_to_expire_is_replaced() {
        let store = Memory::default();
        let minted = Minted::default();
        let github = takes("ghs_1");
        store.0.borrow_mut().insert(
            42,
            Token {
                token: "ghs_old".to_owned(),
                expires_at: NOW + REFRESH_BEFORE,
            },
        );
        let tokens = Tokens::new(&store, &minted, &github, NOW);
        assert_eq!(block_on(tokens.send(42, register)).unwrap().status, 201);
        assert_eq!(minted.0.get(), 1);
        assert_eq!(github.calls.get(), 1);
        assert_eq!(store.0.borrow()[&42].token, "ghs_1");
    }

    #[test]
    fn a_rejected_token_is_replaced() {
        let store = Memory::default();
        let minted = Minted::default();
        let github = takes("ghs_1");
        store.0.borrow_mut().insert(
            42,
            Token {
                token: "ghs_revoked".to_owned(),
                expires_at: NOW + HOUR,
            },
        );
        let tokens = Tokens::new(&store, &minted, &github, NOW);
        assert_eq!(block_on(tokens.send(42, register)).unwrap().status, 201);
        assert_eq!(minted.0.get(), 1);
        assert_eq!(github.calls.get(), 2);
        assert_eq!(store.0.borrow()[&42].token, "ghs_1");
        // The request goes on with the new token.
        assert_eq!(block_on(tokens.send(42, register)).unwrap().status, 201);
        assert_eq!((minted.0.get(), github.calls.get()), (1, 3));
    }

    #[test]
    fn a_new_token_that_is_refused_is_not_minted_again() {
        let store = Memory::default();
        let minted = Minted::default();
        let github = takes("ghs_other");
        let tokens = Tokens::new(&store, &minted, &github, NOW);
        assert_eq!(block_on(tokens.send(42, register)).unwrap().status, 401);
        assert_eq!((minted.0.get(), github.calls.get()), (1, 1));
    }

    #[test]
    fn tokens_are_fresh_until_ten_minutes_before_they_expire() {
        let token = Token {
            token: "ghs_x".to_owned(),
            expires_at: NOW + HOUR,
        };
        assert!(token.fresh(NOW));
        assert!(token.fresh(NOW + HOUR - REFRESH_BEFORE - 1));
        assert!(!token.fresh(NOW + HOUR - REFRESH_BEFORE));
        assert!(!format!("{token:?}").contains("ghs_x"));
    }
}
