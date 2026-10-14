// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The `instance_types` and `instance_type_regions` tables.

use serde::Deserialize;
use worker::{D1Database, Result};

use super::Entry;
use crate::int;
use crate::labels::Arch;

/// Rows per `INSERT`, which takes them as one JSON parameter.
const ROWS_PER_INSERT: usize = 200;

/// Replaces `region`'s catalog with `entries` and records the refresh,
/// all or nothing.
pub(crate) async fn replace(
    db: &D1Database,
    region: &str,
    entries: &[Entry],
    now: u64,
) -> Result<()> {
    let mut statements = vec![
        db.prepare("DELETE FROM instance_types WHERE region = ?1")
            .bind(&[region.into()])?,
    ];
    for chunk in entries.chunks(ROWS_PER_INSERT) {
        let rows: Vec<serde_json::Value> = chunk.iter().map(row).collect();
        statements.push(
            db.prepare(
                "INSERT INTO instance_types (region, name, family, category, generation, arch, \
                 vcpus, memory_mib, gpus, accelerators, bare_metal, burstable, local_storage, \
                 spot, zones, on_demand_micros, spot_micros) \
                 SELECT ?1, value ->> 0, value ->> 1, value ->> 2, value ->> 3, value ->> 4, \
                 value ->> 5, value ->> 6, value ->> 7, value ->> 8, value ->> 9, value ->> 10, \
                 value ->> 11, value ->> 12, value ->> 13, value ->> 14, value ->> 15 \
                 FROM json_each(?2)",
            )
            .bind(&[
                region.into(),
                serde_json::to_string(&rows)
                    .map_err(|err| worker::Error::RustError(err.to_string()))?
                    .into(),
            ])?,
        );
    }
    statements.push(
        db.prepare(
            "INSERT INTO instance_type_regions (region, refreshed_at, attempted_at, types, error) \
             VALUES (?1, ?2, ?2, ?3, NULL) ON CONFLICT (region) DO UPDATE SET \
             refreshed_at = ?2, attempted_at = ?2, types = ?3, error = NULL",
        )
        .bind(&[
            region.into(),
            int(now)?,
            u32::try_from(entries.len()).unwrap_or(u32::MAX).into(),
        ])?,
    );
    db.batch(statements).await?;
    Ok(())
}

/// Records that refreshing `region` failed with `error`; its catalog
/// stays as it was.
pub(crate) async fn failed(db: &D1Database, region: &str, error: &str, now: u64) -> Result<()> {
    db.prepare(
        "INSERT INTO instance_type_regions (region, attempted_at, error) VALUES (?1, ?2, ?3) \
         ON CONFLICT (region) DO UPDATE SET attempted_at = ?2, error = ?3",
    )
    .bind(&[region.into(), int(now)?, error.into()])?
    .run()
    .await?;
    Ok(())
}

/// `region`'s catalog, by name; empty before its first refresh.
pub(crate) async fn region(db: &D1Database, region: &str) -> Result<Vec<Entry>> {
    let rows = db
        .prepare(
            "SELECT name, family, category, generation, arch, vcpus, memory_mib, gpus, \
             accelerators, bare_metal, burstable, local_storage, spot, zones, \
             on_demand_micros, spot_micros FROM instance_types WHERE region = ?1 ORDER BY name",
        )
        .bind(&[region.into()])?
        .all()
        .await?
        .results::<Row>()?;
    Ok(rows.into_iter().map(Row::entry).collect())
}

/// What `region` offers: its catalog, or the static families before
/// its catalog is first read.
pub(crate) async fn offered(db: &D1Database, region: &str) -> Result<Vec<Entry>> {
    let entries = self::region(db, region).await?;
    Ok(if entries.is_empty() {
        crate::instance::static_offerings(region)
    } else {
        entries
    })
}

/// When `region`'s catalog was last refreshed, and why the last
/// attempt failed if it did.
pub(crate) async fn refreshed(
    db: &D1Database,
    region: &str,
) -> Result<(Option<u64>, Option<String>)> {
    #[derive(Deserialize)]
    struct Status {
        refreshed_at: Option<u64>,
        error: Option<String>,
    }
    let status: Option<Status> = db
        .prepare("SELECT refreshed_at, error FROM instance_type_regions WHERE region = ?1")
        .bind(&[region.into()])?
        .first(None)
        .await?;
    Ok(status.map_or((None, None), |status| (status.refreshed_at, status.error)))
}

fn row(entry: &Entry) -> serde_json::Value {
    serde_json::json!([
        entry.name,
        entry.family,
        entry.category,
        entry.generation,
        entry.arch.as_str(),
        entry.vcpus,
        entry.memory_mib,
        entry.gpus,
        u8::from(entry.accelerators),
        u8::from(entry.bare_metal),
        u8::from(entry.burstable),
        u8::from(entry.local_storage),
        u8::from(entry.spot),
        entry.zones.join(","),
        entry.on_demand_micros,
        entry.spot_micros,
    ])
}

#[derive(Deserialize)]
struct Row {
    name: String,
    family: String,
    category: String,
    generation: u16,
    arch: Arch,
    vcpus: u16,
    memory_mib: u32,
    gpus: u16,
    accelerators: u8,
    bare_metal: u8,
    burstable: u8,
    local_storage: u8,
    spot: u8,
    zones: String,
    on_demand_micros: Option<u64>,
    spot_micros: Option<u64>,
}

impl Row {
    fn entry(self) -> Entry {
        Entry {
            name: self.name,
            family: self.family,
            category: self.category,
            generation: self.generation,
            arch: self.arch,
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            gpus: self.gpus,
            accelerators: self.accelerators == 1,
            bare_metal: self.bare_metal == 1,
            burstable: self.burstable == 1,
            local_storage: self.local_storage == 1,
            spot: self.spot == 1,
            zones: self
                .zones
                .split(',')
                .filter(|zone| !zone.is_empty())
                .map(str::to_owned)
                .collect(),
            on_demand_micros: self.on_demand_micros,
            spot_micros: self.spot_micros,
        }
    }
}
