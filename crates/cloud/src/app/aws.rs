// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Connecting a tenant's AWS account (see [`crate::connect`]).
//!
//! - `GET /api/tenants/<org>/aws` gives the Launch Stack link for each
//!   region and the account connected now.
//! - `POST /api/tenants/<org>/aws` with `{"region", "roleArn"}` checks
//!   the stack in that region and saves it.

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::Response;

use super::tenants::{authorize, show};
use super::{Answer, Ctx, Failure, get, need};
use crate::aws::{self, Credentials, ec2};
use crate::fetch::send;
use crate::launch::{AWS_ENDPOINT_URL, tenant_credentials};
use crate::{connect, int, jwt, now_ms, oidc, settings};

/// Variable with the public URL of the rendered connect.yaml.
const TEMPLATE_URL: &str = "CONNECT_TEMPLATE_URL";

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

    let endpoint = aws::endpoint("ec2", &body.region, endpoint_override.as_deref());
    let mut request = ec2::describe_launch_templates(&endpoint, &row.id);
    let templates = ec2::launch_templates(&signed(&mut request, &credentials, &body.region).await?)
        .map_err(|err| describe_failure("DescribeLaunchTemplates", &err))?;
    let Some(template) = connect::newest(templates) else {
        return Err(refused(format!(
            "The role works, but {} has no hatchrun launch template for tenant {}. \
             Check that the stack is in this region and finished creating.",
            body.region, row.id
        )));
    };
    if template.subnets.is_empty() {
        return Err(refused(format!(
            "Launch template {} has no subnets tagged. Update the stack to the latest \
             connect.yaml.",
            template.id
        )));
    }
    let mut request = ec2::describe_subnets(&endpoint, &template.subnets);
    let found =
        ec2::subnets(&signed(&mut request, &credentials, &body.region).await?).map_err(|err| {
            match err.code.as_str() {
                "InvalidSubnetID.NotFound" => refused(format!(
                    "A subnet of the stack no longer exists: {}",
                    err.message
                )),
                _ => describe_failure("DescribeSubnets", &err),
            }
        })?;
    if let Some(missing) = template
        .subnets
        .iter()
        .find(|subnet| !found.iter().any(|found| &found.id == *subnet))
    {
        return Err(refused(format!("Subnet {missing} does not exist")));
    }

    save(ctx, &row.id, role_arn, &body.region, &template).await?;
    crate::launch::recheck(ctx.db, &row.id).await?;
    show(ctx, &row.org_login).await
}

/// Records the verified connection and the stack's limits.
async fn save(
    ctx: &Ctx<'_>,
    tenant: &str,
    role_arn: &str,
    region: &str,
    template: &ec2::LaunchTemplate,
) -> worker::Result<()> {
    ctx.db
        .prepare(
            "UPDATE tenants SET role_arn = ?2, region = ?3, launch_template_id = ?4, \
             subnet_ids = ?5, updated_at = ?6, instance_types = ?7, volume_size_limit = ?8, \
             root_volume_size = ?9, fleet = ?10, excluded_instance_types = ?11 WHERE id = ?1",
        )
        .bind(&[
            tenant.into(),
            role_arn.into(),
            region.into(),
            template.id.as_str().into(),
            template.subnets.join(",").into(),
            int(now_ms())?,
            template
                .instance_types
                .as_deref()
                .map_or(JsValue::NULL, Into::into),
            template.volume_size_limit.map_or(JsValue::NULL, Into::into),
            template.root_volume_size.map_or(JsValue::NULL, Into::into),
            u8::from(template.fleet).into(),
            template
                .excluded_instance_types
                .as_deref()
                .map_or(JsValue::NULL, Into::into),
        ])?
        .run()
        .await?;
    Ok(())
}

fn describe_failure(action: &str, err: &aws::Error) -> Failure {
    if err.code == "UnauthorizedOperation" {
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
) -> Result<crate::http::Response, Failure> {
    aws::sign_v4(request, credentials, region, "ec2", now_ms())?;
    Ok(send(request).await?)
}
