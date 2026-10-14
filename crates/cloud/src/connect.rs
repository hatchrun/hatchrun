// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Connecting a tenant's AWS account: the Launch Stack links for
//! deploy/cloudformation/connect.yaml and the checks on what the stack
//! made, kept apart from the Workers runtime so they test natively.
//!
//! The owner opens a link for a region, which fills in the tenant id,
//! creates the stack and pastes its `RoleArn` output into the app. The
//! Worker then assumes the role with a token for the tenant, finds the
//! stack's launch template by its `hatchrun:tenant` tag, checks that the
//! subnets on its `hatchrun:subnets` tag exist, and only then saves the
//! connection.

use crate::aws::ec2::LaunchTemplate;
use crate::http::encode;

/// Regions a tenant can connect, with their names. Each needs runner
/// images, which the images pipeline copies to every region.
pub const REGIONS: [(&str, &str); 17] = [
    ("us-east-1", "US East (N. Virginia)"),
    ("us-east-2", "US East (Ohio)"),
    ("us-west-1", "US West (N. California)"),
    ("us-west-2", "US West (Oregon)"),
    ("ca-central-1", "Canada (Central)"),
    ("sa-east-1", "South America (São Paulo)"),
    ("eu-west-1", "Europe (Ireland)"),
    ("eu-west-2", "Europe (London)"),
    ("eu-west-3", "Europe (Paris)"),
    ("eu-central-1", "Europe (Frankfurt)"),
    ("eu-north-1", "Europe (Stockholm)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-southeast-1", "Asia Pacific (Singapore)"),
    ("ap-southeast-2", "Asia Pacific (Sydney)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("ap-northeast-2", "Asia Pacific (Seoul)"),
    ("ap-northeast-3", "Asia Pacific (Osaka)"),
];

/// Name of the stack the links create.
pub const STACK_NAME: &str = "hatchrun";

/// Whether `region` is one of [`REGIONS`].
#[must_use]
pub fn known_region(region: &str) -> bool {
    REGIONS.iter().any(|(id, _)| *id == region)
}

/// The console link that creates the connect stack in `region` from the
/// template at `template_url`, with the tenant id filled in.
#[must_use]
pub fn launch_stack_url(template_url: &str, region: &str, tenant: &str) -> String {
    format!(
        "https://{region}.console.aws.amazon.com/cloudformation/home?region={region}\
         #/stacks/create/review?templateURL={}&stackName={STACK_NAME}&param_TenantId={tenant}",
        encode(template_url)
    )
}

/// Whether `arn` is an IAM role ARN.
#[must_use]
pub fn valid_role_arn(arn: &str) -> bool {
    let mut parts = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some("iam"), Some(""), Some(account), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    partition.starts_with("aws")
        && account.len() == 12
        && account.bytes().all(|byte| byte.is_ascii_digit())
        && resource.strip_prefix("role/").is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+=,.@_-/".contains(&byte))
        })
}

/// The launch template to use: the newest of the tenant's stacks in the
/// region.
#[must_use]
pub fn newest(templates: Vec<LaunchTemplate>) -> Option<LaunchTemplate> {
    templates
        .into_iter()
        .max_by(|a, b| a.created.cmp(&b.created))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_open_the_stack_with_the_tenant_filled_in() {
        assert_eq!(
            launch_stack_url(
                "https://hatchrun-templates.s3.amazonaws.com/connect.yaml",
                "eu-west-1",
                "abcdefgh12345678"
            ),
            "https://eu-west-1.console.aws.amazon.com/cloudformation/home?region=eu-west-1\
             #/stacks/create/review?templateURL=https%3A%2F%2Fhatchrun-templates.s3.amazonaws.com\
             %2Fconnect.yaml&stackName=hatchrun&param_TenantId=abcdefgh12345678"
        );
        assert!(known_region("eu-west-1"));
        assert!(!known_region("eu-west-9"));
        assert!(!known_region(""));
    }

    #[test]
    fn checks_role_arns() {
        let account = "1".repeat(12);
        assert!(valid_role_arn(&format!(
            "arn:aws:iam::{account}:role/hatchrun-ControlPlaneRole-Ab1"
        )));
        assert!(valid_role_arn(&format!(
            "arn:aws-cn:iam::{account}:role/path/to/role"
        )));
        assert!(!valid_role_arn(&format!("arn:aws:iam::{account}:user/x")));
        assert!(!valid_role_arn(&format!("arn:aws:iam::{account}:role/")));
        assert!(!valid_role_arn("arn:aws:iam::12:role/x"));
        assert!(!valid_role_arn(&format!("arn:aws:s3::{account}:role/x")));
        assert!(!valid_role_arn(&format!("arn:aws:iam::{account}:role/x y")));
        assert!(!valid_role_arn("not an arn"));
    }

    #[test]
    fn picks_the_newest_stack() {
        let template = |id: &str, created: &str| LaunchTemplate {
            id: id.to_owned(),
            created: created.to_owned(),
            subnets: Vec::new(),
            instance_types: None,
            volume_size_limit: None,
            root_volume_size: None,
        };
        assert_eq!(
            newest(vec![
                template("lt-old", "2026-01-01T00:00:00.000Z"),
                template("lt-new", "2026-10-01T00:00:00.000Z"),
            ])
            .unwrap()
            .id,
            "lt-new"
        );
        assert_eq!(newest(Vec::new()), None);
    }
}
