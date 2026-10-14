// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Runner profiles: the named runners a workflow asks for with
//! `runs-on: <profile>` (see [`crate::labels`]).
//!
//! Every organization has the built-in profiles without setting
//! anything up: `hatchrun-2cpu` to `hatchrun-16cpu`, general purpose
//! (category `m`) with at least 4 GiB per vCPU, amd64, and the same with
//! an `-arm64` suffix, all on-demand, plus `hatchrun`, which is
//! `hatchrun-2cpu`. An organization adds its own in the app, and one
//! with the name of a built-in replaces it for that organization.
//!
//! A profile holds requirements in Karpenter's vocabulary: instance
//! categories, the oldest generation, the architecture, vCPU and memory
//! bounds, a root volume size (or the connect stack's default), the
//! capacity type and an image. Naming instance families is an optional
//! pin. [`crate::instance`] turns it into instance types for the
//! tenant's region within what its connect stack allows, preferring the
//! newest generation and then the lowest price. `extras` and
//! `warm_pool` are kept for features to come and must be empty for now.
//!
//! Everything here but [`store`], which reads and writes the
//! `runner_profiles` table, runs and tests without the Workers runtime.

pub(crate) mod store;

use serde::{Deserialize, Serialize};

use crate::catalog::{self, Family};
use crate::labels::{self, Arch, CPU_MAX, DISK_MAX, DISK_MIN, Overrides, RAM_MAX};

/// Images a profile can run, newest first. The `images` table holds
/// one AMI per region and architecture for each.
pub const IMAGES: &[&str] = &["ubuntu-26.04"];
/// Most families one profile may pin, and categories it may list.
pub const FAMILIES_MAX: usize = 8;
/// The newest generation a profile may ask for at least.
pub const GENERATION_MAX: u16 = 20;
/// Most custom profiles an organization may have.
pub const PROFILES_MAX: usize = 50;

/// A runner profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// What `runs-on` names, lowercase (see [`labels::valid_name`]).
    pub name: String,
    /// CPU architecture.
    pub arch: Arch,
    /// Instance categories, such as `m` for general purpose or `c` for
    /// compute; empty for any.
    #[serde(default)]
    pub categories: Vec<String>,
    /// The oldest generation, such as 7; `None` for any. The newest
    /// generation that fits is preferred either way.
    #[serde(default)]
    pub min_generation: Option<u16>,
    /// Instance families to pin, such as `m8g`; empty for any family
    /// that meets the rest.
    #[serde(default)]
    pub families: Vec<String>,
    /// Fewest vCPUs.
    pub cpu_min: u16,
    /// Most vCPUs, or no limit.
    pub cpu_max: Option<u16>,
    /// Least memory in GiB.
    pub memory_min: u16,
    /// Most memory in GiB, or no limit.
    pub memory_max: Option<u16>,
    /// Root volume size in GiB, or the connect stack's default.
    pub disk_gib: Option<u16>,
    /// On-demand, Spot, or Spot falling back to on-demand.
    #[serde(default)]
    pub capacity: Capacity,
    /// One of [`IMAGES`].
    pub image: String,
    /// Reserved for add-ons such as a run cache; empty for now.
    #[serde(default)]
    pub extras: Vec<String>,
    /// Reserved for warm instances kept ready; 0 for now.
    #[serde(default)]
    pub warm_pool: u16,
}

/// Karpenter's capacity types, with Spot falling back to on-demand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capacity {
    /// On-demand only.
    #[default]
    OnDemand,
    /// Spot only; the job waits while Spot has no capacity.
    Spot,
    /// Spot, then on-demand while Spot has no capacity.
    SpotWithFallback,
}

impl Capacity {
    /// The `runner_profiles.capacity` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on-demand",
            Self::Spot => "spot",
            Self::SpotWithFallback => "spot-with-fallback",
        }
    }

    /// Reads the column value; anything else is on-demand.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "spot" => Self::Spot,
            "spot-with-fallback" => Self::SpotWithFallback,
            _ => Self::OnDemand,
        }
    }

    /// The markets to try in order, `true` for Spot.
    #[must_use]
    pub fn markets(self) -> &'static [bool] {
        match self {
            Self::OnDemand => &[false],
            Self::Spot => &[true],
            Self::SpotWithFallback => &[true, false],
        }
    }

    /// Whether it tries Spot.
    #[must_use]
    pub fn spot(self) -> bool {
        self != Self::OnDemand
    }
}

