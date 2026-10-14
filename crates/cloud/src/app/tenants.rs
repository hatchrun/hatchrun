// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Setting up an organization and linking the App's installation.
//!
//! - `GET /api/orgs` lists the organizations the user owns on GitHub and
//!   whether each already has a tenant.
//! - `POST /api/tenants` with `{"org": login}` creates the tenant for an
//!   organization the user owns, or adds the user to the one that
//!   exists, and makes them a member.
//! - `GET /api/tenants` lists the user's tenants;
//!   `GET /api/tenants/<org>` shows one, with what is left to set up.
//! - `GET /api/github/setup` is the App's setup URL: GitHub sends the
//!   browser there after an install, with the installation id and the
//!   state the install link carried.
//!
//! Membership in D1 says which tenants a user sees. Acting on one also
//! needs the user to own the organization on GitHub at that moment, so
//! someone removed as an owner loses access at once.

use serde::{Deserialize, Serialize};
use worker::{Request, Response};

use super::billing::{Billing, billing, join_listed_beta};
use super::{Answer, Ctx, Failure, found};
use crate::fetch::send;
use crate::github::{self, Membership};
use crate::launch::{APP_KEY, app_jwt};
use crate::{int, jwt, now_ms, session, tenant, webcrypto};

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
}

pub(crate) const TENANT_COLUMNS: &str = "tenants.id, tenants.org_id, tenants.org_login, \
    tenants.installation_id, tenants.role_arn, tenants.region, tenants.launch_template_id, \
    tenants.subnet_ids, tenants.max_runners, tenants.beta";

