// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The instance types for a runner profile in a tenant's region.
//!
//! Each family the profile lists, in its order, offers its smallest size
//! with the vCPUs and memory the profile asks for, if the region has the
//! family and the connect stack's `InstanceTypes` allow that size. The
//! first is launched; the others are tried in order when EC2 has no
//! capacity for it. A Spot profile tries all of them as Spot first and
//! then all of them on-demand.
//!
//! The stack's IAM policy has the final word: a profile saved when the
//! stack allowed more, or a label's overrides, can ask for what the role
//! now refuses, and then the job fails with the reason instead of
//! launching something AWS would refuse.

use crate::catalog::Family;
use crate::labels::Arch;
use crate::profiles::{self, Profile};

/// The connect stack's `InstanceTypes` and volume parameters for tenants
/// whose launch template predates the tags that carry them, as the
/// template's defaults were then.
pub const LEGACY_INSTANCE_TYPES: &str = "c7g.*,c7i.*,m7g.*,m7i.*,r7g.*,r7i.*";
/// See [`LEGACY_INSTANCE_TYPES`].
pub const LEGACY_VOLUME_SIZE_LIMIT: u16 = 200;
/// See [`LEGACY_INSTANCE_TYPES`].
pub const LEGACY_ROOT_VOLUME_SIZE: u16 = 30;

/// What a tenant's connect stack lets hatchrun launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `InstanceTypes`: patterns IAM matches with `StringLike`, where
    /// `*` matches any run of characters and `?` any one.
    pub instance_types: Vec<String>,
    /// `VolumeSizeLimit`: the largest root volume in GiB.
    pub volume_size_limit: u16,
    /// `RootVolumeSize`: the root volume of a profile that sets none.
    pub root_volume_size: u16,
}

impl Limits {
    /// The limits from the `tenants` columns, which are NULL for a
    /// tenant connected before they were recorded.
    #[must_use]
    pub fn from_columns(
        instance_types: Option<&str>,
        volume_size_limit: Option<u16>,
        root_volume_size: Option<u16>,
    ) -> Self {
        Self {
            instance_types: instance_types
                .unwrap_or(LEGACY_INSTANCE_TYPES)
                .split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_owned)
                .collect(),
            volume_size_limit: volume_size_limit.unwrap_or(LEGACY_VOLUME_SIZE_LIMIT),
            root_volume_size: root_volume_size.unwrap_or(LEGACY_ROOT_VOLUME_SIZE),
        }
    }

    /// Whether the role may launch `instance_type`.
    #[must_use]
    pub fn allows(&self, instance_type: &str) -> bool {
        self.instance_types
            .iter()
            .any(|pattern| matches(pattern.as_bytes(), instance_type.as_bytes()))
    }
}

/// IAM's `StringLike`: `*` is any run of characters, `?` any one.
fn matches(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| matches(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && matches(rest, &text[1..]),
        Some((byte, rest)) => text.first() == Some(byte) && matches(rest, &text[1..]),
    }
}

/// The size name with `vcpus` vCPUs, as in `large` or `4xlarge`.
#[must_use]
pub fn size_name(vcpus: u16) -> String {
    match vcpus {
        1 => "medium".to_owned(),
        2 => "large".to_owned(),
        4 => "xlarge".to_owned(),
        _ => format!("{}xlarge", vcpus / 4),
    }
}

/// The vCPUs of the sizes of `family` that fit the profile's bounds,
/// smallest first.
fn sizes_in<'a>(family: &'a Family, profile: &'a Profile) -> impl Iterator<Item = u16> + 'a {
    family.sizes.iter().copied().filter(move |vcpus| {
        let memory = u32::from(*vcpus) * u32::from(family.gib_per_vcpu);
        *vcpus >= profile.cpu_min
            && profile.cpu_max.is_none_or(|max| *vcpus <= max)
            && memory >= u32::from(profile.memory_min)
            && profile
                .memory_max
                .is_none_or(|max| memory <= u32::from(max))
    })
}

/// The vCPUs of the smallest size of `family` that fits the profile,
/// wherever it is offered.
#[must_use]
pub fn size_in(family: &Family, profile: &Profile) -> Option<u16> {
    sizes_in(family, profile).next()
}

