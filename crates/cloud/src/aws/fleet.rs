// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starting a runner with EC2 Fleet: an instant fleet of one over every
//! instance type and subnet that may run it, where EC2 picks the
//! cheapest offering with capacity (`lowest-price` on-demand,
//! `price-capacity-optimized` Spot).
//!
//! A fleet takes no user data or block devices of its own, so each
//! launch first makes a version of the tenant's launch template with the
//! runner's image, ticket, root volume and tags, launches from that
//! version, and deletes it after. The template's `$Default` version is
//! never changed. A launch that dies before the delete leaves its
//! version behind; the reconciler lists the template's versions and
//! deletes those (see [`stale`]), since EC2 refuses new versions past
//! 10,000 per template.
//!
//! The instance is tagged through the version, never by the fleet. EC2
//! authorizes the instances a fleet starts as `RunInstances` calls of
//! the caller, with the version's metadata options, volumes, instance
//! type and tags as their condition keys, so the role's `RunInstances`
//! conditions bind a fleet launch as they bind a direct one. A fleet
//! that tags instances itself is instead authorized for
//! `ec2:CreateFleet` on `instance/*` with none of those keys.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::{Error, document, query};
use crate::http::{Request, Response};

/// EC2 API version.
const VERSION: &str = "2016-11-15";

/// A launch template version for one runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateVersion<'a> {
    /// The tenant's launch template, `lt-...`.
    pub launch_template_id: &'a str,
    /// The runner's name, which describes the version.
    pub runner_name: &'a str,
    /// The runner image.
    pub image_id: &'a str,
    /// User data, before base64.
    pub user_data: &'a str,
    /// Root volume device and size in GiB, to replace the template's
    /// volume size.
    pub root_volume: Option<(&'a str, u16)>,
    /// Tags for the runner's instance and volumes.
    pub tags: &'a [(&'a str, &'a str)],
    /// Makes a retried call idempotent.
    pub client_token: &'a str,
}

impl TemplateVersion<'_> {
    /// The unsigned `CreateLaunchTemplateVersion` request, based on the
    /// template's default version.
    #[must_use]
    pub fn request(&self, endpoint: &str) -> Request {
        let mut params: Vec<(String, String)> = Vec::new();
        let mut add = |key: &str, value: &str| params.push((key.to_owned(), value.to_owned()));
        add("LaunchTemplateId", self.launch_template_id);
        add("SourceVersion", "$Default");
        add("VersionDescription", self.runner_name);
        add("ClientToken", self.client_token);
        let data = "LaunchTemplateData";
        add(&format!("{data}.ImageId"), self.image_id);
        // A warm pool instance has none: its agent finds nothing to run.
        if !self.user_data.is_empty() {
            add(
                &format!("{data}.UserData"),
                &STANDARD.encode(self.user_data),
            );
        }
        add(
            &format!("{data}.InstanceInitiatedShutdownBehavior"),
            "terminate",
        );
        add(&format!("{data}.MetadataOptions.HttpEndpoint"), "enabled");
        add(&format!("{data}.MetadataOptions.HttpTokens"), "required");
        add(
            &format!("{data}.MetadataOptions.HttpPutResponseHopLimit"),
            "1",
        );
        add(
            &format!("{data}.MetadataOptions.InstanceMetadataTags"),
            "enabled",
        );
        if let Some((device, size)) = self.root_volume {
            let bdm = format!("{data}.BlockDeviceMapping.1");
            add(&format!("{bdm}.DeviceName"), device);
            add(&format!("{bdm}.Ebs.VolumeSize"), &size.to_string());
            add(&format!("{bdm}.Ebs.VolumeType"), "gp3");
            add(&format!("{bdm}.Ebs.Encrypted"), "true");
            add(&format!("{bdm}.Ebs.DeleteOnTermination"), "true");
        }
        for (spec, resource) in ["instance", "volume"].into_iter().enumerate() {
            let spec = format!("{data}.TagSpecification.{}", spec + 1);
            add(&format!("{spec}.ResourceType"), resource);
            for (at, (key, value)) in self.tags.iter().enumerate() {
                add(&format!("{spec}.Tag.{}.Key", at + 1), key);
                add(&format!("{spec}.Tag.{}.Value", at + 1), value);
            }
        }
        query(endpoint, "CreateLaunchTemplateVersion", VERSION, &params)
    }
}

