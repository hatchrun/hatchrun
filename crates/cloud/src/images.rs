// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Runner image releases from hatchrun/images.
//!
//! Each release of hatchrun/images carries a `manifest.json` with the
//! public AMI of every region and architecture:
//!
//! ```json
//! { "version": "20261013.1915", "ubuntu": "26.04", "runner": "...",
//!   "agent": "...", "amis": { "amd64": { "us-east-1": "ami-..." },
//!   "arm64": { ... } } }
//! ```
//!
//! Its build posts the manifest to `POST /api/internal/images` once the
//! AMIs are public, with a GitHub Actions token that only its release
//! job can get (see [`POLICY`]). The Worker writes one row per region
//! and architecture to the `images` table, which launches read.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::actions::{self, Policy};

/// Where the build posts the manifest.
pub const PATH: &str = "/api/internal/images";
/// The audience the build asks GitHub for.
pub const AUDIENCE: &str = "https://app.hatchrun.io";
/// The only callers: the `register.yml` job of hatchrun/images, on
/// main, in its `image-build` environment. `build.yml` calls it after
/// the AMIs are public, and it runs on its own to post a release again.
pub const POLICY: Policy<'static> = Policy {
    issuer: actions::ISSUER,
    audience: AUDIENCE,
    repository: "hatchrun/images",
    repository_id: "1411692062",
    git_ref: "refs/heads/main",
    job_workflow_ref: "hatchrun/images/.github/workflows/register.yml@refs/heads/main",
    environment: "image-build",
};
/// The architectures a release has, as the `images` table names them.
pub const ARCHES: [&str; 2] = ["amd64", "arm64"];
/// The root device of every runner image. Ubuntu AMIs boot from
/// `/dev/sda1`; a `disk=N` label resizes the volume there.
pub const ROOT_DEVICE: &str = "/dev/sda1";
/// The most regions one architecture may list. AWS has about 35.
pub const MAX_REGIONS: usize = 64;
/// The largest manifest accepted, in bytes.
pub const MAX_BODY: usize = 64 * 1024;

/// A release's images, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The image version, `YYYYMMDD.HHMM` in UTC.
    pub version: String,
    /// One per region and architecture.
    pub images: Vec<Image>,
}

/// The image for one region and architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// AWS region.
    pub region: String,
    /// One of [`ARCHES`].
    pub arch: &'static str,
    /// The AMI id.
    pub image_id: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    amis: BTreeMap<String, BTreeMap<String, String>>,
}

/// Reads and checks a release manifest: a version, both architectures,
/// region names and AMI ids that look like AWS's, and no more than
/// [`MAX_REGIONS`] regions each.
///
/// # Errors
///
/// When any of that does not hold.
pub fn parse(body: &[u8]) -> Result<Release, String> {
    if body.len() > MAX_BODY {
        return Err(format!("manifest is larger than {MAX_BODY} bytes"));
    }
    let manifest: Manifest =
        serde_json::from_slice(body).map_err(|err| format!("bad manifest: {err}"))?;
    if !is_version(&manifest.version) {
        return Err(format!(
            "version {:?} is not YYYYMMDD.HHMM",
            manifest.version
        ));
    }
    if let Some(arch) = manifest
        .amis
        .keys()
        .find(|arch| !ARCHES.contains(&arch.as_str()))
    {
        return Err(format!("unknown architecture {arch:?}"));
    }
    let mut images = Vec::new();
    for arch in ARCHES {
        let regions = manifest
            .amis
            .get(arch)
            .filter(|regions| !regions.is_empty())
            .ok_or_else(|| format!("no {arch} images"))?;
        if regions.len() > MAX_REGIONS {
            return Err(format!("{arch} lists more than {MAX_REGIONS} regions"));
        }
        for (region, image_id) in regions {
            if !is_region(region) {
                return Err(format!("{region:?} is not a region"));
            }
            if !is_image_id(image_id) {
                return Err(format!(
                    "{arch} image {image_id:?} in {region} is not an AMI id"
                ));
            }
            images.push(Image {
                region: region.clone(),
                arch,
                image_id: image_id.clone(),
            });
        }
    }
    Ok(Release {
        version: manifest.version,
        images,
    })
}

/// Whether a release of `version` may replace images of `stored`, the
/// newest version in the table. Versions are fixed-width timestamps, so
/// they compare as text. Rows loaded by hand carry no version.
#[must_use]
pub fn is_current(version: &str, stored: Option<&str>) -> bool {
    stored.is_none_or(|stored| version >= stored)
}

/// Writes one image, unless the table holds a newer version for its
/// region and architecture. Binds region, arch, image id, root device,
/// version and the time in milliseconds.
pub const UPSERT: &str = "INSERT INTO images (region, arch, image_id, root_device, version, updated_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
     ON CONFLICT (region, arch) DO UPDATE SET image_id = excluded.image_id, \
     root_device = excluded.root_device, version = excluded.version, updated_at = excluded.updated_at \
     WHERE images.version IS NULL OR images.version <= excluded.version";

