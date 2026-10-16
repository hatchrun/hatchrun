// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A tenant's settings, checked apart from the Workers runtime so the
//! rules test natively.

use crate::connect;

/// Most runners a tenant may run at once. AWS's own instance quotas
/// bind long before this; it stops a typo.
pub const MAX_RUNNERS: u32 = 1000;

/// Settings as the app sends them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Runners launching or running at once.
    pub max_runners: u32,
    /// Regions runners may use; `None` for every region.
    pub allowed_regions: Option<Vec<String>>,
}

impl Settings {
    /// Checks the settings and puts the regions in [`connect::REGIONS`]
    /// order without repeats.
    ///
    /// # Errors
    ///
    /// What is wrong, in words for the owner.
    pub fn validate(mut self) -> Result<Self, String> {
        if self.max_runners > MAX_RUNNERS {
            return Err(format!("At most {MAX_RUNNERS} runners at once"));
        }
        if let Some(regions) = &self.allowed_regions {
            if let Some(unknown) = regions.iter().find(|region| !connect::known_region(region)) {
                return Err(format!("{unknown} is not a region hatchrun runs in"));
            }
            if regions.is_empty() {
                return Err("Allow at least one region".to_owned());
            }
            self.allowed_regions = Some(
                connect::REGIONS
                    .iter()
                    .filter(|(id, _)| regions.iter().any(|region| region == id))
                    .map(|(id, _)| (*id).to_owned())
                    .collect(),
            );
        }
        Ok(self)
    }

    /// The `allowed_regions` column value.
    #[must_use]
    pub fn regions_column(&self) -> Option<String> {
        self.allowed_regions
            .as_ref()
            .map(|regions| regions.join(","))
    }
}

/// Whether `region` is allowed by an `allowed_regions` column value.
#[must_use]
pub fn region_allowed(region: &str, column: Option<&str>) -> bool {
    column.is_none_or(|regions| regions.split(',').any(|allowed| allowed.trim() == region))
}

/// An `allowed_regions` column value as a list; `None` for every region.
#[must_use]
pub fn regions(column: Option<&str>) -> Option<Vec<String>> {
    column.map(|regions| {
        regions
            .split(',')
            .map(str::trim)
            .filter(|region| !region.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(max_runners: u32, regions: Option<&[&str]>) -> Settings {
        Settings {
            max_runners,
            allowed_regions: regions
                .map(|regions| regions.iter().map(|region| (*region).to_owned()).collect()),
        }
    }

    #[test]
    fn orders_and_dedupes_regions() {
        let checked = settings(10, Some(&["eu-west-1", "us-east-1", "eu-west-1"]))
            .validate()
            .unwrap();
        assert_eq!(
            checked.regions_column().as_deref(),
            Some("us-east-1,eu-west-1")
        );
        assert_eq!(settings(0, None).validate().unwrap().regions_column(), None);
    }

    #[test]
    fn refuses_bad_settings() {
        assert_eq!(
            settings(1001, None).validate().unwrap_err(),
            "At most 1000 runners at once"
        );
        assert_eq!(
            settings(1, Some(&["mars-1"])).validate().unwrap_err(),
            "mars-1 is not a region hatchrun runs in"
        );
        assert_eq!(
            settings(1, Some(&[])).validate().unwrap_err(),
            "Allow at least one region"
        );
    }

    #[test]
    fn checks_regions_against_the_column() {
        assert!(region_allowed("eu-west-1", None));
        assert!(region_allowed("eu-west-1", Some("us-east-1,eu-west-1")));
        assert!(!region_allowed("eu-west-2", Some("us-east-1,eu-west-1")));
        assert_eq!(
            regions(Some("us-east-1, eu-west-1,")),
            Some(vec!["us-east-1".to_owned(), "eu-west-1".to_owned()])
        );
        assert_eq!(regions(None), None);
    }
}
