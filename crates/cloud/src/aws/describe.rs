// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a region offers: `DescribeInstanceTypes`,
//! `DescribeInstanceTypeOfferings` by zone id and
//! `DescribeSpotPriceHistory`, a page at a time, for the instance type
//! catalog (see [`crate::instance_types`]). The tenant role may call
//! all three (deploy/cloudformation/connect.yaml).

use super::{Error, document, query};
use crate::http::{Request, Response};
use crate::labels::Arch;

/// EC2 API version.
const VERSION: &str = "2016-11-15";

fn params(pairs: &[(&str, &str)], next_token: Option<&str>) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    if let Some(token) = next_token {
        all.push(("NextToken".to_owned(), token.to_owned()));
    }
    all
}

/// Current generation instance types that run amd64 or arm64, a page
/// at a time.
#[must_use]
pub fn instance_types(endpoint: &str, next_token: Option<&str>) -> Request {
    query(
        endpoint,
        "DescribeInstanceTypes",
        VERSION,
        &params(
            &[
                ("Filter.1.Name", "current-generation"),
                ("Filter.1.Value.1", "true"),
                ("Filter.2.Name", "processor-info.supported-architecture"),
                ("Filter.2.Value.1", "x86_64"),
                ("Filter.2.Value.2", "arm64"),
                ("MaxResults", "100"),
            ],
            next_token,
        ),
    )
}

/// A page of results and the next page's token.
pub type Page<T> = (Vec<T>, Option<String>);

/// An instance type as EC2 describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct InstanceType {
    /// As in `m8g.xlarge`.
    pub name: String,
    /// What it runs; types that run neither amd64 nor arm64 are left out.
    pub arch: Arch,
    /// Default vCPUs.
    pub vcpus: u16,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// GPUs.
    pub gpus: u16,
    /// Inference accelerators, Neuron devices or FPGAs.
    pub accelerators: bool,
    /// A whole host.
    pub bare_metal: bool,
    /// Burstable, such as `t4g`.
    pub burstable: bool,
    /// Has instance store volumes.
    pub local_storage: bool,
    /// Can run as Spot.
    pub spot: bool,
}

/// Reads a page of instance types and the next page's token.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn described_types(response: &Response) -> Result<Page<InstanceType>, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let mut found = Vec::new();
    for item in items(root, "instanceTypeSet") {
        let name = child_text(item, "instanceType")
            .ok_or_else(|| Error::malformed("instance type without a name"))?;
        let archs: Vec<&str> = child(item, "processorInfo")
            .map(|info| {
                items(info, "supportedArchitectures")
                    .filter_map(|n| n.text())
                    .collect()
            })
            .unwrap_or_default();
        let arch = if archs.contains(&"arm64") {
            Arch::Arm64
        } else if archs.contains(&"x86_64") {
            Arch::Amd64
        } else {
            continue;
        };
        let number = |section: &str, field: &str| {
            child(item, section)
                .and_then(|node| child_text(node, field))
                .and_then(|text| text.parse::<u32>().ok())
        };
        let flag = |field: &str| child_text(item, field) == Some("true");
        let vcpus = number("vCpuInfo", "defaultVCpus")
            .and_then(|vcpus| u16::try_from(vcpus).ok())
            .ok_or_else(|| Error::malformed(&format!("{name} without vCPUs")))?;
        let memory_mib = number("memoryInfo", "sizeInMiB")
            .ok_or_else(|| Error::malformed(&format!("{name} without memory")))?;
        let gpus = child(item, "gpuInfo").map_or(0, |info| {
            items(info, "gpus")
                .filter_map(|gpu| child_text(gpu, "count")?.parse::<u16>().ok())
                .sum()
        });
        let accelerators = ["inferenceAcceleratorInfo", "neuronInfo", "fpgaInfo"]
            .iter()
            .any(|section| child(item, section).is_some());
        found.push(InstanceType {
            name: name.to_owned(),
            arch,
            vcpus,
            memory_mib,
            gpus,
            accelerators,
            bare_metal: flag("bareMetal"),
            burstable: flag("burstablePerformanceSupported"),
            local_storage: flag("instanceStorageSupported"),
            spot: items(item, "supportedUsageClasses").any(|n| n.text() == Some("spot")),
        });
    }
    Ok((found, next_token(root)))
}

