// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `on:` block of a workflow, with GitHub's event and filter names.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

use crate::call::CallTrigger;
use crate::config::Parameter;

/// Ordered GitHub filter patterns. A leading `!` negates a pattern.
///
/// In YAML it is a list of strings or a single string. A pattern that
/// is plain text, text then `**`, or `**` then text is matched as text;
/// that shape is worked out when the filter is made and costs a byte per
/// pattern. Any other pattern is compiled when it is matched, so a large
/// filter takes no more memory than its text.
#[derive(Clone, Default)]
pub struct Filter {
    patterns: Vec<String>,
    /// The shape of each pattern.
    shapes: Vec<Shape>,
}

impl Filter {
    /// A filter from patterns in the order they apply.
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        let shapes = patterns.iter().map(|p| Shape::of(body(p))).collect();
        Self { patterns, shapes }
    }

    /// The patterns in the order they apply.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// How many patterns are compiled when matched: those that are not
    /// plain text, text then `**`, or `**` then text.
    pub(crate) fn globs(&self) -> usize {
        self.shapes
            .iter()
            .filter(|s| matches!(s, Shape::Glob))
            .count()
    }

    /// Whether `value` passes the filter.
    ///
    /// Patterns apply in order and the last one that matches decides, so
    /// a `!` pattern after a positive one excludes. A value that no
    /// pattern matches does not pass.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        let mut scratch = Scratch::with_work(usize::MAX);
        self.matches_in(value, &mut scratch).unwrap_or(true)
    }

    /// [`Filter::matches`], with buffers and compiled patterns kept
    /// between calls. `None` when the scratch's work ran out first.
    fn matches_in(&self, value: &str, scratch: &mut Scratch) -> Option<bool> {
        let mut split = false;
        // The last pattern that matches decides, so look from the end.
        for i in (0..self.patterns.len()).rev() {
            let text = body(&self.patterns[i]);
            let matched = match self.shapes[i] {
                Shape::Literal => text == value,
                Shape::Prefix => text
                    .strip_suffix("**")
                    .is_some_and(|head| value.starts_with(head)),
                Shape::Suffix => text
                    .strip_prefix("**")
                    .is_some_and(|tail| value.ends_with(tail)),
                // A pattern whose leading text the value does not start
                // with cannot match; that takes no work.
                Shape::Glob if prefix_differs(text, value) => false,
                Shape::Glob => {
                    if !split {
                        scratch.buffers.set(value);
                        split = true;
                    }
                    scratch.glob(i, text)?
                }
            };
            if matched {
                return Some(!self.patterns[i].starts_with('!'));
            }
        }
        Some(false)
    }
}

/// Whether `value` differs from the text a glob pattern starts with,
/// the characters before its first wildcard, class, repeat or escape,
/// so the pattern cannot match. A character that `?` or `+` follows is
/// not checked. Stops at the first difference.
fn prefix_differs(pattern: &str, value: &str) -> bool {
    let mut value = value.chars();
    let mut pattern = pattern.chars().peekable();
    while let Some(c) = pattern.next() {
        if matches!(c, '*' | '?' | '+' | '[' | '\\') || matches!(pattern.peek(), Some('?' | '+')) {
            return false;
        }
        if value.next() != Some(c) {
            return true;
        }
    }
    false
}

/// A pattern without its leading `!`.
fn body(pattern: &str) -> &str {
    pattern.strip_prefix('!').unwrap_or(pattern)
}

/// How a pattern is matched.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// No wildcard, class, repeat or escape: only the text matches.
    Literal,
    /// Such text then `**`: what starts with the text matches.
    Prefix,
    /// `**` then such text: what ends with the text matches.
    Suffix,
    /// Anything else, compiled to tokens when matched.
    Glob,
}

impl Shape {
    fn of(pattern: &str) -> Self {
        let plain = |t: &str| !t.contains(['*', '?', '+', '[', '\\']);
        if plain(pattern) {
            Shape::Literal
        } else if let Some(text) = pattern.strip_suffix("**")
            && plain(text)
        {
            Shape::Prefix
        } else if let Some(text) = pattern.strip_prefix("**")
            && plain(text)
        {
            Shape::Suffix
        } else {
            Shape::Glob
        }
    }
}

