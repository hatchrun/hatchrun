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
/// In YAML it is a list of strings or a single string. Each pattern is
/// compiled once, when the filter is made.
#[derive(Clone, Default)]
pub struct Filter {
    patterns: Vec<String>,
    /// Each pattern, compiled: whether it is negated, and its glob.
    compiled: Vec<(bool, Glob)>,
}

impl Filter {
    /// A filter from patterns in the order they apply.
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        let compiled = patterns
            .iter()
            .map(|pattern| match pattern.strip_prefix('!') {
                Some(rest) => (true, Glob::new(rest)),
                None => (false, Glob::new(pattern)),
            })
            .collect();
        Self { patterns, compiled }
    }

    /// The patterns in the order they apply.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Whether `value` passes the filter.
    ///
    /// Patterns apply in order and the last one that matches decides, so
    /// a `!` pattern after a positive one excludes. A value that no
    /// pattern matches does not pass.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        self.matches_in(value, &mut Scratch::default())
    }

    /// [`Filter::matches`], with buffers kept between calls.
    fn matches_in(&self, value: &str, scratch: &mut Scratch) -> bool {
        let mut split = false;
        // The last pattern that matches decides, so look from the end.
        self.compiled
            .iter()
            .rev()
            .find(|(_, glob)| match glob {
                Glob::Literal(text) => text == value,
                Glob::Prefix(text) => value.starts_with(text.as_str()),
                Glob::Suffix(text) => value.ends_with(text.as_str()),
                Glob::Tokens(toks) => {
                    if !split {
                        scratch.chars.clear();
                        scratch.chars.extend(value.chars());
                        split = true;
                    }
                    glob_matches(toks, scratch)
                }
            })
            .is_some_and(|(negated, _)| !negated)
    }
}

/// Buffers for matching: the value's characters and two position sets.
#[derive(Default)]
struct Scratch {
    chars: Vec<char>,
    at: Vec<bool>,
    next: Vec<bool>,
}

impl PartialEq for Filter {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for Filter {}

impl fmt::Debug for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Filter").field(&self.patterns).finish()
    }
}

/// A compiled pattern.
#[derive(Debug, Clone)]
enum Glob {
    /// A pattern with no wildcard, class or repeat: only this text
    /// matches.
    Literal(String),
    /// Text then `**`: what starts with the text matches.
    Prefix(String),
    /// `**` then text: what ends with the text matches.
    Suffix(String),
    /// Anything else.
    Tokens(Vec<Tok>),
}

impl Glob {
    fn new(pattern: &str) -> Self {
        let toks = compile(pattern);
        let text = |toks: &[Tok]| -> Option<String> {
            toks.iter()
                .map(|t| match t {
                    Tok::Lit(c) => Some(*c),
                    _ => None,
                })
                .collect()
        };
        if let Some(lit) = text(&toks) {
            return Glob::Literal(lit);
        }
        if let Some((Tok::StarStar, rest)) = toks.split_last()
            && let Some(lit) = text(rest)
        {
            return Glob::Prefix(lit);
        }
        if let Some((Tok::StarStar, rest)) = toks.split_first()
            && let Some(lit) = text(rest)
        {
            return Glob::Suffix(lit);
        }
        Glob::Tokens(toks)
    }
}