/// The number of the version `CreateLaunchTemplateVersion` made.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn version_number(response: &Response) -> Result<u64, Error> {
    let doc = document(response)?;
    doc.descendants()
        .find(|node| node.has_tag_name("versionNumber"))
        .and_then(|node| node.text())
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| Error::malformed("no versionNumber in CreateLaunchTemplateVersion"))
}

/// The unsigned request to delete a version made for one runner.
#[must_use]
pub fn delete_version(endpoint: &str, launch_template_id: &str, version: u64) -> Request {
    delete_versions(endpoint, launch_template_id, &[version])
}

/// Versions one `DeleteLaunchTemplateVersions` call may delete.
pub const DELETE_BATCH: usize = 200;

/// The unsigned request to delete `versions`, at most [`DELETE_BATCH`].
#[must_use]
pub fn delete_versions(endpoint: &str, launch_template_id: &str, versions: &[u64]) -> Request {
    let mut params = vec![("LaunchTemplateId".to_owned(), launch_template_id.to_owned())];
    for (at, version) in versions.iter().enumerate() {
        params.push((
            format!("LaunchTemplateVersion.{}", at + 1),
            version.to_string(),
        ));
    }
    query(endpoint, "DeleteLaunchTemplateVersions", VERSION, &params)
}

/// What `DeleteLaunchTemplateVersions` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Deleted {
    /// The versions it deleted.
    pub deleted: Vec<u64>,
    /// The versions it did not, with EC2's code and message.
    pub failed: Vec<(u64, String)>,
}

/// Reads what `DeleteLaunchTemplateVersions` did.
///
/// # Errors
///
/// EC2's error for the call as a whole, or a malformed answer.
pub fn deleted(response: &Response) -> Result<Deleted, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let number = |item: roxmltree::Node<'_, '_>| {
        child_text(item, "versionNumber").and_then(|text| text.parse().ok())
    };
    let mut done = Deleted::default();
    for item in items(root, "successfullyDeletedLaunchTemplateVersionSet") {
        done.deleted.extend(number(item));
    }
    for item in items(root, "unsuccessfullyDeletedLaunchTemplateVersionSet") {
        let error = child(item, "responseError");
        let reason = format!(
            "{}: {}",
            error
                .and_then(|n| child_text(n, "code"))
                .unwrap_or_default(),
            error
                .and_then(|n| child_text(n, "message"))
                .unwrap_or_default()
        );
        done.failed.push((number(item).unwrap_or_default(), reason));
    }
    Ok(done)
}

/// The unsigned request for a page of the template's versions other
/// than `$Default`. Needs `ec2:DescribeLaunchTemplateVersions`, which
/// the role has from connect stack version 5.
#[must_use]
pub fn describe_versions(endpoint: &str, launch_template_id: &str, next: Option<&str>) -> Request {
    let mut params = vec![
        ("LaunchTemplateId".to_owned(), launch_template_id.to_owned()),
        ("Filter.1.Name".to_owned(), "is-default-version".to_owned()),
        ("Filter.1.Value.1".to_owned(), "false".to_owned()),
        ("MaxResults".to_owned(), "200".to_owned()),
    ];
    if let Some(next) = next {
        params.push(("NextToken".to_owned(), next.to_owned()));
    }
    query(endpoint, "DescribeLaunchTemplateVersions", VERSION, &params)
}

/// A version of a launch template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Its number.
    pub number: u64,
    /// When it was made, as EC2 writes it.
    pub created: String,
    /// Its description: the runner's name for a version made by a
    /// launch.
    pub description: String,
    /// Whether it is the template's `$Default`.
    pub default: bool,
}

/// Reads a page of `DescribeLaunchTemplateVersions`, and the token of
/// the next one.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn versions(response: &Response) -> Result<(Vec<Version>, Option<String>), Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let mut found = Vec::new();
    for item in items(root, "launchTemplateVersionSet") {
        let number = child_text(item, "versionNumber")
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| Error::malformed("a launch template version with no number"))?;
        found.push(Version {
            number,
            created: child_text(item, "createTime")
                .unwrap_or_default()
                .to_owned(),
            description: child_text(item, "versionDescription")
                .unwrap_or_default()
                .to_owned(),
            default: child_text(item, "defaultVersion") == Some("true"),
        });
    }
    let next = child_text(root, "nextToken")
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Ok((found, next))
}