/// The most tokens one matching call keeps compiled, about 2 MiB:
/// every pattern of a filter at the 256-pattern limit, at up to 256
/// characters each. Past it, patterns are compiled again for each value.
const CACHED_TOKENS: usize = 1 << 16;

/// What matching keeps between values: buffers, the patterns compiled
/// so far while they fit [`CACHED_TOKENS`], and the work left.
struct Scratch {
    buffers: Buffers,
    compiled: Vec<Option<Vec<Tok>>>,
    cached: usize,
    /// Steps left: a compiled character, or a position looked at for
    /// one token.
    left: usize,
}

impl Scratch {
    fn with_work(left: usize) -> Self {
        Self {
            buffers: Buffers::default(),
            compiled: Vec::new(),
            cached: 0,
            left,
        }
    }

    /// Takes `steps` from the work left; false when there is not enough.
    fn spend(&mut self, steps: usize) -> bool {
        match self.left.checked_sub(steps) {
            Some(left) => {
                self.left = left;
                true
            }
            None => false,
        }
    }

    /// Whether `buffers.chars` matches pattern `i`, whose text is `text`.
    /// `None` when the work runs out.
    fn glob(&mut self, i: usize, text: &str) -> Option<bool> {
        if let Some(Some(toks)) = self.compiled.get(i) {
            return glob_matches(toks, &mut self.buffers, &mut self.left);
        }
        if !self.spend(text.len()) {
            return None;
        }
        let toks = compile(text);
        let matched = glob_matches(&toks, &mut self.buffers, &mut self.left);
        if self.cached + toks.len() <= CACHED_TOKENS {
            self.cached += toks.len();
            if self.compiled.len() <= i {
                self.compiled.resize_with(i + 1, || None);
            }
            self.compiled[i] = Some(toks);
        }
        matched
    }
}

/// The value's characters, the positions reachable before and after
/// the current token in ascending order, and a mark per position that
/// says it was reached by the current token.
#[derive(Default)]
struct Buffers {
    chars: Vec<char>,
    at: Vec<usize>,
    next: Vec<usize>,
    marks: Vec<u32>,
    /// The mark of the current token.
    mark: u32,
}

