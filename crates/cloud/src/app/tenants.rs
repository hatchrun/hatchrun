// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Setting up an organization, which starts with installing the App.
//!
//! A user token from the App sees only accounts where the App is
//! installed, so the App goes on first and GitHub's own install page
//! picks the organization.
//!
//! - `GET /api/installations` lists the App's installations on
//!   organizations the user can see, whether each has a tenant they are
//!   in, and the install link for another one.
//! - `GET /api/github/setup` is the App's setup URL: GitHub sends the
//!   browser there after an install, with the installation id and the
//!   state the install link carried.
//! - `POST /api/tenants` with `{"installationId": id}` does what the
//!   setup URL does for an installation already listed.
//! - `GET /api/tenants` lists the user's tenants;
//!   `GET /api/tenants/<org>` shows one, with the user's role in it.
//!
//! The setup URL and `POST /api/tenants` create the organization's
//! tenant, or add the user to the one that exists, once [`install`]
//! accepts the installation.
//!
//! Membership in the database says which tenants a user sees. Acting on one also
//! needs the user to own the organization on GitHub at that moment, so
//! someone removed as an owner loses access at once.

use serde::{Deserialize, Serialize};
use worker::{Request, Response, console_error};

use super::billing::join_listed_beta;
use super::{Answer, Ctx, Failure, found};
use crate::db::params;
use crate::fetch::send;
use crate::github::{self, Installation, Membership};
use crate::install::{self, Refusal};
use crate::start::app_jwt;
use crate::tokens::APP_KEY;
use crate::{jwt, now_ms, session, tenant, webcrypto};

/// A tenant row.
#[derive(Debug, Deserialize)]
pub(crate) struct TenantRow {
    pub id: String,
    pub org_id: u64,
    pub org_login: String,
    pub installation_id: Option<u64>,
    pub beta: bool,
}

pub(crate) const TENANT_COLUMNS: &str = "tenants.id, tenants.org_id, tenants.org_login, \
    tenants.installation_id, tenants.beta";

/// The tenant of an organization, by login, that a user is a member
/// of. Binds the login and the user id.
pub(crate) fn authorize_query() -> String {
    format!(
        "SELECT {TENANT_COLUMNS} FROM tenants \
         JOIN members ON members.tenant_id = tenants.id \
         WHERE lower(tenants.org_login) = lower($1) AND members.user_id = $2"
    )
}

/// A tenant as the app shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TenantView {
    id: String,
    org: OrgView,
    installation_id: Option<u64>,
    /// Installs the App again after it was removed.
    install_url: String,
    /// What the user may do in the organization: `admin` for an owner,
    /// `member` for anyone else in it.
    role: Role,
}

/// The user's role in an organization, from GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Admin,
    Member,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrgView {
    id: u64,
    login: String,
    avatar_url: String,
}

fn install_url(ctx: &Ctx<'_>) -> String {
    github::install_url(
        &ctx.config.github_web,
        &ctx.config.app_slug,
        &session::install_state(&ctx.session.id),
    )
}

fn avatar(org_id: u64) -> String {
    format!("https://avatars.githubusercontent.com/u/{org_id}?v=4")
}

fn view(ctx: &Ctx<'_>, row: TenantRow, role: Role) -> TenantView {
    TenantView {
        install_url: install_url(ctx),
        id: row.id,
        org: OrgView {
            id: row.org_id,
            avatar_url: avatar(row.org_id),
            login: row.org_login,
        },
        installation_id: row.installation_id,
        role,
    }
}

/// The user's membership of `org` on GitHub, if they own it.
async fn owned(ctx: &Ctx<'_>, org: &str) -> Result<Option<Membership>, Failure> {
    Ok(membership(ctx, org)
        .await?
        .map_err(|refusal| Failure::new(403, refusal.message(org)))?
        .filter(Membership::is_admin))
}

/// The user's membership of `org`, `None` when they are not in it, or
/// [`Refusal::OwnerUnknown`] when GitHub refuses to say.
async fn membership(
    ctx: &Ctx<'_>,
    org: &str,
) -> Result<Result<Option<Membership>, Refusal>, Failure> {
    let answer = send(&github::membership(
        &ctx.config.github_api,
        &ctx.session.token,
        org,
    ))
    .await?;
    if answer.status == 403 {
        return Ok(Err(Refusal::OwnerUnknown));
    }
    Ok(Ok(github::parse_membership(&answer)?))
}

