// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `unavailable_offerings` table.

use serde::Deserialize;
use worker::{D1Database, Result};

use super::Unavailable;
use crate::int;

/// The tenant's offerings that have not lapsed.
const CURRENT: &str = "SELECT instance_type, subnet_id, spot, until FROM unavailable_offerings \
     WHERE tenant_id = ?1 AND until > ?2";

/// Records a refused offering, keeping the later of two lapse times.
const REMEMBER: &str = "INSERT INTO unavailable_offerings \
     (tenant_id, instance_type, subnet_id, spot, code, until) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
     ON CONFLICT (tenant_id, instance_type, subnet_id, spot) \
     DO UPDATE SET code = ?5, until = MAX(until, ?6)";

/// A row of [`CURRENT`].
#[derive(Deserialize)]
struct Row {
    instance_type: String,
    subnet_id: String,
    spot: u8,
    until: u64,
}

/// The tenant's offerings that are still unavailable at `now`.
pub(crate) async fn current(
    db: &D1Database,
    tenant_id: &str,
    now: u64,
) -> Result<Vec<Unavailable>> {
    let rows = db
        .prepare(CURRENT)
        .bind(&[tenant_id.into(), int(now)?])?
        .all()
        .await?
        .results::<Row>()?;
    Ok(rows
        .into_iter()
        .map(|row| Unavailable {
            instance_type: row.instance_type,
            subnet_id: Some(row.subnet_id).filter(|subnet| !subnet.is_empty()),
            spot: row.spot == 1,
            until: row.until,
        })
        .collect())
}

/// Records offerings EC2 refused with `code`s, as (offering, code).
pub(crate) async fn remember(
    db: &D1Database,
    tenant_id: &str,
    refused: &[(Unavailable, String)],
) -> Result<()> {
    if refused.is_empty() {
        return Ok(());
    }
    let mut statements = Vec::new();
    for (gone, code) in refused {
        statements.push(db.prepare(REMEMBER).bind(&[
            tenant_id.into(),
            gone.instance_type.as_str().into(),
            gone.subnet_id.as_deref().unwrap_or_default().into(),
            u8::from(gone.spot).into(),
            code.as_str().into(),
            int(gone.until)?,
        ])?);
    }
    db.batch(statements).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;

    fn remember(gone: &Unavailable, code: &str) -> Vec<Param> {
        params![
            "t1",
            gone.instance_type.as_str(),
            gone.subnet_id.as_deref().unwrap_or_default(),
            u32::from(u8::from(gone.spot)),
            code,
            gone.until,
        ]
    }

    #[test]
    fn remembers_until_the_later_lapse() {
        let db = Db::new();
        let spot = Unavailable {
            instance_type: "m8g.large".into(),
            subnet_id: None,
            spot: true,
            until: NOW + 60_000,
        };
        let subnet = Unavailable {
            instance_type: "m8g.large".into(),
            subnet_id: Some("subnet-a".into()),
            spot: false,
            until: NOW - 1,
        };
        db.batch(&[
            (REMEMBER, remember(&spot, "InsufficientInstanceCapacity")),
            (REMEMBER, remember(&subnet, "InsufficientInstanceCapacity")),
        ])
        .unwrap();
        // An earlier lapse does not shorten the row.
        let earlier = Unavailable {
            until: NOW + 1000,
            ..spot
        };
        db.batch(&[(REMEMBER, remember(&earlier, "SpotMaxPriceTooLow"))])
            .unwrap();

        let rows: Vec<Row> = db.all(CURRENT, &params!["t1", NOW]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].instance_type, "m8g.large");
        assert_eq!(rows[0].subnet_id, "");
        assert_eq!(rows[0].spot, 1);
        assert_eq!(rows[0].until, NOW + 60_000);
        let code: Option<String> = db.column(
            "SELECT code FROM unavailable_offerings WHERE spot = 1",
            &[],
            "code",
        );
        assert_eq!(code.as_deref(), Some("SpotMaxPriceTooLow"));
        assert!(db.all::<Row>(CURRENT, &params!["t2", NOW]).is_empty());
    }
}
