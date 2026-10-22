// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Sealed CI secrets.
//!
//! A value is sealed with AES-256-GCM under a fresh 96-bit nonce. The
//! associated data names the tenant, the scope (a repository id, an
//! environment of a repository, or the organization), the kind (always a secret: vars are not sealed) and
//! the name, so a ciphertext copied to another tenant, repository or
//! name does not open. The row keeps the id of the key that sealed it;
//! [`Keyring`] holds the current key and, while a rotation runs, the
//! previous one. Nothing here puts a value in an error or in `Debug`
//! output.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit as _, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hatchrun_workflow::SecretRefs;
use worker::Env;

use crate::store::{Owner, SealedRow};

/// The most bytes a secret or var value may have.
pub const MAX_VALUE_BYTES: usize = 48 * 1024;
/// The most secrets, and the most vars, one repository holds.
pub const MAX_PER_REPO: u32 = 100;
/// The most secrets, and the most vars, one organization holds.
pub const MAX_PER_ORG: u32 = 1_000;
/// The most repositories an organization secret or variable may select.
pub const MAX_SELECTED_REPOS: usize = 1_000;
/// The longest name.
pub const NAME_MAX: usize = 100;
/// Bytes of an AES-GCM nonce.
pub const NONCE_BYTES: usize = 12;

/// A sealing key and its id.
#[derive(Clone)]
pub struct Key {
    /// Stored next to every value this key seals.
    pub id: String,
    /// The 256-bit key.
    pub bytes: [u8; 32],
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("id", &self.id)
            .field("bytes", &"<hidden>")
            .finish()
    }
}

/// The keys the Worker can open values with. Values are sealed with
/// `current`; `previous` only opens, until the re-seal step has moved
/// every row off it.
#[derive(Debug, Clone)]
pub struct Keyring {
    /// Seals new values.
    pub current: Key,
    /// The key being rotated out.
    pub previous: Option<Key>,
}

impl Keyring {
    /// The keys of the Secrets Store bindings `CI_SECRETS_KEY` and
    /// `CI_SECRETS_KEY_OLD` (base64 of 32 bytes), with ids from the
    /// variables `CI_SECRETS_KEY_ID` and `CI_SECRETS_KEY_OLD_ID`. The old
    /// pair is optional: an empty or missing old id means none.
    ///
    /// # Errors
    ///
    /// A binding or id is missing or the key is not 32 bytes of base64.
    /// The message names the binding, never the key.
    pub async fn load(env: &Env) -> Result<Keyring, String> {
        let current = load_key(env, "CI_SECRETS_KEY", "CI_SECRETS_KEY_ID")
            .await?
            .ok_or("CI_SECRETS_KEY_ID is not set")?;
        let previous = load_key(env, "CI_SECRETS_KEY_OLD", "CI_SECRETS_KEY_OLD_ID").await?;
        Ok(Keyring { current, previous })
    }

    fn key(&self, id: &str) -> Option<&Key> {
        std::iter::once(&self.current)
            .chain(self.previous.as_ref())
            .find(|key| key.id == id)
    }
}

/// `None` when the id variable is empty.
async fn load_key(env: &Env, binding: &str, id_var: &str) -> Result<Option<Key>, String> {
    let id = env
        .var(id_var)
        .map(|value| value.to_string())
        .unwrap_or_default();
    if id.is_empty() {
        return Ok(None);
    }
    let value = env
        .secret_store(binding)
        .map_err(|_| format!("{binding} is not bound"))?
        .get()
        .await
        .map_err(|_| format!("{binding} cannot be read"))?
        .ok_or_else(|| format!("{binding} is empty"))?;
    Ok(Some(Key {
        id,
        bytes: parse_key(&value).ok_or_else(|| format!("{binding} is not 32 bytes of base64"))?,
    }))
}

fn parse_key(text: &str) -> Option<[u8; 32]> {
    STANDARD.decode(text.trim()).ok()?.try_into().ok()
}

