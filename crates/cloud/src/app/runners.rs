// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! An organization's runner profiles (see [`crate::profiles`]).
//!
//! - `GET /api/tenants/<org>/runners` lists the profiles workflows can
//!   name, with the instance type each gets in the tenant's region and
//!   its hourly price, and the families and limits a profile can use.
//! - `POST /api/tenants/<org>/runners/preview` with a profile says what
//!   it would get, or why it cannot be launched, without saving it.
//! - `POST /api/tenants/<org>/runners` with a profile adds it.
//! - `PUT /api/tenants/<org>/runners/<name>` with a profile replaces
//!   that profile, renaming it when the names differ. On a built-in it
//!   saves the organization's own version.
//! - `DELETE /api/tenants/<org>/runners/<name>` removes the
//!   organization's profile; a built-in it replaced comes back.
//!
//! Changes answer with the new listing. A profile the connect stack
//! cannot launch is refused once AWS is connected; before that only the
//! profile itself is checked.

use serde::Serialize;
use worker::{Method, Request, Response};

use super::tenants::{TenantRow, authorize};
use super::{Answer, Ctx, Failure};
use crate::catalog::{self, Family};
use crate::instance::{self, Limits, size_name};
use crate::launch;
use crate::profiles::{self, IMAGES, PROFILES_MAX, Profile, Source, built_in, store};

/// Where prices come from before AWS is connected.
const PRICING_REGION: &str = "us-east-1";

/// What a tenant's profiles are checked against.
struct Target {
    region: String,
    /// The connect stack's limits, once AWS is connected.
    limits: Option<Limits>,
}

impl Target {
    fn of(row: &TenantRow) -> Self {
        match &row.region {
            Some(region) if row.role_arn.is_some() => Self {
                region: region.clone(),
                limits: Some(Limits::from_columns(
                    row.instance_types.as_deref(),
                    row.volume_size_limit,
                    row.root_volume_size,
                )),
            },
            _ => Self {
                region: PRICING_REGION.to_owned(),
                limits: None,
            },
        }
    }