/// The versions among `found` that launches left behind: made for a
/// runner (see `launch::runner_name`) before `before`, Unix
/// milliseconds, and not `$Default`. Versions the stack made, and ones
/// with a time that does not read, are kept.
#[must_use]
pub fn stale(found: &[Version], before: u64) -> Vec<u64> {
    found
        .iter()
        .filter(|version| {
            !version.default
                && version.description.starts_with("hatchrun-")
                && crate::reconcile::time::parse(&version.created).is_some_and(|at| at < before)
        })
        .map(|version| version.number)
        .collect()
}

/// An instance type in a subnet the fleet may launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// As in `m8g.large`.
    pub instance_type: String,
    /// `subnet-...`, or the template's network when `None`.
    pub subnet_id: Option<String>,
}

/// An instant fleet of one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fleet<'a> {
    /// The tenant's launch template, `lt-...`.
    pub launch_template_id: &'a str,
    /// The version made for this runner.
    pub version: u64,
    /// What it may launch; at most 300.
    pub overrides: &'a [Override],
    /// Spot rather than on-demand.
    pub spot: bool,
    /// Makes a retried call idempotent.
    pub client_token: &'a str,
}

impl Fleet<'_> {
    /// The unsigned `CreateFleet` request.
    #[must_use]
    pub fn request(&self, endpoint: &str) -> Request {
        let mut params: Vec<(String, String)> = Vec::new();
        let mut add = |key: &str, value: &str| params.push((key.to_owned(), value.to_owned()));
        add("Type", "instant");
        add("ClientToken", self.client_token);
        let config = "LaunchTemplateConfigs.1";
        add(
            &format!("{config}.LaunchTemplateSpecification.LaunchTemplateId"),
            self.launch_template_id,
        );
        add(
            &format!("{config}.LaunchTemplateSpecification.Version"),
            &self.version.to_string(),
        );
        for (at, choice) in self.overrides.iter().enumerate() {
            let one = format!("{config}.Overrides.{}", at + 1);
            add(&format!("{one}.InstanceType"), &choice.instance_type);
            if let Some(subnet) = &choice.subnet_id {
                add(&format!("{one}.SubnetId"), subnet);
            }
        }
        let capacity = if self.spot { "spot" } else { "on-demand" };
        add("TargetCapacitySpecification.TotalTargetCapacity", "1");
        add(
            "TargetCapacitySpecification.DefaultTargetCapacityType",
            capacity,
        );
        if self.spot {
            add("TargetCapacitySpecification.SpotTargetCapacity", "1");
            add("SpotOptions.AllocationStrategy", "price-capacity-optimized");
            add("SpotOptions.InstanceInterruptionBehavior", "terminate");
        } else {
            add("TargetCapacitySpecification.OnDemandTargetCapacity", "1");
            add("OnDemandOptions.AllocationStrategy", "lowest-price");
        }
        query(endpoint, "CreateFleet", VERSION, &params)
    }
}

/// An override the fleet could not launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// As in `m8g.large`.
    pub instance_type: String,
    /// The override's subnet.
    pub subnet_id: Option<String>,
    /// As in `InsufficientInstanceCapacity`.
    pub code: String,
    /// EC2's message.
    pub message: String,
}

/// What an instant fleet did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launched {
    /// The instance, its type and subnet, if it started one.
    pub instance: Option<(String, String, Option<String>)>,
    /// The overrides it could not launch, and why.
    pub refused: Vec<Refused>,
}