/// vCPU counts of the built-in sizes.
const BUILT_IN_CPUS: [u16; 4] = [2, 4, 8, 16];
/// The category of the built-ins: general purpose, 4 GiB per vCPU.
const BUILT_IN_CATEGORY: &str = "m";

/// The name of the built-in with `cpu` vCPUs on `arch`.
fn built_in_name(cpu: u16, arch: Arch) -> String {
    match arch {
        Arch::Amd64 => format!("hatchrun-{cpu}cpu"),
        Arch::Arm64 => format!("hatchrun-{cpu}cpu-arm64"),
    }
}

fn built_in_profile(name: String, cpu: u16, arch: Arch) -> Profile {
    Profile {
        name,
        arch,
        categories: vec![BUILT_IN_CATEGORY.to_owned()],
        min_generation: None,
        families: Vec::new(),
        cpu_min: cpu,
        cpu_max: Some(cpu),
        memory_min: cpu * 4,
        memory_max: None,
        disk_gib: None,
        capacity: Capacity::OnDemand,
        image: IMAGES[0].to_owned(),
        extras: Vec::new(),
        warm_pool: 0,
    }
}

/// The built-in profiles, in the order the app lists them.
#[must_use]
pub fn built_ins() -> Vec<Profile> {
    let mut all = vec![built_in_profile(labels::PREFIX.to_owned(), 2, Arch::Amd64)];
    for arch in [Arch::Amd64, Arch::Arm64] {
        for cpu in BUILT_IN_CPUS {
            all.push(built_in_profile(built_in_name(cpu, arch), cpu, arch));
        }
    }
    all
}

/// The built-in profile called `name`.
#[must_use]
pub fn built_in(name: &str) -> Option<Profile> {
    built_ins().into_iter().find(|profile| profile.name == name)
}

/// The family called `name` in the static catalog, which has prices
/// for regions whose instance type catalog was not read yet.
#[must_use]
pub fn family(name: &str) -> Option<&'static Family> {
    catalog::FAMILIES.iter().find(|family| family.name == name)
}

