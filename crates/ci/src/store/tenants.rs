// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tenants: who owns the runs and where their subscription stands.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

/// Where a tenant stands with its subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiBilling {
    /// No subscription row ever.
    Never,
    /// A subscription exists but none is active or trialing.
    Lapsed,
    /// An active or trialing subscription: a card on file.
    Active,
}

/// A tenant, as far as CI needs it. The billing state is read in the
/// same statement as the tenant, so the gate costs no extra query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TenantRow {
    /// The tenant id.
    pub id: String,
    /// The organization's login.
    pub org_login: String,
    /// Whether the tenant has a card for CI.
    pub ci_billing: CiBilling,
    /// How many days the tenant's runs, logs and state are kept: 30 or 90.
    pub retention_days: u32,
}

/// The columns of a [`TenantRow`]. `ci_billing` is `active` for an
/// active or trialing subscription, `lapsed` for another one, `never`
/// for none. `retention_days` is the tier: 30, or 90
/// for a tenant whose `retention_days` (shared with hatchrun.io, 1 to 90)
/// is above 30. R2 lifecycle rules exist per tier, so CI keeps for 30 or
/// 90 days and nothing between.
const TENANT_COLUMNS: &str = "id, org_login, CASE \
     WHEN EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id \
     AND subscriptions.status IN ('active', 'trialing')) \
     THEN 'active' \
     WHEN EXISTS (SELECT 1 FROM subscriptions WHERE subscriptions.tenant_id = tenants.id) \
     THEN 'lapsed' \
     ELSE 'never' END AS ci_billing, \
     CASE WHEN retention_days > 30 THEN 90 ELSE 30 END AS retention_days";

/// The tenant of installation `$1`, where `{columns}` is
/// [`TENANT_COLUMNS`].
const TENANT_FOR_INSTALLATION: &str = "SELECT {columns} FROM tenants WHERE installation_id = $1";

/// Tenant `$1`, where `{columns}` is [`TENANT_COLUMNS`].
const TENANT: &str = "SELECT {columns} FROM tenants WHERE id = $1";

/// Whether tenant `$1` has an active or trialing subscription.
const HAS_CARD: &str = "SELECT EXISTS (SELECT 1 FROM subscriptions WHERE tenant_id = $1 \
     AND status IN ('active', 'trialing')) AS card";

/// The tenant of GitHub organization id `$1`, where `{columns}` is
/// [`TENANT_COLUMNS`]. The id is unique and survives renames; the login
/// is neither.
const TENANT_BY_ORG: &str = "SELECT {columns} FROM tenants WHERE org_id = $1";

/// The tenant of `installation_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_for_installation(db: &Db, installation_id: u64) -> Result<Option<TenantRow>> {
    db.first(
        &TENANT_FOR_INSTALLATION.replace("{columns}", TENANT_COLUMNS),
        &params![installation_id],
    )
    .await
}

/// Tenant `tenant_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant(db: &Db, tenant_id: &str) -> Result<Option<TenantRow>> {
    db.first(
        &TENANT.replace("{columns}", TENANT_COLUMNS),
        &params![tenant_id],
    )
    .await
}

/// Whether tenant `tenant_id` has an active or trialing subscription.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn has_card(db: &Db, tenant_id: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct Card {
        card: bool,
    }
    Ok(db
        .first::<Card>(HAS_CARD, &params![tenant_id])
        .await?
        .is_some_and(|row| row.card))
}

/// The tenant of the GitHub organization with id `org_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn tenant_by_org_id(db: &Db, org_id: u64) -> Result<Option<TenantRow>> {
    db.first(
        &TENANT_BY_ORG.replace("{columns}", TENANT_COLUMNS),
        &params![org_id],
    )
    .await
}
