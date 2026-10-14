// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The instance type catalog: every current generation amd64 and arm64
//! instance type a region offers, with the zones that offer it, its
//! Linux on-demand price and its lowest Spot price. The reconciler
//! refreshes a region's catalog once a day (see [`store`]) through a
//! tenant role in that region, so types AWS adds show up the next day
//! without a release.
//!
//! Types are described in Karpenter's vocabulary: `m8g.xlarge` is
//! family `m8g`, category `m`, generation 8, size `xlarge`.
//!
//! Everything here but [`store`] runs and tests without the Workers
//! runtime.

pub(crate) mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

use serde::Serialize;

use crate::aws::describe;
use crate::labels::Arch;

/// How long a region's catalog stays fresh, in milliseconds.
pub const FRESH_FOR: u64 = 24 * 60 * 60 * 1000;
/// How long after a failed refresh the next one is tried.
pub const RETRY_AFTER: u64 = 60 * 60 * 1000;

/// The location names AWS's price lists use, by region.
const PRICE_LOCATIONS: [(&str, &str); 17] = [
    ("us-east-1", "US East (N. Virginia)"),
    ("us-east-2", "US East (Ohio)"),
    ("us-west-1", "US West (N. California)"),
    ("us-west-2", "US West (Oregon)"),
    ("ca-central-1", "Canada (Central)"),
    ("sa-east-1", "South America (Sao Paulo)"),
    ("eu-west-1", "EU (Ireland)"),
    ("eu-west-2", "EU (London)"),
    ("eu-west-3", "EU (Paris)"),
    ("eu-central-1", "EU (Frankfurt)"),
    ("eu-north-1", "EU (Stockholm)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-southeast-1", "Asia Pacific (Singapore)"),
    ("ap-southeast-2", "Asia Pacific (Sydney)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("ap-northeast-2", "Asia Pacific (Seoul)"),
    ("ap-northeast-3", "Asia Pacific (Osaka)"),
];

/// The Linux on-demand price list behind aws.amazon.com/ec2/pricing for
/// `region`, a few hundred KB gzipped.
#[must_use]
pub fn price_list_url(region: &str) -> Option<String> {
    let (_, location) = PRICE_LOCATIONS.iter().find(|(id, _)| *id == region)?;
    let location: String = location
        .bytes()
        .map(|byte| match byte {
            b' ' => "%20".to_owned(),
            b'(' => "%28".to_owned(),
            b')' => "%29".to_owned(),
            _ => char::from(byte).to_string(),
        })
        .collect();
    Some(format!(
        "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/\
         ec2-ondemand-without-sec-sel/{location}/Linux/index.json"
    ))
}

/// Linux on-demand prices per hour in millionths of a US dollar, by
/// instance type, from a price list. The list is served gzipped with
/// no `Content-Encoding`, so a gzipped body is inflated here.
///
/// # Errors
///
/// When the body is neither JSON nor gzipped JSON of that shape.
pub fn on_demand_prices(body: &[u8]) -> Result<HashMap<String, u64>, String> {
    let mut inflated = Vec::new();
    let json = if body.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(body)
            .read_to_end(&mut inflated)
            .map_err(|err| format!("price list: {err}"))?;
        &inflated[..]
    } else {
        body
    };
    let list: serde_json::Value =
        serde_json::from_slice(json).map_err(|err| format!("price list: {err}"))?;
    let regions = list["regions"]
        .as_object()
        .ok_or("price list: no regions")?;
    let mut prices = HashMap::new();
    for entries in regions.values().filter_map(serde_json::Value::as_object) {
        for entry in entries.values() {
            let field = |name: &str| entry[name].as_str();
            if field("Pre Installed S/W").is_some_and(|value| value != "NA")
                || field("License Model").is_some_and(|value| value != "No License required")
            {
                continue;
            }
            let (Some(name), Some(price)) = (
                field("Instance Type"),
                field("price").and_then(describe::micros),
            ) else {
                continue;
            };
            prices.insert(name.to_owned(), price);
        }
    }
    Ok(prices)
}

