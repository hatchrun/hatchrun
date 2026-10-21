// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Secrets and variables an organization's owners set for its jobs, in
//! the `secrets`, `variables` and `environments` tables (see [`store`]).
//! The rules and the sealing are kept apart from the Workers runtime so
//! they test natively.
//!
//! A secret or variable belongs to the organization, one repository or
//! one environment ([`Scope`]). Names follow GitHub's: letters, digits
//! and `_`, not starting with a digit, compared without case, and not
//! starting with `GITHUB_` or `HATCHRUN_` ([`check_name`]). So do the
//! limits: [`VALUE_MAX`] per value, [`ORG_MAX`], [`REPO_MAX`] and
//! [`ENVIRONMENT_MAX`] per scope, for secrets and variables each.
//!
//! A secret's value is sealed with AES-256-GCM through `WebCrypto`
//! ([`seal`]), under a random 96-bit nonce and with
//! `tenant|scope|repo|environment|name` as additional data
//! ([`additional_data`]), so a row copied to another scope or tenant
//! does not open. Plaintext lives only in the request that seals it and
//! is never logged or returned.
//!
//! The key is 32 random bytes, base64, in Cloudflare Secrets Store: the
//! [`KEY_BINDING`] binding in `wrangler.toml`, which `worker` 0.8 reads
//! with `Env::secret_store`. Secrets Store keeps it out of the Worker's
//! own settings, and an account's store holds up to 100 secrets of 1 KB,
//! enough for our keys but not customer values, hence Postgres for
//! those. [`KEY_ID`] names the key; each row records the id of the key
//! that sealed it. To rotate, add the new key to the store, bind the
//! old one as [`PREVIOUS_KEY_BINDING`] with its id in
//! [`PREVIOUS_KEY_ID`], and point [`KEY_BINDING`] and [`KEY_ID`] at the
//! new one: new values are sealed with the new key and old rows still
//! open ([`open`]) until they are sealed again.

pub(crate) mod store;

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use worker::Env;

/// Bytes in a value, as GitHub allows.
pub(crate) const VALUE_MAX: usize = 48 * 1024;
/// Secrets, or variables, of the organization itself.
pub(crate) const ORG_MAX: u64 = 1000;
/// Secrets, or variables, of one repository.
pub(crate) const REPO_MAX: u64 = 100;
/// Secrets, or variables, of one environment.
pub(crate) const ENVIRONMENT_MAX: u64 = 100;
/// Characters in a name. GitHub sets none; this one keeps rows small.
pub(crate) const NAME_MAX: usize = 255;
/// Prefixes GitHub and hatchrun set themselves.
const RESERVED: [&str; 2] = ["GITHUB_", "HATCHRUN_"];

/// Characters in an environment name.
const ENVIRONMENT_NAME_MAX: usize = 255;
/// Branch patterns of one environment.
const BRANCHES_MAX: usize = 50;
/// Characters in a branch pattern, as in a git ref name.
const PATTERN_MAX: usize = 255;
/// Reviewers of one environment, as GitHub allows.
const REVIEWERS_MAX: usize = 6;
/// Characters in a repository name, as GitHub allows.
const REPO_NAME_MAX: usize = 100;

/// Secrets Store binding with the current key.
pub(crate) const KEY_BINDING: &str = "SECRETS_KEY";
/// Variable with the current key's id.
pub(crate) const KEY_ID: &str = "SECRETS_KEY_ID";
/// Secrets Store binding with the key before it, while keys rotate.
pub(crate) const PREVIOUS_KEY_BINDING: &str = "SECRETS_KEY_PREVIOUS";
/// Variable with the previous key's id; the previous key is read only
/// while it is set.
pub(crate) const PREVIOUS_KEY_ID: &str = "SECRETS_KEY_PREVIOUS_ID";
/// Bytes in a key.
pub(crate) const KEY_BYTES: usize = 32;
/// Bytes in a nonce.
pub(crate) const NONCE_BYTES: usize = 12;

/// Where a secret or variable belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Every repository of the organization.
    Org,
    /// One repository, by name without its owner, in lower case.
    Repo(String),
    /// One environment.
    Environment {
        id: u64,
        name: String,
        /// The environment's repository; `None` when it covers every
        /// repository.
        repo: Option<String>,
    },
}

