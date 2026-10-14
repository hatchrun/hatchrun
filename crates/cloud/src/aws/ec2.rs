// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `RunInstances` and `TerminateInstances` for runner instances, and the
//! lookups that check a connected account before it is saved.
//!
//! The customer's role allows `RunInstances` only from the stack's launch
//! template, with the `hatchrun:tenant` tag, `IMDSv2` and a hop limit of 1
//! (deploy/cloudformation/connect.yaml). IAM checks those on the request,
//! so the request repeats the template's metadata options rather than
//! relying on the template to fill them in.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::{Error, child_text, document, query};
use crate::http::{Request, Response};

/// EC2 API version.
const VERSION: &str = "2016-11-15";

/// Error codes that mean "try the next subnet".
pub const CAPACITY_ERRORS: &[&str] = &[
    "InsufficientInstanceCapacity",
    "InsufficientFreeAddressesInSubnet",
    "Unsupported",
    "SpotMaxPriceTooLow",
];

/// Tag with the tenant id; the role only launches and manages instances
/// that carry it.
pub const TAG_TENANT: &str = "hatchrun:tenant";
/// Tag connect.yaml puts on its launch template with the subnets runners
/// launch into, comma separated, so the app finds them from the role.
pub const TAG_SUBNETS: &str = "hatchrun:subnets";
/// Tag with the workflow job id.
pub const TAG_JOB_ID: &str = "hatchrun:job-id";
/// Tag with the runner name.
pub const TAG_RUNNER_NAME: &str = "hatchrun:runner-name";

/// One runner instance to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInstances<'a> {
    /// The tenant's launch template, `lt-...`.
    pub launch_template_id: &'a str,
    /// The runner image.
    pub image_id: &'a str,
    /// The instance type.
    pub instance_type: &'a str,
    /// The subnet to try, or the template's network when `None`.
    pub subnet_id: Option<&'a str>,
    /// User data, before base64.
    pub user_data: &'a str,
    /// Makes a retried call idempotent; at most 64 ASCII characters.
    pub client_token: &'a str,
    /// Tags for the instance, its volumes and its Spot request.
    pub tags: &'a [(&'a str, &'a str)],
    /// Request a one-time Spot instance.
    pub spot: bool,
    /// Root volume device and size in GiB, to replace the template's
    /// volume size.
    pub root_volume: Option<(&'a str, u16)>,
}

impl RunInstances<'_> {
    /// The Query API parameters.
    #[must_use]
    pub fn params(&self) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = [
            ("LaunchTemplate.LaunchTemplateId", self.launch_template_id),
            ("LaunchTemplate.Version", "$Default"),
            ("ImageId", self.image_id),
            ("InstanceType", self.instance_type),
            ("MinCount", "1"),
            ("MaxCount", "1"),
            ("ClientToken", self.client_token),
            ("InstanceInitiatedShutdownBehavior", "terminate"),
            ("MetadataOptions.HttpEndpoint", "enabled"),
            ("MetadataOptions.HttpTokens", "required"),
            ("MetadataOptions.HttpPutResponseHopLimit", "1"),
            ("MetadataOptions.InstanceMetadataTags", "enabled"),
        ]
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
        let mut add = |key: String, value: &str| params.push((key, value.to_owned()));

        add("UserData".to_owned(), &STANDARD.encode(self.user_data));
        if let Some(subnet) = self.subnet_id {
            add("SubnetId".to_owned(), subnet);
        }
        if self.spot {
            add("InstanceMarketOptions.MarketType".to_owned(), "spot");
            add(
                "InstanceMarketOptions.SpotOptions.SpotInstanceType".to_owned(),
                "one-time",
            );
            add(
                "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior".to_owned(),
                "terminate",
            );
        }
        if let Some((device, size)) = self.root_volume {
            let bdm = "BlockDeviceMapping.1";
            add(format!("{bdm}.DeviceName"), device);
            add(format!("{bdm}.Ebs.VolumeSize"), &size.to_string());
            add(format!("{bdm}.Ebs.VolumeType"), "gp3");
            add(format!("{bdm}.Ebs.Encrypted"), "true");
            add(format!("{bdm}.Ebs.DeleteOnTermination"), "true");
        }

        let mut resources = vec!["instance", "volume"];
        if self.spot {
            resources.push("spot-instances-request");
        }
        for (spec, resource) in resources.into_iter().enumerate() {
            let spec = format!("TagSpecification.{}", spec + 1);
            add(format!("{spec}.ResourceType"), resource);
            for (tag, (key, value)) in self.tags.iter().enumerate() {
                add(format!("{spec}.Tag.{}.Key", tag + 1), key);
                add(format!("{spec}.Tag.{}.Value", tag + 1), value);
            }
        }
        params
    }

    /// The unsigned request.
    #[must_use]
    pub fn request(&self, endpoint: &str) -> Request {
        query(endpoint, "RunInstances", VERSION, &self.params())
    }
}

