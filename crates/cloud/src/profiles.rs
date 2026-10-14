// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Runner profiles: the named runners a workflow asks for with
//! `runs-on: <profile>` (see [`crate::labels`]).
//!
//! Every organization has the built-in profiles without setting
//! anything up: `hatchrun-2cpu` to `hatchrun-16cpu` with 4 GiB per vCPU,
//! amd64, and the same with an `-arm64` suffix, all on-demand, plus
//! `hatchrun`, which is `hatchrun-2cpu`. An organization adds its own in
//! the app, and one with the name of a built-in replaces it for that
//! organization.
//!
//! A profile names instance families in order of preference, vCPU and
//! memory bounds, a root volume size (or the connect stack's default),
//! Spot or on-demand, and an image. [`crate::instance`] turns it into
//! instance types for the tenant's region within what its connect stack
//! allows. `extras` and `warm_pool` are kept for features to come and
//! must be empty for now.
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
/// Most families one profile may list.
pub const FAMILIES_MAX: usize = 8;
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
    /// Instance families, preferred first, such as `m8g`.
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
    /// Spot, falling back to on-demand when Spot has no capacity.
    pub spot: bool,
    /// One of [`IMAGES`].
    pub image: String,
    /// Reserved for add-ons such as a run cache; empty for now.
    #[serde(default)]
    pub extras: Vec<String>,
    /// Reserved for warm instances kept ready; 0 for now.
    #[serde(default)]
    pub warm_pool: u16,
}

/// vCPU counts of the built-in sizes.
const BUILT_IN_CPUS: [u16; 4] = [2, 4, 8, 16];
/// Families of the built-ins, preferred first: the newest generation
/// offered in every region hatchrun runs in (Graviton4), or nearly every
/// one (Intel's 8th generation, missing from two), and the previous
/// generation where it is not. On-demand prices per vCPU-hour in
/// us-east-1 were about 4.5 cents for `m8g` and 5.3 cents for `m8i`.
const BUILT_IN_ARM64: [&str; 2] = ["m8g", "m7g"];
const BUILT_IN_AMD64: [&str; 2] = ["m8i", "m7i"];

/// The name of the built-in with `cpu` vCPUs on `arch`.
fn built_in_name(cpu: u16, arch: Arch) -> String {
    match arch {
        Arch::Amd64 => format!("hatchrun-{cpu}cpu"),
        Arch::Arm64 => format!("hatchrun-{cpu}cpu-arm64"),
    }
}

fn built_in_profile(name: String, cpu: u16, arch: Arch) -> Profile {
    let families = match arch {
        Arch::Amd64 => BUILT_IN_AMD64,
        Arch::Arm64 => BUILT_IN_ARM64,
    };
    Profile {
        name,
        arch,
        families: families.iter().map(|family| (*family).to_owned()).collect(),
        cpu_min: cpu,
        cpu_max: Some(cpu),
        memory_min: cpu * 4,
        memory_max: None,
        disk_gib: None,
        spot: false,
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

/// The family called `name` in the catalog.
#[must_use]
pub fn family(name: &str) -> Option<&'static Family> {
    catalog::FAMILIES.iter().find(|family| family.name == name)
}

impl Profile {
    /// Checks the profile on its own, apart from any tenant's region or
    /// limits, and puts the name and families in lowercase.
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
        for family in &mut self.families {
            *family = family.trim().to_ascii_lowercase();
        }
        if self.families.is_empty() {
            return Err("Choose at least one instance family".to_owned());
        }
        if self.families.len() > FAMILIES_MAX {
            return Err(format!("Choose at most {FAMILIES_MAX} instance families"));
        }
        for (at, name) in self.families.iter().enumerate() {
            let Some(family) = family(name) else {
                return Err(format!("{name} is not an instance family hatchrun knows"));
            };
            if family.arch != self.arch {
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
        if !self.families.iter().any(|name| {
            family(name).is_some_and(|family| crate::instance::size_in(family, &self).is_some())
        }) {
            return Err(format!(
                "No size of {} has {} and {}",
                self.families.join(", "),
                self.cpu_text(),
                self.memory_text()
            ));
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
            self.spot = spot;
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
            spot: true,
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
            families: vec!["c8g".to_owned(), "m8g".to_owned()],
            cpu_min: 8,
            cpu_max: Some(16),
            memory_min: 16,
            memory_max: None,
            disk_gib: Some(100),
            spot: true,
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
        assert!(profile.spot);
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
        assert!(!within.spot);
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
        assert_eq!(
            refused(|p| p.families.clear()),
            "Choose at least one instance family"
        );
        assert_eq!(
            refused(|p| p.families = vec!["m8i".to_owned()]),
            "m8i does not run arm64"
        );
        assert_eq!(
            refused(|p| p.families = vec!["x9z".to_owned()]),
            "x9z is not an instance family hatchrun knows"
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
        assert_eq!(
            refused(|p| {
                p.families = vec!["c8g".to_owned()];
                p.cpu_max = Some(8);
                p.memory_min = 64;
            }),
            "No size of c8g has 8 to 8 vCPUs and at least 64 GiB".replace("8 to 8", "8")
        );
    }

    #[test]
    fn says_bounds_in_words() {
        assert_eq!(custom().cpu_text(), "8 to 16 vCPUs");
        assert_eq!(custom().memory_text(), "at least 16 GiB");
        assert_eq!(built_in("hatchrun").unwrap().cpu_text(), "2 vCPUs");
    }

    #[test]
    fn the_catalog_has_the_built_in_families() {
        for name in BUILT_IN_ARM64.iter().chain(&BUILT_IN_AMD64) {
            assert!(family(name).is_some(), "{name}");
        }
        for family in catalog::FAMILIES {
            assert!(!family.sizes.is_empty(), "{}", family.name);
            assert!(family.sizes.windows(2).all(|pair| pair[0] < pair[1]));
            for (region, _) in family.prices {
                assert!(crate::connect::known_region(region), "{region}");
            }
        }
    }
}
