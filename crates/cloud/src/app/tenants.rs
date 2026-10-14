// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

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
//!   `GET /api/tenants/<org>` shows one, with what is left to set up.
//!
//! The setup URL and `POST /api/tenants` create the organization's
//! tenant, or add the user to the one that exists, once [`install`]
//! accepts the installation.
//!
//! Membership in D1 says which tenants a user sees. Acting on one also
//! needs the user to own the organization on GitHub at that moment, so
//! someone removed as an owner loses access at once.

use serde::{Deserialize, Serialize};
use worker::{Request, Response};

use super::billing::{Billing, billing, join_listed_beta};
use super::{Answer, Ctx, Failure, found};
use crate::fetch::send;
use crate::github::{self, Installation, Membership};
use crate::install::{self, Refusal};
use crate::launch::{APP_KEY, app_jwt};
use crate::{int, jwt, now_ms, session, settings, tenant, webcrypto};

/// A tenant row.
#[derive(Debug, Deserialize)]
pub(crate) struct TenantRow {
    pub id: String,
    pub org_id: u64,
    pub org_login: String,
    pub installation_id: Option<u64>,
    pub role_arn: Option<String>,
    pub region: Option<String>,
    pub launch_template_id: Option<String>,
    pub subnet_ids: Option<String>,
    pub max_runners: u32,
    pub beta: u8,
    pub allowed_regions: Option<String>,
    pub instance_types: Option<String>,
    pub volume_size_limit: Option<u16>,
    pub root_volume_size: Option<u16>,
}

pub(crate) const TENANT_COLUMNS: &str = "tenants.id, tenants.org_id, tenants.org_login, \
    tenants.installation_id, tenants.role_arn, tenants.region, tenants.launch_template_id, \
    tenants.subnet_ids, tenants.max_runners, tenants.beta, tenants.allowed_regions, \
    tenants.instance_types, tenants.volume_size_limit, tenants.root_volume_size";

/// The setup steps a tenant has done.
#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)] // One per step, as the app shows them.
#[serde(rename_all = "camelCase")]
struct Steps {
    app: bool,
    billing: bool,
    aws: bool,
}