impl Buffers {
    /// Makes `value` the value matched.
    fn set(&mut self, value: &str) {
        self.chars.clear();
        self.chars.extend(value.chars());
        self.marks.clear();
        self.marks.resize(self.chars.len() + 1, 0);
        self.mark = 0;
    }
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

/// One element of a compiled pattern.
#[derive(Debug)]
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
            '+' | '?' => match toks.pop() {
                Some(prev @ (Tok::Lit(_) | Tok::Class { .. })) if c == '+' => {
                    Tok::Plus(Box::new(prev))
                }
                Some(prev @ (Tok::Lit(_) | Tok::Class { .. })) => Tok::Opt(Box::new(prev)),
                Some(other) => {
                    toks.push(other);
                    Tok::Lit(c)
                }
                None => Tok::Lit(c),
            },
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

/// Adds position `q` to `next` unless the current token, marked `m`,
/// already reached it.
fn reach(marks: &mut [u32], m: u32, q: usize, next: &mut Vec<usize>) {
    if marks[q] != m {
        marks[q] = m;
        next.push(q);
    }
}

/// Whether the whole of `buffers.chars` matches the tokens. Tracks the
/// reachable positions, so the cost stays polynomial for any pattern.
/// Each reachable position a token looks at, and each position a run
/// walks over, is one step charged to `left`; `None` when it runs out.
fn glob_matches(toks: &[Tok], buffers: &mut Buffers, left: &mut usize) -> Option<bool> {
    let Buffers {
        chars,
        at,
        next,
        marks,
        mark,
    } = buffers;
    let value = &chars[..];
    let n = value.len();
    at.clear();
    at.push(0);
    for tok in toks {
        next.clear();
        *mark = mark.wrapping_add(1);
        if *mark == 0 {
            marks.fill(0);
            *mark = 1;
        }
        let m = *mark;
        // Positions are visited in ascending order and every run walks
        // right, so `next` stays ascending.
        *left = left.checked_sub(at.len())?;
        for &p in at.iter() {
            match tok {
                // A run already reached `p` from further left and went on
                // as far as one from `p` would.
                Tok::Star | Tok::StarStar | Tok::Plus(_) if marks[p] == m => {}
                Tok::Star | Tok::StarStar => {
                    let mut q = p;
                    reach(marks, m, q, next);
                    while q < n && (matches!(tok, Tok::StarStar) || value[q] != '/') {
                        *left = left.checked_sub(1)?;
                        q += 1;
                        reach(marks, m, q, next);
                    }
                }
                Tok::Opt(inner) => {
                    reach(marks, m, p, next);
                    if p < n && inner.accepts(value[p]) {
                        reach(marks, m, p + 1, next);
                    }
                }
                Tok::Plus(inner) => {
                    let mut q = p;
                    while q < n && inner.accepts(value[q]) {
                        *left = left.checked_sub(1)?;
                        q += 1;
                        reach(marks, m, q, next);
                    }
                }
                single => {
                    if p < n && single.accepts(value[p]) {
                        reach(marks, m, p + 1, next);
                    }
                }
            }
        }
        std::mem::swap(at, next);
        if at.is_empty() {
            return Some(false);
        }
    }
    Some(at.last() == Some(&n))
}

/// Reads a string or a list of strings into a list.
fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
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
    /// Activity on a pull request, run from the base branch's config
    /// with the base repository's secrets. It takes the filters and
    /// types of `pull_request`.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub pull_request_target: Option<PullRequestTrigger>,
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
    /// Another workflow of the config finishing.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub workflow_run: Option<WorkflowRunTrigger>,
    /// Activity on a release.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub release: Option<ReleaseTrigger>,
    /// A branch or tag being created. GitHub has no filters for it.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub create: Option<RefTrigger>,
    /// A branch or tag being deleted. GitHub has no filters for it.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub delete: Option<RefTrigger>,
    /// Another workflow calling this one. No event starts it. See
    /// [`crate::call`].
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub call: Option<CallTrigger>,
    /// A pull request entering the merge queue, for its checks.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub merge_group: Option<MergeGroupTrigger>,
    /// A `repository_dispatch` request sent to the repository.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub repository_dispatch: Option<RepositoryDispatchTrigger>,
    /// Activity on an issue. A plain issue and a pull request's
    /// conversation are different places: a pull request is not an issue
    /// event here.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub issues: Option<IssuesTrigger>,
    /// Activity on a comment of an issue or a pull request's
    /// conversation.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub issue_comment: Option<IssueCommentTrigger>,
    /// Chat commands: a comment whose line starts with `/<name>`. See
    /// [`Commands`].
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub command: Option<Commands>,
}

impl Triggers {
    /// The options of the chat command `name`, `None` when the workflow
    /// has no `on.command` or no command of that name. The control plane
    /// reads the permission, the sha rule and the reaction of a command
    /// it matched from this.
    #[must_use]
    pub fn command_options(&self, name: &str) -> Option<&CommandOptions> {
        self.command.as_ref()?.get(name)
    }
}

/// `on: create` and `on: delete`, which take no filters.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RefTrigger {}

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

/// Filters for `on: pull_request` and `on: pull_request_target`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PullRequestTrigger {
    /// Activity types; `opened`, `synchronize` and `reopened` when absent.
    /// One type may be written as a plain string, as GitHub allows.
    #[serde(deserialize_with = "one_or_many")]
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

/// `on: workflow_run`: the workflow starts after another workflow of
/// the same config finishes. It is run by hatchrun itself, so it works
/// the same on every host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WorkflowRunTrigger {
    /// The workflows, by their keys under `workflows:`, whose runs start
    /// this one. One may be written as a plain string.
    #[serde(deserialize_with = "one_or_many")]
    pub workflows: Vec<String>,
    /// Activity types, from [`WORKFLOW_RUN_TYPES`]; `completed` when
    /// absent.
    #[serde(
        default = "completed",
        deserialize_with = "one_or_many",
        skip_serializing_if = "is_completed"
    )]
    pub types: Vec<String>,
    /// Branches the finished run ran on that trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Filter>,
    /// Branches the finished run ran on that do not trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches_ignore: Option<Filter>,
}

/// The activity types `on.workflow_run` accepts.
pub const WORKFLOW_RUN_TYPES: &[&str] = &["requested", "in_progress", "completed"];

fn completed() -> Vec<String> {
    vec!["completed".to_owned()]
}

fn is_completed(types: &[String]) -> bool {
    types == ["completed"]
}