/// The id of the instance `RunInstances` started.
///
/// # Errors
///
/// EC2's error, such as a capacity error from [`CAPACITY_ERRORS`], or a
/// malformed answer.
pub fn instance_id(response: &Response) -> Result<String, Error> {
    let doc = document(response)?;
    child_text(doc.root(), "instanceId")
        .map(str::to_owned)
        .ok_or_else(|| Error::malformed("no instanceId in RunInstances"))
}

/// The unsigned request to terminate an instance.
#[must_use]
pub fn terminate_instances(endpoint: &str, instance_id: &str) -> Request {
    query(
        endpoint,
        "TerminateInstances",
        VERSION,
        &[("InstanceId.1".to_owned(), instance_id.to_owned())],
    )
}

/// Checks a `TerminateInstances` answer.
///
/// # Errors
///
/// EC2's error. An instance that no longer exists is not one.
pub fn terminated(response: &Response) -> Result<(), Error> {
    match document(response) {
        Ok(_) => Ok(()),
        Err(err) if err.code == "InvalidInstanceID.NotFound" => Ok(()),
        Err(err) => Err(err),
    }
}

/// The launch templates of `tenant`'s stack, found by their
/// [`TAG_TENANT`] tag.
#[must_use]
pub fn describe_launch_templates(endpoint: &str, tenant: &str) -> Request {
    query(
        endpoint,
        "DescribeLaunchTemplates",
        VERSION,
        &[
            ("Filter.1.Name".to_owned(), format!("tag:{TAG_TENANT}")),
            ("Filter.1.Value.1".to_owned(), tenant.to_owned()),
        ],
    )
}

/// A launch template of a tenant's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTemplate {
    /// `lt-...`
    pub id: String,
    /// When it was made, ISO 8601; the newest stack wins.
    pub created: String,
    /// The subnets from its [`TAG_SUBNETS`] tag, in order.
    pub subnets: Vec<String>,
}

/// Reads the launch templates out of the answer.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn launch_templates(response: &Response) -> Result<Vec<LaunchTemplate>, Error> {
    let doc = document(response)?;
    let Some(set) = doc
        .descendants()
        .find(|node| node.tag_name().name() == "launchTemplates")
    else {
        return Ok(Vec::new());
    };
    set.children()
        .filter(|node| node.tag_name().name() == "item")
        .map(|item| {
            let field = |name: &str| {
                item.children()
                    .find(|child| child.tag_name().name() == name)
                    .and_then(|child| child.text())
            };
            let subnets = item
                .descendants()
                .filter(|node| node.tag_name().name() == "item")
                .find(|tag| child_text(*tag, "key") == Some(TAG_SUBNETS))
                .and_then(|tag| child_text(tag, "value"))
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|subnet| !subnet.is_empty())
                .map(str::to_owned)
                .collect();
            Ok(LaunchTemplate {
                id: field("launchTemplateId")
                    .ok_or_else(|| Error::malformed("launch template without an id"))?
                    .to_owned(),
                created: field("createTime").unwrap_or_default().to_owned(),
                subnets,
            })
        })
        .collect()
}

/// The subnets named `ids`.
#[must_use]
pub fn describe_subnets(endpoint: &str, ids: &[String]) -> Request {
    let params: Vec<(String, String)> = ids
        .iter()
        .enumerate()
        .map(|(at, id)| (format!("SubnetId.{}", at + 1), id.clone()))
        .collect();
    query(endpoint, "DescribeSubnets", VERSION, &params)
}

/// A subnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subnet {
    /// `subnet-...`
    pub id: String,
    /// Its availability zone.
    pub availability_zone: String,
}