/// A tenant as the app shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TenantView {
    id: String,
    org: OrgView,
    installation_id: Option<u64>,
    aws: Option<AwsView>,
    max_runners: u32,
    /// `None` for every region.
    allowed_regions: Option<Vec<String>>,
    billing: Billing,
    steps: Steps,
    /// Installs the App again after it was removed.
    install_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrgView {
    id: u64,
    login: String,
    avatar_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AwsView {
    role_arn: String,
    region: String,
    launch_template_id: String,
    subnet_ids: Vec<String>,
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

async fn view(ctx: &Ctx<'_>, row: TenantRow) -> worker::Result<TenantView> {
    let billing = billing(ctx.db, &row.id, row.beta == 1).await?;
    let aws = match (
        row.role_arn,
        row.region,
        row.launch_template_id,
        row.subnet_ids,
    ) {
        (Some(role_arn), Some(region), Some(launch_template_id), Some(subnets)) => Some(AwsView {
            role_arn,
            region,
            launch_template_id,
            subnet_ids: subnets
                .split(',')
                .map(str::trim)
                .filter(|subnet| !subnet.is_empty())
                .map(str::to_owned)
                .collect(),
        }),
        _ => None,
    };
    Ok(TenantView {
        install_url: install_url(ctx),
        steps: Steps {
            app: row.installation_id.is_some(),
            billing: billing.licensed,
            aws: aws.is_some(),
        },
        id: row.id,
        org: OrgView {
            id: row.org_id,
            avatar_url: avatar(row.org_id),
            login: row.org_login,
        },
        installation_id: row.installation_id,
        aws,
        max_runners: row.max_runners,
        allowed_regions: settings::regions(row.allowed_regions.as_deref()),
        billing,
    })
}

/// The user's membership of `org` on GitHub, if they own it.
async fn owned(ctx: &Ctx<'_>, org: &str) -> Result<Option<Membership>, Failure> {
    let membership = github::parse_membership(
        &send(&github::membership(
            &ctx.config.github_api,
            &ctx.session.token,
            org,
        ))
        .await?,
    )?;
    Ok(membership.filter(Membership::is_admin))
}

/// The tenant of `org`, if the user is a member and owns the
/// organization on GitHub. Not found otherwise, so the answer does not
/// say which organizations use hatchrun.
pub(crate) async fn authorize(ctx: &Ctx<'_>, org: &str) -> Result<TenantRow, Failure> {
    let missing = || Failure::new(404, format!("no organization {org} here"));
    if !tenant::valid_login(org) {
        return Err(missing());
    }
    let row: Option<TenantRow> = ctx
        .db
        .prepare(format!(
            "SELECT {TENANT_COLUMNS} FROM tenants \
             JOIN members ON members.tenant_id = tenants.id \
             WHERE tenants.org_login = ?1 COLLATE NOCASE AND members.user_id = ?2"
        ))
        .bind(&[org.into(), int(ctx.session.user.id)?])?
        .first(None)
        .await?;
    let row = row.ok_or_else(missing)?;
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

    let joined = ctx
        .db
        .prepare(
            "SELECT tenants.org_id, tenants.org_login FROM tenants \
             JOIN members ON members.tenant_id = tenants.id WHERE members.user_id = ?1",
        )
        .bind(&[int(ctx.session.user.id)?])?
        .all()
        .await?
        .results::<Joined>()?;
    // Organizations get renamed; keep the login the app routes by
    // current.
    let now = int(now_ms())?;
    let renames = orgs
        .iter()
        .filter(|installation| {
            joined.iter().any(|tenant| {
                tenant.org_id == installation.account.id
                    && tenant.org_login != installation.account.login
            })
        })
        .map(|installation| {
            ctx.db
                .prepare("UPDATE tenants SET org_login = ?2, updated_at = ?3 WHERE org_id = ?1")
                .bind(&[
                    int(installation.account.id)?,
                    installation.account.login.as_str().into(),
                    now.clone(),
                ])
        })
        .collect::<worker::Result<Vec<_>>>()?;
    if !renames.is_empty() {
        ctx.db.batch(renames).await?;
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
    let membership = github::parse_membership(
        &send(&github::membership(
            &ctx.config.github_api,
            &ctx.session.token,
            &login,
        ))
        .await?,
    )?;
    if let Err(refusal) = install::check_owner(&installation, membership.as_ref()) {
        return Ok(Err((refusal, login)));
    }

    let org_id = int(installation.account.id)?;
    let now = int(now_ms())?;
    let id = tenant::new_id(&webcrypto::random_bytes(tenant::ID_LEN)?);
    ctx.db
        .batch(vec![
            // One tenant per organization: a second owner joins the
            // first one's.
            ctx.db
                .prepare(
                    "INSERT INTO tenants \
                     (id, org_id, org_login, installation_id, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                     ON CONFLICT (org_id) DO UPDATE \
                     SET org_login = ?3, installation_id = ?4, updated_at = ?5",
                )
                .bind(&[
                    id.as_str().into(),
                    org_id.clone(),
                    login.as_str().into(),
                    int(installation.id)?,
                    now.clone(),
                ])?,
            ctx.db
                .prepare(
                    "INSERT INTO members (tenant_id, user_id, created_at) \
                     SELECT id, ?2, ?3 FROM tenants WHERE org_id = ?1 \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&[org_id, int(ctx.session.user.id)?, now])?,
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

/// `GET /api/tenants`
pub(crate) async fn mine(ctx: &Ctx<'_>) -> Answer {
    let rows = ctx
        .db
        .prepare(
            "SELECT tenants.id, tenants.org_login, tenants.org_id FROM tenants \
             JOIN members ON members.tenant_id = tenants.id \
             WHERE members.user_id = ?1 ORDER BY tenants.org_login",
        )
        .bind(&[int(ctx.session.user.id)?])?
        .all()
        .await?
        .results::<Summary>()?;
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
    let mut row = authorize(ctx, org).await?;
    if row.beta == 0 {
        join_listed_beta(ctx.db, &row.id).await?;
        row = authorize(ctx, org).await?;
    }
    Ok(Response::from_json(&view(ctx, row).await?)?)
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
    Ok(match adopt(ctx, installation_id).await? {
        Ok(login) => back(&format!("/o/{login}?installed=1"))?,
        Err((refusal, login)) => back(&format!(
            "/?error={}&account={}",
            refusal.code(),
            crate::http::encode(&login)
        ))?,
    })
}
