// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `on:` block of a workflow, with GitHub's event and filter names.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

use crate::config::Parameter;

/// Ordered GitHub filter patterns. A leading `!` negates a pattern.
///
/// In YAML it is a list of strings or a single string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter(Vec<String>);

impl Filter {
    /// A filter from patterns in the order they apply.
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        Self(patterns)
    }

    /// The patterns in the order they apply.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FilterVisitor;

        impl<'de> Visitor<'de> for FilterVisitor {
            type Value = Filter;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a pattern or a list of patterns")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Filter, E> {
                Ok(Filter(vec![value.to_owned()]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Filter, A::Error> {
                let mut patterns = Vec::new();
                while let Some(pattern) = seq.next_element::<String>()? {
                    patterns.push(pattern);
                }
                Ok(Filter(patterns))
            }
        }

        deserializer.deserialize_any(FilterVisitor)
    }
}

impl Serialize for Filter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for pattern in &self.0 {
            seq.serialize_element(pattern)?;
        }
        seq.end()
    }
}

/// The events that start a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    /// A push of a branch or a tag.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub push: Option<PushTrigger>,
    /// Activity on a pull request.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub pull_request: Option<PullRequestTrigger>,
    /// Cron schedules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<Schedule>,
    /// A manual start, with inputs.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub workflow_dispatch: Option<Dispatch>,
}

/// Filters for `on: push`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PushTrigger {
    /// Branches that trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Filter>,
    /// Branches that do not trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches_ignore: Option<Filter>,
    /// Tags that trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Filter>,
    /// Tags that do not trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_ignore: Option<Filter>,
    /// Changed paths that trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Filter>,
    /// Changed paths that do not trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths_ignore: Option<Filter>,
}

/// Filters for `on: pull_request`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PullRequestTrigger {
    /// Activity types; `opened`, `synchronize` and `reopened` when absent.
    pub types: Vec<String>,
    /// Base branches that trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<Filter>,
    /// Base branches that do not trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches_ignore: Option<Filter>,
    /// Changed paths that trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Filter>,
    /// Changed paths that do not trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths_ignore: Option<Filter>,
}

impl Default for PullRequestTrigger {
    fn default() -> Self {
        Self {
            types: ["opened", "synchronize", "reopened"]
                .map(String::from)
                .to_vec(),
            branches: None,
            branches_ignore: None,
            paths: None,
            paths_ignore: None,
        }
    }
}

/// One entry of `on: schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// A five-field cron expression, in UTC.
    pub cron: String,
}

/// `on: workflow_dispatch`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dispatch {
    /// Inputs asked for when the workflow is started by hand.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, Parameter>,
}

/// Reads a present key with an empty value (`pull_request:`) as the
/// trigger with no filters, as GitHub does.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Some(
        Option::<T>::deserialize(deserializer)?.unwrap_or_default(),
    ))
}