/// `on: release`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ReleaseTrigger {
    /// Activity types, from [`RELEASE_TYPES`]; `published` when absent.
    /// One type may be written as a plain string.
    #[serde(deserialize_with = "one_or_many")]
    pub types: Vec<String>,
    /// Release tags that trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Filter>,
    /// Release tags that do not trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_ignore: Option<Filter>,
}

/// `on: merge_group`: a merge queue entry asks for its checks. The run
/// is on the merge group's head commit, so the queue sees its results.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct MergeGroupTrigger {
    /// Activity types, from [`MERGE_GROUP_TYPES`]; `checks_requested`
    /// when absent. One type may be written as a plain string.
    #[serde(deserialize_with = "one_or_many")]
    pub types: Vec<String>,
}

/// The activity types `on.merge_group` accepts.
pub const MERGE_GROUP_TYPES: &[&str] = &["checks_requested"];

impl Default for MergeGroupTrigger {
    fn default() -> Self {
        Self {
            types: vec!["checks_requested".to_owned()],
        }
    }
}

/// `on: repository_dispatch`: a `POST /repos/{repo}/dispatches` request
/// starts the workflow on the default branch. The request's
/// `client_payload` is in `github.event`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct RepositoryDispatchTrigger {
    /// The `event_type` values that start the workflow; any when empty.
    /// One may be written as a plain string.
    #[serde(
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub types: Vec<String>,
}

/// `on: issues`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct IssuesTrigger {
    /// Activity types, from [`ISSUES_TYPES`]; every one when absent. One
    /// type may be written as a plain string.
    #[serde(deserialize_with = "one_or_many")]
    pub types: Vec<String>,
}

/// The activity types `on.issues` accepts: those GitHub starts
/// workflows on.
pub const ISSUES_TYPES: &[&str] = &[
    "opened",
    "edited",
    "deleted",
    "transferred",
    "pinned",
    "unpinned",
    "closed",
    "reopened",
    "assigned",
    "unassigned",
    "labeled",
    "unlabeled",
    "locked",
    "unlocked",
    "milestoned",
    "demilestoned",
    "typed",
    "untyped",
    "field_added",
    "field_removed",
];

impl Default for IssuesTrigger {
    /// Every type GitHub runs `issues` on.
    fn default() -> Self {
        Self {
            types: ISSUES_TYPES.iter().map(|t| (*t).to_owned()).collect(),
        }
    }
}

/// `on: issue_comment`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct IssueCommentTrigger {
    /// Activity types, from [`ISSUE_COMMENT_TYPES`]; every one when
    /// absent. One type may be written as a plain string.
    #[serde(deserialize_with = "one_or_many")]
    pub types: Vec<String>,
}

/// The activity types `on.issue_comment` accepts.
pub const ISSUE_COMMENT_TYPES: &[&str] = &["created", "edited", "deleted"];

impl Default for IssueCommentTrigger {
    /// All three types GitHub runs `issue_comment` on.
    fn default() -> Self {
        Self {
            types: ISSUE_COMMENT_TYPES
                .iter()
                .map(|t| (*t).to_owned())
                .collect(),
        }
    }
}

/// The GitHub repository permission a commenter must hold, lowest
/// first, so `Write <= Maintain` compares as the roles do.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Deserialize, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CommandPermission {
    /// Read access.
    Read,
    /// Triage: manage issues and pull requests, no code.
    Triage,
    /// Write access, the default.
    #[default]
    Write,
    /// Maintain: write, and manage the repository without its secrets.
    Maintain,
    /// Admin.
    Admin,
}

impl CommandPermission {
    /// The role's name as GitHub's collaborator permission API gives it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Triage => "triage",
            Self::Write => "write",
            Self::Maintain => "maintain",
            Self::Admin => "admin",
        }
    }
}

/// Where a command's comment may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPlace {
    /// The conversation of a pull request.
    PullRequest,
    /// A plain issue.
    Issue,
}

/// Whether a command's comment must name the pull request's head sha
/// with `--sha=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShaPolicy {
    /// Required when the pull request is from a fork, else optional.
    #[default]
    Auto,
    /// Always required.
    Required,
    /// Never required; a sha that is given is still compared.
    Optional,
}

/// The reactions the platform leaves on a command's comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandReaction {
    /// `+1` on success, `-1` on failure.
    #[default]
    Default,
    /// `rocket` on success, `confused` on failure.
    Rocket,
}