impl Scope {
    /// The `scope` column.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Scope::Org => "org",
            Scope::Repo(_) => "repo",
            Scope::Environment { .. } => "env",
        }
    }

    /// The `repo` column: set for a repository only, since an
    /// environment names its own.
    pub(crate) fn repo_column(&self) -> Option<&str> {
        match self {
            Scope::Repo(repo) => Some(repo),
            _ => None,
        }
    }

    /// The `environment_id` column.
    pub(crate) fn environment_id(&self) -> Option<u64> {
        match self {
            Scope::Environment { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// The repository it concerns, if one, for the activity list.
    pub(crate) fn repo(&self) -> Option<&str> {
        match self {
            Scope::Org => None,
            Scope::Repo(repo) => Some(repo),
            Scope::Environment { repo, .. } => repo.as_deref(),
        }
    }

    /// The environment's name, if one.
    pub(crate) fn environment(&self) -> Option<&str> {
        match self {
            Scope::Environment { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Most secrets, or variables, it holds.
    pub(crate) fn limit(&self) -> u64 {
        match self {
            Scope::Org => ORG_MAX,
            Scope::Repo(_) => REPO_MAX,
            Scope::Environment { .. } => ENVIRONMENT_MAX,
        }
    }

    /// The scope in words, as in "At most 100 secrets for repository web".
    pub(crate) fn describe(&self) -> String {
        match self {
            Scope::Org => "for the organization".to_owned(),
            Scope::Repo(repo) => format!("for repository {repo}"),
            Scope::Environment { name, .. } => format!("for environment {name}"),
        }
    }
}

/// Checks a secret or variable name.
///
/// # Errors
///
/// What is wrong, in words for the owner.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    let starts_well = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_');
    if !starts_well || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err("Names use letters, digits and _, and do not start with a digit".to_owned());
    }
    if name.len() > NAME_MAX {
        return Err(format!("Names are at most {NAME_MAX} characters"));
    }
    let upper = name.to_ascii_uppercase();
    if let Some(prefix) = RESERVED.iter().find(|prefix| upper.starts_with(*prefix)) {
        return Err(format!("Names starting with {prefix} are reserved"));
    }
    Ok(())
}

/// Checks a value's size. The value itself never appears in the error.
///
/// # Errors
///
/// When it is empty or over [`VALUE_MAX`] bytes.
pub(crate) fn check_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("The value is empty".to_owned());
    }
    if value.len() > VALUE_MAX {
        return Err(format!("Values are at most {} KB", VALUE_MAX / 1024));
    }
    Ok(())
}

/// A repository of `org` as the tables name it: `name` or
/// `org/name`, without the owner, in lower case, since GitHub's names
/// ignore case.
///
/// # Errors
///
/// When it is not a repository name, or names another owner.
pub(crate) fn repo_name(org: &str, given: &str) -> Result<String, String> {
    let name = match given.split_once('/') {
        Some((owner, name)) if owner.eq_ignore_ascii_case(org) => name,
        Some(_) => return Err(format!("{given} is not a repository of {org}")),
        None => given,
    };
    let valid = !name.is_empty()
        && name.len() <= REPO_NAME_MAX
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(format!("{given} is not a repository name"));
    }
    Ok(name.to_ascii_lowercase())
}

/// Checks an environment name. GitHub allows more; these stay safe in
/// a URL path as they are.
///
/// # Errors
///
/// What is wrong, in words for the owner.
pub(crate) fn check_environment_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= ENVIRONMENT_NAME_MAX
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err("Environment names use letters, digits, -, _ and .".to_owned())
    }
}

/// What an environment asks of the jobs that use it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Rules {
    /// Branch patterns, as in a workflow's `branches:` filter; empty
    /// for every branch.
    pub branches: Vec<String>,
    /// Whether a job waits for a reviewer's approval.
    pub require_approval: bool,
    /// GitHub logins who may approve.
    pub reviewers: Vec<String>,
}

impl Rules {
    /// Checks the rules and drops repeated patterns and reviewers.
    ///
    /// # Errors
    ///
    /// What is wrong, in words for the owner.
    pub(crate) fn validate(mut self) -> Result<Self, String> {
        self.branches = dedupe(self.branches, false);
        self.reviewers = dedupe(self.reviewers, true);
        if self.branches.len() > BRANCHES_MAX {
            return Err(format!("At most {BRANCHES_MAX} branch patterns"));
        }
        if let Some(bad) = self.branches.iter().find(|pattern| {
            pattern.trim().is_empty()
                || pattern.len() > PATTERN_MAX
                || pattern.chars().any(char::is_control)
        }) {
            return Err(format!("{bad:?} is not a branch pattern"));
        }
        if self.reviewers.len() > REVIEWERS_MAX {
            return Err(format!("At most {REVIEWERS_MAX} reviewers"));
        }
        if let Some(bad) = self
            .reviewers
            .iter()
            .find(|login| !crate::tenant::valid_login(login))
        {
            return Err(format!("{bad} is not a GitHub login"));
        }
        Ok(self)
    }
}