/// One element of a compiled pattern.
#[derive(Debug, Clone)]
enum Tok {
    /// A literal character.
    Lit(char),
    /// `X?`: zero or one of the single-character token `X`.
    Opt(Box<Tok>),
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
            Tok::Class { negated, items } => {
                items.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated
            }
            Tok::Star | Tok::StarStar | Tok::Plus(_) | Tok::Opt(_) => false,
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
            '+' | '?'
                if toks
                    .last()
                    .is_some_and(|t| matches!(t, Tok::Lit(_) | Tok::Class { .. })) =>
            {
                let prev = Box::new(toks.pop().expect("checked by the guard"));
                if c == '+' {
                    Tok::Plus(prev)
                } else {
                    Tok::Opt(prev)
                }
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

/// Whether the whole of `scratch.chars` matches the tokens. Tracks the
/// set of reachable positions, so the cost stays polynomial for any
/// pattern.
fn glob_matches(toks: &[Tok], scratch: &mut Scratch) -> bool {
    let Scratch { chars, at, next } = scratch;
    let value = &chars[..];
    let n = value.len();
    at.clear();
    at.resize(n + 1, false);
    at[0] = true;
    for tok in toks {
        next.clear();
        next.resize(n + 1, false);
        let mut any = false;
        for p in (0..=n).filter(|&p| at[p]) {
            match tok {
                // A run already reached `p` from further left and went on
                // as far as one from `p` would.
                Tok::Star | Tok::StarStar | Tok::Plus(_) if next[p] => {}
                Tok::Star | Tok::StarStar => {
                    let mut q = p;
                    next[q] = true;
                    while q < n && (matches!(tok, Tok::StarStar) || value[q] != '/') {
                        q += 1;
                        next[q] = true;
                    }
                    any = true;
                }
                Tok::Opt(inner) => {
                    next[p] = true;
                    if p < n && inner.accepts(value[p]) {
                        next[p + 1] = true;
                    }
                    any = true;
                }
                Tok::Plus(inner) => {
                    let mut q = p;
                    while q < n && inner.accepts(value[q]) {
                        q += 1;
                        next[q] = true;
                        any = true;
                    }
                }
                single => {
                    if p < n && single.accepts(value[p]) {
                        next[p + 1] = true;
                        any = true;
                    }
                }
            }
        }
        std::mem::swap(at, next);
        if !any {
            return false;
        }
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
                Ok(Filter::new(vec![value.to_owned()]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Filter, A::Error> {
                let mut patterns = Vec::new();
                while let Some(pattern) = seq.next_element::<String>()? {
                    patterns.push(pattern);
                }
                Ok(Filter::new(patterns))
            }
        }

        deserializer.deserialize_any(FilterVisitor)
    }
}

impl Serialize for Filter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.patterns.len()))?;
        for pattern in &self.patterns {
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
        /// Paths changed by the push. Empty when the caller could not
        /// compute them; path filters then pass, as GitHub runs the
        /// workflow. Path filters are not evaluated for tag pushes.
        changed: Vec<String>,
    },
    /// Activity on a pull request.
    PullRequest {
        /// The activity type, such as `opened`.
        action: String,
        /// The base branch, as a short name.
        base_ref: String,
        /// Paths changed by the pull request. Empty when the caller could
        /// not compute them; path filters then pass.
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
    let is_tag = full_ref.starts_with("refs/tags/");
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
    // GitHub does not evaluate path filters for tag pushes.
    ref_ok && (is_tag || paths_pass(push.paths.as_ref(), push.paths_ignore.as_ref(), changed))
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

/// Applies the path filters: it passes when any changed path passes, or
/// when the changed list is empty (unknown).
fn paths_pass(include: Option<&Filter>, ignore: Option<&Filter>, changed: &[String]) -> bool {
    match (include, ignore) {
        (None, None) => true,
        (Some(_), Some(_)) => false,
        _ if changed.is_empty() => true,
        (Some(include), None) => {
            let mut scratch = Scratch::default();
            changed.iter().any(|p| include.matches_in(p, &mut scratch))
        }
        (None, Some(ignore)) => {
            let mut scratch = Scratch::default();
            changed.iter().any(|p| !ignore.matches_in(p, &mut scratch))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain set-of-positions match, with no shortcuts.
    fn reference(toks: &[Tok], value: &[char]) -> bool {
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
                    Tok::Opt(inner) => {
                        next[p] = true;
                        if p < n && inner.accepts(value[p]) {
                            next[p + 1] = true;
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

    /// Every string of up to `len` pieces drawn from `parts`.
    fn strings(parts: &[&str], len: usize) -> Vec<String> {
        let mut out = vec![String::new()];
        let mut last = vec![String::new()];
        for _ in 0..len {
            last = last
                .iter()
                .flat_map(|s| parts.iter().map(move |p| format!("{s}{p}")))
                .collect();
            out.extend(last.iter().cloned());
        }
        out
    }

    #[test]
    fn compiled_filters_match_like_the_plain_glob() {
        let patterns = strings(
            &[
                "a", "b", "/", "*", "**", "?", "+", "[ab]", "[!a]", "\\*", "!",
            ],
            3,
        );
        let values = strings(&["a", "b", "/", "*"], 4);
        for pattern in &patterns {
            let filter = Filter::new(vec!["x".to_owned(), pattern.clone()]);
            let (negated, body) = match pattern.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern.as_str()),
            };
            let toks = compile(body);
            for value in &values {
                let chars: Vec<char> = value.chars().collect();
                let want = if reference(&toks, &chars) {
                    !negated
                } else {
                    value == "x"
                };
                assert_eq!(filter.matches(value), want, "{pattern:?} on {value:?}");
            }
        }
    }
}