/// The options of one `on.command` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandOptions {
    /// The permission the commenter must hold on the repository.
    pub permission: CommandPermission,
    /// Where the comment may be. Written `in:`.
    #[serde(rename = "in")]
    pub places: Vec<CommandPlace>,
    /// Whether the comment must name the head sha.
    pub sha: ShaPolicy,
    /// Names for the positional words, read as
    /// `github.event.command.named.<name>`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The reactions the platform leaves.
    pub reaction: CommandReaction,
}

impl Default for CommandOptions {
    /// A `write` commenter, on pull requests, with an automatic sha rule and no arguments.
    fn default() -> Self {
        Self {
            permission: CommandPermission::Write,
            places: vec![CommandPlace::PullRequest],
            sha: ShaPolicy::Auto,
            args: Vec::new(),
            reaction: CommandReaction::Default,
        }
    }
}

impl CommandOptions {
    /// Whether a comment on a pull request (`true`) or on a plain issue
    /// (`false`) may run the command.
    #[must_use]
    pub fn allows(&self, on_pull_request: bool) -> bool {
        let place = if on_pull_request {
            CommandPlace::PullRequest
        } else {
            CommandPlace::Issue
        };
        self.places.contains(&place)
    }

    /// Whether a command on a pull request must carry `--sha=`. `fork` is
    /// whether the pull request's head is in another repository. A plain
    /// issue has no head, so the question does not arise there.
    #[must_use]
    pub fn sha_required(&self, fork: bool) -> bool {
        match self.sha {
            ShaPolicy::Required => true,
            ShaPolicy::Optional => false,
            ShaPolicy::Auto => fork,
        }
    }
}

/// The longest name of a command.
pub const MAX_COMMAND_NAME: usize = 32;

/// Whether `name` is a command name: lowercase letters, digits, `-` and
/// `_`, at most [`MAX_COMMAND_NAME`] of them.
#[must_use]
pub fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_COMMAND_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// `on: command`, the chat commands of a workflow by name.
///
/// In YAML it is a map of names to [`CommandOptions`] (an empty value
/// takes the defaults), one name as a string, or a list of names, the
/// last two with the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Commands(BTreeMap<String, CommandOptions>);

impl Commands {
    /// The options of the command `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CommandOptions> {
        self.0.get(name)
    }

    /// The commands by name, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CommandOptions)> {
        self.0
            .iter()
            .map(|(name, options)| (name.as_str(), options))
    }

    /// Whether no command is named.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, CommandOptions)> for Commands {
    /// Collects named options into the commands of a trigger.
    fn from_iter<T: IntoIterator<Item = (String, CommandOptions)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'de> Deserialize<'de> for Commands {
    /// Reads a name, a list of names or a map of names to options.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CommandsVisitor;

        impl<'de> Visitor<'de> for CommandsVisitor {
            type Value = Commands;

            /// Describes the shapes `on.command` takes, for error messages.
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a command name, a list of names or a map of names to options")
            }

            /// One name, with the default options.
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Commands, E> {
                Ok(Commands(BTreeMap::from([(
                    value.to_owned(),
                    CommandOptions::default(),
                )])))
            }

            /// A list of names with the default options, each named once.
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Commands, A::Error> {
                let mut commands = BTreeMap::new();
                while let Some(name) = seq.next_element::<String>()? {
                    if commands
                        .insert(name.clone(), CommandOptions::default())
                        .is_some()
                    {
                        return Err(de::Error::custom(format!(
                            "command `{name}` is listed twice"
                        )));
                    }
                }
                Ok(Commands(commands))
            }

            /// A map of names to options, each named once; an empty value takes the defaults.
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Commands, A::Error> {
                let mut commands = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    let options = map
                        .next_value::<Option<CommandOptions>>()?
                        .unwrap_or_default();
                    if commands.insert(name.clone(), options).is_some() {
                        return Err(de::Error::custom(format!(
                            "command `{name}` is given twice"
                        )));
                    }
                }
                Ok(Commands(commands))
            }
        }

        deserializer.deserialize_any(CommandsVisitor)
    }
}

impl Serialize for Commands {
    /// Writes the commands as a map of names to options.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, options) in &self.0 {
            map.serialize_entry(name, options)?;
        }
        map.end()
    }
}