/// `items` in their order, each once; logins compare without case.
fn dedupe(items: Vec<String>, ignore_case: bool) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let seen = kept.iter().any(|other| {
            if ignore_case {
                other.eq_ignore_ascii_case(&item)
            } else {
                *other == item
            }
        });
        if !seen {
            kept.push(item);
        }
    }
    kept
}

/// The additional data a value of `scope` named `name` is sealed with:
/// `tenant|scope|repo|environment|name`, with the environment by id,
/// which outlives nothing it could be confused with, and the name in
/// upper case, since names ignore case. None of the parts can hold a
/// `|`, so no two places share one.
pub(crate) fn additional_data(tenant_id: &str, scope: &Scope, name: &str) -> Vec<u8> {
    let environment = scope
        .environment_id()
        .map(|id| id.to_string())
        .unwrap_or_default();
    format!(
        "{tenant_id}|{}|{}|{environment}|{}",
        scope.kind(),
        scope.repo_column().unwrap_or_default(),
        name.to_ascii_uppercase()
    )
    .into_bytes()
}

/// A key and its id.
#[derive(Clone)]
pub(crate) struct Key {
    pub id: String,
    bytes: [u8; KEY_BYTES],
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Key {
    /// The key `id` from its base64 value.
    ///
    /// # Errors
    ///
    /// When the id is empty or the value is not 32 bytes of base64. The
    /// value never appears in the error.
    pub(crate) fn parse(id: &str, value: &str) -> Result<Self, String> {
        if id.trim().is_empty() {
            return Err("a key needs an id".to_owned());
        }
        let bytes = STANDARD
            .decode(value.trim())
            .ok()
            .and_then(|bytes| <[u8; KEY_BYTES]>::try_from(bytes).ok())
            .ok_or_else(|| format!("key {id} is not {KEY_BYTES} bytes of base64"))?;
        Ok(Self {
            id: id.trim().to_owned(),
            bytes,
        })
    }
}

/// The keys the Worker holds: the one that seals, and while keys
/// rotate, the one before it.
#[derive(Debug, Clone)]
pub(crate) struct Keys {
    pub current: Key,
    pub previous: Option<Key>,
}

impl Keys {
    /// The key with this id.
    pub(crate) fn find(&self, id: &str) -> Option<&Key> {
        std::iter::once(&self.current)
            .chain(&self.previous)
            .find(|key| key.id == id)
    }

