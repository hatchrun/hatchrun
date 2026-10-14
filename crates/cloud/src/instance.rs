// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The instance types for a runner profile in a tenant's region.
//!
//! The region's instance type catalog (see [`crate::instance_types`])
//! lists what it offers. A type fits a profile when it runs the
//! profile's architecture, has its vCPUs and memory, is of a category
//! and generation it asks for (or of a family it pins), and is plain
//! compute: GPUs, accelerators, bare metal, burstable and flex types
//! only fit a profile that pins their family. Of each fitting family the
//! smallest size the connect stack allows is kept; the newest generation
//! goes first and, within it, the lowest on-demand price. A launch hands
//! a generation at a time to EC2 Fleet, which picks the cheapest with
//! capacity (see [`crate::offerings`]).
//!
//! Before a region's catalog is first read, the static families in
//! [`crate::catalog`] stand in for it.
//!
//! The stack's IAM policy has the final word: a profile saved when the
//! stack allowed more, or a label's overrides, can ask for what the role
//! now refuses, and then the job fails with the reason instead of
//! launching something AWS would refuse.

use crate::instance_types::{self, Entry};
use crate::labels::Arch;
use crate::profiles::{Capacity, Profile};

/// The connect stack's `InstanceTypes` and volume parameters for tenants
/// whose launch template predates the tags that carry them, as the
/// template's defaults were then.
pub const LEGACY_INSTANCE_TYPES: &str = "c7g.*,c7i.*,m7g.*,m7i.*,r7g.*,r7i.*";
/// See [`LEGACY_INSTANCE_TYPES`].
pub const LEGACY_VOLUME_SIZE_LIMIT: u16 = 200;
/// See [`LEGACY_INSTANCE_TYPES`].
pub const LEGACY_ROOT_VOLUME_SIZE: u16 = 30;
/// Most instance types one choice keeps.
const CHOICE_MAX: usize = 40;

/// What a tenant's connect stack lets hatchrun launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `InstanceTypes`: patterns IAM matches with `StringLike`, where
    /// `*` matches any run of characters and `?` any one.
    pub instance_types: Vec<String>,
    /// `ExcludedInstanceTypes`: patterns the role is denied, as above.
    pub excluded: Vec<String>,
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
        excluded: Option<&str>,
        volume_size_limit: Option<u16>,
        root_volume_size: Option<u16>,
    ) -> Self {
        let patterns = |list: &str| -> Vec<String> {
            list.split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_owned)
                .collect()
        };
        Self {
            instance_types: patterns(instance_types.unwrap_or(LEGACY_INSTANCE_TYPES)),
            excluded: patterns(excluded.unwrap_or_default()),
            volume_size_limit: volume_size_limit.unwrap_or(LEGACY_VOLUME_SIZE_LIMIT),
            root_volume_size: root_volume_size.unwrap_or(LEGACY_ROOT_VOLUME_SIZE),
        }
    }

    /// Whether the role may launch `instance_type`.
    #[must_use]
    pub fn allows(&self, instance_type: &str) -> bool {
        let like = |pattern: &String| matches(pattern.as_bytes(), instance_type.as_bytes());
        self.instance_types.iter().any(like) && !self.excluded.iter().any(like)
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

/// The runner to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Instance types, newest generation first and cheapest first
    /// within it; never empty.
    pub instance_types: Vec<String>,
    /// Architecture, which picks the image.
    pub arch: Arch,
    /// On-demand, Spot, or Spot then on-demand.
    pub capacity: Capacity,
    /// The root volume to ask for, or the launch template's.
    pub root_volume: Option<u16>,
    /// The root volume the runner gets, in GiB.
    pub disk_gib: u16,
    /// vCPUs of the first type.
    pub vcpus: u16,
    /// Memory of the first type in GiB.
    pub memory_gib: u32,
    /// On-demand price of the first type per hour, in millionths of a
    /// US dollar, when known.
    pub hourly_micros: u64,
}

