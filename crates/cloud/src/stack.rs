// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a tenant's connect stack allows, as its settings parameter says
//! (see [`crate::connect`]), kept on the tenant row. The
//! app records it when an owner verifies the role and the reconciler
//! again once a day, so a stack the owner updated is picked up either
//! way.

use wasm_bindgen::JsValue;
use worker::{D1Database, Result};

use crate::connect::Stack;
use crate::int;

/// How often the reconciler reads a tenant's stack settings again, in
/// milliseconds.
pub const CHECK_EVERY: u64 = 24 * 60 * 60 * 1000;

/// Records `stack` on the tenant at `now`. Returns whether anything it
/// says changed.
pub(crate) async fn record(
    db: &D1Database,
    tenant_id: &str,
    stack: &Stack,
    now: u64,
) -> Result<bool> {
    let values: [JsValue; 11] = [
        tenant_id.into(),
        stack.launch_template_id.as_str().into(),
        stack.subnets.join(",").into(),
        stack.instance_types.as_str().into(),
        stack.volume_size_limit.into(),
        stack.root_volume_size.into(),
        u8::from(stack.fleet).into(),
        stack.excluded_instance_types.as_str().into(),
        stack.version.into(),
        int(now)?,
        stack
            .cache_bucket
            .as_deref()
            .map_or(JsValue::NULL, Into::into),
    ];
    let results = db
        .batch(vec![
            db.prepare(
                "UPDATE tenants SET launch_template_id = ?2, subnet_ids = ?3, \
                 instance_types = ?4, volume_size_limit = ?5, root_volume_size = ?6, \
                 fleet = ?7, excluded_instance_types = ?8, template_version = ?9, \
                 updated_at = ?10, cache_bucket = ?11 \
                 WHERE id = ?1 AND (launch_template_id IS NOT ?2 OR subnet_ids IS NOT ?3 \
                 OR instance_types IS NOT ?4 OR volume_size_limit IS NOT ?5 \
                 OR root_volume_size IS NOT ?6 OR fleet IS NOT ?7 \
                 OR excluded_instance_types IS NOT ?8 OR template_version IS NOT ?9 \
                 OR cache_bucket IS NOT ?11)",
            )
            .bind(&values)?,
            db.prepare("UPDATE tenants SET stack_checked_at = ?2 WHERE id = ?1")
                .bind(&[tenant_id.into(), int(now)?])?,
        ])
        .await?;
    Ok(results
        .first()
        .and_then(|result| result.meta().ok().flatten())
        .and_then(|meta| meta.changes)
        == Some(1))
}