/// Whether `name` reads as an instance family, as in `m8g` or
/// `c7i-flex`.
fn family_name(name: &str) -> bool {
    crate::instance_types::split(&format!("{name}.large"))
        .is_some_and(|parts| parts.family == name && parts.generation.is_some())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl Profile {
    /// Checks the profile on its own, apart from any tenant's region or
    /// limits, and puts the name, categories and families in lowercase.
    ///
    /// # Errors
    ///
    /// What is wrong, in words for the owner.
    pub fn validate(mut self) -> Result<Self, String> {
        self.name = self.name.trim().to_ascii_lowercase();
        if !labels::valid_name(&self.name) {
            return Err(format!(
                "Name the profile hatchrun-something: lowercase letters, digits and single \
                 hyphens, at most {} characters",
                labels::NAME_MAX
            ));
        }
        for item in self.categories.iter_mut().chain(&mut self.families) {
            *item = item.trim().to_ascii_lowercase();
        }
        if self.categories.len() > FAMILIES_MAX {
            return Err(format!("Choose at most {FAMILIES_MAX} instance categories"));
        }
        for (at, name) in self.categories.iter().enumerate() {
            if name.is_empty() || name.len() > 4 || !name.bytes().all(|b| b.is_ascii_lowercase()) {
                return Err(format!(
                    "{name} is not an instance category, such as c, m or r"
                ));
            }
            if self.categories[..at].contains(name) {
                return Err(format!("{name} is listed twice"));
            }
        }
        if self
            .min_generation
            .is_some_and(|generation| !(1..=GENERATION_MAX).contains(&generation))
        {
            return Err(format!("Ask for generation 1 to {GENERATION_MAX}, or any"));
        }
        if self.families.len() > FAMILIES_MAX {
            return Err(format!("Pin at most {FAMILIES_MAX} instance families"));
        }
        for (at, name) in self.families.iter().enumerate() {
            if !family_name(name) {
                return Err(format!("{name} is not an instance family, such as m8g"));
            }
            if family(name).is_some_and(|family| family.arch != self.arch) {
                return Err(format!("{name} does not run {}", self.arch.as_str()));
            }
            if self.families[..at].contains(name) {
                return Err(format!("{name} is listed twice"));
            }
        }
        if !(1..=CPU_MAX).contains(&self.cpu_min) {
            return Err(format!("Ask for 1 to {CPU_MAX} vCPUs"));
        }
        if self
            .cpu_max
            .is_some_and(|max| max < self.cpu_min || max > CPU_MAX)
        {
            return Err(format!(
                "The most vCPUs must be between {} and {CPU_MAX}",
                self.cpu_min
            ));
        }
        if !(1..=RAM_MAX).contains(&self.memory_min) {
            return Err(format!("Ask for 1 to {RAM_MAX} GiB of memory"));
        }
        if self
            .memory_max
            .is_some_and(|max| max < self.memory_min || max > RAM_MAX)
        {
            return Err(format!(
                "The most memory must be between {} and {RAM_MAX} GiB",
                self.memory_min
            ));
        }
        if self
            .disk_gib
            .is_some_and(|disk| !(DISK_MIN..=DISK_MAX).contains(&disk))
        {
            return Err(format!("Give the root volume {DISK_MIN} to {DISK_MAX} GiB"));
        }
        if !IMAGES.contains(&self.image.as_str()) {
            return Err(format!("{} is not an image hatchrun has", self.image));
        }
        if !self.extras.is_empty() {
            return Err("No extras are available yet".to_owned());
        }
        if self.warm_pool != 0 {
            return Err("Warm pools are not available yet".to_owned());
        }
        Ok(self)
    }

    /// This profile with a label's overrides applied. A vCPU or memory
    /// override raises the least the runner gets, and drops the most when
    /// it would be under it.
    #[must_use]
    pub fn with(mut self, overrides: &Overrides) -> Self {
        if let Some(cpu) = overrides.cpu {
            self.cpu_min = cpu;
            self.cpu_max = self.cpu_max.filter(|max| *max >= cpu);
        }
        if let Some(ram) = overrides.ram_gib {
            self.memory_min = ram;
            self.memory_max = self.memory_max.filter(|max| *max >= ram);
        }
        if let Some(disk) = overrides.disk_gib {
            self.disk_gib = Some(disk);
        }
        if let Some(spot) = overrides.spot {
            self.capacity = if spot {
                Capacity::SpotWithFallback
            } else {
                Capacity::OnDemand
            };
        }
        self
    }

    /// The vCPU bounds in words, as in `4 vCPUs` or `2 to 8 vCPUs`.
    #[must_use]
    pub fn cpu_text(&self) -> String {
        bounds(self.cpu_min, self.cpu_max, "vCPUs")
    }

    /// The memory bounds in words.
    #[must_use]
    pub fn memory_text(&self) -> String {
        bounds(self.memory_min, self.memory_max, "GiB")
    }
}

fn bounds(min: u16, max: Option<u16>, unit: &str) -> String {
    match max {
        Some(max) if max == min => format!("{min} {unit}"),
        Some(max) => format!("{min} to {max} {unit}"),
        None => format!("at least {min} {unit}"),
    }
}

/// The profile a label names: the organization's own when it has one by
/// that name, the built-in otherwise, with the label's overrides.
///
/// # Errors
///
/// When neither exists.
pub fn resolve(spec: &labels::Spec, custom: Option<Profile>) -> Result<Profile, String> {
    custom
        .or_else(|| built_in(&spec.profile))
        .map(|profile| profile.with(&spec.overrides))
        .ok_or_else(|| format!("no runner profile {}", spec.profile))
}

/// Where a profile comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// A built-in nobody changed.
    BuiltIn,
    /// The organization's own version of a built-in.
    Replaced,
    /// The organization's own.
    Custom,
}