/// The runner to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Instance types to try, preferred first; never empty.
    pub instance_types: Vec<String>,
    /// Architecture, which picks the image.
    pub arch: Arch,
    /// Try Spot before on-demand.
    pub spot: bool,
    /// The root volume to ask for, or the launch template's.
    pub root_volume: Option<u16>,
    /// The root volume the runner gets, in GiB.
    pub disk_gib: u16,
    /// vCPUs of the first type.
    pub vcpus: u16,
    /// Memory of the first type in GiB.
    pub memory_gib: u32,
    /// On-demand price of the first type per hour, in millionths of a
    /// US dollar.
    pub hourly_micros: u64,
}

/// Picks the instance types for `profile` in `region` within `limits`.
///
/// # Errors
///
/// Why nothing can be launched, in words for the owner.
pub fn choose(profile: &Profile, region: &str, limits: &Limits) -> Result<Choice, String> {
    if let Some(disk) = profile.disk_gib
        && disk > limits.volume_size_limit
    {
        return Err(format!(
            "{} asks for a {disk} GiB root volume and the connect stack allows at most {} GiB. \
             Raise VolumeSizeLimit on the stack or ask for less.",
            profile.name, limits.volume_size_limit
        ));
    }

    // (instance type, vCPUs, family) of every family's best fit in the
    // region, and whether IAM allows it.
    let mut fits: Vec<(String, u16, &Family, bool)> = Vec::new();
    for family in profile
        .families
        .iter()
        .filter_map(|name| profiles::family(name))
    {
        if price(family, region).is_none() {
            continue;
        }
        let mut sizes = sizes_in(family, profile).peekable();
        let Some(&smallest) = sizes.peek() else {
            continue;
        };
        let allowed = sizes
            .map(|vcpus| (format!("{}.{}", family.name, size_name(vcpus)), vcpus))
            .find(|(instance_type, _)| limits.allows(instance_type));
        match allowed {
            Some((instance_type, vcpus)) => fits.push((instance_type, vcpus, family, true)),
            None => fits.push((
                format!("{}.{}", family.name, size_name(smallest)),
                smallest,
                family,
                false,
            )),
        }
    }

    let allowed: Vec<&(String, u16, &Family, bool)> =
        fits.iter().filter(|(_, _, _, allowed)| *allowed).collect();
    let Some((_, vcpus, family, _)) = allowed.first() else {
        if fits.is_empty() {
            return Err(format!(
                "{region} offers none of {} with {} and {}",
                profile.families.join(", "),
                profile.cpu_text(),
                profile.memory_text()
            ));
        }
        let wanted: Vec<&str> = fits.iter().map(|(name, ..)| name.as_str()).collect();
        return Err(format!(
            "The connect stack allows none of the instance types that fit {} ({}). It allows {}. \
             Add one to InstanceTypes on the stack or change the profile.",
            profile.name,
            wanted.join(", "),
            limits.instance_types.join(", ")
        ));
    };
    Ok(Choice {
        instance_types: allowed.iter().map(|(name, ..)| name.clone()).collect(),
        arch: profile.arch,
        spot: profile.spot,
        root_volume: profile.disk_gib,
        disk_gib: profile.disk_gib.unwrap_or(limits.root_volume_size),
        vcpus: *vcpus,
        memory_gib: u32::from(*vcpus) * u32::from(family.gib_per_vcpu),
        hourly_micros: u64::from(*vcpus) * u64::from(price(family, region).unwrap_or_default()),
    })
}