/// What a value is bound to.
#[derive(Debug, Clone, Copy)]
pub struct Aad<'a> {
    /// The tenant.
    pub tenant: &'a str,
    /// Whose row it is.
    pub owner: Owner,
    /// The secret's name, already uppercased.
    pub name: &'a str,
}

impl Aad<'_> {
    fn bytes(&self) -> Vec<u8> {
        let scope = match self.owner {
            Owner::Org => "org".to_owned(),
            Owner::Repo(id) => id.to_string(),
            Owner::Environment {
                repo_id,
                environment_id,
            } => format!("{repo_id}/env/{environment_id}"),
        };
        format!(
            "hatchrun-ci-secret\0{}\0{scope}\0{}",
            self.tenant, self.name
        )
        .into_bytes()
    }
}

/// Why a value did not open. Never carries the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// No key of the keyring has this id.
    UnknownKey(String),
    /// The sealed text is damaged, or the key or the associated data is
    /// not the ones it was sealed with.
    Corrupt,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::UnknownKey(id) => write!(f, "sealed with unknown key {id}"),
            SecretError::Corrupt => f.write_str("sealed value does not open"),
        }
    }
}

impl std::error::Error for SecretError {}

/// Seals `value` under `key`: base64 of `nonce || ciphertext`. `nonce`
/// must be fresh for every call (see [`fresh_nonce`]).
///
/// # Panics
///
/// Never for a value within [`MAX_VALUE_BYTES`]: AES-GCM refuses only
/// inputs of 64 GiB and more.
#[must_use]
pub fn seal(key: &Key, aad: &Aad<'_>, nonce: [u8; NONCE_BYTES], value: &str) -> String {
    let cipher = Aes256Gcm::new(&key.bytes.into());
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: value.as_bytes(),
                aad: &aad.bytes(),
            },
        )
        .expect("AES-GCM seals any value of at most MAX_VALUE_BYTES");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    STANDARD.encode(out)
}

/// Opens what [`seal`] sealed with the key `key_id` names.
///
/// # Errors
///
/// [`SecretError::UnknownKey`] when the keyring has no such key;
/// [`SecretError::Corrupt`] when the text is damaged or `aad` is not the
/// one it was sealed with.
pub fn open(
    keyring: &Keyring,
    key_id: &str,
    aad: &Aad<'_>,
    sealed: &str,
) -> Result<String, SecretError> {
    let key = keyring
        .key(key_id)
        .ok_or_else(|| SecretError::UnknownKey(key_id.to_owned()))?;
    let bytes = STANDARD.decode(sealed).map_err(|_| SecretError::Corrupt)?;
    if bytes.len() < NONCE_BYTES {
        return Err(SecretError::Corrupt);
    }
    let (nonce, data) = bytes.split_at(NONCE_BYTES);
    let plain = Aes256Gcm::new(&key.bytes.into())
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: data,
                aad: &aad.bytes(),
            },
        )
        .map_err(|_| SecretError::Corrupt)?;
    String::from_utf8(plain).map_err(|_| SecretError::Corrupt)
}

/// Opens a value with the key it names and seals it again with the
/// keyring's current key.
///
/// # Errors
///
/// As [`open`].
pub fn reseal(
    keyring: &Keyring,
    key_id: &str,
    aad: &Aad<'_>,
    sealed: &str,
    nonce: [u8; NONCE_BYTES],
) -> Result<String, SecretError> {
    let value = open(keyring, key_id, aad, sealed)?;
    Ok(seal(&keyring.current, aad, nonce, &value))
}