/// Whether `entry` fits `profile`, apart from the stack's limits.
fn fits(profile: &Profile, entry: &Entry) -> bool {
    let pinned = !profile.families.is_empty();
    let memory_gib = entry.memory_mib / 1024;
    let kind = if pinned {
        profile.families.contains(&entry.family)
    } else {
        (profile.categories.is_empty() || profile.categories.contains(&entry.category))
            && entry.gpus == 0
            && !entry.accelerators
            && !entry.bare_metal
            && (!entry.burstable || profile.categories.iter().any(|category| category == "t"))
            && !entry.family.ends_with("-flex")
    };
    kind && entry.arch == profile.arch
        && profile
            .min_generation
            .is_none_or(|oldest| entry.generation >= oldest)
        && entry.vcpus >= profile.cpu_min
        && profile.cpu_max.is_none_or(|max| entry.vcpus <= max)
        && memory_gib >= u32::from(profile.memory_min)
        && profile
            .memory_max
            .is_none_or(|max| memory_gib <= u32::from(max))
        && (profile.capacity != Capacity::Spot || entry.spot)
}

/// What `profile` asks for in words, for errors.
fn wanted(profile: &Profile) -> String {
    let kind = if profile.families.is_empty() {
        match profile.categories.as_slice() {
            [] => "instance types".to_owned(),
            categories => format!("category {} instance types", categories.join(", ")),
        }
    } else {
        profile.families.join(", ")
    };
    let generation = profile
        .min_generation
        .map(|oldest| format!(" of generation {oldest} or newer"))
        .unwrap_or_default();
    format!(
        "{kind}{generation} for {} with {} and {}",
        profile.arch.as_str(),
        profile.cpu_text(),
        profile.memory_text()
    )
}

/// Picks the instance types for `profile` in `region`, which offers
/// `offered`, within `limits`.
///
/// # Errors
///
/// Why nothing can be launched, in words for the owner.
pub fn choose(
    profile: &Profile,
    region: &str,
    offered: &[Entry],
    limits: &Limits,
) -> Result<Choice, String> {
    if let Some(disk) = profile.disk_gib
        && disk > limits.volume_size_limit
    {
        return Err(format!(
            "{} asks for a {disk} GiB root volume and the connect stack allows at most {} GiB. \
             Raise VolumeSizeLimit on the stack or ask for less.",
            profile.name, limits.volume_size_limit
        ));
    }
    let fitting: Vec<&Entry> = offered
        .iter()
        .filter(|entry| fits(profile, entry))
        .collect();
    if fitting.is_empty() {
        return Err(format!("{region} offers no {}", wanted(profile)));
    }
    let mut allowed: Vec<&Entry> = fitting
        .iter()
        .copied()
        .filter(|entry| limits.allows(&entry.name))
        .collect();
    if allowed.is_empty() {
        let mut sample: Vec<&str> = fitting.iter().map(|entry| entry.name.as_str()).collect();
        sample.truncate(4);
        return Err(format!(
            "The connect stack allows none of the instance types that fit {} ({}{}). It allows \
             {}{}. Change InstanceTypes on the stack or change the profile.",
            profile.name,
            sample.join(", "),
            if fitting.len() > sample.len() {
                ", ..."
            } else {
                ""
            },
            limits.instance_types.join(", "),
            if limits.excluded.is_empty() {
                String::new()
            } else {
                format!(" except {}", limits.excluded.join(", "))
            }
        ));
    }
    // The smallest allowed size of each family.
    allowed.sort_by(|a, b| {
        (a.family.as_str(), a.vcpus, a.memory_mib).cmp(&(b.family.as_str(), b.vcpus, b.memory_mib))
    });
    allowed.dedup_by(|later, first| later.family == first.family);
    allowed.sort_by(|a, b| {
        b.generation
            .cmp(&a.generation)
            .then(
                a.on_demand_micros
                    .unwrap_or(u64::MAX)
                    .cmp(&b.on_demand_micros.unwrap_or(u64::MAX)),
            )
            .then(a.name.cmp(&b.name))
    });
    allowed.truncate(CHOICE_MAX);
    let first = allowed[0];
    Ok(Choice {
        instance_types: allowed.iter().map(|entry| entry.name.clone()).collect(),
        arch: profile.arch,
        capacity: profile.capacity,
        root_volume: profile.disk_gib,
        disk_gib: profile.disk_gib.unwrap_or(limits.root_volume_size),
        vcpus: first.vcpus,
        memory_gib: first.memory_mib / 1024,
        hourly_micros: first.on_demand_micros.unwrap_or_default(),
    })
}