/// Which instance types each availability zone offers, by zone id
/// (which names the same zone in every account), a page at a time.
#[must_use]
pub fn offerings(endpoint: &str, next_token: Option<&str>) -> Request {
    query(
        endpoint,
        "DescribeInstanceTypeOfferings",
        VERSION,
        &params(
            &[
                ("LocationType", "availability-zone-id"),
                ("MaxResults", "1000"),
            ],
            next_token,
        ),
    )
}

/// Reads a page of (instance type, zone id) and the next page's token.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn described_offerings(response: &Response) -> Result<Page<(String, String)>, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let found = items(root, "instanceTypeOfferingSet")
        .filter_map(|item| {
            Some((
                child_text(item, "instanceType")?.to_owned(),
                child_text(item, "location")?.to_owned(),
            ))
        })
        .collect();
    Ok((found, next_token(root)))
}

/// The Linux Spot prices in effect at `now` (RFC 3339), one per
/// instance type and zone, a page at a time.
#[must_use]
pub fn spot_prices(endpoint: &str, now: &str, next_token: Option<&str>) -> Request {
    query(
        endpoint,
        "DescribeSpotPriceHistory",
        VERSION,
        &params(
            &[
                ("ProductDescription.1", "Linux/UNIX"),
                ("StartTime", now),
                ("EndTime", now),
                ("MaxResults", "1000"),
            ],
            next_token,
        ),
    )
}

/// Reads a page of (instance type, price per hour in millionths of a
/// US dollar) and the next page's token.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn described_spot_prices(response: &Response) -> Result<Page<(String, u64)>, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let found = items(root, "spotPriceHistorySet")
        .filter_map(|item| {
            Some((
                child_text(item, "instanceType")?.to_owned(),
                micros(child_text(item, "spotPrice")?)?,
            ))
        })
        .collect();
    Ok((found, next_token(root)))
}

