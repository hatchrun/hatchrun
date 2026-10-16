// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Custom profiles in the `runner_profiles` table.

use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, Result};

use super::{Capacity, Profile, built_in};
use crate::labels::{Arch, Spec};
use crate::{int, now_ms};

const COLUMNS: &str = "runner_profiles.name, runner_profiles.arch, runner_profiles.families, \
    runner_profiles.cpu_min, runner_profiles.cpu_max, runner_profiles.memory_min, \
    runner_profiles.memory_max, runner_profiles.disk_gib, runner_profiles.capacity, \
    runner_profiles.image, runner_profiles.extras, runner_profiles.warm_pool, \
    runner_profiles.categories, runner_profiles.min_generation";

/// A `runner_profiles` row.
#[derive(Debug, Deserialize)]
pub(crate) struct Row {
    name: String,
    arch: String,
    families: String,
    cpu_min: u16,
    cpu_max: Option<u16>,
    memory_min: u16,
    memory_max: Option<u16>,
    disk_gib: Option<u16>,
    capacity: String,
    image: String,
    extras: String,
    warm_pool: u16,
    categories: String,
    min_generation: Option<u16>,
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

impl From<Row> for Profile {
    fn from(row: Row) -> Self {
        Self {
            name: row.name,
            arch: if row.arch == "arm64" {
                Arch::Arm64
            } else {
                Arch::Amd64
            },
            categories: list(&row.categories),
            min_generation: row.min_generation,
            families: list(&row.families),
            cpu_min: row.cpu_min,
            cpu_max: row.cpu_max,
            memory_min: row.memory_min,
            memory_max: row.memory_max,
            disk_gib: row.disk_gib,
            capacity: Capacity::parse(&row.capacity),
            image: row.image,
            extras: list(&row.extras),
            warm_pool: row.warm_pool,
        }
    }
}

/// The tenant's own profiles, by name.
fn custom_query() -> String {
    format!("SELECT {COLUMNS} FROM runner_profiles WHERE tenant_id = ?1 ORDER BY name")
}

/// The tenant's own profile with a name.
fn named_query() -> String {
    format!("SELECT {COLUMNS} FROM runner_profiles WHERE tenant_id = ?1 AND name = ?2")
}

/// The installation's tenant, with the profile's name when it has one.
const UNKNOWN: &str = "SELECT tenants.id, runner_profiles.name FROM tenants \
     LEFT JOIN runner_profiles ON runner_profiles.tenant_id = tenants.id \
     AND runner_profiles.name = ?2 WHERE tenants.installation_id = ?1";

/// Inserts or replaces a profile.
const SAVE: &str = "INSERT INTO runner_profiles (tenant_id, name, arch, families, cpu_min, \
     cpu_max, memory_min, memory_max, disk_gib, spot, image, extras, warm_pool, created_at, \
     updated_at, categories, min_generation, capacity) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15, ?16, ?17) \
     ON CONFLICT (tenant_id, name) DO UPDATE SET arch = ?3, families = ?4, cpu_min = ?5, \
     cpu_max = ?6, memory_min = ?7, memory_max = ?8, disk_gib = ?9, spot = ?10, \
     image = ?11, extras = ?12, warm_pool = ?13, updated_at = ?14, categories = ?15, \
     min_generation = ?16, capacity = ?17";

/// How many profiles a tenant has.
const COUNT: &str = "SELECT COUNT(*) AS count FROM runner_profiles WHERE tenant_id = ?1";

/// Removes a tenant's profile.
const DELETE: &str = "DELETE FROM runner_profiles WHERE tenant_id = ?1 AND name = ?2";

/// A row of [`COUNT`].
#[derive(Deserialize)]
struct Count {
    count: usize,
}

/// The tenant's own profiles, by name.
pub(crate) async fn custom(db: &D1Database, tenant_id: &str) -> Result<Vec<Profile>> {
    Ok(db
        .prepare(custom_query())
        .bind(&[tenant_id.into()])?
        .all()
        .await?
        .results::<Row>()?
        .into_iter()
        .map(Profile::from)
        .collect())
}

/// The tenant's own profile called `name`.
pub(crate) async fn named(db: &D1Database, tenant_id: &str, name: &str) -> Result<Option<Profile>> {
    Ok(db
        .prepare(named_query())
        .bind(&[tenant_id.into(), name.into()])?
        .first::<Row>(None)
        .await?
        .map(Profile::from))
}

/// Why a job's label names no profile of the tenant behind
/// `installation_id`, or `None` when it names one. Jobs of an
/// installation without a tenant are left to the launch, which says so
/// on the job.
pub(crate) async fn unknown(
    db: &D1Database,
    installation_id: u64,
    spec: &Spec,
) -> Result<Option<String>> {
    if built_in(&spec.profile).is_some() {
        return Ok(None);
    }
    let found: Option<serde_json::Value> = db
        .prepare(UNKNOWN)
        .bind(&[int(installation_id)?, spec.profile.as_str().into()])?
        .first(None)
        .await?;
    Ok(match found {
        Some(row) if row["name"].is_null() => Some(format!("no runner profile {}", spec.profile)),
        _ => None,
    })
}

/// Stores `profile` for the tenant, replacing one of the same name.
pub(crate) async fn save(db: &D1Database, tenant_id: &str, profile: &Profile) -> Result<()> {
    let optional = |value: Option<u16>| value.map_or(JsValue::NULL, Into::into);
    db.prepare(SAVE)
        .bind(&[
            tenant_id.into(),
            profile.name.as_str().into(),
            profile.arch.as_str().into(),
            profile.families.join(",").into(),
            profile.cpu_min.into(),
            optional(profile.cpu_max),
            profile.memory_min.into(),
            optional(profile.memory_max),
            optional(profile.disk_gib),
            u8::from(profile.capacity.spot()).into(),
            profile.image.as_str().into(),
            profile.extras.join(",").into(),
            profile.warm_pool.into(),
            int(now_ms())?,
            profile.categories.join(",").into(),
            optional(profile.min_generation),
            profile.capacity.as_str().into(),
        ])?
        .run()
        .await?;
    Ok(())
}

/// How many profiles the tenant has.
pub(crate) async fn count(db: &D1Database, tenant_id: &str) -> Result<usize> {
    Ok(db
        .prepare(COUNT)
        .bind(&[tenant_id.into()])?
        .first::<Count>(None)
        .await?
        .map_or(0, |row| row.count))
}

/// Removes the tenant's profile called `name`. Whether there was one.
pub(crate) async fn delete(db: &D1Database, tenant_id: &str, name: &str) -> Result<bool> {
    let result = db
        .prepare(DELETE)
        .bind(&[tenant_id.into(), name.into()])?
        .run()
        .await?;
    Ok(result.meta()?.and_then(|meta| meta.changes) == Some(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;

    fn profile(name: &str, memory_max: Option<u16>) -> Profile {
        Profile {
            name: name.to_owned(),
            arch: Arch::Arm64,
            categories: vec!["c".to_owned(), "m".to_owned()],
            min_generation: Some(7),
            families: vec!["c8g".to_owned()],
            cpu_min: 4,
            cpu_max: Some(16),
            memory_min: 8,
            memory_max,
            disk_gib: None,
            capacity: Capacity::SpotWithFallback,
            image: "ubuntu-26.04".to_owned(),
            extras: Vec::new(),
            warm_pool: 0,
        }
    }

    /// What [`save`] binds.
    fn save(tenant_id: &str, profile: &Profile) -> Vec<Param> {
        let optional = |value: Option<u16>| value.map(u32::from);
        params![
            tenant_id,
            profile.name.as_str(),
            profile.arch.as_str(),
            profile.families.join(","),
            u32::from(profile.cpu_min),
            optional(profile.cpu_max),
            u32::from(profile.memory_min),
            optional(profile.memory_max),
            optional(profile.disk_gib),
            u32::from(u8::from(profile.capacity.spot())),
            profile.image.as_str(),
            profile.extras.join(","),
            u32::from(profile.warm_pool),
            NOW,
            profile.categories.join(","),
            optional(profile.min_generation),
            profile.capacity.as_str(),
        ]
    }

    fn seed() -> Db {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            &params!["t1", 9u64, "acme", 42u64, 1u64],
        );
        db
    }

    #[test]
    fn saved_profiles_read_back() {
        let db = seed();
        let big = profile("big", Some(64));
        db.run(SAVE, &save("t1", &profile("big", None)));
        db.run(SAVE, &save("t1", &big));
        db.run(SAVE, &save("t1", &profile("arm", None)));

        let all: Vec<Profile> = db
            .all::<Row>(&custom_query(), &params!["t1"])
            .into_iter()
            .map(Profile::from)
            .collect();
        assert_eq!(all, [profile("arm", None), big.clone()]);
        let named = db
            .first::<Row>(&named_query(), &params!["t1", "big"])
            .map(Profile::from);
        assert_eq!(named, Some(big));
        assert!(
            db.first::<Row>(&named_query(), &params!["t1", "x"])
                .is_none()
        );

        let count = db
            .first::<Count>(COUNT, &params!["t1"])
            .map(|row| row.count);
        assert_eq!(count, Some(2));
        assert_eq!(db.run(DELETE, &params!["t1", "big"]), 1);
        assert_eq!(db.run(DELETE, &params!["t1", "big"]), 0);
        let count = db
            .first::<Count>(COUNT, &params!["t1"])
            .map(|row| row.count);
        assert_eq!(count, Some(1));
    }

    #[test]
    fn unknown_profiles_are_named() {
        let db = seed();
        db.run(SAVE, &save("t1", &profile("big", None)));
        let found = |installation: u64, name: &str| {
            db.first::<serde_json::Value>(UNKNOWN, &params![installation, name])
        };
        assert_eq!(found(42, "big").unwrap()["name"], "big");
        assert!(found(42, "small").unwrap()["name"].is_null());
        assert!(found(43, "big").is_none());
    }

    #[test]
    fn rows_become_profiles() {
        let row = Row {
            name: "hatchrun-big".to_owned(),
            arch: "arm64".to_owned(),
            families: "c8g, m8g".to_owned(),
            cpu_min: 8,
            cpu_max: None,
            memory_min: 16,
            memory_max: Some(64),
            disk_gib: Some(100),
            capacity: "spot-with-fallback".to_owned(),
            image: "ubuntu-26.04".to_owned(),
            extras: String::new(),
            warm_pool: 0,
            categories: String::new(),
            min_generation: None,
        };
        let profile = Profile::from(row);
        assert_eq!(profile.arch, Arch::Arm64);
        assert_eq!(profile.families, ["c8g", "m8g"]);
        assert_eq!(profile.extras.len(), 0);
        assert_eq!(profile.capacity, Capacity::SpotWithFallback);
        assert_eq!(profile.categories, Vec::<String>::new());
        assert_eq!(profile.clone().validate(), Ok(profile));
    }
}
