// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Custom profiles in the `runner_profiles` table.

use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, Result};

use super::{Profile, built_in};
use crate::labels::{Arch, Spec};
use crate::{int, now_ms};

const COLUMNS: &str = "runner_profiles.name, runner_profiles.arch, runner_profiles.families, \
    runner_profiles.cpu_min, runner_profiles.cpu_max, runner_profiles.memory_min, \
    runner_profiles.memory_max, runner_profiles.disk_gib, runner_profiles.spot, \
    runner_profiles.image, runner_profiles.extras, runner_profiles.warm_pool";

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
    spot: u8,
    image: String,
    extras: String,
    warm_pool: u16,
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
            families: list(&row.families),
            cpu_min: row.cpu_min,
            cpu_max: row.cpu_max,
            memory_min: row.memory_min,
            memory_max: row.memory_max,
            disk_gib: row.disk_gib,
            spot: row.spot == 1,
            image: row.image,
            extras: list(&row.extras),
            warm_pool: row.warm_pool,
        }
    }
}

/// The tenant's own profiles, by name.
pub(crate) async fn custom(db: &D1Database, tenant_id: &str) -> Result<Vec<Profile>> {
    Ok(db
        .prepare(format!(
            "SELECT {COLUMNS} FROM runner_profiles WHERE tenant_id = ?1 ORDER BY name"
        ))
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
        .prepare(format!(
            "SELECT {COLUMNS} FROM runner_profiles WHERE tenant_id = ?1 AND name = ?2"
        ))
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
        .prepare(
            "SELECT tenants.id, runner_profiles.name FROM tenants \
             LEFT JOIN runner_profiles ON runner_profiles.tenant_id = tenants.id \
             AND runner_profiles.name = ?2 WHERE tenants.installation_id = ?1",
        )
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
    db.prepare(
        "INSERT INTO runner_profiles (tenant_id, name, arch, families, cpu_min, cpu_max, \
         memory_min, memory_max, disk_gib, spot, image, extras, warm_pool, created_at, \
         updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET arch = ?3, families = ?4, cpu_min = ?5, \
         cpu_max = ?6, memory_min = ?7, memory_max = ?8, disk_gib = ?9, spot = ?10, \
         image = ?11, extras = ?12, warm_pool = ?13, updated_at = ?14",
    )
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
        u8::from(profile.spot).into(),
        profile.image.as_str().into(),
        profile.extras.join(",").into(),
        profile.warm_pool.into(),
        int(now_ms())?,
    ])?
    .run()
    .await?;
    Ok(())
}

/// How many profiles the tenant has.
pub(crate) async fn count(db: &D1Database, tenant_id: &str) -> Result<usize> {
    #[derive(Deserialize)]
    struct Count {
        count: usize,
    }
    Ok(db
        .prepare("SELECT COUNT(*) AS count FROM runner_profiles WHERE tenant_id = ?1")
        .bind(&[tenant_id.into()])?
        .first::<Count>(None)
        .await?
        .map_or(0, |row| row.count))
}

/// Removes the tenant's profile called `name`. Whether there was one.
pub(crate) async fn delete(db: &D1Database, tenant_id: &str, name: &str) -> Result<bool> {
    let result = db
        .prepare("DELETE FROM runner_profiles WHERE tenant_id = ?1 AND name = ?2")
        .bind(&[tenant_id.into(), name.into()])?
        .run()
        .await?;
    Ok(result.meta()?.and_then(|meta| meta.changes) == Some(1))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            spot: 1,
            image: "ubuntu-26.04".to_owned(),
            extras: String::new(),
            warm_pool: 0,
        };
        let profile = Profile::from(row);
        assert_eq!(profile.arch, Arch::Arm64);
        assert_eq!(profile.families, ["c8g", "m8g"]);
        assert_eq!(profile.extras.len(), 0);
        assert!(profile.spot);
        assert_eq!(profile.clone().validate(), Ok(profile));
    }
}
