// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Connecting a tenant's AWS account: the Launch Stack links for
//! deploy/cloudformation/connect.yaml and the checks on what the stack
//! made, kept apart from the Workers runtime so they test natively.
//!
//! The owner opens a link for a region, which fills in the tenant id,
//! creates the stack and pastes its `RoleArn` output into the app. The
//! Worker then assumes the role with a token for the tenant, reads what
//! the stack allows from its settings parameter ([`parameter_name`]),
//! checks that the subnets there exist, and only then saves the
//! connection.
//!
//! The parameter holds a [`Stack`] as JSON, rewritten on every stack
//! update; tags on the launch template, where these settings lived
//! before version 4, keep the values the stack was created with. When a release changes connect.yaml,
//! [`TEMPLATE_VERSION`] goes up with it and [`CHANGES`] says what the
//! new version brings; the app tells owners of older stacks to update,
//! and the reconciler reads the parameter again daily, so an updated
//! stack is picked up without anyone verifying the role again.

use serde::Deserialize;

use crate::http::encode;

/// Regions a tenant can connect, with their names. Each needs runner
/// images, which AWS Marketplace serves in every region.
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

/// The version of deploy/cloudformation/connect.yaml in this release,
/// as its settings parameter says. A stack from before the version was
/// recorded is version 1.
pub const TEMPLATE_VERSION: u32 = 4;

/// What each version of connect.yaml brings, in words for the owner.
pub const CHANGES: &[(u32, &str)] = &[
    (
        2,
        "Runners launch through EC2 Fleet, which picks the cheapest instance type with \
         capacity. hatchrun reads which instance types your region offers and what they \
         cost each day. The default limit allows any size up to 8xlarge and refuses GPU, \
         accelerator and bare metal types, instead of naming families.",
    ),
    (
        3,
        "Runners launch from the hatchrun images in AWS Marketplace. Subscribe this \
         account to the amd64 and arm64 images once, then update the stack; until then \
         runners do not start.",
    ),
    (
        4,
        "The stack keeps its settings in a Systems Manager parameter that hatchrun reads, \
         so later stack updates, such as new instance type limits, take effect. Until you \
         update, hatchrun cannot read this stack and runners do not start.",
    ),
];

/// The `InstanceTypes` default of connect.yaml before version 2, which
/// names families; a stack updated with its previous values keeps it.
pub const FAMILY_LIST_DEFAULT: &str =
    "c7g.*,c7i.*,c8g.*,c8i.*,m7g.*,m7i.*,m8g.*,m8i.*,r7g.*,r7i.*,r8g.*,r8i.*";
/// The `InstanceTypes` default from version 2 on.
pub const SIZE_CAP_DEFAULT: &str = "*.medium,*.large,*.xlarge,*.2xlarge,*.4xlarge,*.8xlarge";

/// What versions after `version` bring, oldest first. `None` is a
/// stack from before the version tag.
#[must_use]
pub fn changes_since(version: Option<u32>) -> Vec<&'static str> {
    let version = version.unwrap_or(1);
    CHANGES
        .iter()
        .filter(|(added, _)| *added > version)
        .map(|(_, change)| *change)
        .collect()
}

/// The console page listing the connect stack in `region`, where the
/// owner chooses Update and replaces the template.
#[must_use]
pub fn stack_console_url(region: &str) -> String {
    format!(
        "https://{region}.console.aws.amazon.com/cloudformation/home?region={region}\
         #/stacks?filteringText={STACK_NAME}&filteringStatus=active&viewNested=true"
    )
}

