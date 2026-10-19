// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a tenant's connect stack allows, as its settings parameter says
//! (see [`crate::connect`]), kept on the tenant row. The
//! app records it when an owner verifies the role and the reconciler
//! again once a day, so a stack the owner updated is picked up either
//! way.

use worker::Result;

use crate::connect::Stack;
use crate::db::{Db, params};

/// How often the reconciler reads a tenant's stack settings again, in
/// milliseconds.
pub const CHECK_EVERY: u64 = 24 * 60 * 60 * 1000;

/// Writes the stack's settings on the tenant, only when one differs.
const RECORD: &str = "UPDATE tenants SET launch_template_id = $2, subnet_ids = $3, \
     instance_types = $4, volume_size_limit = $5, root_volume_size = $6, \
     fleet = $7, excluded_instance_types = $8, template_version = $9, \
     updated_at = $10, cache_bucket = $11 \
     WHERE id = $1 AND (launch_template_id IS DISTINCT FROM $2 OR subnet_ids IS DISTINCT FROM $3 \
     OR instance_types IS DISTINCT FROM $4 OR volume_size_limit IS DISTINCT FROM $5 \
     OR root_volume_size IS DISTINCT FROM $6 OR fleet IS DISTINCT FROM $7 \
     OR excluded_instance_types IS DISTINCT FROM $8 OR template_version IS DISTINCT FROM $9 \
     OR cache_bucket IS DISTINCT FROM $11)";

/// Stamps when the tenant's stack was last read.
const CHECKED: &str = "UPDATE tenants SET stack_checked_at = $2 WHERE id = $1";

/// Records `stack` on the tenant at `now`. Returns whether anything it
/// says changed.
pub(crate) async fn record(db: &Db, tenant_id: &str, stack: &Stack, now: u64) -> Result<bool> {
    let values = params![
        tenant_id,
        stack.launch_template_id.as_str(),
        stack.subnets.join(","),
        stack.instance_types.as_str(),
        stack.volume_size_limit,
        stack.root_volume_size,
        stack.fleet,
        stack.excluded_instance_types.as_str(),
        stack.version,
        now,
        stack.cache_bucket.as_deref(),
    ];
    let results = db
        .batch(&[(RECORD, values), (CHECKED, params![tenant_id, now])])
        .await?;
    Ok(results.first() == Some(&1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb::{Db, Param, params};

    const NOW: u64 = 1_792_140_000_000;

    fn values(bucket: Option<&str>, root: u16) -> Vec<Param> {
        let stack = Stack {
            version: 5,
            launch_template_id: "lt-1".into(),
            subnets: vec!["subnet-a".into(), "subnet-b".into()],
            instance_types: "m8g.*".into(),
            excluded_instance_types: String::new(),
            volume_size_limit: 200,
            root_volume_size: root,
            fleet: true,
            cache_bucket: bucket.map(str::to_owned),
        };
        params![
            "t1",
            stack.launch_template_id.as_str(),
            stack.subnets.join(","),
            stack.instance_types.as_str(),
            u32::from(stack.volume_size_limit),
            u32::from(stack.root_volume_size),
            stack.fleet,
            stack.excluded_instance_types.as_str(),
            stack.version,
            NOW,
            stack.cache_bucket.as_deref(),
        ]
    }

    fn record(db: &Db, values: &[Param]) -> bool {
        // The first statement's changes decide the return value.
        let changed = db.run(RECORD, values);
        db.run(CHECKED, &params!["t1", NOW]);
        changed == 1
    }

    #[test]
    fn records_a_stack_once() {
        let db = Db::new();
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $4)",
            &params!["t1", 9u64, "acme", 1u64],
        );
        db.batch(&[
            (RECORD, values(Some("bucket"), 30)),
            (CHECKED, params!["t1", NOW]),
        ])
        .unwrap();
        let rows = db
            .rows(
                "SELECT subnet_ids, fleet, template_version, cache_bucket, stack_checked_at \
                 FROM tenants",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["subnet_ids"], "subnet-a,subnet-b");
        assert_eq!(rows[0]["fleet"], true);
        assert_eq!(rows[0]["template_version"], 5);
        assert_eq!(rows[0]["cache_bucket"], "bucket");
        assert_eq!(rows[0]["stack_checked_at"], NOW);

        assert!(!record(&db, &values(Some("bucket"), 30)));
        assert!(record(&db, &values(None, 30)));
        assert!(!record(&db, &values(None, 30)));
        assert!(record(&db, &values(None, 40)));
    }
}