/// Reads what `CreateFleet` did.
///
/// # Errors
///
/// EC2's error for the call as a whole, or a malformed answer.
pub fn launched(response: &Response) -> Result<Launched, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let overrides = |item: roxmltree::Node<'_, '_>| {
        let chosen = child(item, "launchTemplateAndOverrides").and_then(|n| child(n, "overrides"));
        (
            chosen
                .and_then(|n| child_text(n, "instanceType"))
                .map(str::to_owned),
            chosen
                .and_then(|n| child_text(n, "subnetId"))
                .map(str::to_owned),
        )
    };
    let mut done = Launched::default();
    for item in items(root, "errorSet") {
        let (instance_type, subnet_id) = overrides(item);
        done.refused.push(Refused {
            instance_type: instance_type.unwrap_or_default(),
            subnet_id,
            code: child_text(item, "errorCode").unwrap_or_default().to_owned(),
            message: child_text(item, "errorMessage")
                .unwrap_or_default()
                .to_owned(),
        });
    }
    for item in items(root, "fleetInstanceSet") {
        let Some(id) = items(item, "instanceIds").find_map(|id| id.text()) else {
            continue;
        };
        let (chosen_type, subnet_id) = overrides(item);
        let instance_type = child_text(item, "instanceType")
            .map(str::to_owned)
            .or(chosen_type)
            .unwrap_or_default();
        done.instance = Some((id.to_owned(), instance_type, subnet_id));
        break;
    }
    Ok(done)
}

fn items<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    set: &str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    child(node, set)
        .into_iter()
        .flat_map(|set| set.children().filter(|item| item.has_tag_name("item")))
}

fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|child| child.tag_name().name() == name)
}

fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|child| child.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(request: &Request) -> String {
        String::from_utf8(request.body.clone()).unwrap()
    }

    #[test]
    fn versions_carry_the_runner() {
        let version = TemplateVersion {
            launch_template_id: "lt-1",
            runner_name: "hatchrun-7-ab",
            image_id: "ami-1",
            user_data: "{}",
            root_volume: Some(("/dev/sda1", 100)),
            tags: &[("hatchrun:tenant", "acme")],
            client_token: "hatchrun-7-ab-v",
        };
        let sent = body(&version.request("https://ec2/"));
        assert!(sent.starts_with(
            "Action=CreateLaunchTemplateVersion&Version=2016-11-15&LaunchTemplateId=lt-1\
             &SourceVersion=%24Default&VersionDescription=hatchrun-7-ab"
        ));
        for part in [
            "LaunchTemplateData.ImageId=ami-1",
            "LaunchTemplateData.UserData=e30%3D",
            "LaunchTemplateData.MetadataOptions.HttpTokens=required",
            "LaunchTemplateData.BlockDeviceMapping.1.Ebs.VolumeSize=100",
            "LaunchTemplateData.BlockDeviceMapping.1.Ebs.Encrypted=true",
            "LaunchTemplateData.TagSpecification.1.ResourceType=instance",
            "LaunchTemplateData.TagSpecification.1.Tag.1.Key=hatchrun%3Atenant",
            "LaunchTemplateData.TagSpecification.2.ResourceType=volume",
            "LaunchTemplateData.TagSpecification.2.Tag.1.Value=acme",
        ] {
            assert!(sent.contains(part), "{part} in {sent}");
        }
        let created = Response {
            headers: Vec::new(),
            status: 200,
            body: b"<CreateLaunchTemplateVersionResponse><launchTemplateVersion>\
                <launchTemplateId>lt-1</launchTemplateId><versionNumber>12</versionNumber>\
                </launchTemplateVersion></CreateLaunchTemplateVersionResponse>"
                .to_vec(),
        };
        assert_eq!(version_number(&created), Ok(12));
        assert_eq!(
            body(&delete_version("https://ec2/", "lt-1", 12)),
            "Action=DeleteLaunchTemplateVersions&Version=2016-11-15\
             &LaunchTemplateId=lt-1&LaunchTemplateVersion.1=12"
        );
    }

    #[test]
    fn lists_and_deletes_left_versions() {
        let first = body(&describe_versions("https://ec2/", "lt-1", None));
        assert_eq!(
            first,
            "Action=DescribeLaunchTemplateVersions&Version=2016-11-15&LaunchTemplateId=lt-1\
             &Filter.1.Name=is-default-version&Filter.1.Value.1=false&MaxResults=200"
        );
        let next = body(&describe_versions("https://ec2/", "lt-1", Some("t/2")));
        assert!(next.ends_with("&MaxResults=200&NextToken=t%2F2"), "{next}");

        let page = Response { headers: Vec::new(),
            status: 200,
            body: br"<DescribeLaunchTemplateVersionsResponse>
  <launchTemplateVersionSet>
    <item>
      <createTime>2026-10-14T09:00:00.000Z</createTime>
      <defaultVersion>false</defaultVersion>
      <launchTemplateData><blockDeviceMappingSet><item><deviceName>/dev/sda1</deviceName></item></blockDeviceMappingSet></launchTemplateData>
      <launchTemplateId>lt-1</launchTemplateId>
      <versionDescription>hatchrun-7-ab</versionDescription>
      <versionNumber>12</versionNumber>
    </item>
    <item>
      <createTime>2026-10-01T00:00:00.000Z</createTime>
      <defaultVersion>true</defaultVersion>
      <versionNumber>3</versionNumber>
    </item>
  </launchTemplateVersionSet>
  <nextToken>t2</nextToken>
</DescribeLaunchTemplateVersionsResponse>"
                .to_vec(),
        };
        let (found, token) = versions(&page).unwrap();
        assert_eq!(token.as_deref(), Some("t2"));
        assert_eq!(
            found,
            [
                Version {
                    number: 12,
                    created: "2026-10-14T09:00:00.000Z".to_owned(),
                    description: "hatchrun-7-ab".to_owned(),
                    default: false,
                },
                Version {
                    number: 3,
                    created: "2026-10-01T00:00:00.000Z".to_owned(),
                    description: String::new(),
                    default: true,
                },
            ]
        );
        let last = Response {
            headers: Vec::new(),
            status: 200,
            body: b"<DescribeLaunchTemplateVersionsResponse><launchTemplateVersionSet/>\
                <nextToken/></DescribeLaunchTemplateVersionsResponse>"
                .to_vec(),
        };
        assert_eq!(versions(&last), Ok((Vec::new(), None)));
        let denied = Response {
            headers: Vec::new(),
            status: 403,
            body: b"<Response><Errors><Error><Code>UnauthorizedOperation</Code>\
                <Message>no</Message></Error></Errors></Response>"
                .to_vec(),
        };
        assert_eq!(versions(&denied).unwrap_err().code, "UnauthorizedOperation");

        assert_eq!(
            body(&delete_versions("https://ec2/", "lt-1", &[4, 9])),
            "Action=DeleteLaunchTemplateVersions&Version=2016-11-15\
             &LaunchTemplateId=lt-1&LaunchTemplateVersion.1=4&LaunchTemplateVersion.2=9"
        );
        let answer = Response { headers: Vec::new(),
            status: 200,
            body: br"<DeleteLaunchTemplateVersionsResponse>
  <successfullyDeletedLaunchTemplateVersionSet>
    <item><launchTemplateId>lt-1</launchTemplateId><versionNumber>4</versionNumber></item>
  </successfullyDeletedLaunchTemplateVersionSet>
  <unsuccessfullyDeletedLaunchTemplateVersionSet>
    <item>
      <launchTemplateId>lt-1</launchTemplateId>
      <responseError><code>launchTemplateVersionDoesNotExist</code><message>gone</message></responseError>
      <versionNumber>9</versionNumber>
    </item>
  </unsuccessfullyDeletedLaunchTemplateVersionSet>
</DeleteLaunchTemplateVersionsResponse>"
                .to_vec(),
        };
        assert_eq!(
            deleted(&answer),
            Ok(Deleted {
                deleted: vec![4],
                failed: vec![(9, "launchTemplateVersionDoesNotExist: gone".to_owned())],
            })
        );
    }

    #[test]
    fn deletes_only_old_runner_versions() {
        let version = |number, created: &str, description: &str, default| Version {
            number,
            created: created.to_owned(),
            description: description.to_owned(),
            default,
        };
        let found = [
            // Left by a launch that died.
            version(5, "2026-10-14T08:00:00.000Z", "hatchrun-7-ab", false),
            // Made by a launch that may still be running.
            version(6, "2026-10-14T10:00:00.000Z", "hatchrun-8-cd", false),
            // Exactly at the cutoff: kept.
            version(7, "2026-10-14T09:00:00.000Z", "hatchrun-9-ef", false),
            // The template's own versions.
            version(2, "2026-10-01T00:00:00.000Z", "", false),
            version(3, "2026-10-01T00:00:00.000Z", "hatchrun-1-00", true),
            version(8, "yesterday", "hatchrun-10-aa", false),
        ];
        let before = crate::reconcile::time::parse("2026-10-14T09:00:00Z").unwrap();
        assert_eq!(stale(&found, before), [5]);
        assert_eq!(stale(&found, 0), Vec::<u64>::new());
    }

    #[test]
    fn fleets_pick_by_price() {
        let overrides = [
            Override {
                instance_type: "m8g.large".to_owned(),
                subnet_id: Some("subnet-a".to_owned()),
            },
            Override {
                instance_type: "c8g.xlarge".to_owned(),
                subnet_id: None,
            },
        ];
        let fleet = |spot| Fleet {
            launch_template_id: "lt-1",
            version: 12,
            overrides: &overrides,
            spot,
            client_token: "r-0",
        };
        let on_demand = body(&fleet(false).request("https://ec2/"));
        for part in [
            "Type=instant",
            "LaunchTemplateConfigs.1.LaunchTemplateSpecification.Version=12",
            "LaunchTemplateConfigs.1.Overrides.1.InstanceType=m8g.large",
            "LaunchTemplateConfigs.1.Overrides.1.SubnetId=subnet-a",
            "LaunchTemplateConfigs.1.Overrides.2.InstanceType=c8g.xlarge",
            "DefaultTargetCapacityType=on-demand",
            "OnDemandOptions.AllocationStrategy=lowest-price",
        ] {
            assert!(on_demand.contains(part), "{part} in {on_demand}");
        }
        assert!(!on_demand.contains("Overrides.2.SubnetId"));
        // Tags travel in the template version, where IAM sees them.
        assert!(!on_demand.contains("TagSpecification"));
        let spot = body(&fleet(true).request("https://ec2/"));
        assert!(spot.contains("SpotOptions.AllocationStrategy=price-capacity-optimized"));
        assert!(spot.contains("DefaultTargetCapacityType=spot"));
    }

    #[test]
    fn reads_the_instance_and_refusals() {
        let answer = Response { headers: Vec::new(),
            status: 200,
            body: br"<CreateFleetResponse>
  <fleetId>fleet-1</fleetId>
  <errorSet><item>
    <launchTemplateAndOverrides>
      <launchTemplateSpecification><launchTemplateId>lt-1</launchTemplateId></launchTemplateSpecification>
      <overrides><instanceType>m8g.large</instanceType><subnetId>subnet-a</subnetId></overrides>
    </launchTemplateAndOverrides>
    <lifecycle>spot</lifecycle>
    <errorCode>InsufficientInstanceCapacity</errorCode>
    <errorMessage>none</errorMessage>
  </item></errorSet>
  <fleetInstanceSet><item>
    <launchTemplateAndOverrides>
      <overrides><instanceType>c8g.xlarge</instanceType><subnetId>subnet-b</subnetId></overrides>
    </launchTemplateAndOverrides>
    <lifecycle>spot</lifecycle>
    <instanceIds><item>i-0abc</item></instanceIds>
    <instanceType>c8g.xlarge</instanceType>
  </item></fleetInstanceSet>
</CreateFleetResponse>"
                .to_vec(),
        };
        let done = launched(&answer).unwrap();
        assert_eq!(
            done.instance,
            Some((
                "i-0abc".to_owned(),
                "c8g.xlarge".to_owned(),
                Some("subnet-b".to_owned())
            ))
        );
        assert_eq!(
            done.refused,
            [Refused {
                instance_type: "m8g.large".to_owned(),
                subnet_id: Some("subnet-a".to_owned()),
                code: "InsufficientInstanceCapacity".to_owned(),
                message: "none".to_owned(),
            }]
        );
        let none = Response { headers: Vec::new(),
            status: 200,
            body: b"<CreateFleetResponse><fleetId>f</fleetId><fleetInstanceSet/></CreateFleetResponse>"
                .to_vec(),
        };
        assert_eq!(launched(&none), Ok(Launched::default()));
    }

    #[test]
    fn a_version_without_user_data_sends_none() {
        let version = TemplateVersion {
            launch_template_id: "lt-1",
            runner_name: "ci-pool-x",
            image_id: "ami-1",
            user_data: "",
            root_volume: None,
            tags: &[],
            client_token: "t",
        };
        assert!(!body(&version.request("https://ec2/")).contains("UserData"));
    }
}