    /// The limits to choose within: the stack's, or none before there
    /// is a stack.
    fn limits(&self) -> Limits {
        self.limits.clone().unwrap_or_else(|| Limits {
            instance_types: vec!["*".to_owned()],
            volume_size_limit: crate::labels::DISK_MAX,
            root_volume_size: instance::LEGACY_ROOT_VOLUME_SIZE,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Estimate {
    instance_types: Vec<String>,
    vcpus: u16,
    memory_gib: u32,
    disk_gib: u16,
    /// On-demand, in millionths of a US dollar.
    hourly_micros: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileView {
    #[serde(flatten)]
    profile: Profile,
    source: Source,
    estimate: Option<Estimate>,
    /// Why it cannot be launched.
    problem: Option<String>,
}

fn check(profile: Profile, source: Source, target: &Target) -> ProfileView {
    match instance::choose(&profile, &target.region, &target.limits()) {
        Ok(choice) => ProfileView {
            profile,
            source,
            estimate: Some(Estimate {
                instance_types: choice.instance_types,
                vcpus: choice.vcpus,
                memory_gib: choice.memory_gib,
                disk_gib: choice.disk_gib,
                hourly_micros: choice.hourly_micros,
            }),
            problem: None,
        },
        Err(problem) => ProfileView {
            profile,
            source,
            estimate: None,
            problem: Some(problem),
        },
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FamilyView {
    name: &'static str,
    arch: crate::labels::Arch,
    gib_per_vcpu: u16,
    /// Size names with their vCPUs, smallest first.
    sizes: Vec<(String, u16)>,
    /// Whether the region offers it.
    offered: bool,
    /// Whether the connect stack allows at least one size.
    allowed: bool,
    /// On-demand per vCPU-hour in the region, millionths of a US dollar.
    price_per_vcpu: Option<u32>,
}

fn family_view(family: &'static Family, target: &Target) -> FamilyView {
    let limits = target.limits();
    let price = family
        .prices
        .iter()
        .find(|(region, _)| *region == target.region)
        .map(|(_, price)| *price);
    FamilyView {
        name: family.name,
        arch: family.arch,
        gib_per_vcpu: family.gib_per_vcpu,
        sizes: family
            .sizes
            .iter()
            .map(|vcpus| (size_name(*vcpus), *vcpus))
            .collect(),
        offered: price.is_some(),
        allowed: family
            .sizes
            .iter()
            .any(|vcpus| limits.allows(&format!("{}.{}", family.name, size_name(*vcpus)))),
        price_per_vcpu: price,
    }
}

async fn listing(ctx: &Ctx<'_>, row: &TenantRow) -> Answer {
    let target = Target::of(row);
    let profiles: Vec<ProfileView> = profiles::merged(store::custom(ctx.db, &row.id).await?)
        .into_iter()
        .map(|(profile, source)| check(profile, source, &target))
        .collect();
    let families: Vec<FamilyView> = catalog::FAMILIES
        .iter()
        .map(|family| family_view(family, &target))
        .collect();
    Ok(Response::from_json(&serde_json::json!({
        "region": target.region,
        "connected": target.limits.is_some(),
        "limits": target.limits.as_ref().map(|limits| serde_json::json!({
            "instanceTypes": limits.instance_types,
            "volumeSizeLimit": limits.volume_size_limit,
            "rootVolumeSize": limits.root_volume_size,
        })),
        "images": IMAGES,
        "families": families,
        "profiles": profiles,
        "maxProfiles": PROFILES_MAX,
    }))?)
}

/// Answers `/api/tenants/<org>/runners` and what is under it; `rest`
/// is the path after `runners`.
pub(crate) async fn route(
    ctx: &Ctx<'_>,
    org: &str,
    method: Method,
    rest: &[&str],
    req: &mut Request,
) -> Answer {
    match (method, rest) {
        (Method::Get, []) => list(ctx, org).await,
        (Method::Post, []) => create(ctx, org, req.json().await?).await,
        (Method::Post, ["preview"]) => preview(ctx, org, req.json().await?).await,
        (Method::Put, [name]) => replace(ctx, org, name, req.json().await?).await,
        (Method::Delete, [name]) => delete(ctx, org, name).await,
        _ => Err(Failure::new(404, "not found")),
    }
}

/// `GET /api/tenants/<org>/runners`
async fn list(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    listing(ctx, &row).await
}

fn refused(message: impl Into<String>) -> Failure {
    Failure::new(422, message)
}

/// The profile checked on its own and, once AWS is connected, against
/// the connect stack.
fn checked(profile: Profile, target: &Target) -> Result<Profile, Failure> {
    let profile = profile.validate().map_err(refused)?;
    if let Some(limits) = &target.limits {
        instance::choose(&profile, &target.region, limits).map_err(refused)?;
    }
    Ok(profile)
}

/// `POST /api/tenants/<org>/runners/preview`
async fn preview(ctx: &Ctx<'_>, org: &str, profile: Profile) -> Answer {
    let row = authorize(ctx, org).await?;
    let target = Target::of(&row);
    let view = match profile.validate() {
        Ok(profile) => check(profile, Source::Custom, &target),
        Err(problem) => {
            return Ok(Response::from_json(
                &serde_json::json!({ "problem": problem }),
            )?);
        }
    };
    Ok(Response::from_json(&view)?)
}

/// `POST /api/tenants/<org>/runners`
async fn create(ctx: &Ctx<'_>, org: &str, profile: Profile) -> Answer {
    let row = authorize(ctx, org).await?;
    let profile = checked(profile, &Target::of(&row))?;
    if built_in(&profile.name).is_some() {
        return Err(Failure::new(
            409,
            format!(
                "{} is built in. Edit it to change it for {}.",
                profile.name, row.org_login
            ),
        ));
    }
    if store::named(ctx.db, &row.id, &profile.name)
        .await?
        .is_some()
    {
        return Err(Failure::new(
            409,
            format!("{} exists already", profile.name),
        ));
    }
    if store::count(ctx.db, &row.id).await? >= PROFILES_MAX {
        return Err(refused(format!("At most {PROFILES_MAX} profiles")));
    }
    store::save(ctx.db, &row.id, &profile).await?;
    launch::recheck(ctx.db, &row.id).await?;
    listing(ctx, &row).await
}

/// `PUT /api/tenants/<org>/runners/<name>`
async fn replace(ctx: &Ctx<'_>, org: &str, name: &str, profile: Profile) -> Answer {
    let row = authorize(ctx, org).await?;
    let profile = checked(profile, &Target::of(&row))?;
    let existing = store::named(ctx.db, &row.id, name).await?;
    if existing.is_none() && built_in(name).is_none() {
        return Err(Failure::new(404, format!("no runner profile {name}")));
    }
    if profile.name != name {
        if built_in(name).is_some() {
            return Err(refused(format!(
                "Built-in profiles keep their name, {name}"
            )));
        }
        if built_in(&profile.name).is_some()
            || store::named(ctx.db, &row.id, &profile.name)
                .await?
                .is_some()
        {
            return Err(Failure::new(
                409,
                format!("{} exists already", profile.name),
            ));
        }
    }
    store::save(ctx.db, &row.id, &profile).await?;
    if profile.name != name {
        store::delete(ctx.db, &row.id, name).await?;
    }
    launch::recheck(ctx.db, &row.id).await?;
    listing(ctx, &row).await
}

/// `DELETE /api/tenants/<org>/runners/<name>`
async fn delete(ctx: &Ctx<'_>, org: &str, name: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    if !store::delete(ctx.db, &row.id, name).await? {
        return Err(Failure::new(
            404,
            if built_in(name).is_some() {
                format!("{name} is built in and has no changes to undo")
            } else {
                format!("no runner profile {name}")
            },
        ));
    }
    launch::recheck(ctx.db, &row.id).await?;
    listing(ctx, &row).await
}