/// Every profile an organization's workflows can name, given its own:
/// the built-ins first, each replaced by the organization's version
/// when it has one, then its own by name.
#[must_use]
pub fn merged(custom: Vec<Profile>) -> Vec<(Profile, Source)> {
    let mut all: Vec<(Profile, Source)> = built_ins()
        .into_iter()
        .map(
            |built_in| match custom.iter().find(|profile| profile.name == built_in.name) {
                Some(mine) => (mine.clone(), Source::Replaced),
                None => (built_in, Source::BuiltIn),
            },
        )
        .collect();
    let mut own: Vec<Profile> = custom
        .into_iter()
        .filter(|profile| built_in(&profile.name).is_none())
        .collect();
    own.sort_by(|a, b| a.name.cmp(&b.name));
    all.extend(own.into_iter().map(|profile| (profile, Source::Custom)));
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_built_ins_then_custom() {
        let replaced = Profile {
            name: "hatchrun-4cpu".to_owned(),
            capacity: Capacity::Spot,
            ..built_in("hatchrun-4cpu").unwrap()
        };
        let all = merged(vec![
            Profile {
                name: "hatchrun-zz".to_owned(),
                ..custom()
            },
            custom(),
            replaced.clone(),
        ]);
        let names: Vec<(&str, Source)> = all
            .iter()
            .map(|(profile, source)| (profile.name.as_str(), *source))
            .collect();
        assert_eq!(names.len(), built_ins().len() + 2);
        assert_eq!(names[2], ("hatchrun-4cpu", Source::Replaced));
        assert_eq!(all[2].0, replaced);
        assert_eq!(names[0], ("hatchrun", Source::BuiltIn));
        assert_eq!(
            names[names.len() - 2..],
            [
                ("hatchrun-big", Source::Custom),
                ("hatchrun-zz", Source::Custom)
            ]
        );
    }

    fn custom() -> Profile {
        Profile {
            name: "hatchrun-big".to_owned(),
            arch: Arch::Arm64,
            categories: vec!["c".to_owned(), "m".to_owned()],
            min_generation: Some(7),
            families: Vec::new(),
            cpu_min: 8,
            cpu_max: Some(16),
            memory_min: 16,
            memory_max: None,
            disk_gib: Some(100),
            capacity: Capacity::SpotWithFallback,
            image: IMAGES[0].to_owned(),
            extras: Vec::new(),
            warm_pool: 0,
        }
    }

    #[test]
    fn built_ins_follow_the_naming() {
        let names: Vec<String> = built_ins()
            .into_iter()
            .map(|profile| profile.name)
            .collect();
        assert_eq!(
            names,
            [
                "hatchrun",
                "hatchrun-2cpu",
                "hatchrun-4cpu",
                "hatchrun-8cpu",
                "hatchrun-16cpu",
                "hatchrun-2cpu-arm64",
                "hatchrun-4cpu-arm64",
                "hatchrun-8cpu-arm64",
                "hatchrun-16cpu-arm64",
            ]
        );
        for profile in built_ins() {
            assert_eq!(profile.clone().validate(), Ok(profile));
        }
        let arm = built_in("hatchrun-4cpu-arm64").unwrap();
        assert_eq!(
            (arm.arch, arm.cpu_min, arm.memory_min),
            (Arch::Arm64, 4, 16)
        );
        assert_eq!(built_in("hatchrun-3cpu"), None);
    }

    #[test]
    fn custom_profiles_win_over_built_ins() {
        let spec = labels::parse("hatchrun-4cpu").unwrap();
        let mine = Profile {
            name: "hatchrun-4cpu".to_owned(),
            ..custom()
        };
        assert_eq!(resolve(&spec, Some(mine.clone())), Ok(mine));
        assert_eq!(resolve(&spec, None), Ok(built_in("hatchrun-4cpu").unwrap()));
        assert_eq!(
            resolve(&labels::parse("hatchrun-nope").unwrap(), None),
            Err("no runner profile hatchrun-nope".to_owned())
        );
    }

    #[test]
    fn overrides_change_the_profile() {
        let spec = labels::parse("hatchrun-4cpu/disk=200/spot=true/cpu=8").unwrap();
        let profile = resolve(&spec, None).unwrap();
        assert_eq!(profile.disk_gib, Some(200));
        assert_eq!(profile.capacity, Capacity::SpotWithFallback);
        assert_eq!((profile.cpu_min, profile.cpu_max), (8, None));
        assert_eq!(profile.memory_min, 16);

        let within = custom().with(&Overrides {
            cpu: Some(12),
            ram_gib: Some(32),
            spot: Some(false),
            ..Overrides::default()
        });
        assert_eq!((within.cpu_min, within.cpu_max), (12, Some(16)));
        assert_eq!(within.memory_min, 32);
        assert_eq!(within.capacity, Capacity::OnDemand);
    }

    #[test]
    fn validates_profiles() {
        let refused = |change: fn(&mut Profile)| {
            let mut profile = custom();
            change(&mut profile);
            profile.validate().unwrap_err()
        };
        assert_eq!(
            Profile {
                name: " HatchRun-Big ".to_owned(),
                families: vec!["C8G".to_owned()],
                ..custom()
            }
            .validate()
            .map(|profile| (profile.name, profile.families)),
            Ok(("hatchrun-big".to_owned(), vec!["c8g".to_owned()]))
        );
        assert!(refused(|p| p.name = "big".to_owned()).starts_with("Name the profile"));
        // Families AWS adds later are fine; known ones must fit the arch.
        assert_eq!(
            Profile {
                families: vec!["m11g".to_owned()],
                ..custom()
            }
            .validate()
            .map(|profile| profile.families),
            Ok(vec!["m11g".to_owned()])
        );
        assert_eq!(
            refused(|p| p.families = vec!["m8i".to_owned()]),
            "m8i does not run arm64"
        );
        assert_eq!(
            refused(|p| p.families = vec!["m8g.large".to_owned()]),
            "m8g.large is not an instance family, such as m8g"
        );
        assert_eq!(
            refused(|p| p.categories = vec!["m".to_owned(), "M".to_owned()]),
            "m is listed twice"
        );
        assert_eq!(
            refused(|p| p.categories = vec!["m8".to_owned()]),
            "m8 is not an instance category, such as c, m or r"
        );
        assert_eq!(
            refused(|p| p.min_generation = Some(0)),
            "Ask for generation 1 to 20, or any"
        );
        assert_eq!(
            refused(|p| p.families = vec!["m8g".to_owned(), "m8g".to_owned()]),
            "m8g is listed twice"
        );
        assert_eq!(refused(|p| p.cpu_min = 0), "Ask for 1 to 384 vCPUs");
        assert_eq!(
            refused(|p| p.cpu_max = Some(4)),
            "The most vCPUs must be between 8 and 384"
        );
        assert_eq!(
            refused(|p| p.memory_max = Some(8)),
            "The most memory must be between 16 and 3072 GiB"
        );
        assert_eq!(
            refused(|p| p.disk_gib = Some(5)),
            "Give the root volume 10 to 16384 GiB"
        );
        assert_eq!(
            refused(|p| p.image = "windows".to_owned()),
            "windows is not an image hatchrun has"
        );
        assert_eq!(
            refused(|p| p.extras = vec!["cache".to_owned()]),
            "No extras are available yet"
        );
        assert_eq!(
            refused(|p| p.warm_pool = 2),
            "Warm pools are not available yet"
        );
    }

    #[test]
    fn capacity_types() {
        assert_eq!(Capacity::OnDemand.markets(), [false]);
        assert_eq!(Capacity::Spot.markets(), [true]);
        assert_eq!(Capacity::SpotWithFallback.markets(), [true, false]);
        for capacity in [
            Capacity::OnDemand,
            Capacity::Spot,
            Capacity::SpotWithFallback,
        ] {
            assert_eq!(Capacity::parse(capacity.as_str()), capacity);
        }
        assert_eq!(
            serde_json::to_string(&Capacity::SpotWithFallback).unwrap(),
            "\"spot-with-fallback\""
        );
    }

    #[test]
    fn says_bounds_in_words() {
        assert_eq!(custom().cpu_text(), "8 to 16 vCPUs");
        assert_eq!(custom().memory_text(), "at least 16 GiB");
        assert_eq!(built_in("hatchrun").unwrap().cpu_text(), "2 vCPUs");
    }

    #[test]
    fn the_static_catalog_is_ordered() {
        for family in catalog::FAMILIES {
            assert!(!family.sizes.is_empty(), "{}", family.name);
            assert!(family.sizes.windows(2).all(|pair| pair[0] < pair[1]));
            for (region, _) in family.prices {
                assert!(crate::connect::known_region(region), "{region}");
            }
        }
    }
}