/// An instance type's name in parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name<'a> {
    /// As in `m8g` or `c7i-flex`.
    pub family: &'a str,
    /// The letters before the generation, as in `m`.
    pub category: &'a str,
    /// As in 8, or `None` for a name without one.
    pub generation: Option<u16>,
    /// As in `xlarge`.
    pub size: &'a str,
}

/// Splits `m8g.xlarge` into family, category, generation and size.
#[must_use]
pub fn split(name: &str) -> Option<Name<'_>> {
    let (family, size) = name.split_once('.')?;
    let letters = family.bytes().take_while(u8::is_ascii_lowercase).count();
    if letters == 0 || size.is_empty() {
        return None;
    }
    let digits = family[letters..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    Some(Name {
        family,
        category: &family[..letters],
        generation: family[letters..letters + digits].parse().ok(),
        size,
    })
}

/// An instance type in a region's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct Entry {
    /// As in `m8g.xlarge`.
    pub name: String,
    /// As in `m8g`.
    pub family: String,
    /// As in `m`.
    pub category: String,
    /// As in 8; 0 for a name without one.
    pub generation: u16,
    /// What it runs.
    pub arch: Arch,
    /// vCPUs.
    pub vcpus: u16,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// GPUs.
    pub gpus: u16,
    /// Inference accelerators, Neuron devices or FPGAs.
    pub accelerators: bool,
    /// A whole host.
    pub bare_metal: bool,
    /// Burstable.
    pub burstable: bool,
    /// Has instance store volumes.
    pub local_storage: bool,
    /// Can run as Spot.
    pub spot: bool,
    /// Zone ids that offer it, sorted.
    pub zones: Vec<String>,
    /// Linux on-demand price per hour in millionths of a US dollar.
    pub on_demand_micros: Option<u64>,
    /// The lowest Linux Spot price per hour across its zones.
    pub spot_micros: Option<u64>,
}