/// The tenant of `org` in the database, if the user is a member of it.
/// Not found otherwise, so the answer does not say which organizations
/// use hatchrun.
async fn listed(ctx: &Ctx<'_>, org: &str) -> Result<TenantRow, Failure> {
    let missing = || Failure::new(404, format!("no organization {org} here"));
    if !tenant::valid_login(org) {
        return Err(missing());
    }
    let row: Option<TenantRow> = ctx
        .db
        .first(&authorize_query(), &params![org, ctx.session.user.id])
        .await?;
    row.ok_or_else(missing)
}

/// The tenant of `org` and the user's role in the organization, if the
/// user is a member of the tenant and still in the organization on
/// GitHub.
async fn authorize_member(ctx: &Ctx<'_>, org: &str) -> Result<(TenantRow, Role), Failure> {
    let row = listed(ctx, org).await?;
    let membership = membership(ctx, &row.org_login)
        .await?
        .map_err(|refusal| Failure::new(403, refusal.message(&row.org_login)))?
        .filter(|membership| membership.state == "active")
        .ok_or_else(|| Failure::new(403, format!("you are not in {} on GitHub", row.org_login)))?;
    let role = if membership.is_admin() {
        Role::Admin
    } else {
        Role::Member
    };
    Ok((row, role))
}

/// The tenant of `org`, if the user is a member and owns the
/// organization on GitHub. Not found otherwise, so the answer does not
/// say which organizations use hatchrun.
pub(crate) async fn authorize(ctx: &Ctx<'_>, org: &str) -> Result<TenantRow, Failure> {
    let row = listed(ctx, org).await?;
    if owned(ctx, &row.org_login).await?.is_none() {
        return Err(Failure::new(
            403,
            format!("only owners of {} can manage it on hatchrun", row.org_login),
        ));
    }
    Ok(row)
}

/// An installation as the start page lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallationView {
    id: u64,
    login: String,
    avatar_url: String,
    suspended: bool,
    /// The organization's login when the user is in its tenant.
    tenant: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Joined {
    org_id: u64,
    org_login: String,
}

/// The organizations of the tenants a user is a member of.
const JOINED: &str = "SELECT tenants.org_id, tenants.org_login FROM tenants \
    JOIN members ON members.tenant_id = tenants.id WHERE members.user_id = $1";

/// Renames an organization. Binds its id, the new login and the time in
/// milliseconds.
const RENAME: &str = "UPDATE tenants SET org_login = $2, updated_at = $3 WHERE org_id = $1";

/// `GET /api/installations`
pub(crate) async fn installations(ctx: &Ctx<'_>) -> Answer {
    let orgs: Vec<Installation> = github::parse_user_installations(
        &send(&github::user_installations(
            &ctx.config.github_api,
            &ctx.session.token,
        ))
        .await?,
    )?
    .into_iter()
    .filter(|installation| installation.account.kind == "Organization")
    .collect();

    let joined: Vec<Joined> = ctx.db.all(JOINED, &params![ctx.session.user.id]).await?;
    // Organizations get renamed; keep the login the app routes by
    // current.
    let now = now_ms();
    let renames: Vec<_> = orgs
        .iter()
        .filter(|installation| {
            joined.iter().any(|tenant| {
                tenant.org_id == installation.account.id
                    && tenant.org_login != installation.account.login
            })
        })
        .map(|installation| {
            (
                RENAME,
                params![
                    installation.account.id,
                    installation.account.login.as_str(),
                    now
                ],
            )
        })
        .collect();
    if !renames.is_empty() {
        ctx.db.batch(&renames).await?;
    }

    let list: Vec<InstallationView> = orgs
        .into_iter()
        .map(|installation| InstallationView {
            tenant: joined
                .iter()
                .any(|tenant| tenant.org_id == installation.account.id)
                .then(|| installation.account.login.clone()),
            id: installation.id,
            avatar_url: avatar(installation.account.id),
            login: installation.account.login,
            suspended: installation.suspended_at.is_some(),
        })
        .collect();
    Ok(Response::from_json(&serde_json::json!({
        "installUrl": install_url(ctx),
        "installations": list,
    }))?)
}

/// Creates the organization's tenant, or links the installation to the
/// one that exists. Binds a new tenant id, the organization's id and
/// login, the installation and the time in milliseconds.
const ADOPT_TENANT: &str = "INSERT INTO tenants \
    (id, org_id, org_login, installation_id, created_at, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $5) \
    ON CONFLICT (org_id) DO UPDATE SET org_login = $3, installation_id = $4, updated_at = $5";