/// The on-demand price per vCPU-hour of `family` in `region`, if it is
/// offered there.
fn price(family: &Family, region: &str) -> Option<u32> {
    family
        .prices
        .iter()
        .find(|(offered, _)| *offered == region)
        .map(|(_, price)| *price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels;
    use crate::profiles::{built_in, resolve};

    fn limits(types: &str, volume: u16) -> Limits {
        Limits::from_columns(Some(types), Some(volume), Some(30))
    }

    fn open() -> Limits {
        limits("*", 16384)
    }

    fn chosen(label: &str, region: &str, limits: &Limits) -> Result<Vec<String>, String> {
        let profile = resolve(&labels::parse(label).unwrap(), None).unwrap();
        choose(&profile, region, limits).map(|choice| choice.instance_types)
    }

    #[test]
    fn built_ins_pick_the_newest_generation() {
        assert_eq!(
            chosen("hatchrun-4cpu-arm64", "us-east-1", &open()),
            Ok(vec!["m8g.xlarge".to_owned(), "m7g.xlarge".to_owned()])
        );
        assert_eq!(
            chosen("hatchrun", "eu-west-1", &open()),
            Ok(vec!["m8i.large".to_owned(), "m7i.large".to_owned()])
        );
        assert_eq!(
            chosen("hatchrun-16cpu", "us-east-1", &open()).unwrap()[0],
            "m8i.4xlarge"
        );
    }

    #[test]
    fn falls_back_where_a_family_is_missing() {
        // Stockholm has no m8i.
        assert_eq!(
            chosen("hatchrun-8cpu", "eu-north-1", &open()),
            Ok(vec!["m7i.2xlarge".to_owned()])
        );
    }

    #[test]
    fn the_stack_has_the_final_word() {
        // The connect stack's old defaults allow only the 7th generation.
        assert_eq!(
            chosen(
                "hatchrun-2cpu-arm64",
                "us-east-1",
                &Limits::from_columns(None, None, None)
            ),
            Ok(vec!["m7g.large".to_owned()])
        );
        // A pattern can allow some sizes and not others.
        assert_eq!(
            chosen(
                "hatchrun-2cpu/cpu=4",
                "us-east-1",
                &limits("m8i.?xlarge,m7i.large", 200)
            ),
            Ok(vec!["m8i.2xlarge".to_owned()])
        );
        let refused = chosen("hatchrun-4cpu", "us-east-1", &limits("c7g.*", 200)).unwrap_err();
        assert_eq!(
            refused,
            "The connect stack allows none of the instance types that fit hatchrun-4cpu \
             (m8i.xlarge, m7i.xlarge). It allows c7g.*. Add one to InstanceTypes on the stack \
             or change the profile."
        );
        let too_big = chosen("hatchrun-4cpu/disk=300", "us-east-1", &limits("*", 200));
        assert!(too_big.unwrap_err().contains("allows at most 200 GiB"));
    }

    #[test]
    fn says_when_the_region_lacks_every_family() {
        let mut profile = built_in("hatchrun-2cpu-arm64").unwrap();
        profile.families = vec!["r9g".to_owned()];
        profile.memory_min = 16;
        assert_eq!(
            choose(&profile, "eu-north-1", &open()),
            Err("eu-north-1 offers none of r9g with 2 vCPUs and at least 16 GiB".to_owned())
        );
    }

    #[test]
    fn families_go_in_the_order_given() {
        let mut profile = built_in("hatchrun-8cpu-arm64").unwrap();
        profile.families = vec!["c8g".to_owned(), "r8g".to_owned(), "m8g".to_owned()];
        profile.cpu_max = None;
        profile.memory_min = 32;
        // c8g needs 16 vCPUs for 32 GiB; r8g and m8g fit at 8.
        let choice = choose(&profile, "us-east-1", &open()).unwrap();
        assert_eq!(
            choice.instance_types,
            ["c8g.4xlarge", "r8g.2xlarge", "m8g.2xlarge"]
        );
        assert_eq!((choice.vcpus, choice.memory_gib), (16, 32));
        profile.cpu_max = Some(8);
        assert_eq!(
            choose(&profile, "us-east-1", &open())
                .unwrap()
                .instance_types,
            ["r8g.2xlarge", "m8g.2xlarge"]
        );
    }

    #[test]
    fn reports_disk_spot_and_price() {
        let choice = choose(
            &resolve(
                &labels::parse("hatchrun-2cpu-arm64/spot=true").unwrap(),
                None,
            )
            .unwrap(),
            "us-east-1",
            &open(),
        )
        .unwrap();
        assert!(choice.spot);
        assert_eq!((choice.root_volume, choice.disk_gib), (None, 30));
        assert_eq!(choice.arch, Arch::Arm64);
        // m8g.large is $0.08976 an hour in us-east-1.
        assert_eq!(choice.hourly_micros, 89_760);
        let disk = chosen("hatchrun/disk=100", "us-east-1", &open());
        assert!(disk.is_ok());
    }

    #[test]
    fn matches_like_iam() {
        assert!(matches(b"m7g.*", b"m7g.large"));
        assert!(matches(b"*", b""));
        assert!(matches(b"m?g.large", b"m8g.large"));
        assert!(!matches(b"m7g.*", b"m7gd.large"));
        assert!(!matches(b"m7g.large", b"m7g.xlarge"));
        assert!(!matches(b"m?g.large", b"m8gd.large"));
        assert_eq!(size_name(1), "medium");
        assert_eq!(size_name(16), "4xlarge");
        assert_eq!(size_name(384), "96xlarge");
    }
}