/// The AWS CLI command that updates the connect stack in `region` to
/// the template at `template_url`. Every parameter keeps its value
/// except `InstanceTypes` when `size_cap`, which moves from the old
/// family list to the size cap; parameters the stack does not have yet
/// take their defaults.
#[must_use]
pub fn update_command(region: &str, template_url: &str, size_cap: bool) -> String {
    let keep = |name: &str| format!("ParameterKey={name},UsePreviousValue=true");
    let mut parameters: Vec<String> = ["TenantId", "OidcProviderArn", "VolumeSizeLimit"]
        .iter()
        .map(|name| keep(name))
        .collect();
    parameters.push(if size_cap {
        format!("'ParameterKey=InstanceTypes,ParameterValue=\"{SIZE_CAP_DEFAULT}\"'")
    } else {
        keep("InstanceTypes")
    });
    parameters.extend(
        ["RootVolumeSize", "VpcId", "SubnetIds"]
            .iter()
            .map(|name| keep(name)),
    );
    format!(
        "aws cloudformation update-stack --region {region} --stack-name {STACK_NAME} \
         --template-url {template_url} --capabilities CAPABILITY_IAM --parameters {}",
        parameters.join(" ")
    )
}

/// Variable with the AWS Marketplace page of the amd64 runner image,
/// where an account subscribes to it.
pub const MARKETPLACE_AMD64_URL: &str = "MARKETPLACE_AMD64_URL";
/// The same for the arm64 image.
pub const MARKETPLACE_ARM64_URL: &str = "MARKETPLACE_ARM64_URL";

/// The code EC2 refuses a launch with when the account has not
/// subscribed to the image's AWS Marketplace product.
pub const NOT_SUBSCRIBED: &str = "OptInRequired";

/// What a launch refused with [`NOT_SUBSCRIBED`] tells the owner: where
/// to subscribe to the `arch` image and how to launch again after.
#[must_use]
pub fn subscribe_message(arch: &str, url: &str) -> String {
    format!(
        "This AWS account has not subscribed to the hatchrun {arch} runner image. Subscribe \
         once, for free, at {url}, then verify the AWS role to launch the waiting jobs."
    )
}

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

/// The settings parameter of `tenant`'s connect stack in a region.
#[must_use]
pub fn parameter_name(tenant: &str) -> String {
    format!("/hatchrun/connect/{tenant}")
}

/// What a tenant's connect stack allows, as its settings parameter
/// says.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Stack {
    /// The connect.yaml version the stack runs.
    pub version: u32,
    /// The launch template runners start from, `lt-...`.
    pub launch_template_id: String,
    /// The subnets runners launch into, in the order to try them.
    #[serde(deserialize_with = "comma_list")]
    pub subnets: Vec<String>,
    /// The `InstanceTypes` parameter, comma separated patterns.
    pub instance_types: String,
    /// The `ExcludedInstanceTypes` parameter, comma separated patterns.
    pub excluded_instance_types: String,
    /// The `VolumeSizeLimit` parameter, GiB.
    pub volume_size_limit: u16,
    /// The `RootVolumeSize` parameter, GiB.
    pub root_volume_size: u16,
    /// Whether the role may launch with EC2 Fleet.
    pub fleet: bool,
}

impl Stack {
    /// Reads the settings parameter's value.
    ///
    /// # Errors
    ///
    /// When the value is not a stack's settings.
    pub fn parse(value: &str) -> Result<Self, String> {
        serde_json::from_str(value).map_err(|err| format!("stack settings: {err}"))
    }
}