/// Makes a user a member of the organization's tenant. Binds the
/// organization's id, the user id and the time in milliseconds.
const ADOPT_MEMBER: &str = "INSERT INTO members (tenant_id, user_id, created_at) \
    SELECT id, $2::bigint, $3::bigint FROM tenants WHERE org_id = $1 ON CONFLICT DO NOTHING";

/// Makes `installation_id` the installation of its organization's
/// tenant, creating the tenant if needed, and the user a member, once
/// [`install::check`] and [`install::check_owner`] accept it. Returns
/// the organization's login, or why not with the account's login.
async fn adopt(
    ctx: &Ctx<'_>,
    installation_id: u64,
) -> Result<Result<String, (Refusal, String)>, Failure> {
    let key = jwt::pkcs8_der(&super::need(ctx.env, APP_KEY)?)
        .map_err(|err| Failure::new(500, format!("{APP_KEY}: {err}")))?;
    let app = app_jwt(&ctx.config.client_id, &key).await?;
    let read = send(&github::installation(
        &ctx.config.github_api,
        &app,
        installation_id,
    ))
    .await?;
    if read.status == 404 {
        return Ok(Err((Refusal::Unseen, String::new())));
    }
    let installation = github::parse_installation(&read)?;
    let visible = github::parse_user_installations(
        &send(&github::user_installations(
            &ctx.config.github_api,
            &ctx.session.token,
        ))
        .await?,
    )?;
    let login = installation.account.login.clone();
    if let Err(refusal) = install::check(&installation, &visible) {
        // Name the account only to someone who can see it.
        let login = if refusal == Refusal::Unseen {
            String::new()
        } else {
            login
        };
        return Ok(Err((refusal, login)));
    }
    let membership = match membership(ctx, &login).await? {
        Ok(membership) => membership,
        Err(refusal) => return Ok(Err((refusal, login))),
    };
    if let Err(refusal) = install::check_owner(&installation, membership.as_ref()) {
        return Ok(Err((refusal, login)));
    }

    let org_id = installation.account.id;
    let now = now_ms();
    let id = tenant::new_id(&webcrypto::random_bytes(tenant::ID_LEN)?);
    ctx.db
        .batch(&[
            // One tenant per organization: a second owner joins the
            // first one's.
            (
                ADOPT_TENANT,
                params![id.as_str(), org_id, login.as_str(), installation.id, now],
            ),
            (ADOPT_MEMBER, params![org_id, ctx.session.user.id, now]),
        ])
        .await?;
    Ok(Ok(login))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateTenant {
    installation_id: u64,
}

/// `POST /api/tenants`
pub(crate) async fn create(ctx: &Ctx<'_>, body: CreateTenant) -> Answer {
    match adopt(ctx, body.installation_id).await? {
        Ok(login) => show(ctx, &login).await,
        Err((refusal, login)) => Err(Failure::new(
            if refusal == Refusal::Unseen { 404 } else { 403 },
            refusal.message(&login),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct Summary {
    id: String,
    org_login: String,
    org_id: u64,
}

/// The tenants a user is a member of, by login.
const MINE: &str = "SELECT tenants.id, tenants.org_login, tenants.org_id FROM tenants \
    JOIN members ON members.tenant_id = tenants.id \
    WHERE members.user_id = $1 ORDER BY tenants.org_login";

/// `GET /api/tenants`
pub(crate) async fn mine(ctx: &Ctx<'_>) -> Answer {
    let rows: Vec<Summary> = ctx.db.all(MINE, &params![ctx.session.user.id]).await?;
    let list: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "login": row.org_login,
                "avatarUrl": avatar(row.org_id),
            })
        })
        .collect();
    Ok(Response::from_json(&list)?)
}

/// `GET /api/tenants/<org>`
pub(crate) async fn show(ctx: &Ctx<'_>, org: &str) -> Answer {
    let (mut row, mut role) = authorize_member(ctx, org).await?;
    if !row.beta {
        join_listed_beta(ctx.db, &row.id).await?;
        (row, role) = authorize_member(ctx, org).await?;
    }
    Ok(Response::from_json(&view(ctx, row, role))?)
}

#[derive(Debug, Deserialize)]
struct SetupQuery {
    installation_id: Option<u64>,
    setup_action: Option<String>,
    state: Option<String>,
}

/// `GET /api/github/setup`: links the installation GitHub names, once
/// the state shows this session started the install and [`adopt`]
/// accepts it, and sends the browser on to the organization's setup.
pub(crate) async fn setup(ctx: &Ctx<'_>, req: &Request) -> Answer {
    let query: SetupQuery = req.query()?;
    let back = |path: &str| found(&format!("{}{path}", ctx.config.app_url), &[]);
    if query.setup_action.as_deref() == Some("request") {
        // A member asked an owner to install it; GitHub sends no
        // installation until an owner approves.
        return Ok(back("/?install=requested")?);
    }
    let Some(installation_id) = query.installation_id else {
        return Ok(back("/")?);
    };
    let Some(state) = query.state else {
        // No install of ours: a change on GitHub's settings page, or an
        // owner approving a member's request. The start page lists the
        // installation to carry on from.
        return Ok(back("/")?);
    };
    if !session::check_install_state(&ctx.session.id, &state) {
        return Ok(back("/?error=install-state")?);
    }
    // The browser came from GitHub, so it gets a page of the app, not
    // a JSON error.
    Ok(match adopt(ctx, installation_id).await {
        Ok(Ok(login)) => back(&format!("/o/{login}?installed=1"))?,
        Ok(Err((refusal, login))) => back(&format!(
            "/?error={}&account={}",
            refusal.code(),
            crate::http::encode(&login)
        ))?,
        Err(failure) => {
            console_error!("setup {installation_id}: {}", failure.message);
            back("/?error=install-failed")?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;

    fn user(db: &Db, id: u64) {
        db.run(
            "INSERT INTO users (id, login, avatar_url, created_at, updated_at) \
             VALUES ($1, $2, '', $3, $3)",
            &params![id, format!("user{id}"), NOW],
        );
    }

    /// The statements [`adopt`] batches.
    fn adopt_batch(
        id: &str,
        org_id: u64,
        login: &str,
        installation: u64,
        user: u64,
    ) -> Vec<(&'static str, Vec<Param>)> {
        vec![
            (ADOPT_TENANT, params![id, org_id, login, installation, NOW]),
            (ADOPT_MEMBER, params![org_id, user, NOW]),
        ]
    }

    fn authorized(db: &Db, org: &str, user: u64) -> Option<TenantRow> {
        db.first(&authorize_query(), &params![org, user])
    }

    #[test]
    fn a_second_owner_joins_the_first_ones_tenant() {
        let db = Db::new();
        user(&db, 5);
        user(&db, 6);
        db.batch(&adopt_batch("aaaaaaaaaaaaaaaa", 11, "Acme", 100, 5))
            .unwrap();
        db.batch(&adopt_batch("bbbbbbbbbbbbbbbb", 11, "Acme", 101, 6))
            .unwrap();
        let first = authorized(&db, "acme", 5).unwrap();
        let second = authorized(&db, "ACME", 6).unwrap();
        assert_eq!(first.id, "aaaaaaaaaaaaaaaa");
        assert_eq!(second.id, first.id);
        assert_eq!(second.installation_id, Some(101));
        assert!(authorized(&db, "acme", 7).is_none());
    }

    #[test]
    fn a_new_tenant_reads_into_its_row() {
        let db = Db::new();
        user(&db, 5);
        db.batch(&adopt_batch("aaaaaaaaaaaaaaaa", 11, "acme", 100, 5))
            .unwrap();
        let row = authorized(&db, "acme", 5).unwrap();
        assert_eq!(row.org_id, 11);
        assert_eq!(row.org_login, "acme");
        assert!(!row.beta);
    }

    #[test]
    fn lists_and_renames_a_users_tenants() {
        let db = Db::new();
        user(&db, 5);
        user(&db, 6);
        db.batch(&adopt_batch("aaaaaaaaaaaaaaaa", 11, "zeta", 100, 5))
            .unwrap();
        db.batch(&adopt_batch("bbbbbbbbbbbbbbbb", 12, "acme", 101, 5))
            .unwrap();
        db.batch(&adopt_batch("cccccccccccccccc", 13, "other", 102, 6))
            .unwrap();

        let mine: Vec<Summary> = db.all(MINE, &params![5u64]);
        let logins: Vec<&str> = mine.iter().map(|row| row.org_login.as_str()).collect();
        assert_eq!(logins, ["acme", "zeta"]);
        assert_eq!(mine[0].id, "bbbbbbbbbbbbbbbb");
        assert_eq!(mine[0].org_id, 12);

        let joined: Vec<Joined> = db.all(JOINED, &params![5u64]);
        assert_eq!(joined.len(), 2);

        db.batch(&[(RENAME, params![11u64, "zeta-labs", NOW])])
            .unwrap();
        let joined: Vec<Joined> = db.all(JOINED, &params![5u64]);
        assert!(
            joined
                .iter()
                .any(|row| row.org_id == 11 && row.org_login == "zeta-labs")
        );
    }
}
