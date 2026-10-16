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
use crate::fetch::send;
use crate::launch::{AWS_ENDPOINT_URL, tenant_credentials};
use crate::{connect, int, jwt, now_ms, oidc, settings};

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

/// `GET /api/tenants/<org>/aws`
pub(crate) async fn instructions(ctx: &Ctx<'_>, org: &str) -> Answer {
    let row = authorize(ctx, org).await?;
    let template = get(ctx.env, TEMPLATE_URL);
    let imaged: Vec<String> = ctx
        .db
        .prepare("SELECT DISTINCT region FROM images")
        .all()
        .await?
        .results::<ImageRegion>()?
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

    let credentials = tenant_credentials(
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

/// Records the verified connection and what the stack allows.
async fn save(
    ctx: &Ctx<'_>,
    tenant: &str,
    role_arn: &str,
    region: &str,
    stack: &connect::Stack,
) -> worker::Result<()> {
    ctx.db
        .prepare("UPDATE tenants SET role_arn = ?2, region = ?3, updated_at = ?4 WHERE id = ?1")
        .bind(&[
            tenant.into(),
            role_arn.into(),
            region.into(),
            int(now_ms())?,
        ])?
        .run()
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