/// The values `refs` names (all of them when `refs.all`), from the
/// organization's, the repository's and the environment's rows; the
/// most specific row of a name wins. A name with no row is left out.
///
/// # Errors
///
/// The first secret that does not open, with the reason. The name is
/// the secret's; the error never carries a value.
pub fn job_secrets(
    refs: &SecretRefs,
    rows: &[SealedRow],
    keyring: &Keyring,
    tenant: &str,
) -> Result<BTreeMap<String, String>, (String, SecretError)> {
    // Organization rows first, then the repository's, then the
    // environment's, so each replaces the one before.
    let mut chosen: BTreeMap<&str, &SealedRow> = BTreeMap::new();
    let mut ordered: Vec<&SealedRow> = rows.iter().collect();
    ordered.sort_by_key(|row| specificity(row.owner()));
    for row in ordered {
        if refs.all || refs.names.contains(&row.name) {
            chosen.insert(&row.name, row);
        }
    }
    let mut out = BTreeMap::new();
    for (name, row) in chosen {
        let aad = Aad {
            tenant,
            owner: row.owner(),
            name,
        };
        let value =
            open(keyring, &row.key_id, &aad, &row.sealed).map_err(|err| (name.to_owned(), err))?;
        out.insert(name.to_owned(), value);
    }
    Ok(out)
}

/// 0 for the organization, 1 for a repository, 2 for an environment.
fn specificity(owner: Owner) -> u8 {
    match owner {
        Owner::Org => 0,
        Owner::Repo(_) => 1,
        Owner::Environment { .. } => 2,
    }
}

/// A random nonce from the Web Crypto RNG.
///
/// # Errors
///
/// The runtime has no `crypto`.
pub fn fresh_nonce() -> Result<[u8; NONCE_BYTES], String> {
    hatchrun_cloud::webcrypto::random_bytes(NONCE_BYTES)
        .map_err(|err| err.to_string())?
        .try_into()
        .map_err(|_| "short nonce".to_owned())
}