/// The static families as catalog rows for `region`, for a region whose
/// catalog was not read yet: every size of every family it offers.
#[must_use]
pub fn static_offerings(region: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for family in crate::catalog::FAMILIES {
        let Some((_, per_vcpu)) = family.prices.iter().find(|(offered, _)| *offered == region)
        else {
            continue;
        };
        for vcpus in family.sizes {
            let name = format!("{}.{}", family.name, size_name(*vcpus));
            let Some(parts) = instance_types::split(&name) else {
                continue;
            };
            entries.push(Entry {
                family: family.name.to_owned(),
                category: parts.category.to_owned(),
                generation: parts.generation.unwrap_or(0),
                arch: family.arch,
                vcpus: *vcpus,
                memory_mib: u32::from(*vcpus) * u32::from(family.gib_per_vcpu) * 1024,
                gpus: 0,
                accelerators: false,
                bare_metal: false,
                burstable: false,
                local_storage: false,
                spot: true,
                zones: Vec::new(),
                on_demand_micros: Some(u64::from(*vcpus) * u64::from(*per_vcpu)),
                spot_micros: None,
                name,
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels;
    use crate::profiles::{built_in, resolve};

    fn limits(types: &str, volume: u16) -> Limits {
        Limits::from_columns(Some(types), None, Some(volume), Some(30))
    }

    fn open() -> Limits {
        limits("*", 16384)
    }

    fn chosen(label: &str, region: &str, limits: &Limits) -> Result<Vec<String>, String> {
        let profile = resolve(&labels::parse(label).unwrap(), None).unwrap();
        choose(&profile, region, &static_offerings(region), limits).map(|c| c.instance_types)
    }

    fn entry(name: &str, arch: Arch, vcpus: u16, gib: u32, price: u64) -> Entry {
        let parts = instance_types::split(name).unwrap();
        Entry {
            name: name.to_owned(),
            family: parts.family.to_owned(),
            category: parts.category.to_owned(),
            generation: parts.generation.unwrap_or(0),
            arch,
            vcpus,
            memory_mib: gib * 1024,
            gpus: 0,
            accelerators: false,
            bare_metal: false,
            burstable: false,
            local_storage: false,
            spot: true,
            zones: vec!["use1-az1".to_owned()],
            on_demand_micros: Some(price),
            spot_micros: None,
        }
    }

    #[test]
    fn built_ins_pick_the_newest_generation_then_the_price() {
        let arm = chosen("hatchrun-4cpu-arm64", "us-east-1", &open()).unwrap();
        assert_eq!(arm[0], "m9g.xlarge");
        assert!(
            arm.iter()
                .all(|name| name.ends_with(".xlarge") && name.starts_with('m'))
        );
        let generations: Vec<u16> = arm
            .iter()
            .map(|name| instance_types::split(name).unwrap().generation.unwrap())
            .collect();
        assert!(
            generations.windows(2).all(|pair| pair[0] >= pair[1]),
            "{arm:?}"
        );
        // Flex families give up sustained full CPU and are left out.
        let amd = chosen("hatchrun-16cpu", "us-east-1", &open()).unwrap();
        assert!(amd.iter().all(|name| !name.contains("flex")), "{amd:?}");
        assert!(amd.iter().all(|name| name.ends_with(".4xlarge")), "{amd:?}");
    }

    #[test]
    fn the_catalog_brings_new_types_and_leaves_out_costly_hardware() {
        let mut gpu = entry("g7.xlarge", Arch::Amd64, 4, 16, 1_000);
        gpu.gpus = 1;
        let mut metal = entry("m9i.metal", Arch::Amd64, 4, 16, 1);
        metal.bare_metal = true;
        let mut burst = entry("t4g.xlarge", Arch::Arm64, 4, 16, 1);
        burst.burstable = true;
        let offered = [
            entry("m8i.xlarge", Arch::Amd64, 4, 16, 200_000),
            entry("m9a.xlarge", Arch::Amd64, 4, 16, 190_000),
            entry("m9i.xlarge", Arch::Amd64, 4, 16, 210_000),
            entry("m9i.2xlarge", Arch::Amd64, 8, 32, 420_000),
            entry("c9i.xlarge", Arch::Amd64, 4, 8, 150_000),
            gpu,
            metal,
            burst,
        ];
        let profile = built_in("hatchrun-4cpu").unwrap();
        let choice = choose(&profile, "us-east-1", &offered, &open()).unwrap();
        assert_eq!(
            choice.instance_types,
            ["m9a.xlarge", "m9i.xlarge", "m8i.xlarge"]
        );
        assert_eq!(
            (choice.vcpus, choice.memory_gib, choice.hourly_micros),
            (4, 16, 190_000)
        );
        // A floor on the generation drops the older one.
        let newer = Profile {
            min_generation: Some(9),
            ..profile.clone()
        };
        assert_eq!(
            choose(&newer, "us-east-1", &offered, &open())
                .unwrap()
                .instance_types,
            ["m9a.xlarge", "m9i.xlarge"]
        );
        // A pin takes what it names, GPUs included.
        let pinned = Profile {
            families: vec!["g7".to_owned()],
            ..profile
        };
        assert_eq!(
            choose(&pinned, "us-east-1", &offered, &open())
                .unwrap()
                .instance_types,
            ["g7.xlarge"]
        );
    }

    #[test]
    fn spot_only_needs_spot() {
        let mut no_spot = entry("m9g.large", Arch::Arm64, 2, 8, 1);
        no_spot.spot = false;
        let profile = Profile {
            capacity: Capacity::Spot,
            ..built_in("hatchrun-2cpu-arm64").unwrap()
        };
        let refused = choose(&profile, "us-east-1", &[no_spot.clone()], &open()).unwrap_err();
        assert!(refused.starts_with("us-east-1 offers no category m instance types for arm64"));
        let fallback = Profile {
            capacity: Capacity::SpotWithFallback,
            ..profile
        };
        assert!(choose(&fallback, "us-east-1", &[no_spot], &open()).is_ok());
    }

    #[test]
    fn the_stack_has_the_final_word() {
        // The connect stack's old defaults allow only the 7th generation.
        assert_eq!(
            chosen(
                "hatchrun-2cpu-arm64",
                "us-east-1",
                &Limits::from_columns(None, None, None, None)
            ),
            Ok(vec!["m7g.large".to_owned()])
        );
        // A size cap with costly hardware excluded.
        let capped = Limits::from_columns(
            Some("*.large,*.xlarge,*.2xlarge"),
            Some("g*,p*,*.metal*"),
            None,
            None,
        );
        assert!(capped.allows("m8g.2xlarge"));
        assert!(!capped.allows("m8g.4xlarge"));
        assert!(!capped.allows("g6.xlarge"));
        assert!(!capped.allows("m7i.metal-24xl"));
        assert!(
            chosen("hatchrun-16cpu", "us-east-1", &capped)
                .unwrap_err()
                .ends_with(
                    "It allows *.large, *.xlarge, *.2xlarge except g*, p*, *.metal*. Change \
                 InstanceTypes on the stack or change the profile."
                )
        );
        let too_big = chosen("hatchrun-4cpu/disk=300", "us-east-1", &limits("*", 200));
        assert!(too_big.unwrap_err().contains("allows at most 200 GiB"));
    }

    #[test]
    fn reports_disk_capacity_and_price() {
        let profile = resolve(
            &labels::parse("hatchrun-2cpu-arm64/spot=true").unwrap(),
            None,
        )
        .unwrap();
        let choice = choose(
            &profile,
            "us-east-1",
            &static_offerings("us-east-1"),
            &open(),
        )
        .unwrap();
        assert_eq!(choice.capacity, Capacity::SpotWithFallback);
        assert_eq!((choice.root_volume, choice.disk_gib), (None, 30));
        assert_eq!(choice.arch, Arch::Arm64);
        assert!(choice.hourly_micros > 0);
    }

    #[test]
    fn static_offerings_follow_the_regions() {
        let stockholm = static_offerings("eu-north-1");
        assert!(stockholm.iter().any(|entry| entry.name == "m7i.large"));
        assert!(!stockholm.iter().any(|entry| entry.family == "m8i"));
        assert_eq!(static_offerings("mars-north-1"), []);
    }

    #[test]
    fn matches_like_iam() {
        assert!(matches(b"m7g.*", b"m7g.large"));
        assert!(matches(b"*", b""));
        assert!(matches(b"m?g.large", b"m8g.large"));
        assert!(!matches(b"m7g.*", b"m7gd.large"));
        assert!(!matches(b"m7g.large", b"m7g.xlarge"));
        assert!(!matches(b"m?g.large", b"m8gd.large"));
        assert!(!matches(b"*.xlarge", b"m8g.2xlarge"));
        assert_eq!(size_name(1), "medium");
        assert_eq!(size_name(16), "4xlarge");
        assert_eq!(size_name(384), "96xlarge");
    }
}
