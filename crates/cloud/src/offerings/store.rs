// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The `unavailable_offerings` table.

use serde::Deserialize;
use worker::{D1Database, Result};

use super::Unavailable;
use crate::int;

/// The tenant's offerings that are still unavailable at `now`.
pub(crate) async fn current(
    db: &D1Database,
    tenant_id: &str,
    now: u64,
) -> Result<Vec<Unavailable>> {
    #[derive(Deserialize)]
    struct Row {
        instance_type: String,
        subnet_id: String,
        spot: u8,
        until: u64,
    }
    let rows = db
        .prepare(
            "SELECT instance_type, subnet_id, spot, until FROM unavailable_offerings \
             WHERE tenant_id = ?1 AND until > ?2",
        )
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
        statements.push(
            db.prepare(
                "INSERT INTO unavailable_offerings \
                 (tenant_id, instance_type, subnet_id, spot, code, until) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (tenant_id, instance_type, subnet_id, spot) \
                 DO UPDATE SET code = ?5, until = MAX(until, ?6)",
            )
            .bind(&[
                tenant_id.into(),
                gone.instance_type.as_str().into(),
                gone.subnet_id.as_deref().unwrap_or_default().into(),
                u8::from(gone.spot).into(),
                code.as_str().into(),
                int(gone.until)?,
            ])?,
        );
    }
    db.batch(statements).await?;
    Ok(())
}