/// Reads the subnets out of the answer. A subnet that does not exist
/// makes EC2 answer `InvalidSubnetID.NotFound`.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn subnets(response: &Response) -> Result<Vec<Subnet>, Error> {
    let doc = document(response)?;
    let Some(set) = doc
        .descendants()
        .find(|node| node.tag_name().name() == "subnetSet")
    else {
        return Ok(Vec::new());
    };
    Ok(set
        .children()
        .filter(|node| node.tag_name().name() == "item")
        .filter_map(|item| {
            Some(Subnet {
                id: child_text(item, "subnetId")?.to_owned(),
                availability_zone: child_text(item, "availabilityZone")
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunInstances<'static> {
        RunInstances {
            launch_template_id: "lt-0abc",
            image_id: "ami-0123",
            instance_type: "m7g.large",
            subnet_id: Some("subnet-a"),
            user_data: r#"{"broker":"https://b","ticket":"t"}"#,
            client_token: "hatchrun-7-ab12-0",
            tags: &[
                (TAG_TENANT, "acme1234"),
                (TAG_JOB_ID, "7"),
                (TAG_RUNNER_NAME, "hatchrun-7-ab12"),
            ],
            spot: false,
            root_volume: None,
        }
    }

    fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn launches_from_the_template_with_its_metadata_options() {
        let params = run().params();
        for (key, value) in [
            ("LaunchTemplate.LaunchTemplateId", "lt-0abc"),
            ("LaunchTemplate.Version", "$Default"),
            ("ImageId", "ami-0123"),
            ("InstanceType", "m7g.large"),
            ("SubnetId", "subnet-a"),
            ("MinCount", "1"),
            ("MaxCount", "1"),
            ("ClientToken", "hatchrun-7-ab12-0"),
            ("MetadataOptions.HttpTokens", "required"),
            ("MetadataOptions.HttpPutResponseHopLimit", "1"),
            (
                "UserData",
                "eyJicm9rZXIiOiJodHRwczovL2IiLCJ0aWNrZXQiOiJ0In0=",
            ),
        ] {
            assert_eq!(param(&params, key), Some(value), "{key}");
        }
        assert_eq!(param(&params, "InstanceMarketOptions.MarketType"), None);
        assert_eq!(param(&params, "BlockDeviceMapping.1.DeviceName"), None);
    }

    #[test]
    fn tags_the_instance_and_volumes_at_launch() {
        let params = run().params();
        for spec in ["TagSpecification.1", "TagSpecification.2"] {
            assert_eq!(
                param(&params, &format!("{spec}.Tag.1.Key")),
                Some("hatchrun:tenant")
            );
            assert_eq!(
                param(&params, &format!("{spec}.Tag.1.Value")),
                Some("acme1234")
            );
            assert_eq!(
                param(&params, &format!("{spec}.Tag.3.Value")),
                Some("hatchrun-7-ab12")
            );
        }
        assert_eq!(
            param(&params, "TagSpecification.1.ResourceType"),
            Some("instance")
        );
        assert_eq!(
            param(&params, "TagSpecification.2.ResourceType"),
            Some("volume")
        );
        assert_eq!(param(&params, "TagSpecification.3.ResourceType"), None);
    }

    #[test]
    fn asks_for_spot_and_a_root_volume() {
        let params = RunInstances {
            spot: true,
            subnet_id: None,
            root_volume: Some(("/dev/sda1", 100)),
            ..run()
        }
        .params();
        assert_eq!(
            param(&params, "InstanceMarketOptions.MarketType"),
            Some("spot")
        );
        assert_eq!(
            param(
                &params,
                "InstanceMarketOptions.SpotOptions.SpotInstanceType"
            ),
            Some("one-time")
        );
        assert_eq!(
            param(&params, "TagSpecification.3.ResourceType"),
            Some("spot-instances-request")
        );
        assert_eq!(
            param(&params, "BlockDeviceMapping.1.DeviceName"),
            Some("/dev/sda1")
        );
        assert_eq!(
            param(&params, "BlockDeviceMapping.1.Ebs.VolumeSize"),
            Some("100")
        );
        assert_eq!(
            param(&params, "BlockDeviceMapping.1.Ebs.Encrypted"),
            Some("true")
        );
        assert_eq!(param(&params, "SubnetId"), None);
    }

    #[test]
    fn request_is_a_query_call() {
        let request = run().request("https://ec2.eu-west-1.amazonaws.com/");
        let body = String::from_utf8(request.body).unwrap();
        assert!(body.starts_with("Action=RunInstances&Version=2016-11-15&"));
        assert!(body.contains("&LaunchTemplate.Version=%24Default&"));
    }

    #[test]
    fn reads_the_instance_id() {
        let response = Response {
            status: 200,
            body: br#"<?xml version="1.0" encoding="UTF-8"?>
<RunInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r</requestId>
  <reservationId>r-0abc</reservationId>
  <ownerId>123456789012</ownerId>
  <groupSet/>
  <instancesSet>
    <item>
      <instanceId>i-0123456789abcdef0</instanceId>
      <imageId>ami-0123</imageId>
      <instanceState><code>0</code><name>pending</name></instanceState>
    </item>
  </instancesSet>
</RunInstancesResponse>"#
                .to_vec(),
        };
        assert_eq!(instance_id(&response).unwrap(), "i-0123456789abcdef0");
    }

    #[test]
    fn capacity_errors_come_back_by_code() {
        let response = Response {
            status: 500,
            body: b"<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code>\
                    <Message>We currently do not have sufficient capacity</Message>\
                    </Error></Errors></Response>"
                .to_vec(),
        };
        let err = instance_id(&response).unwrap_err();
        assert!(CAPACITY_ERRORS.contains(&err.code.as_str()));
    }

    #[test]
    fn terminating_a_gone_instance_is_fine() {
        let request = terminate_instances("https://ec2/", "i-1");
        assert!(
            String::from_utf8(request.body)
                .unwrap()
                .ends_with("&InstanceId.1=i-1")
        );
        let gone = Response {
            status: 400,
            body: b"<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code>\
                    <Message>gone</Message></Error></Errors></Response>"
                .to_vec(),
        };
        assert!(terminated(&gone).is_ok());
        let denied = Response {
            status: 403,
            body: b"<Response><Errors><Error><Code>UnauthorizedOperation</Code>\
                    <Message>no</Message></Error></Errors></Response>"
                .to_vec(),
        };
        assert!(terminated(&denied).is_err());
    }

    #[test]
    fn finds_the_stacks_launch_template_and_subnets() {
        let request = describe_launch_templates("https://ec2.eu-west-1.amazonaws.com/", "abc12345");
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "Action=DescribeLaunchTemplates&Version=2016-11-15\
             &Filter.1.Name=tag%3Ahatchrun%3Atenant&Filter.1.Value.1=abc12345"
        );
        let answer = Response {
            status: 200,
            body: br#"<DescribeLaunchTemplatesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r</requestId>
  <launchTemplates>
    <item>
      <createTime>2026-10-14T10:00:00.000Z</createTime>
      <defaultVersionNumber>1</defaultVersionNumber>
      <launchTemplateId>lt-0abc</launchTemplateId>
      <launchTemplateName>LaunchTemplate-x</launchTemplateName>
      <tagSet>
        <item><key>hatchrun:tenant</key><value>abc12345</value></item>
        <item><key>hatchrun:subnets</key><value>subnet-a, subnet-b</value></item>
      </tagSet>
    </item>
  </launchTemplates>
</DescribeLaunchTemplatesResponse>"#
                .to_vec(),
        };
        assert_eq!(
            launch_templates(&answer).unwrap(),
            [LaunchTemplate {
                id: "lt-0abc".to_owned(),
                created: "2026-10-14T10:00:00.000Z".to_owned(),
                subnets: vec!["subnet-a".to_owned(), "subnet-b".to_owned()],
            }]
        );
        let none = Response {
            status: 200,
            body: b"<DescribeLaunchTemplatesResponse><launchTemplates/></DescribeLaunchTemplatesResponse>"
                .to_vec(),
        };
        assert_eq!(launch_templates(&none).unwrap(), []);

        let request = describe_subnets(
            "https://ec2.eu-west-1.amazonaws.com/",
            &["subnet-a".to_owned(), "subnet-b".to_owned()],
        );
        assert!(
            String::from_utf8(request.body)
                .unwrap()
                .ends_with("&SubnetId.1=subnet-a&SubnetId.2=subnet-b")
        );
        let answer = Response {
            status: 200,
            body: b"<DescribeSubnetsResponse><subnetSet><item><subnetId>subnet-a</subnetId>\
                    <vpcId>vpc-1</vpcId><availabilityZone>eu-west-1a</availabilityZone></item>\
                    </subnetSet></DescribeSubnetsResponse>"
                .to_vec(),
        };
        assert_eq!(
            subnets(&answer).unwrap(),
            [Subnet {
                id: "subnet-a".to_owned(),
                availability_zone: "eu-west-1a".to_owned(),
            }]
        );
        let missing = Response {
            status: 400,
            body: b"<Response><Errors><Error><Code>InvalidSubnetID.NotFound</Code><Message>The subnet ID 'subnet-b' does not exist</Message></Error></Errors></Response>".to_vec(),
        };
        assert_eq!(
            subnets(&missing).unwrap_err().code,
            "InvalidSubnetID.NotFound"
        );
    }
}
