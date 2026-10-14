// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! What a tenant's connect stack allows, as the tags on its launch
//! template say (see [`crate::connect`]), kept on the tenant row. The
//! app records it when an owner verifies the role and the reconciler
//! again once a day, so a stack the owner updated is picked up either
//! way.

use wasm_bindgen::JsValue;
use worker::{D1Database, Result};

use crate::aws::ec2::LaunchTemplate;
use crate::int;

/// How often the reconciler reads a tenant's launch template again, in
/// milliseconds.
pub const CHECK_EVERY: u64 = 24 * 60 * 60 * 1000;

/// Records `template` on the tenant at `now`. Returns whether anything
/// it says changed.
pub(crate) async fn record(
    db: &D1Database,
    tenant_id: &str,
    template: &LaunchTemplate,
    now: u64,
) -> Result<bool> {
    let text = |value: Option<&str>| value.map_or(JsValue::NULL, Into::into);
    let number = |value: Option<u16>| value.map_or(JsValue::NULL, Into::into);
    let values = [
        tenant_id.into(),
        template.id.as_str().into(),
        template.subnets.join(",").into(),
        text(template.instance_types.as_deref()),
        number(template.volume_size_limit),
        number(template.root_volume_size),
        u8::from(template.fleet).into(),
        text(template.excluded_instance_types.as_deref()),
        template.template_version.map_or(JsValue::NULL, Into::into),
        int(now)?,
    ];
    let results = db
        .batch(vec![
            db.prepare(
                "UPDATE tenants SET launch_template_id = ?2, subnet_ids = ?3, \
                 instance_types = ?4, volume_size_limit = ?5, root_volume_size = ?6, \
                 fleet = ?7, excluded_instance_types = ?8, template_version = ?9, \
                 updated_at = ?10 \
                 WHERE id = ?1 AND (launch_template_id IS NOT ?2 OR subnet_ids IS NOT ?3 \
                 OR instance_types IS NOT ?4 OR volume_size_limit IS NOT ?5 \
                 OR root_volume_size IS NOT ?6 OR fleet IS NOT ?7 \
                 OR excluded_instance_types IS NOT ?8 OR template_version IS NOT ?9)",
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