/// The setup steps a tenant has done.
#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)] // One per step, as the app shows them.
#[serde(rename_all = "camelCase")]
struct Steps {
    organization: bool,
    billing: bool,
    aws: bool,
    app: bool,
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
    billing: Billing,
    steps: Steps,
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
        install_url: github::install_url(
            &ctx.config.github_web,
            &ctx.config.app_slug,
            &session::install_state(&ctx.session.id, &row.id),
        ),
        steps: Steps {
            organization: true,
            billing: billing.licensed,
            aws: aws.is_some(),
            app: row.installation_id.is_some(),
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrgChoice {
    id: u64,
    login: String,
    avatar_url: String,
    /// The tenant's id when the organization has one.
    tenant: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Existing {
    id: String,
    org_id: u64,
}

/// `GET /api/orgs`
pub(crate) async fn orgs(ctx: &Ctx<'_>) -> Answer {
    let owned: Vec<Membership> = github::parse_memberships(
        &send(&github::memberships(
            &ctx.config.github_api,
            &ctx.session.token,
        ))
        .await?,
    )?
    .into_iter()
    .filter(Membership::is_admin)
    .collect();

    let mut existing = Vec::new();
    if !owned.is_empty() {
        let placeholders = (1..=owned.len())
            .map(|at| format!("?{at}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ids = owned
            .iter()
            .map(|membership| int(membership.organization.id))
            .collect::<worker::Result<Vec<_>>>()?;
        existing = ctx
            .db
            .prepare(format!(
                "SELECT id, org_id FROM tenants WHERE org_id IN ({placeholders})"
            ))
            .bind(&ids)?
            .all()
            .await?
            .results::<Existing>()?;
        // Organizations get renamed; keep the login the app routes by
        // current.
        let now = int(now_ms())?;
        let renames = owned
            .iter()
            .filter(|membership| {
                existing
                    .iter()
                    .any(|tenant| tenant.org_id == membership.organization.id)
            })
            .map(|membership| {
                ctx.db
                    .prepare(
                        "UPDATE tenants SET org_login = ?2, updated_at = ?3 \
                         WHERE org_id = ?1 AND org_login <> ?2",
                    )
                    .bind(&[
                        int(membership.organization.id)?,
                        membership.organization.login.as_str().into(),
                        now.clone(),
                    ])
            })
            .collect::<worker::Result<Vec<_>>>()?;
        if !renames.is_empty() {
            ctx.db.batch(renames).await?;
        }
    }

    let choices: Vec<OrgChoice> = owned
        .into_iter()
        .map(|membership| {
            let org = membership.organization;
            OrgChoice {
                tenant: existing
                    .iter()
                    .find(|tenant| tenant.org_id == org.id)
                    .map(|tenant| tenant.id.clone()),
                id: org.id,
                login: org.login,
                avatar_url: org.avatar_url,
            }
        })
        .collect();
    Ok(Response::from_json(&choices)?)
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTenant {
    org: String,
}

/// `POST /api/tenants`
pub(crate) async fn create(ctx: &Ctx<'_>, body: CreateTenant) -> Answer {
    if !tenant::valid_login(&body.org) {
        return Err(Failure::new(400, "not an organization login"));
    }
    let Some(membership) = owned(ctx, &body.org).await? else {
        return Err(Failure::new(
            403,
            format!("you are not an owner of {} on GitHub", body.org),
        ));
    };
    let org = membership.organization;
    let now = int(now_ms())?;
    let id = tenant::new_id(&webcrypto::random_bytes(tenant::ID_LEN)?);
    ctx.db
        .batch(vec![
            // One tenant per organization: a second owner joins the
            // first one's.
            ctx.db
                .prepare(
                    "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?4) \
                     ON CONFLICT (org_id) DO UPDATE SET org_login = ?3, updated_at = ?4",
                )
                .bind(&[
                    id.as_str().into(),
                    int(org.id)?,
                    org.login.as_str().into(),
                    now.clone(),
                ])?,
            ctx.db
                .prepare(
                    "INSERT INTO members (tenant_id, user_id, created_at) \
                     SELECT id, ?2, ?3 FROM tenants WHERE org_id = ?1 \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&[int(org.id)?, int(ctx.session.user.id)?, now])?,
        ])
        .await?;
    show(ctx, &org.login).await
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
struct Setup {
    installation_id: Option<u64>,
    state: Option<String>,
}

/// `GET /api/github/setup`: records the installation once it is
/// certain that it belongs to the tenant's organization and that the
/// user owns that organization.
pub(crate) async fn setup(ctx: &Ctx<'_>, req: &Request) -> Answer {
    let query: Setup = req.query()?;
    let back = |path: &str| found(&format!("{}{path}", ctx.config.app_url), &[]);
    let (Some(installation_id), Some(state)) = (query.installation_id, query.state) else {
        // An update to an existing installation, or an install started
        // on GitHub rather than here: nothing to link.
        return Ok(back("/")?);
    };
    let Some(tenant_id) = session::check_install_state(&ctx.session.id, &state) else {
        return Ok(back("/?error=install-state")?);
    };
    let login: Option<String> = ctx
        .db
        .prepare(
            "SELECT tenants.org_login FROM tenants \
             JOIN members ON members.tenant_id = tenants.id \
             WHERE tenants.id = ?1 AND members.user_id = ?2",
        )
        .bind(&[tenant_id.into(), int(ctx.session.user.id)?])?
        .first(Some("org_login"))
        .await?;
    let Some(login) = login else {
        return Ok(back("/?error=install-state")?);
    };
    let row = authorize(ctx, &login).await?;
    let page = format!("/o/{}", row.org_login);

    let key = jwt::pkcs8_der(&super::need(ctx.env, APP_KEY)?)
        .map_err(|err| Failure::new(500, format!("{APP_KEY}: {err}")))?;
    let app = app_jwt(&ctx.config.client_id, &key).await?;
    let installation = github::parse_installation(
        &send(&github::installation(
            &ctx.config.github_api,
            &app,
            installation_id,
        ))
        .await?,
    )?;
    if installation.account.id != row.org_id || installation.account.kind != "Organization" {
        return Ok(back(&format!("{page}?error=install-account"))?);
    }
    ctx.db
        .prepare("UPDATE tenants SET installation_id = ?2, updated_at = ?3 WHERE id = ?1")
        .bind(&[
            row.id.as_str().into(),
            int(installation.id)?,
            int(now_ms())?,
        ])?
        .run()
        .await?;
    Ok(back(&format!("{page}?installed=1"))?)
}
