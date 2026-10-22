// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Whether an installation of the App can become a tenant for the
//! signed-in user, kept apart from the Workers runtime so it tests
//! natively.
//!
//! The installation id arrives in a URL anyone can write, so it counts
//! for nothing until three answers from GitHub agree: the App's JWT
//! reads the installation, the user's token sees it in
//! `/user/installations`, and the user's membership of its organization
//! is an active owner's.

use crate::github::{Installation, Membership};

/// Why an installation cannot become a tenant for this user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The user's token does not see it: someone else's installation,
    /// or a made-up id.
    Unseen,
    /// It is on a personal account. Runners register with an
    /// organization.
    PersonalAccount,
    /// An owner suspended it.
    Suspended,
    /// The user is not an active owner of its organization.
    NotOwner,
    /// GitHub would not say who owns the organization: the App lacks
    /// the Members permission there, usually because an owner has not
    /// accepted it yet.
    OwnerUnknown,
}

impl Refusal {
    /// The code the setup URL hands the app in `?error=`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unseen => "install-unseen",
            Self::PersonalAccount => "install-personal",
            Self::Suspended => "install-suspended",
            Self::NotOwner => "install-not-owner",
            Self::OwnerUnknown => "install-owner-unknown",
        }
    }

    /// What the app shows, for the account named `login`.
    #[must_use]
    pub fn message(self, login: &str) -> String {
        match self {
            Self::Unseen => "that installation isn't one you can see on GitHub".to_owned(),
            Self::PersonalAccount => format!(
                "{login} is a personal account. hatchrun runs jobs for organizations, so install it on one of those"
            ),
            Self::Suspended => format!(
                "hatchrun is suspended on {login}. An owner can unsuspend it in the organization's GitHub settings"
            ),
            Self::NotOwner => format!("only owners of {login} can set it up on hatchrun"),
            Self::OwnerUnknown => format!(
                "GitHub won't tell hatchrun who owns {login}. An owner needs to accept hatchrun's updated permissions in the organization's settings"
            ),
        }
    }
}

/// Checks the installation the App's JWT read against the ones the
/// user's token sees, before asking about ownership.
///
/// # Errors
///
/// The first thing that rules the installation out.
pub fn check(installation: &Installation, visible: &[Installation]) -> Result<(), Refusal> {
    if !visible.iter().any(|seen| seen.id == installation.id) {
        return Err(Refusal::Unseen);
    }
    if installation.account.kind != "Organization" {
        return Err(Refusal::PersonalAccount);
    }
    if installation.suspended_at.is_some() {
        return Err(Refusal::Suspended);
    }
    Ok(())
}

/// Checks that `membership`, the user's membership of the
/// installation's organization, is an active owner's of that same
/// organization.
///
/// # Errors
///
/// [`Refusal::NotOwner`] otherwise.
pub fn check_owner(
    installation: &Installation,
    membership: Option<&Membership>,
) -> Result<(), Refusal> {
    match membership {
        Some(membership)
            if membership.is_admin() && membership.organization.id == installation.account.id =>
        {
            Ok(())
        }
        _ => Err(Refusal::NotOwner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Account, Org};

    fn installation(id: u64, kind: &str) -> Installation {
        Installation {
            id,
            account: Account {
                id: 9,
                login: "acme".to_owned(),
                kind: kind.to_owned(),
            },
            suspended_at: None,
            permissions: std::collections::BTreeMap::new(),
        }
    }

    fn membership(org: u64, role: &str, state: &str) -> Membership {
        Membership {
            role: role.to_owned(),
            state: state.to_owned(),
            organization: Org {
                id: org,
                login: "acme".to_owned(),
                avatar_url: String::new(),
            },
        }
    }

    #[test]
    fn accepts_an_organization_the_user_sees() {
        let org = installation(5, "Organization");
        assert_eq!(check(&org, &[installation(4, "User"), org.clone()]), Ok(()));
        assert_eq!(
            check_owner(&org, Some(&membership(9, "admin", "active"))),
            Ok(())
        );
    }

    #[test]
    fn refuses_an_installation_the_user_cannot_see() {
        let org = installation(5, "Organization");
        assert_eq!(
            check(&org, &[installation(6, "Organization")]),
            Err(Refusal::Unseen)
        );
        assert_eq!(check(&org, &[]), Err(Refusal::Unseen));
    }

    #[test]
    fn refuses_personal_accounts() {
        let user = installation(5, "User");
        assert_eq!(
            check(&user, std::slice::from_ref(&user)),
            Err(Refusal::PersonalAccount)
        );
    }

    #[test]
    fn refuses_suspended_installations() {
        let mut org = installation(5, "Organization");
        org.suspended_at = Some("2026-10-01T00:00:00Z".to_owned());
        assert_eq!(
            check(&org, std::slice::from_ref(&org)),
            Err(Refusal::Suspended)
        );
    }

    #[test]
    fn needs_an_active_owner_of_that_organization() {
        let org = installation(5, "Organization");
        for membership in [
            None,
            Some(membership(9, "member", "active")),
            Some(membership(9, "admin", "pending")),
            Some(membership(10, "admin", "active")),
        ] {
            assert_eq!(
                check_owner(&org, membership.as_ref()),
                Err(Refusal::NotOwner),
                "{membership:?}"
            );
        }
    }

    #[test]
    fn codes_are_distinct() {
        let all = [
            Refusal::Unseen,
            Refusal::PersonalAccount,
            Refusal::Suspended,
            Refusal::NotOwner,
            Refusal::OwnerUnknown,
        ];
        let mut codes: Vec<_> = all.iter().map(|refusal| refusal.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert!(Refusal::NotOwner.message("acme").contains("acme"));
    }
}
