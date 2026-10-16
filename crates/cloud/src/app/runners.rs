// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

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
//! - `GET /api/tenants/<org>/runners/instance-types` lists the instance
//!   type catalog of the tenant's region (see [`crate::instance_types`]),
//!   with when it was refreshed.
//!
//! Changes answer with the new listing. A profile the connect stack
//! cannot launch is refused once AWS is connected; before that only the
//! profile itself is checked.

use serde::Serialize;
use worker::{Method, Request, Response};

use super::tenants::{TenantRow, authorize};
use super::{Answer, Ctx, Failure};
use crate::instance::{self, Limits};
use crate::instance_types::Entry;
use crate::launch;
use crate::profiles::{self, IMAGES, PROFILES_MAX, Profile, Source, built_in, store};

/// Where prices come from before AWS is connected.
const PRICING_REGION: &str = "us-east-1";

/// What a tenant's profiles are checked against.
struct Target {
    region: String,
    /// The connect stack's limits, once AWS is connected.
    limits: Option<Limits>,
    /// What the region offers.
    offered: Vec<Entry>,
}

impl Target {
    async fn load(ctx: &Ctx<'_>, row: &TenantRow) -> worker::Result<Self> {
        let (region, limits) = match &row.region {
            Some(region) if row.role_arn.is_some() => (
                region.clone(),
                Some(Limits::from_columns(
                    row.instance_types.as_deref(),
                    row.excluded_instance_types.as_deref(),
                    row.volume_size_limit,
                    row.root_volume_size,
                )),
            ),
            _ => (PRICING_REGION.to_owned(), None),
        };
        let offered = crate::instance_types::store::offered(ctx.db, &region).await?;
        Ok(Self {
            region,
            limits,
            offered,
        })
    }

    fn choose(&self, profile: &Profile, limits: &Limits) -> Result<instance::Choice, String> {
        instance::choose(profile, &self.region, &self.offered, limits)
    }

    /// The limits to choose within: the stack's, or none before there
    /// is a stack.
    fn limits(&self) -> Limits {
        self.limits.clone().unwrap_or_else(|| Limits {
            instance_types: vec!["*".to_owned()],
            excluded: Vec::new(),
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
    match target.choose(&profile, &target.limits()) {
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
    name: String,
    /// As in `m`.
    category: String,
    /// As in 8.
    generation: u16,
    arch: crate::labels::Arch,
    /// Memory per vCPU of its smallest size, in GiB.
    gib_per_vcpu: u32,
    /// Size names with their vCPUs, smallest first.
    sizes: Vec<(String, u16)>,
    /// Whether the region offers it; the list holds only those.
    offered: bool,
    /// Whether the connect stack allows at least one size.
    allowed: bool,
    /// GPUs, accelerators, bare metal, burstable or flex: only a pin
    /// launches it.
    special: bool,
    /// On-demand per vCPU-hour of its smallest size, millionths of a US
    /// dollar.
    price_per_vcpu: Option<u64>,
}

/// The region's families, by name, from what it offers.
fn family_views(target: &Target) -> Vec<FamilyView> {
    let limits = target.limits();
    let mut entries: Vec<&Entry> = target.offered.iter().collect();
    entries.sort_by(|a, b| (a.family.as_str(), a.vcpus).cmp(&(b.family.as_str(), b.vcpus)));
    let mut views: Vec<FamilyView> = Vec::new();
    for entry in entries {
        let size = entry
            .name
            .split_once('.')
            .map_or(String::new(), |(_, size)| size.to_owned());
        let allowed = limits.allows(&entry.name);
        if let Some(view) = views.last_mut().filter(|view| view.name == entry.family) {
            view.sizes.push((size, entry.vcpus));
            view.allowed |= allowed;
            continue;
        }
        views.push(FamilyView {
            name: entry.family.clone(),
            category: entry.category.clone(),
            generation: entry.generation,
            arch: entry.arch,
            gib_per_vcpu: entry.memory_mib / 1024 / u32::from(entry.vcpus.max(1)),
            sizes: vec![(size, entry.vcpus)],
            offered: true,
            allowed,
            special: entry.gpus > 0
                || entry.accelerators
                || entry.bare_metal
                || entry.burstable
                || entry.family.ends_with("-flex"),
            price_per_vcpu: entry
                .on_demand_micros
                .map(|price| price / u64::from(entry.vcpus.max(1))),
        });
    }
    views
}

async fn listing(ctx: &Ctx<'_>, row: &TenantRow) -> Answer {
    let target = Target::load(ctx, row).await?;
    let profiles: Vec<ProfileView> = profiles::merged(store::custom(ctx.db, &row.id).await?)
        .into_iter()
        .map(|(profile, source)| check(profile, source, &target))
        .collect();
    let families = family_views(&target);
    Ok(Response::from_json(&serde_json::json!({
        "region": target.region,
        "connected": target.limits.is_some(),
        "limits": target.limits.as_ref().map(|limits| serde_json::json!({
            "instanceTypes": limits.instance_types,
            "excludedInstanceTypes": limits.excluded,
            "volumeSizeLimit": limits.volume_size_limit,
            "rootVolumeSize": limits.root_volume_size,
        })),
        "images": IMAGES,
        "families": families,
        "profiles": profiles,
        "maxProfiles": PROFILES_MAX,
    }))?)
}

/// `GET /api/tenants/<org>/runners/instance-types`
async fn instance_types(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let Some(region) = row.region.as_deref() else {
        return Ok(Response::from_json(&serde_json::json!({
            "region": null, "refreshedAt": null, "error": null, "types": [],
        }))?);
    };
    let (refreshed_at, error) = crate::instance_types::store::refreshed(ctx.db, region).await?;
    let types = crate::instance_types::store::region(ctx.db, region).await?;
    Ok(Response::from_json(&serde_json::json!({
        "region": region,
        "refreshedAt": refreshed_at,
        "error": error,
        "types": types,
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
        (Method::Get, ["instance-types"]) => instance_types(ctx, org).await,
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
        target.choose(&profile, limits).map_err(refused)?;
    }
    Ok(profile)
}

/// `POST /api/tenants/<org>/runners/preview`
async fn preview(ctx: &Ctx<'_>, org: &str, profile: Profile) -> Answer {
    let row = authorize(ctx, org).await?;
    let target = Target::load(ctx, &row).await?;
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
    let profile = checked(profile, &Target::load(ctx, &row).await?)?;
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
    let profile = checked(profile, &Target::load(ctx, &row).await?)?;
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
