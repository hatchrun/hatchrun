// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tenants: one per GitHub organization that uses hatchrun.io.

/// Characters of a tenant id. connect.yaml accepts `[a-z0-9]`; these
/// are 32 of them, so each random byte gives five bits.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// Characters in a tenant id: 80 random bits.
pub const ID_LEN: usize = 16;

/// A tenant id from [`ID_LEN`] random bytes. The id names the tenant in
/// tokens (`tenant:<id>`), so it is random rather than the org's name,
/// which its owners can change.
///
/// # Panics
///
/// When given fewer than [`ID_LEN`] bytes.
#[must_use]
pub fn new_id(random: &[u8]) -> String {
    assert!(random.len() >= ID_LEN, "a tenant id needs {ID_LEN} bytes");
    random[..ID_LEN]
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(byte & 31)]))
        .collect()
}

/// Whether `org` can be a GitHub organization login, so it is safe in
/// an API path.
#[must_use]
pub fn valid_login(org: &str) -> bool {
    !org.is_empty()
        && org.len() <= 39
        && org
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !org.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_fit_connect_yaml() {
        let id = new_id(&[0, 31, 32, 255, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99]);
        assert_eq!(id, "a7a7bcdefghijklm");
        assert_eq!(id.len(), ID_LEN);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
    }

    #[test]
    fn logins_are_path_safe() {
        assert!(valid_login("acme-corp"));
        assert!(valid_login("A1"));
        assert!(!valid_login(""));
        assert!(!valid_login("-acme"));
        assert!(!valid_login("acme/../x"));
        assert!(!valid_login(&"a".repeat(40)));
    }
}