/// The stack joins its list parameters with commas.
fn comma_list<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let joined = String::deserialize(deserializer)?;
    Ok(joined
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect())
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
    fn tells_where_to_subscribe() {
        assert_eq!(
            subscribe_message("arm64", "https://aws.amazon.com/marketplace/pp/prodview-x"),
            "This AWS account has not subscribed to the hatchrun arm64 runner image. Subscribe \
             once, for free, at https://aws.amazon.com/marketplace/pp/prodview-x, then verify \
             the AWS role to launch the waiting jobs."
        );
    }

    #[test]
    fn the_template_carries_this_version() {
        let template = include_str!("../../../deploy/cloudformation/connect.yaml");
        let version = format!("{{\"version\":{TEMPLATE_VERSION},");
        assert!(
            template.contains(&version),
            "connect.yaml's settings parameter must start with {version}; bump \
             TEMPLATE_VERSION and CHANGES with the template"
        );
        assert_eq!(
            CHANGES.last().map(|(version, _)| *version),
            Some(TEMPLATE_VERSION)
        );
        assert!(template.contains(&format!("Default: \"{SIZE_CAP_DEFAULT}\"")));
    }

    #[test]
    fn says_what_an_update_brings() {
        assert_eq!(changes_since(None).len(), 3);
        assert_eq!(changes_since(Some(1)), changes_since(None));
        assert_eq!(changes_since(Some(TEMPLATE_VERSION)), Vec::<&str>::new());
        assert_eq!(
            stack_console_url("ap-south-1"),
            "https://ap-south-1.console.aws.amazon.com/cloudformation/home?region=ap-south-1\
             #/stacks?filteringText=hatchrun&filteringStatus=active&viewNested=true"
        );
        let keep = update_command("ap-south-1", "https://t.example/connect.yaml", false);
        assert!(keep.starts_with(
            "aws cloudformation update-stack --region ap-south-1 --stack-name hatchrun \
             --template-url https://t.example/connect.yaml --capabilities CAPABILITY_IAM"
        ));
        assert!(keep.contains("ParameterKey=InstanceTypes,UsePreviousValue=true"));
        assert!(!keep.contains("ExcludedInstanceTypes"));
        let capped = update_command("ap-south-1", "https://t.example/connect.yaml", true);
        assert!(capped.contains(&format!(
            "'ParameterKey=InstanceTypes,ParameterValue=\"{SIZE_CAP_DEFAULT}\"'"
        )));
        assert_eq!(capped.matches("UsePreviousValue=true").count(), 6);
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
    fn reads_the_stack_settings() {
        let value = r#"{"version":4,"launchTemplateId":"lt-0abc","subnets":"subnet-a, subnet-b",
            "instanceTypes":"*.large,*.xlarge","excludedInstanceTypes":"","volumeSizeLimit":500,
            "rootVolumeSize":40,"fleet":true}"#;
        assert_eq!(
            Stack::parse(value).unwrap(),
            Stack {
                version: 4,
                launch_template_id: "lt-0abc".to_owned(),
                subnets: vec!["subnet-a".to_owned(), "subnet-b".to_owned()],
                instance_types: "*.large,*.xlarge".to_owned(),
                excluded_instance_types: String::new(),
                volume_size_limit: 500,
                root_volume_size: 40,
                fleet: true,
            }
        );
        assert!(Stack::parse(r#"{"version":4}"#).is_err());
        assert!(Stack::parse(&value.replace("\"fleet\":true", "\"fleet\":true,\"x\":1")).is_err());
        assert_eq!(parameter_name("abc"), "/hatchrun/connect/abc");
    }

    #[test]
    fn the_template_writes_settings_this_release_reads() {
        let template = include_str!("../../../deploy/cloudformation/connect.yaml");
        let line = template
            .lines()
            .find(|line| line.contains(r#"'{"version":"#))
            .expect("connect.yaml has a settings parameter");
        let mut value = line
            .trim()
            .trim_start_matches("- ")
            .trim_matches('\'')
            .to_owned();
        for (name, sample) in [
            ("LaunchTemplate", "lt-0abc"),
            ("Subnets", "subnet-a,subnet-b"),
            ("Types", "*.large"),
            ("Excluded", "g*,*.metal*"),
            ("VolumeSizeLimit", "500"),
            ("RootVolumeSize", "40"),
        ] {
            value = value.replace(&format!("${{{name}}}"), sample);
        }
        let stack = Stack::parse(&value).unwrap();
        assert_eq!(stack.version, TEMPLATE_VERSION);
        assert_eq!(stack.subnets, ["subnet-a", "subnet-b"]);
        assert!(stack.fleet);
    }
}
