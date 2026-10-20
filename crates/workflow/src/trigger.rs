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

    /// Whether `value` passes the filter.
    ///
    /// Patterns apply in order and the last one that matches decides, so
    /// a `!` pattern after a positive one excludes. A value that no
    /// pattern matches does not pass.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        let chars: Vec<char> = value.chars().collect();
        let mut passes = false;
        for pattern in &self.0 {
            let (negated, body) = match pattern.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern.as_str()),
            };
            if glob_matches(&compile(body), &chars) {
                passes = !negated;
            }
        }
        passes
    }
}

/// One element of a compiled pattern.
#[derive(Debug)]
enum Tok {
    /// A literal character.
    Lit(char),
    /// `?`: any one character.
    One,
    /// `[...]`: a character class.
    Class {
        negated: bool,
        items: Vec<(char, char)>,
    },
    /// `*`: any run of characters except `/`.
    Star,
    /// `**`: any run of characters.
    StarStar,
    /// `X+`: one or more of the single-character token `X`.
    Plus(Box<Tok>),
}

impl Tok {
    /// Whether this single-character token accepts `c`.
    fn accepts(&self, c: char) -> bool {
        match self {
            Tok::Lit(l) => *l == c,
            Tok::One => true,
            Tok::Class { negated, items } => {
                items.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated
            }
            Tok::Star | Tok::StarStar | Tok::Plus(_) => false,
        }
    }
}

/// Translates a GitHub filter pattern into tokens.
fn compile(pattern: &str) -> Vec<Tok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        let tok = match c {
            '*' if chars.get(i) == Some(&'*') => {
                i += 1;
                Tok::StarStar
            }
            '*' => Tok::Star,
            '?' => Tok::One,
            '\\' if i < chars.len() => {
                i += 1;
                Tok::Lit(chars[i - 1])
            }
            '[' => match class(&chars, i) {
                Some((tok, next)) => {
                    i = next;
                    tok
                }
                None => Tok::Lit('['),
            },
            '+' if toks
                .last()
                .is_some_and(|t| matches!(t, Tok::Lit(_) | Tok::One | Tok::Class { .. })) =>
            {
                let prev = toks.pop().expect("checked by the guard");
                Tok::Plus(Box::new(prev))
            }
            other => Tok::Lit(other),
        };
        toks.push(tok);
    }
    toks
}

/// Reads a character class whose `[` sits just before `start`. Returns the
/// token and the index after the closing `]`, or `None` when unterminated.
fn class(chars: &[char], start: usize) -> Option<(Tok, usize)> {
    let mut i = start;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let c = *chars.get(i)?;
        i += 1;
        if c == ']' && !first {
            return Some((Tok::Class { negated, items }, i));
        }
        first = false;
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|&e| e != ']') {
            items.push((c, chars[i + 1]));
            i += 2;
        } else {
            items.push((c, c));
        }
    }
}

/// Whether the whole of `value` matches the tokens. Tracks the set of
/// reachable positions, so the cost stays polynomial for any pattern.
fn glob_matches(toks: &[Tok], value: &[char]) -> bool {
    let n = value.len();
    let mut at = vec![false; n + 1];
    at[0] = true;
    for tok in toks {
        let mut next = vec![false; n + 1];
        for p in (0..=n).filter(|&p| at[p]) {
            match tok {
                Tok::Star | Tok::StarStar => {
                    let mut q = p;
                    next[q] = true;
                    while q < n && (matches!(tok, Tok::StarStar) || value[q] != '/') {
                        q += 1;
                        next[q] = true;
                    }
                }
                Tok::Plus(inner) => {
                    let mut q = p;
                    while q < n && inner.accepts(value[q]) {
                        q += 1;
                        next[q] = true;
                    }
                }
                single => {
                    if p < n && single.accepts(value[p]) {
                        next[p + 1] = true;
                    }
                }
            }
        }
        at = next;
    }
    at[n]
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

/// A GitHub event, reduced to what trigger matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A push. `ref` is the full ref (`refs/heads/main`, `refs/tags/v1`).
    Push {
        /// The pushed ref.
        r#ref: String,
        /// Paths changed by the push.
        changed: Vec<String>,
    },
    /// Activity on a pull request.
    PullRequest {
        /// The activity type, such as `opened`.
        action: String,
        /// The base branch, as a short name.
        base_ref: String,
        /// Paths changed by the pull request.
        changed: Vec<String>,
    },
    /// A cron schedule firing.
    Schedule {
        /// The cron string of the schedule that fired.
        cron: String,
    },
    /// A manual start.
    Dispatch,
}

/// Whether `event` starts a workflow with the triggers `on`.
#[must_use]
pub fn triggered(on: &Triggers, event: &Event) -> bool {
    match event {
        Event::Push { r#ref, changed } => on
            .push
            .as_ref()
            .is_some_and(|push| push_matches(push, r#ref, changed)),
        Event::PullRequest {
            action,
            base_ref,
            changed,
        } => on.pull_request.as_ref().is_some_and(|pr| {
            pr.types.iter().any(|t| t == action)
                && ref_passes(pr.branches.as_ref(), pr.branches_ignore.as_ref(), base_ref)
                    == Some(true)
                && paths_pass(pr.paths.as_ref(), pr.paths_ignore.as_ref(), changed)
        }),
        Event::Schedule { cron } => on.schedule.iter().any(|s| &s.cron == cron),
        Event::Dispatch => on.workflow_dispatch.is_some(),
    }
}

fn push_matches(push: &PushTrigger, full_ref: &str, changed: &[String]) -> bool {
    let has_branch_filter = push.branches.is_some() || push.branches_ignore.is_some();
    let has_tag_filter = push.tags.is_some() || push.tags_ignore.is_some();
    let (name, own_filter, other_filter, include, ignore) =
        if let Some(name) = full_ref.strip_prefix("refs/heads/") {
            (
                name,
                has_branch_filter,
                has_tag_filter,
                &push.branches,
                &push.branches_ignore,
            )
        } else if let Some(name) = full_ref.strip_prefix("refs/tags/") {
            (
                name,
                has_tag_filter,
                has_branch_filter,
                &push.tags,
                &push.tags_ignore,
            )
        } else {
            return false;
        };
    // A trigger that filters only the other kind of ref never fires here.
    let ref_ok = if own_filter {
        ref_passes(include.as_ref(), ignore.as_ref(), name) == Some(true)
    } else {
        !other_filter
    };
    ref_ok && paths_pass(push.paths.as_ref(), push.paths_ignore.as_ref(), changed)
}

/// Applies an include filter and an ignore filter to a short ref name.
/// `None` means both were given, which never matches.
fn ref_passes(include: Option<&Filter>, ignore: Option<&Filter>, name: &str) -> Option<bool> {
    match (include, ignore) {
        (Some(_), Some(_)) => None,
        (Some(include), None) => Some(include.matches(name)),
        (None, Some(ignore)) => Some(!ignore.matches(name)),
        (None, None) => Some(true),
    }
}

/// Applies the path filters: it passes when any changed path passes.
fn paths_pass(include: Option<&Filter>, ignore: Option<&Filter>, changed: &[String]) -> bool {
    match (include, ignore) {
        (None, None) => true,
        (Some(_), Some(_)) => false,
        (Some(include), None) => changed.iter().any(|p| include.matches(p)),
        (None, Some(ignore)) => changed.iter().any(|p| !ignore.matches(p)),
    }
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