/// A price in US dollars, as in `0.034500`, in millionths, rounded.
#[must_use]
pub fn micros(dollars: &str) -> Option<u64> {
    let (whole, fraction) = dollars
        .trim()
        .split_once('.')
        .unwrap_or((dollars.trim(), ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits: Vec<u64> = fraction
        .bytes()
        .map(|byte| u64::from(byte - b'0'))
        .collect();
    let mut millionths = digits
        .iter()
        .chain(std::iter::repeat(&0))
        .take(6)
        .fold(0, |sum, digit| sum * 10 + digit);
    if digits.get(6).is_some_and(|digit| *digit >= 5) {
        millionths += 1;
    }
    whole.checked_mul(1_000_000)?.checked_add(millionths)
}

/// The `item`s of the direct child `set` of `node`.
fn items<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    set: &str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    child(node, set)
        .into_iter()
        .flat_map(|set| set.children().filter(|item| item.has_tag_name("item")))
}

fn next_token(root: roxmltree::Node<'_, '_>) -> Option<String> {
    child_text(root, "nextToken")
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
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

    fn ok(body: &str) -> Response {
        Response {
            headers: Vec::new(),
            status: 200,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn asks_for_current_amd64_and_arm64_types() {
        let request = instance_types("https://ec2/", Some("t/1"));
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "Action=DescribeInstanceTypes&Version=2016-11-15\
             &Filter.1.Name=current-generation&Filter.1.Value.1=true\
             &Filter.2.Name=processor-info.supported-architecture\
             &Filter.2.Value.1=x86_64&Filter.2.Value.2=arm64&MaxResults=100&NextToken=t%2F1"
        );
    }

    #[test]
    fn reads_instance_types() {
        let page = ok(
            r#"<DescribeInstanceTypesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r</requestId>
  <instanceTypeSet>
    <item>
      <instanceType>m8g.xlarge</instanceType>
      <currentGeneration>true</currentGeneration>
      <bareMetal>false</bareMetal>
      <burstablePerformanceSupported>false</burstablePerformanceSupported>
      <instanceStorageSupported>false</instanceStorageSupported>
      <supportedUsageClasses><item>on-demand</item><item>spot</item></supportedUsageClasses>
      <processorInfo><supportedArchitectures><item>arm64</item></supportedArchitectures></processorInfo>
      <vCpuInfo><defaultVCpus>4</defaultVCpus><defaultCores>4</defaultCores></vCpuInfo>
      <memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo>
    </item>
    <item>
      <instanceType>g6.xlarge</instanceType>
      <bareMetal>false</bareMetal>
      <instanceStorageSupported>true</instanceStorageSupported>
      <supportedUsageClasses><item>on-demand</item></supportedUsageClasses>
      <processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo>
      <vCpuInfo><defaultVCpus>4</defaultVCpus></vCpuInfo>
      <memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo>
      <gpuInfo><gpus><item><count>1</count><name>L4</name></item></gpus></gpuInfo>
    </item>
    <item>
      <instanceType>inf2.xlarge</instanceType>
      <processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo>
      <vCpuInfo><defaultVCpus>4</defaultVCpus></vCpuInfo>
      <memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo>
      <neuronInfo><neuronDevices><item><count>1</count></item></neuronDevices></neuronInfo>
    </item>
    <item>
      <instanceType>mac2.metal</instanceType>
      <processorInfo><supportedArchitectures><item>arm64_mac</item></supportedArchitectures></processorInfo>
      <vCpuInfo><defaultVCpus>12</defaultVCpus></vCpuInfo>
      <memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo>
    </item>
  </instanceTypeSet>
  <nextToken>more</nextToken>
</DescribeInstanceTypesResponse>"#,
        );
        let (types, next) = described_types(&page).unwrap();
        assert_eq!(next.as_deref(), Some("more"));
        assert_eq!(
            types[0],
            InstanceType {
                name: "m8g.xlarge".to_owned(),
                arch: Arch::Arm64,
                vcpus: 4,
                memory_mib: 16384,
                gpus: 0,
                accelerators: false,
                bare_metal: false,
                burstable: false,
                local_storage: false,
                spot: true,
            }
        );
        assert_eq!(
            (types[1].gpus, types[1].spot, types[1].local_storage),
            (1, false, true)
        );
        assert!(types[2].accelerators);
        assert_eq!(types.len(), 3, "mac2.metal runs neither");
    }

    #[test]
    fn reads_offerings_and_spot_prices() {
        let offered = ok(
            "<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet>\
            <item><instanceType>m8g.large</instanceType><locationType>availability-zone-id</locationType>\
            <location>aps1-az1</location></item></instanceTypeOfferingSet>\
            <nextToken/></DescribeInstanceTypeOfferingsResponse>",
        );
        assert_eq!(
            described_offerings(&offered).unwrap(),
            (vec![("m8g.large".to_owned(), "aps1-az1".to_owned())], None)
        );
        let spot = ok("<DescribeSpotPriceHistoryResponse><spotPriceHistorySet>\
            <item><instanceType>m8g.large</instanceType><productDescription>Linux/UNIX</productDescription>\
            <spotPrice>0.034500</spotPrice><timestamp>2026-10-14T00:00:00.000Z</timestamp>\
            <availabilityZone>ap-south-1a</availabilityZone></item></spotPriceHistorySet>\
            <nextToken>n</nextToken></DescribeSpotPriceHistoryResponse>");
        assert_eq!(
            described_spot_prices(&spot).unwrap(),
            (vec![("m8g.large".to_owned(), 34_500)], Some("n".to_owned()))
        );
    }

    #[test]
    fn prices_in_millionths() {
        assert_eq!(micros("0.034500"), Some(34_500));
        assert_eq!(micros("3.3818400000"), Some(3_381_840));
        assert_eq!(micros("0.00000051"), Some(1));
        assert_eq!(micros("12"), Some(12_000_000));
        assert_eq!(micros(".5"), Some(500_000));
        assert_eq!(micros("1.2a"), None);
        assert_eq!(micros(""), None);
    }

    #[test]
    fn refuses_errors() {
        let denied = Response {
            headers: Vec::new(),
            status: 403,
            body: b"<Response><Errors><Error><Code>UnauthorizedOperation</Code>\
                <Message>no</Message></Error></Errors></Response>"
                .to_vec(),
        };
        assert_eq!(
            described_types(&denied).unwrap_err().code,
            "UnauthorizedOperation"
        );
    }
}