/// The uppercased name, if it is a valid secret or var name: letters,
/// digits and `_`, not starting with a digit or `GITHUB_`, at most
/// [`NAME_MAX`] characters.
///
/// # Errors
///
/// The message to show the user.
pub fn valid_name(name: &str) -> Result<String, String> {
    let name = name.to_ascii_uppercase();
    if name.is_empty() {
        return Err("Secret names cannot be empty".to_owned());
    }
    if name.chars().count() > NAME_MAX {
        return Err(format!("Secret names are at most {NAME_MAX} characters"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("Use letters, digits and _ in secret names".to_owned());
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return Err("Secret names cannot start with a digit".to_owned());
    }
    if name.starts_with("GITHUB_") {
        return Err("Secret names cannot start with GITHUB_".to_owned());
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::tests::{random_key, random_nonce};
    use std::sync::LazyLock;

    const PLAIN: &str = "hunter2-token";

    /// The current key for this test run, shared by `ring()` calls.
    static CURRENT: LazyLock<[u8; 32]> = LazyLock::new(random_key);

    fn key(id: &str) -> Key {
        Key {
            id: id.into(),
            bytes: random_key(),
        }
    }

    fn ring() -> Keyring {
        Keyring {
            current: Key {
                id: "2026-10".into(),
                bytes: *CURRENT,
            },
            previous: None,
        }
    }

    fn aad(name: &str) -> Aad<'_> {
        Aad {
            tenant: "t1",
            owner: Owner::Repo(7),
            name,
        }
    }

    #[test]
    fn round_trips() {
        let ring = ring();
        let sealed = seal(&ring.current, &aad("A"), random_nonce(), PLAIN);
        assert!(!sealed.contains(PLAIN));
        assert_eq!(open(&ring, "2026-10", &aad("A"), &sealed).unwrap(), PLAIN);
    }

    #[test]
    fn another_row_cannot_open_it() {
        let ring = ring();
        let sealed = seal(&ring.current, &aad("A"), random_nonce(), PLAIN);
        let other = [
            aad("B"),
            Aad {
                tenant: "t2",
                ..aad("A")
            },
            Aad {
                owner: Owner::Repo(8),
                ..aad("A")
            },
            Aad {
                owner: Owner::Org,
                ..aad("A")
            },
        ];
        assert_eq!(
            open(&ring, "2026-10", &aad("B"), &sealed),
            Err(SecretError::Corrupt)
        );
        for aad in other {
            assert_eq!(
                open(&ring, "2026-10", &aad, &sealed),
                Err(SecretError::Corrupt)
            );
        }
    }

    #[test]
    fn a_wrong_key_or_damaged_text_fails() {
        let ring = ring();
        let sealed = seal(&ring.current, &aad("A"), random_nonce(), PLAIN);
        let wrong = Keyring {
            current: key("2026-10"),
            previous: None,
        };
        assert_eq!(
            open(&wrong, "2026-10", &aad("A"), &sealed),
            Err(SecretError::Corrupt)
        );
        let mut bytes = STANDARD.decode(&sealed).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        assert_eq!(
            open(&ring, "2026-10", &aad("A"), &STANDARD.encode(bytes)),
            Err(SecretError::Corrupt)
        );
        assert_eq!(
            open(&ring, "2026-10", &aad("A"), "AAAA"),
            Err(SecretError::Corrupt)
        );
        assert_eq!(
            open(&ring, "2026-10", &aad("A"), "not base64!"),
            Err(SecretError::Corrupt)
        );
    }

    #[test]
    fn a_nonce_makes_a_different_ciphertext() {
        let k = key("k");
        assert_ne!(
            seal(&k, &aad("A"), random_nonce(), PLAIN),
            seal(&k, &aad("A"), random_nonce(), PLAIN)
        );
    }

    #[test]
    fn opens_a_value_sealed_with_the_previous_key() {
        let old = key("2025-01");
        let sealed = seal(&old, &aad("A"), random_nonce(), PLAIN);
        let ring = Keyring {
            current: ring().current,
            previous: Some(old),
        };
        assert_eq!(open(&ring, "2025-01", &aad("A"), &sealed).unwrap(), PLAIN);
    }

    #[test]
    fn unknown_key_id_is_an_error() {
        let sealed = seal(&ring().current, &aad("A"), random_nonce(), PLAIN);
        assert_eq!(
            open(&ring(), "2025-01", &aad("A"), &sealed),
            Err(SecretError::UnknownKey("2025-01".into()))
        );
    }

    #[test]
    fn reseal_moves_a_value_to_the_current_key() {
        let old = key("2025-01");
        let sealed = seal(&old, &aad("A"), random_nonce(), PLAIN);
        let ring = Keyring {
            current: ring().current,
            previous: Some(old),
        };
        let again = reseal(&ring, "2025-01", &aad("A"), &sealed, random_nonce()).unwrap();
        let only_current = Keyring {
            current: ring.current.clone(),
            previous: None,
        };
        assert_eq!(
            open(&only_current, "2026-10", &aad("A"), &again).unwrap(),
            PLAIN
        );
    }

    #[test]
    fn errors_never_hold_the_value() {
        let ring = ring();
        let sealed = seal(&ring.current, &aad("A"), random_nonce(), PLAIN);
        let errors = [
            open(&ring, "2026-10", &aad("B"), &sealed).unwrap_err(),
            open(&ring, "2025-01", &aad("A"), &sealed).unwrap_err(),
        ];
        for err in errors {
            assert!(!format!("{err:?}").contains(PLAIN));
            assert!(!err.to_string().contains(PLAIN));
        }
        let shown = format!("{ring:?}");
        assert!(!shown.contains(&format!("{:?}", ring.current.bytes)));
        assert!(shown.contains("<hidden>"));
    }

    #[test]
    fn names_are_uppercased() {
        assert_eq!(valid_name("npm_token").unwrap(), "NPM_TOKEN");
    }

    #[test]
    fn bad_names_are_refused() {
        assert_eq!(
            valid_name("GITHUB_X").unwrap_err(),
            "Secret names cannot start with GITHUB_"
        );
        assert_eq!(
            valid_name("1A").unwrap_err(),
            "Secret names cannot start with a digit"
        );
        assert_eq!(
            valid_name("A-B").unwrap_err(),
            "Use letters, digits and _ in secret names"
        );
        assert_eq!(
            valid_name(&"A".repeat(101)).unwrap_err(),
            "Secret names are at most 100 characters"
        );
        assert!(valid_name(&"A".repeat(100)).is_ok());
        assert!(valid_name("").is_err());
    }

    #[test]
    fn keys_parse_from_base64() {
        let k = random_key();
        assert_eq!(parse_key(&STANDARD.encode(k)), Some(k));
        assert_eq!(parse_key(&STANDARD.encode(&k[..31])), None);
        assert_eq!(parse_key("%%"), None);
    }

    fn row(owner: Owner, name: &str, value: &str) -> SealedRow {
        let ring = ring();
        let aad = Aad {
            tenant: "t1",
            owner,
            name,
        };
        SealedRow {
            repo_id: owner.repo_id(),
            environment_id: owner.environment_id(),
            name: name.into(),
            key_id: ring.current.id.clone(),
            sealed: seal(&ring.current, &aad, random_nonce(), value),
        }
    }

    fn refs(all: bool, names: &[&str]) -> SecretRefs {
        SecretRefs {
            all,
            names: names.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    #[test]
    fn repo_value_wins() {
        let rows = [
            row(Owner::Org, "NPM_TOKEN", "org"),
            row(Owner::Repo(7), "NPM_TOKEN", "repo"),
        ];
        let got = job_secrets(&refs(false, &["NPM_TOKEN"]), &rows, &ring(), "t1").unwrap();
        assert_eq!(got, BTreeMap::from([("NPM_TOKEN".into(), "repo".into())]));
    }

    #[test]
    fn the_most_specific_row_wins() {
        let env = Owner::Environment {
            repo_id: 7,
            environment_id: 1,
        };
        // Handed over in no particular order.
        let rows = [
            row(env, "T", "env"),
            row(Owner::Org, "T", "org"),
            row(Owner::Repo(7), "T", "repo"),
        ];
        let got = job_secrets(&refs(false, &["T"]), &rows, &ring(), "t1").unwrap();
        assert_eq!(got, BTreeMap::from([("T".into(), "env".into())]));
    }

    #[test]
    fn environment_values_are_bound_to_their_environment() {
        let ring = ring();
        let env = |environment_id| Owner::Environment {
            repo_id: 7,
            environment_id,
        };
        let at = |owner| Aad {
            tenant: "t1",
            owner,
            name: "A",
        };
        let sealed = seal(&ring.current, &at(env(1)), random_nonce(), PLAIN);
        assert_eq!(open(&ring, "2026-10", &at(env(1)), &sealed).unwrap(), PLAIN);
        for other in [env(2), Owner::Repo(7), Owner::Org] {
            assert_eq!(
                open(&ring, "2026-10", &at(other), &sealed),
                Err(SecretError::Corrupt)
            );
        }
        // Organization and repository values open as before.
        for owner in [Owner::Org, Owner::Repo(7)] {
            let sealed = seal(&ring.current, &at(owner), random_nonce(), PLAIN);
            assert_eq!(open(&ring, "2026-10", &at(owner), &sealed).unwrap(), PLAIN);
        }
    }

    #[test]
    fn only_named_secrets_go() {
        let rows = [row(Owner::Repo(7), "A", "a"), row(Owner::Repo(7), "B", "b")];
        let got = job_secrets(&refs(false, &["A"]), &rows, &ring(), "t1").unwrap();
        assert_eq!(got, BTreeMap::from([("A".into(), "a".into())]));
    }

    #[test]
    fn all_refs_send_every_secret() {
        let rows = [row(Owner::Repo(7), "A", "a"), row(Owner::Org, "B", "b")];
        let got = job_secrets(&refs(true, &[]), &rows, &ring(), "t1").unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn missing_secret_is_left_out() {
        let got = job_secrets(&refs(false, &["A"]), &[], &ring(), "t1").unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn unreadable_secret_names_itself() {
        let mut bad = row(Owner::Repo(7), "A", "a");
        bad.key_id = "retired".into();
        let err = job_secrets(&refs(false, &["A"]), &[bad], &ring(), "t1").unwrap_err();
        assert_eq!(err, ("A".into(), SecretError::UnknownKey("retired".into())));
    }
}