/// `YYYYMMDD.HHMM`.
fn is_version(version: &str) -> bool {
    let bytes = version.as_bytes();
    bytes.len() == 13
        && bytes[8] == b'.'
        && bytes
            .iter()
            .enumerate()
            .all(|(at, byte)| at == 8 || byte.is_ascii_digit())
}

/// `us-east-1`, `ap-southeast-5`, `us-gov-west-1` and the like.
fn is_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    let [area, .., number] = parts.as_slice() else {
        return false;
    };
    let middle = &parts[1..parts.len() - 1];
    region.len() <= 32
        && area.len() == 2
        && area.bytes().all(|byte| byte.is_ascii_lowercase())
        && !middle.is_empty()
        && middle
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase()))
        && (1..=2).contains(&number.len())
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// `ami-` and 8 or 17 lowercase hex digits.
fn is_image_id(id: &str) -> bool {
    id.strip_prefix("ami-").is_some_and(|hex| {
        matches!(hex.len(), 8 | 17)
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> serde_json::Value {
        serde_json::json!({
            "version": "20261013.1915",
            "ubuntu": "26.04",
            "runner": "2.330.0",
            "agent": "0.4.0",
            "amis": {
                "amd64": {
                    "us-east-1": "ami-0123456789abcdef0",
                    "ap-south-1": "ami-0fedcba9876543210",
                },
                "arm64": {
                    "us-east-1": "ami-00000000000000001",
                    "ap-south-1": "ami-1234abcd",
                },
            },
        })
    }

    fn parse_json(value: &serde_json::Value) -> Result<Release, String> {
        parse(value.to_string().as_bytes())
    }

    #[test]
    fn reads_a_release() {
        let release = parse_json(&manifest()).unwrap();
        assert_eq!(release.version, "20261013.1915");
        let rows: Vec<(&str, &str, &str)> = release
            .images
            .iter()
            .map(|image| (image.region.as_str(), image.arch, image.image_id.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("ap-south-1", "amd64", "ami-0fedcba9876543210"),
                ("us-east-1", "amd64", "ami-0123456789abcdef0"),
                ("ap-south-1", "arm64", "ami-1234abcd"),
                ("us-east-1", "arm64", "ami-00000000000000001"),
            ]
        );
    }

    #[test]
    fn refuses_bad_manifests() {
        let refused = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut value = manifest();
            edit(&mut value);
            parse_json(&value).unwrap_err()
        };
        for version in [
            "2026101.1915",
            "20261013-1915",
            "20261013.191a",
            "",
            "v20261013.1915",
        ] {
            assert!(refused(&|m| m["version"] = version.into()).contains("version"));
        }
        assert!(refused(&|m| m["amis"]["riscv64"] = serde_json::json!({})).contains("riscv64"));
        assert!(
            refused(&|m| m["amis"]["x86_64"] = manifest()["amis"]["amd64"].clone())
                .contains("unknown architecture")
        );
        assert!(
            refused(&|m| m["amis"]
                .as_object_mut()
                .unwrap()
                .remove("arm64")
                .map(drop)
                .unwrap())
            .contains("no arm64")
        );
        assert!(refused(&|m| m["amis"]["arm64"] = serde_json::json!({})).contains("no arm64"));
        for region in [
            "US-EAST-1",
            "us-east",
            "useast-1",
            "us--1",
            "us-east-100",
            "../x-y-1",
            "x-y-1",
        ] {
            assert!(
                refused(&|m| m["amis"]["amd64"][region] = "ami-1234abcd".into()).contains("region"),
                "{region}"
            );
        }
        for ami in [
            "ami-1234abc",
            "ami-1234ABCD",
            "snap-1234abcd",
            "ami-0123456789abcdef",
            "ami-1234abcd'",
        ] {
            assert!(
                refused(&|m| m["amis"]["amd64"]["us-east-1"] = ami.into())
                    .contains("not an AMI id"),
                "{ami}"
            );
        }
        let many: serde_json::Map<String, serde_json::Value> = (1..=MAX_REGIONS + 1)
            .map(|at| (format!("us-east-{at}"), "ami-1234abcd".into()))
            .collect();
        assert!(refused(&|m| m["amis"]["amd64"] = many.clone().into()).contains("more than"));
        assert!(parse(b"[]").is_err());
        assert!(
            parse(&vec![b' '; MAX_BODY + 1])
                .unwrap_err()
                .contains("larger")
        );
    }

    #[test]
    fn knows_aws_regions() {
        for region in [
            "us-east-1",
            "ap-southeast-5",
            "us-gov-west-1",
            "il-central-1",
            "mx-central-1",
        ] {
            assert!(is_region(region), "{region}");
        }
    }

    #[test]
    fn never_goes_back_a_version() {
        assert!(is_current("20261013.1915", None));
        assert!(is_current("20261013.1915", Some("20261013.1915")));
        assert!(is_current("20261020.0317", Some("20261013.1915")));
        assert!(!is_current("20261006.0317", Some("20261013.1915")));
    }
}