    /// The keys from the Worker's bindings and variables.
    ///
    /// # Errors
    ///
    /// A binding or id is missing, or a key is not 32 bytes.
    pub(crate) async fn read(env: &Env) -> Result<Self, String> {
        let var = |name: &str| {
            env.var(name)
                .map(|value| value.to_string())
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let id = var(KEY_ID).ok_or_else(|| format!("{KEY_ID} is not set"))?;
        let current = load(env, KEY_BINDING, &id).await?;
        let previous = match var(PREVIOUS_KEY_ID) {
            Some(id) => Some(load(env, PREVIOUS_KEY_BINDING, &id).await?),
            None => None,
        };
        Ok(Self { current, previous })
    }
}

/// The key in Secrets Store binding `binding`.
async fn load(env: &Env, binding: &str, id: &str) -> Result<Key, String> {
    let value = env
        .secret_store(binding)
        .map_err(|err| format!("{binding}: {err}"))?
        .get()
        .await
        .map_err(|err| format!("{binding}: {err}"))?
        .ok_or_else(|| format!("{binding} is empty"))?;
    Key::parse(id, &value).map_err(|err| format!("{binding}: {err}"))
}

/// A sealed value as its row keeps it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Sealed {
    /// The ciphertext and its tag, base64.
    pub ciphertext: String,
    /// The nonce, base64.
    pub nonce: String,
    pub key_id: String,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// AES-256-GCM. [`WebCrypto`] in the Worker; tests seal natively.
pub(crate) trait Cipher {
    /// The ciphertext of `plaintext` followed by its 16-byte tag.
    async fn encrypt(
        &self,
        key: &[u8; KEY_BYTES],
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, String>;

    /// The plaintext of what [`Cipher::encrypt`] returned, if key,
    /// nonce and additional data match and nothing was changed.
    async fn decrypt(
        &self,
        key: &[u8; KEY_BYTES],
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        sealed: &[u8],
    ) -> Result<Vec<u8>, String>;
}

/// The Workers runtime's `WebCrypto`.
pub(crate) struct WebCrypto;

impl Cipher for WebCrypto {
    async fn encrypt(
        &self,
        key: &[u8; KEY_BYTES],
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, String> {
        crate::webcrypto::aes_gcm_encrypt(key, nonce, aad, plaintext)
            .await
            .map_err(|err| err.to_string())
    }

    async fn decrypt(
        &self,
        key: &[u8; KEY_BYTES],
        nonce: &[u8; NONCE_BYTES],
        aad: &[u8],
        sealed: &[u8],
    ) -> Result<Vec<u8>, String> {
        crate::webcrypto::aes_gcm_decrypt(key, nonce, aad, sealed)
            .await
            .map_err(|err| err.to_string())
    }
}

/// Seals `value` with `key`, a fresh random `nonce` and the
/// [`additional_data`] of where it is stored.
///
/// # Errors
///
/// When the cipher fails.
pub(crate) async fn seal<C: Cipher>(
    cipher: &C,
    key: &Key,
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    value: &str,
) -> Result<Sealed, String> {
    let ciphertext = cipher
        .encrypt(&key.bytes, nonce, aad, value.as_bytes())
        .await?;
    Ok(Sealed {
        ciphertext: STANDARD.encode(ciphertext),
        nonce: STANDARD.encode(nonce),
        key_id: key.id.clone(),
    })
}

/// Opens a sealed value with the key its row names.
///
/// # Errors
///
/// When the key is gone, the additional data differs or the row was
/// changed. The error never holds the value.
// Plan 3 opens values for the jobs that name them; until then only the
// tests do.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn open<C: Cipher>(
    cipher: &C,
    keys: &Keys,
    sealed: &Sealed,
    aad: &[u8],
) -> Result<String, String> {
    let key = keys
        .find(&sealed.key_id)
        .ok_or_else(|| format!("no key {}", sealed.key_id))?;
    let nonce = STANDARD
        .decode(&sealed.nonce)
        .ok()
        .and_then(|nonce| <[u8; NONCE_BYTES]>::try_from(nonce).ok())
        .ok_or("the nonce is not 12 bytes")?;
    let ciphertext = STANDARD
        .decode(&sealed.ciphertext)
        .map_err(|_| "the ciphertext is not base64")?;
    let plaintext = cipher
        .decrypt(&key.bytes, &nonce, aad, &ciphertext)
        .await
        .map_err(|_| "the value does not decrypt here".to_owned())?;
    String::from_utf8(plaintext).map_err(|_| "the value is not UTF-8".to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};

    use super::*;
    use crate::testdb::block_on;

    /// AES-256-GCM from the `aes-gcm` crate, which lays out its output
    /// as `WebCrypto` does: ciphertext, then a 16-byte tag.
    pub(crate) struct Native;

    // The crate answers at once.
    #[allow(clippy::unused_async_trait_impl)]
    impl Cipher for Native {
        async fn encrypt(
            &self,
            key: &[u8; KEY_BYTES],
            nonce: &[u8; NONCE_BYTES],
            aad: &[u8],
            plaintext: &[u8],
        ) -> Result<Vec<u8>, String> {
            Aes256Gcm::new(key.into())
                .encrypt(
                    &Nonce::from(*nonce),
                    Payload {
                        msg: plaintext,
                        aad,
                    },
                )
                .map_err(|err| err.to_string())
        }

        async fn decrypt(
            &self,
            key: &[u8; KEY_BYTES],
            nonce: &[u8; NONCE_BYTES],
            aad: &[u8],
            sealed: &[u8],
        ) -> Result<Vec<u8>, String> {
            Aes256Gcm::new(key.into())
                .decrypt(&Nonce::from(*nonce), Payload { msg: sealed, aad })
                .map_err(|err| err.to_string())
        }
    }

    pub(crate) fn key(id: &str, byte: u8) -> Key {
        Key::parse(id, &STANDARD.encode([byte; KEY_BYTES])).unwrap()
    }

    const NONCE: [u8; NONCE_BYTES] = [7; NONCE_BYTES];

    fn repo(name: &str) -> Scope {
        Scope::Repo(name.to_owned())
    }

    fn environment(id: u64) -> Scope {
        Scope::Environment {
            id,
            name: "prod".to_owned(),
            repo: Some("web".to_owned()),
        }
    }

    #[test]
    fn names_follow_githubs_rules() {
        for name in ["TOKEN", "npm_token", "_X", "a1", "A_B_9"] {
            assert_eq!(check_name(name), Ok(()), "{name}");
        }
        for name in ["", "1TOKEN", "MY-TOKEN", "MY TOKEN", "TOKÉN", "a.b", "x/y"] {
            assert_eq!(
                check_name(name).unwrap_err(),
                "Names use letters, digits and _, and do not start with a digit",
                "{name:?}"
            );
        }
        assert!(check_name(&"A".repeat(NAME_MAX)).is_ok());
        assert!(check_name(&"A".repeat(NAME_MAX + 1)).is_err());
    }

    #[test]
    fn reserved_prefixes_are_refused_in_any_case() {
        assert_eq!(
            check_name("GITHUB_TOKEN").unwrap_err(),
            "Names starting with GITHUB_ are reserved"
        );
        assert!(check_name("github_token").is_err());
        assert_eq!(
            check_name("Hatchrun_Job").unwrap_err(),
            "Names starting with HATCHRUN_ are reserved"
        );
        assert!(check_name("GITHUB").is_ok());
        assert!(check_name("MY_GITHUB_TOKEN").is_ok());
    }

    #[test]
    fn values_are_at_most_48_kb() {
        assert!(check_value(&"x".repeat(VALUE_MAX)).is_ok());
        let err = check_value(&"s".repeat(VALUE_MAX + 1)).unwrap_err();
        assert_eq!(err, "Values are at most 48 KB");
        assert_eq!(check_value("").unwrap_err(), "The value is empty");
        // Bytes count, not characters.
        assert!(check_value(&"é".repeat(VALUE_MAX / 2 + 1)).is_err());
    }

    #[test]
    fn each_scope_has_its_limit() {
        assert_eq!(Scope::Org.limit(), 1000);
        assert_eq!(repo("web").limit(), 100);
        assert_eq!(environment(1).limit(), 100);
        assert_eq!(Scope::Org.kind(), "org");
        assert_eq!(repo("web").kind(), "repo");
        assert_eq!(environment(1).kind(), "env");
        assert_eq!(environment(1).repo_column(), None);
        assert_eq!(environment(1).repo(), Some("web"));
        assert_eq!(environment(1).describe(), "for environment prod");
    }

    #[test]
    fn repositories_are_named_without_their_owner() {
        assert_eq!(repo_name("acme", "Web").unwrap(), "web");
        assert_eq!(repo_name("acme", "ACME/web.js").unwrap(), "web.js");
        assert_eq!(
            repo_name("acme", "other/web").unwrap_err(),
            "other/web is not a repository of acme"
        );
        for bad in ["", "..", "a/b/c", "we b", "x?y", &"a".repeat(101)] {
            assert!(repo_name("acme", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn environment_names_are_path_safe() {
        assert!(check_environment_name("prod").is_ok());
        assert!(check_environment_name("eu-west.1_b").is_ok());
        for bad in ["", "pro d", "a/b", "%2F", &"e".repeat(256)] {
            assert!(check_environment_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn checks_environment_rules() {
        let rules = Rules {
            branches: vec!["main".into(), "release/*".into(), "main".into()],
            require_approval: true,
            reviewers: vec!["octo".into(), "Octo".into(), "hubot".into()],
        }
        .validate()
        .unwrap();
        assert_eq!(rules.branches, ["main", "release/*"]);
        assert_eq!(rules.reviewers, ["octo", "hubot"]);
        assert!(Rules::default().validate().is_ok());

        let bad = |rules: Rules| rules.validate().unwrap_err();
        assert_eq!(
            bad(Rules {
                branches: vec![" ".into()],
                ..Rules::default()
            }),
            "\" \" is not a branch pattern"
        );
        assert_eq!(
            bad(Rules {
                reviewers: vec!["not a login".into()],
                ..Rules::default()
            }),
            "not a login is not a GitHub login"
        );
        assert_eq!(
            bad(Rules {
                reviewers: (0..7).map(|i| format!("user{i}")).collect(),
                ..Rules::default()
            }),
            "At most 6 reviewers"
        );
        assert_eq!(
            bad(Rules {
                branches: (0..51).map(|i| format!("b{i}")).collect(),
                ..Rules::default()
            }),
            "At most 50 branch patterns"
        );
    }

    #[test]
    fn additional_data_names_the_place() {
        assert_eq!(
            additional_data("t1", &Scope::Org, "token"),
            b"t1|org|||TOKEN".to_vec()
        );
        assert_eq!(
            additional_data("t1", &repo("web"), "TOKEN"),
            b"t1|repo|web||TOKEN".to_vec()
        );
        assert_eq!(
            additional_data("t1", &environment(42), "Token"),
            b"t1|env||42|TOKEN".to_vec()
        );
    }

    #[test]
    fn keys_are_32_bytes_of_base64_and_never_printed() {
        let key = key("2026-10", 9);
        assert_eq!(key.id, "2026-10");
        assert!(!format!("{key:?}").contains("9, 9"));
        let short = Key::parse("k", &STANDARD.encode([1u8; 16])).unwrap_err();
        assert_eq!(short, "key k is not 32 bytes of base64");
        assert!(Key::parse("k", "not base64!").is_err());
        assert!(Key::parse(" ", &STANDARD.encode([1u8; 32])).is_err());
    }

    #[test]
    fn a_sealed_value_opens_where_it_was_sealed() {
        let keys = Keys {
            current: key("k1", 1),
            previous: None,
        };
        let aad = additional_data("t1", &repo("web"), "TOKEN");
        let sealed = block_on(seal(&Native, &keys.current, &NONCE, &aad, "hunter2")).unwrap();
        assert_eq!(sealed.key_id, "k1");
        assert_eq!(STANDARD.decode(&sealed.nonce).unwrap(), NONCE);
        // Ciphertext and a 16-byte tag; no plaintext in the row.
        assert_eq!(STANDARD.decode(&sealed.ciphertext).unwrap().len(), 7 + 16);
        assert!(!sealed.ciphertext.contains("hunter2"));
        assert!(!format!("{sealed:?}").contains(&sealed.ciphertext));
        assert_eq!(
            block_on(open(&Native, &keys, &sealed, &aad)).unwrap(),
            "hunter2"
        );
        // The name ignores case.
        let same = additional_data("t1", &repo("web"), "token");
        assert_eq!(
            block_on(open(&Native, &keys, &sealed, &same)).unwrap(),
            "hunter2"
        );
    }

    #[test]
    fn a_value_moved_anywhere_else_does_not_open() {
        let keys = Keys {
            current: key("k1", 1),
            previous: None,
        };
        let aad = additional_data("t1", &repo("web"), "TOKEN");
        let sealed = block_on(seal(&Native, &keys.current, &NONCE, &aad, "hunter2")).unwrap();
        for elsewhere in [
            additional_data("t2", &repo("web"), "TOKEN"),
            additional_data("t1", &repo("api"), "TOKEN"),
            additional_data("t1", &Scope::Org, "TOKEN"),
            additional_data("t1", &environment(1), "TOKEN"),
            additional_data("t1", &repo("web"), "OTHER"),
        ] {
            let err = block_on(open(&Native, &keys, &sealed, &elsewhere)).unwrap_err();
            assert_eq!(err, "the value does not decrypt here");
        }

        let mut changed = STANDARD.decode(&sealed.ciphertext).unwrap();
        changed[0] ^= 1;
        let tampered = Sealed {
            ciphertext: STANDARD.encode(changed),
            ..sealed.clone()
        };
        assert!(block_on(open(&Native, &keys, &tampered, &aad)).is_err());
    }

    #[test]
    fn rotated_keys_open_old_rows() {
        let old = key("k1", 1);
        let aad = additional_data("t1", &Scope::Org, "TOKEN");
        let sealed = block_on(seal(&Native, &old, &NONCE, &aad, "v1")).unwrap();
        let rotated = Keys {
            current: key("k2", 2),
            previous: Some(old),
        };
        assert_eq!(
            block_on(open(&Native, &rotated, &sealed, &aad)).unwrap(),
            "v1"
        );
        let resealed = block_on(seal(&Native, &rotated.current, &NONCE, &aad, "v1")).unwrap();
        assert_eq!(resealed.key_id, "k2");

        let retired = Keys {
            current: key("k2", 2),
            previous: None,
        };
        assert_eq!(
            block_on(open(&Native, &retired, &sealed, &aad)).unwrap_err(),
            "no key k1"
        );
        // A row that names a key it was not sealed with does not open.
        let relabeled = Sealed {
            key_id: "k2".to_owned(),
            ..sealed
        };
        assert!(block_on(open(&Native, &retired, &relabeled, &aad)).is_err());
    }
}
