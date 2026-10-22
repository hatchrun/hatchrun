// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Connecting a tenant's AWS account (see [`crate::connect`]).
//!
//! - `GET /api/tenants/<org>/aws` gives the AWS Marketplace pages that
//!   subscribe the account to the runner images, the Launch Stack link
//!   for each region and the account connected now.
//! - `POST /api/tenants/<org>/aws` with `{"region", "roleArn"}` checks
//!   the stack in that region and saves it.

use serde::{Deserialize, Serialize};
use worker::Response;

use super::tenants::{authorize, show};
use super::{Answer, Ctx, Failure, get, need};
use crate::aws::{self, Credentials, ec2, ssm};
use crate::db::params;
use crate::fetch::send;
use crate::launch::AWS_ENDPOINT_URL;
use crate::start::role_credentials;
use crate::{connect, jwt, now_ms, oidc, settings};

/// Variable with the public URL of the rendered connect.yaml.
pub(crate) const TEMPLATE_URL: &str = "CONNECT_TEMPLATE_URL";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Region {
    id: &'static str,
    name: &'static str,
    /// Whether runner images are published there yet.
    has_image: bool,
    /// Whether the organization's settings allow it.
    allowed: bool,
    launch_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImageRegion {
    region: String,
}

/// Every region with a runner image.
const IMAGE_REGIONS: &str = "SELECT DISTINCT region FROM images";

/// `GET /api/tenants/<org>/aws`
pub(crate) async fn instructions(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let template = get(ctx.env, TEMPLATE_URL);
    let imaged: Vec<String> = ctx
        .db
        .all::<ImageRegion>(IMAGE_REGIONS, &[])
        .await?
        .into_iter()
        .map(|row| row.region)
        .collect();
    let regions: Vec<Region> = connect::REGIONS
        .iter()
        .map(|(id, name)| Region {
            allowed: settings::region_allowed(id, row.allowed_regions.as_deref()),
            id,
            name,
            has_image: imaged.iter().any(|region| region == id),
            launch_url: template
                .as_deref()
                .map(|url| connect::launch_stack_url(url, id, &row.id)),
        })
        .collect();
    Ok(Response::from_json(&serde_json::json!({
        "tenantId": row.id,
        "marketplace": {
            "amd64": need(ctx.env, connect::MARKETPLACE_AMD64_URL)?,
            "arm64": need(ctx.env, connect::MARKETPLACE_ARM64_URL)?,
        },
        "templateUrl": template,
        "regions": regions,
    }))?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Connect {
    region: String,
    role_arn: String,
}

/// A check that failed, in words for the owner.
fn refused(message: impl Into<String>) -> Failure {
    Failure::new(422, message)
}

/// `POST /api/tenants/<org>/aws`
pub(crate) async fn verify(ctx: &Ctx<'_>, org: &str, body: Connect) -> Answer {
    let row = authorize(ctx, org).await?;
    let role_arn = body.role_arn.trim();
    if !connect::known_region(&body.region) {
        return Err(refused(format!(
            "{} is not a region hatchrun runs in",
            body.region
        )));
    }
    if !settings::region_allowed(&body.region, row.allowed_regions.as_deref()) {
        return Err(refused(format!(
            "{} does not allow runners in {}. Change the allowed regions in settings.",
            row.org_login, body.region
        )));
    }
    if !connect::valid_role_arn(role_arn) {
        return Err(refused(
            "That is not a role ARN. Paste the RoleArn output of the stack.",
        ));
    }
    let oidc_key = jwt::pkcs8_der(&need(ctx.env, oidc::SIGNING_KEY)?)
        .map_err(|err| Failure::new(500, format!("{}: {err}", oidc::SIGNING_KEY)))?;
    let oidc_kid = need(ctx.env, oidc::KEY_ID)?;
    let endpoint_override = get(ctx.env, AWS_ENDPOINT_URL);

    let credentials = role_credentials(
        &oidc_key,
        &oidc_kid,
        endpoint_override.as_deref(),
        &row.id,
        role_arn,
        &body.region,
        "hatchrun-connect",
    )
    .await
    .map_err(|err| match err.code.as_str() {
        "AccessDenied" | "InvalidIdentityToken" => refused(format!(
            "AWS would not let hatchrun assume {role_arn}. Check that the stack was created \
             with tenant ID {} and that this is its RoleArn output. AWS said: {}",
            row.id, err.message
        )),
        _ => Failure::new(502, format!("AssumeRoleWithWebIdentity: {err}")),
    })?;

    let ssm = aws::endpoint("ssm", &body.region, endpoint_override.as_deref());
    let mut request = ssm::get_parameter(&ssm, &connect::parameter_name(&row.id));
    let answer = signed(&mut request, &credentials, &body.region, "ssm").await?;
    let stack = ssm::value(&answer)
        .map_err(|err| match err.code.as_str() {
            "ParameterNotFound" => refused(format!(
                "The role works, but {} has no hatchrun stack settings for tenant {}. Check \
                 that the stack is in this region and finished creating, or update it to the \
                 latest connect.yaml.",
                body.region, row.id
            )),
            _ => describe_failure("GetParameter", &err),
        })
        .and_then(|value| {
            connect::Stack::parse(&value).map_err(|err| {
                refused(format!(
                    "The stack's settings are not what hatchrun expects ({err}). Update the \
                     stack to the latest connect.yaml."
                ))
            })
        })?;
    if stack.subnets.is_empty() {
        return Err(refused(
            "The stack names no subnets. Update the stack to the latest connect.yaml.",
        ));
    }
    let endpoint = aws::endpoint("ec2", &body.region, endpoint_override.as_deref());
    let mut request = ec2::describe_subnets(&endpoint, &stack.subnets);
    let found = ec2::subnets(&signed(&mut request, &credentials, &body.region, "ec2").await?)
        .map_err(|err| match err.code.as_str() {
            "InvalidSubnetID.NotFound" => refused(format!(
                "A subnet of the stack no longer exists: {}",
                err.message
            )),
            _ => describe_failure("DescribeSubnets", &err),
        })?;
    if let Some(missing) = stack
        .subnets
        .iter()
        .find(|subnet| !found.iter().any(|found| &found.id == *subnet))
    {
        return Err(refused(format!("Subnet {missing} does not exist")));
    }

    save(ctx, &row.id, role_arn, &body.region, &stack).await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    show(ctx, &row.org_login).await
}

/// Saves a verified role. Binds the tenant, role ARN, region and the
/// time in milliseconds.
const SAVE_ROLE: &str =
    "UPDATE tenants SET role_arn = $2, region = $3, updated_at = $4 WHERE id = $1";

/// Records the verified connection and what the stack allows.
async fn save(
    ctx: &Ctx<'_>,
    tenant: &str,
    role_arn: &str,
    region: &str,
    stack: &connect::Stack,
) -> worker::Result<()> {
    ctx.db
        .run(SAVE_ROLE, &params![tenant, role_arn, region, now_ms()])
        .await?;
    crate::stack::record(ctx.db, tenant, stack, now_ms()).await?;
    Ok(())
}

fn describe_failure(action: &str, err: &aws::Error) -> Failure {
    if matches!(
        err.code.as_str(),
        "UnauthorizedOperation" | "AccessDeniedException"
    ) {
        return refused(format!(
            "The role may not call {action}. Update the stack to the latest connect.yaml."
        ));
    }
    Failure::new(502, format!("{action}: {err}"))
}

async fn signed(
    request: &mut crate::http::Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
) -> Result<crate::http::Response, Failure> {
    aws::sign_v4(request, credentials, region, service, now_ms())?;
    Ok(send(request).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::{ROOT_DEVICE, UPSERT};
    use crate::testdb::{Db, params};

    const NOW: u64 = 1_792_140_000_000;

    #[test]
    fn lists_each_imaged_region_once() {
        let db = Db::new();
        for (region, arch) in [
            ("us-east-1", "amd64"),
            ("us-east-1", "arm64"),
            ("eu-west-1", "amd64"),
        ] {
            db.run(
                UPSERT,
                &params![
                    region,
                    arch,
                    "ami-0123abcd",
                    ROOT_DEVICE,
                    "20261013.1915",
                    NOW
                ],
            );
        }
        let mut regions: Vec<String> = db
            .all::<ImageRegion>(IMAGE_REGIONS, &[])
            .into_iter()
            .map(|row| row.region)
            .collect();
        regions.sort();
        assert_eq!(regions, ["eu-west-1", "us-east-1"]);
    }

    #[test]
    fn saves_the_role_on_its_tenant() {
        let db = Db::new();
        for (id, org) in [("t1", 11u64), ("t2", 12)] {
            db.run(
                "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $4)",
                &params![id, org, format!("org{org}"), NOW],
            );
        }
        let role = "arn:aws:iam::123456789012:role/hatchrun";
        let changed = db.run(SAVE_ROLE, &params!["t1", role, "eu-west-1", NOW + 1]);
        assert_eq!(changed, 1);
        let rows = db
            .rows(
                "SELECT id, role_arn, region, updated_at FROM tenants ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0]["role_arn"], role);
        assert_eq!(rows[0]["region"], "eu-west-1");
        assert_eq!(rows[0]["updated_at"], NOW + 1);
        assert!(rows[1]["role_arn"].is_null());
    }
}