/// A region's catalog from what EC2 described and the prices: every
/// type at least one zone offers, by name.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn merge(
    types: Vec<describe::InstanceType>,
    offerings: &[(String, String)],
    on_demand: &HashMap<String, u64>,
    spot_prices: &[(String, u64)],
) -> Vec<Entry> {
    let mut zones: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for (name, zone) in offerings {
        zones.entry(name).or_default().insert(zone);
    }
    let mut spot: HashMap<&str, u64> = HashMap::new();
    for (name, price) in spot_prices {
        spot.entry(name)
            .and_modify(|lowest| *lowest = (*lowest).min(*price))
            .or_insert(*price);
    }
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    for described in types {
        let Some(parts) = split(&described.name) else {
            continue;
        };
        let Some(offered) = zones.get(described.name.as_str()) else {
            continue;
        };
        let entry = Entry {
            family: parts.family.to_owned(),
            category: parts.category.to_owned(),
            generation: parts.generation.unwrap_or(0),
            arch: described.arch,
            vcpus: described.vcpus,
            memory_mib: described.memory_mib,
            gpus: described.gpus,
            accelerators: described.accelerators,
            bare_metal: described.bare_metal,
            burstable: described.burstable,
            local_storage: described.local_storage,
            spot: described.spot,
            zones: offered.iter().map(|zone| (*zone).to_owned()).collect(),
            on_demand_micros: on_demand.get(&described.name).copied(),
            spot_micros: spot
                .get(described.name.as_str())
                .copied()
                .filter(|_| described.spot),
            name: described.name,
        };
        entries.insert(entry.name.clone(), entry);
    }
    entries.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(name: &str, arch: Arch, vcpus: u16) -> describe::InstanceType {
        describe::InstanceType {
            name: name.to_owned(),
            arch,
            vcpus,
            memory_mib: u32::from(vcpus) * 4096,
            gpus: 0,
            accelerators: false,
            bare_metal: false,
            burstable: false,
            local_storage: false,
            spot: true,
        }
    }

    #[test]
    fn splits_names_like_karpenter() {
        assert_eq!(
            split("m8g.xlarge"),
            Some(Name {
                family: "m8g",
                category: "m",
                generation: Some(8),
                size: "xlarge",
            })
        );
        let flex = split("c7i-flex.2xlarge").unwrap();
        assert_eq!(
            (flex.family, flex.category, flex.generation),
            ("c7i-flex", "c", Some(7))
        );
        let inf = split("inf2.xlarge").unwrap();
        assert_eq!((inf.category, inf.generation), ("inf", Some(2)));
        assert_eq!(split("u-6tb1.56xlarge").unwrap().generation, None);
        assert_eq!(split("m8g"), None);
        assert_eq!(split("8g.large"), None);
    }

    #[test]
    fn price_lists_by_region() {
        assert_eq!(
            price_list_url("ap-south-1").unwrap(),
            "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/\
             ec2-ondemand-without-sec-sel/Asia%20Pacific%20%28Mumbai%29/Linux/index.json"
        );
        assert_eq!(price_list_url("mars-north-1"), None);
        for (region, _) in crate::connect::REGIONS {
            assert!(price_list_url(region).is_some(), "{region}");
        }
    }

    #[test]
    fn reads_on_demand_prices_gzipped_or_not() {
        let list = serde_json::json!({
            "manifest": {},
            "regions": { "Asia Pacific (Mumbai)": {
                "m8g xlarge": {
                    "price": "0.0898000000", "Instance Type": "m8g.xlarge",
                    "Pre Installed S/W": "NA", "License Model": "No License required",
                },
                "m8g xlarge sql": {
                    "price": "1.5", "Instance Type": "m8g.xlarge",
                    "Pre Installed S/W": "SQL Std",
                },
            }},
        })
        .to_string();
        let mut gzipped = Vec::new();
        {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::default());
            encoder.write_all(list.as_bytes()).unwrap();
        }
        for body in [list.as_bytes(), &gzipped[..]] {
            let prices = on_demand_prices(body).unwrap();
            assert_eq!(prices.get("m8g.xlarge"), Some(&89_800));
            assert_eq!(prices.len(), 1);
        }
        assert!(on_demand_prices(b"<html>").is_err());
    }

    #[test]
    fn merges_types_zones_and_prices() {
        let types = vec![
            described("m8g.xlarge", Arch::Arm64, 4),
            described("m8i.xlarge", Arch::Amd64, 4),
            // Described but offered in no zone of the region.
            described("m9g.xlarge", Arch::Arm64, 4),
        ];
        let offerings = [
            ("m8g.xlarge".to_owned(), "aps1-az3".to_owned()),
            ("m8g.xlarge".to_owned(), "aps1-az1".to_owned()),
            ("m8i.xlarge".to_owned(), "aps1-az1".to_owned()),
        ];
        let on_demand = HashMap::from([("m8g.xlarge".to_owned(), 89_800)]);
        let spot = [
            ("m8g.xlarge".to_owned(), 30_000),
            ("m8g.xlarge".to_owned(), 25_000),
        ];
        let catalog = merge(types, &offerings, &on_demand, &spot);
        assert_eq!(catalog.len(), 2);
        let m8g = &catalog[0];
        assert_eq!(m8g.name, "m8g.xlarge");
        assert_eq!((m8g.category.as_str(), m8g.generation), ("m", 8));
        assert_eq!(m8g.zones, ["aps1-az1", "aps1-az3"]);
        assert_eq!(
            (m8g.on_demand_micros, m8g.spot_micros),
            (Some(89_800), Some(25_000))
        );
        assert_eq!(
            (catalog[1].on_demand_micros, catalog[1].spot_micros),
            (None, None)
        );
    }
}
