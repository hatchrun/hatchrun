// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `instance_types` and `instance_type_regions` tables.

use serde::Deserialize;
use worker::Result;

use super::Entry;
use crate::db::{Db, Param, params};
use crate::labels::Arch;

/// Rows per `INSERT`, which takes them as one JSON parameter.
const ROWS_PER_INSERT: usize = 200;

/// Drops a region's catalog before it is written again.
const CLEAR: &str = "DELETE FROM instance_types WHERE region = $1";

/// Inserts a JSON array of [`row`]s into a region's catalog.
const INSERT: &str = "INSERT INTO instance_types (region, name, family, category, generation, \
     arch, vcpus, memory_mib, gpus, accelerators, bare_metal, burstable, local_storage, \
     spot, zones, on_demand_micros, spot_micros) \
     SELECT $1, value ->> 0, value ->> 1, value ->> 2, (value ->> 3)::bigint, value ->> 4, \
     (value ->> 5)::bigint, (value ->> 6)::bigint, (value ->> 7)::bigint, \
     (value ->> 8)::boolean, (value ->> 9)::boolean, (value ->> 10)::boolean, \
     (value ->> 11)::boolean, (value ->> 12)::boolean, value ->> 13, \
     (value ->> 14)::bigint, (value ->> 15)::bigint \
     FROM json_array_elements($2::json) AS value";

/// Records a region's successful refresh.
const REFRESHED: &str = "INSERT INTO instance_type_regions \
     (region, refreshed_at, attempted_at, types, error) \
     VALUES ($1, $2, $2, $3, NULL) ON CONFLICT (region) DO UPDATE SET \
     refreshed_at = $2, attempted_at = $2, types = $3, error = NULL";

/// Records a region's failed refresh.
const FAILED: &str = "INSERT INTO instance_type_regions (region, attempted_at, error) \
     VALUES ($1, $2, $3) ON CONFLICT (region) DO UPDATE SET attempted_at = $2, error = $3";

/// A region's catalog, by name.
const REGION: &str = "SELECT name, family, category, generation, arch, vcpus, memory_mib, gpus, \
     accelerators, bare_metal, burstable, local_storage, spot, zones, \
     on_demand_micros, spot_micros FROM instance_types WHERE region = $1 ORDER BY name";

/// A region's refresh status.
const STATUS: &str = "SELECT refreshed_at, error FROM instance_type_regions WHERE region = $1";

/// A row of [`STATUS`].
#[derive(Deserialize)]
struct Status {
    refreshed_at: Option<u64>,
    error: Option<String>,
}

/// Replaces `region`'s catalog with `entries` and records the refresh,
/// all or nothing.
pub(crate) async fn replace(db: &Db, region: &str, entries: &[Entry], now: u64) -> Result<()> {
    let mut statements: Vec<(&str, Vec<Param>)> = vec![(CLEAR, params![region])];
    for chunk in entries.chunks(ROWS_PER_INSERT) {
        let rows: Vec<serde_json::Value> = chunk.iter().map(row).collect();
        let rows = serde_json::to_string(&rows)
            .map_err(|err| worker::Error::RustError(err.to_string()))?;
        statements.push((INSERT, params![region, rows]));
    }
    statements.push((REFRESHED, params![region, now, entries.len()]));
    db.batch(&statements).await?;
    Ok(())
}

/// Records that refreshing `region` failed with `error`; its catalog
/// stays as it was.
pub(crate) async fn failed(db: &Db, region: &str, error: &str, now: u64) -> Result<()> {
    db.run(FAILED, &params![region, now, error]).await?;
    Ok(())
}

/// `region`'s catalog, by name; empty before its first refresh.
pub(crate) async fn region(db: &Db, region: &str) -> Result<Vec<Entry>> {
    let rows: Vec<Row> = db.all(REGION, &params![region]).await?;
    Ok(rows.into_iter().map(Row::entry).collect())
}

/// What `region` offers: its catalog, or the static families before
/// its catalog is first read.
pub(crate) async fn offered(db: &Db, region: &str) -> Result<Vec<Entry>> {
    let entries = self::region(db, region).await?;
    Ok(if entries.is_empty() {
        crate::instance::static_offerings(region)
    } else {
        entries
    })
}