/// The activity types `on.release` accepts.
pub const RELEASE_TYPES: &[&str] = &[
    "published",
    "unpublished",
    "created",
    "edited",
    "deleted",
    "prereleased",
    "released",
];

impl Default for ReleaseTrigger {
    fn default() -> Self {
        Self {
            types: vec!["published".to_owned()],
            tags: None,
            tags_ignore: None,
        }
    }
}

/// The most changed paths path filters look at. GitHub limits diffs to
/// 300 files: a change outside the first 300 does not start a workflow.
pub const MAX_CHANGED: usize = 300;

/// The most work, in steps, matching one path filter against an
/// event's changed paths may take. A step is one reachable position
/// looked at for one token, or one character a `*`, `**` or `+` run
/// walks over; a pattern whose leading text the path does not start
/// with takes none. Ordinary patterns reach a few positions per path
/// and use a small part of this: 256 patterns like `apps/a07/**/*.tsx`
/// against 300 paths take under 2 ms. It runs out only for patterns
/// with many runs, such as `*a*a*a...b`, against long paths, where
/// most positions stay reachable for most tokens: about 50 ms per
/// filter. A filter that runs out passes, as path filters do when the
/// changed paths are not known.
const PATH_WORK: usize = 1 << 25;

/// The most patterns one filter may have.
pub const MAX_PATTERNS: usize = 256;

/// The most glob patterns, those not matched as plain text, a config's
/// filters may have in all. Each is compiled when it is matched.
pub const MAX_GLOBS: usize = 4096;

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
        /// Only the first [`MAX_CHANGED`] paths are looked at, as GitHub
        /// limits diffs to 300 files.
        changed: Vec<String>,
    },
    /// Activity on a pull request.
    PullRequest {
        /// The activity type, such as `opened`.
        action: String,
        /// The base branch, as a short name.
        base_ref: String,
        /// Paths changed by the pull request. Empty when the caller could
        /// not compute them; path filters then pass. Only the first
        /// [`MAX_CHANGED`] paths are looked at.
        changed: Vec<String>,
    },
    /// Activity on a pull request, for a workflow that runs from the base
    /// branch (`on: pull_request_target`). The fields are those of
    /// [`Event::PullRequest`]; `base_ref` is the branch the config was
    /// read from.
    PullRequestTarget {
        /// The activity type, such as `opened`.
        action: String,
        /// The base branch, as a short name.
        base_ref: String,
        /// Paths changed by the pull request. Empty when the caller could
        /// not compute them; path filters then pass. Only the first
        /// [`MAX_CHANGED`] paths are looked at.
        changed: Vec<String>,
    },
    /// A cron schedule firing.
    Schedule {
        /// The cron string of the schedule that fired.
        cron: String,
    },
    /// A manual start.
    Dispatch,
    /// A run of another workflow of the config changing state.
    WorkflowRun {
        /// The activity type: `requested`, `in_progress` or `completed`.
        action: String,
        /// The key of the workflow that ran.
        workflow: String,
        /// The branch it ran on, as a short name.
        branch: String,
    },
    /// Activity on a release.
    Release {
        /// The activity type, such as `published`.
        action: String,
        /// The release's tag, as a short name.
        tag: String,
    },
    /// A branch or tag was created.
    Create,
    /// A branch or tag was deleted.
    Delete,
    /// A merge queue entry asks for its checks.
    MergeGroup {
        /// The activity type, `checks_requested`.
        action: String,
    },
    /// A repository dispatch request.
    RepositoryDispatch {
        /// The request's `event_type`.
        action: String,
    },
    /// Activity on an issue. A pull request's conversation is not an
    /// issue event.
    Issues {
        /// The activity type, such as `opened`.
        action: String,
    },
    /// Activity on a comment of an issue or of a pull request.
    IssueComment {
        /// The activity type: `created`, `edited` or `deleted`.
        action: String,
    },
    /// A chat command found in a created comment (see
    /// [`crate::command`]). It matches the workflows whose `on.command`
    /// has the name and allows the place.
    Command {
        /// The command's name, without the `/`.
        name: String,
        /// Whether the comment is on a pull request, else on a plain
        /// issue.
        on_pull_request: bool,
    },
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
        } => on
            .pull_request
            .as_ref()
            .is_some_and(|pr| pull_request_matches(pr, action, base_ref, changed)),
        Event::PullRequestTarget {
            action,
            base_ref,
            changed,
        } => on
            .pull_request_target
            .as_ref()
            .is_some_and(|pr| pull_request_matches(pr, action, base_ref, changed)),
        Event::Schedule { cron } => on.schedule.iter().any(|s| &s.cron == cron),
        Event::Dispatch => on.workflow_dispatch.is_some(),
        Event::WorkflowRun {
            action,
            workflow,
            branch,
        } => on.workflow_run.as_ref().is_some_and(|run| {
            run.types.iter().any(|t| t == action)
                && run.workflows.iter().any(|w| w == workflow)
                && ref_passes(run.branches.as_ref(), run.branches_ignore.as_ref(), branch)
                    == Some(true)
        }),
        Event::Release { action, tag } => on.release.as_ref().is_some_and(|release| {
            release.types.iter().any(|t| t == action)
                && ref_passes(release.tags.as_ref(), release.tags_ignore.as_ref(), tag)
                    == Some(true)
        }),
        Event::Create => on.create.is_some(),
        Event::Delete => on.delete.is_some(),
        Event::MergeGroup { action } => on
            .merge_group
            .as_ref()
            .is_some_and(|group| group.types.iter().any(|t| t == action)),
        Event::RepositoryDispatch { action } => on
            .repository_dispatch
            .as_ref()
            .is_some_and(|d| d.types.is_empty() || d.types.iter().any(|t| t == action)),
        Event::Issues { action } => on
            .issues
            .as_ref()
            .is_some_and(|issues| issues.types.iter().any(|t| t == action)),
        Event::IssueComment { action } => on
            .issue_comment
            .as_ref()
            .is_some_and(|comment| comment.types.iter().any(|t| t == action)),
        Event::Command {
            name,
            on_pull_request,
        } => on
            .command_options(name)
            .is_some_and(|options| options.allows(*on_pull_request)),
    }
}