/// When `region`'s catalog was last refreshed, and why the last
/// attempt failed if it did.
pub(crate) async fn refreshed(db: &Db, region: &str) -> Result<(Option<u64>, Option<String>)> {
    let status: Option<Status> = db.first(STATUS, &params![region]).await?;
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
        entry.accelerators,
        entry.bare_metal,
        entry.burstable,
        entry.local_storage,
        entry.spot,
        entry.zones.join(","),
        entry.on_demand_micros,
        entry.spot_micros,
    ])
}

/// A row of [`REGION`], a field for each of the table's flags.
#[allow(clippy::struct_excessive_bools)]
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
    accelerators: bool,
    bare_metal: bool,
    burstable: bool,
    local_storage: bool,
    spot: bool,
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
            accelerators: self.accelerators,
            bare_metal: self.bare_metal,
            burstable: self.burstable,
            local_storage: self.local_storage,
            spot: self.spot,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::Db;

    const NOW: u64 = 1_792_140_000_000;

    fn entry(name: &str, on_demand: Option<u64>) -> Entry {
        Entry {
            name: name.into(),
            family: "m8g".into(),
            category: "m".into(),
            generation: 8,
            arch: Arch::Arm64,
            vcpus: 4,
            memory_mib: 16384,
            gpus: 0,
            accelerators: false,
            bare_metal: false,
            burstable: false,
            local_storage: true,
            spot: true,
            zones: vec!["aps1-az1".into(), "aps1-az2".into()],
            on_demand_micros: on_demand,
            spot_micros: None,
        }
    }

    /// The statements [`replace`] batches.
    fn replace(region: &str, entries: &[Entry]) -> Vec<(&'static str, Vec<Param>)> {
        let mut statements = vec![(CLEAR, params![region])];
        for chunk in entries.chunks(ROWS_PER_INSERT) {
            let rows: Vec<serde_json::Value> = chunk.iter().map(row).collect();
            statements.push((
                INSERT,
                params![region, serde_json::to_string(&rows).unwrap()],
            ));
        }
        statements.push((REFRESHED, params![region, NOW, entries.len()]));
        statements
    }

    #[test]
    fn a_catalog_reads_back_as_written() {
        let db = Db::new();
        let entries = vec![entry("m8g.xlarge", Some(180_000)), entry("m8g.large", None)];
        let statements = replace("ap-south-1", &entries);
        db.batch(&statements).unwrap();
        // A second refresh replaces the first.
        db.batch(&statements).unwrap();

        let rows: Vec<Row> = db.all(REGION, &params!["ap-south-1"]);
        let read: Vec<Entry> = rows.into_iter().map(Row::entry).collect();
        let mut wanted = entries;
        wanted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(read, wanted);

        let status: Status = db.first(STATUS, &params!["ap-south-1"]).unwrap();
        assert_eq!(status.refreshed_at, Some(NOW));
        assert_eq!(status.error, None);
        let types: Option<u64> = db.column(
            "SELECT types FROM instance_type_regions WHERE region = $1",
            &params!["ap-south-1"],
            "types",
        );
        assert_eq!(types, Some(2));
    }

    #[test]
    fn a_failure_keeps_the_last_refresh() {
        let db = Db::new();
        assert!(db.first::<Status>(STATUS, &params!["ap-south-1"]).is_none());
        db.run(FAILED, &params!["ap-south-1", NOW, "throttled"]);
        let status: Status = db.first(STATUS, &params!["ap-south-1"]).unwrap();
        assert_eq!(status.refreshed_at, None);
        assert_eq!(status.error.as_deref(), Some("throttled"));

        db.batch(&replace("ap-south-1", &[entry("m8g.large", None)]))
            .unwrap();
        db.run(FAILED, &params!["ap-south-1", NOW + 1, "denied"]);
        let status: Status = db.first(STATUS, &params!["ap-south-1"]).unwrap();
        assert_eq!(status.refreshed_at, Some(NOW));
        assert_eq!(status.error.as_deref(), Some("denied"));
        assert_eq!(db.all::<Row>(REGION, &params!["ap-south-1"]).len(), 1);
    }
}