fn pull_request_matches(
    pr: &PullRequestTrigger,
    action: &str,
    base_ref: &str,
    changed: &[String],
) -> bool {
    pr.types.iter().any(|t| t == action)
        && ref_passes(pr.branches.as_ref(), pr.branches_ignore.as_ref(), base_ref) == Some(true)
        && paths_pass(pr.paths.as_ref(), pr.paths_ignore.as_ref(), changed)
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
    let changed = &changed[..changed.len().min(MAX_CHANGED)];
    match (include, ignore) {
        (None, None) => true,
        (Some(_), Some(_)) => false,
        _ if changed.is_empty() => true,
        // A filter that needs more than PATH_WORK to decide passes, as
        // when the changed paths are unknown.
        (Some(include), None) => {
            let mut scratch = Scratch::with_work(PATH_WORK);
            changed
                .iter()
                .any(|p| include.matches_in(p, &mut scratch).unwrap_or(true))
        }
        (None, Some(ignore)) => {
            let mut scratch = Scratch::with_work(PATH_WORK);
            changed
                .iter()
                .any(|p| ignore.matches_in(p, &mut scratch).is_none_or(|m| !m))
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

/// Reads a present key as its value, so an empty value is an error
/// rather than no trigger.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
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
            // One scratch across values, as paths_pass keeps it.
            let mut scratch = Scratch::with_work(usize::MAX);
            for value in &values {
                let chars: Vec<char> = value.chars().collect();
                let want = if reference(&toks, &chars) {
                    !negated
                } else {
                    value == "x"
                };
                assert_eq!(filter.matches(value), want, "{pattern:?} on {value:?}");
                assert_eq!(
                    filter.matches_in(value, &mut scratch),
                    Some(want),
                    "{pattern:?} on {value:?}, kept scratch"
                );
            }
        }
    }

    #[test]
    fn a_filter_out_of_work_is_undecided() {
        let filter = Filter::new(vec![format!("{}b", "*a".repeat(100))]);
        let value = "a".repeat(200);
        let mut scratch = Scratch::with_work(1000);
        assert_eq!(filter.matches_in(&value, &mut scratch), None);
        let mut scratch = Scratch::with_work(usize::MAX);
        assert_eq!(filter.matches_in(&value, &mut scratch), Some(false));
        // Text shapes take no work.
        let filter = Filter::new(vec!["docs/**".to_owned()]);
        let mut scratch = Scratch::with_work(0);
        assert_eq!(filter.matches_in("docs/a.md", &mut scratch), Some(true));
    }
}
