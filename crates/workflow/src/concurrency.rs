// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `concurrency` key of a workflow or a job.
//!
//! Within one repository, runs (or jobs) that name the same group do not
//! run together: at most one runs and one waits. A newer arrival takes
//! the place of the one that waits, which ends cancelled. With
//! `cancel-in-progress` the arrival also cancels the one that runs.
//! The group is an expression text; the host evaluates it with
//! [`Concurrency::resolve`] when the run or job is about to start.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::expr::{self, ExprError, Functions, JobStatus};

/// The longest group name, in bytes, after evaluation.
pub const MAX_GROUP: usize = 400;

/// Contexts a workflow's `concurrency` may read.
pub(crate) const WORKFLOW_CONTEXTS: &[&str] = &["hatchrun", "github", "inputs", "vars"];

/// Contexts a job's `concurrency` may read.
pub(crate) const JOB_CONTEXTS: &[&str] =
    &["hatchrun", "github", "inputs", "vars", "matrix", "needs"];

/// Whether a newer arrival cancels the member that runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Cancel {
    /// The running member finishes first.
    #[default]
    No,
    /// The running member is cancelled.
    Yes,
    /// Decided when the arrival evaluates this expression.
    When(String),
}

/// A `concurrency` key: a group and what a newer arrival does to the
/// member that runs. In YAML a bare string is a group, otherwise a map
/// with `group` and `cancel-in-progress` (a boolean or an expression).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "ConcurrencyDe", into = "ConcurrencyDe")]
pub struct Concurrency {
    /// The group, which may hold `${{ }}` expressions.
    pub group: String,
    /// What a newer arrival does to the running member.
    pub cancel_in_progress: Cancel,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum ConcurrencyDe {
    Group(String),
    Map(ConcurrencyMap),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct ConcurrencyMap {
    group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancel_in_progress: Option<CancelDe>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum CancelDe {
    Flag(bool),
    Expr(String),
}

impl TryFrom<ConcurrencyDe> for Concurrency {
    type Error = String;

    fn try_from(de: ConcurrencyDe) -> Result<Self, String> {
        let (group, cancel) = match de {
            ConcurrencyDe::Group(group) => (group, None),
            ConcurrencyDe::Map(map) => (map.group, map.cancel_in_progress),
        };
        if group.trim().is_empty() {
            return Err("a concurrency group cannot be empty".into());
        }
        let cancel_in_progress = match cancel {
            None | Some(CancelDe::Flag(false)) => Cancel::No,
            Some(CancelDe::Flag(true)) => Cancel::Yes,
            Some(CancelDe::Expr(text)) => Cancel::When(text),
        };
        Ok(Self {
            group,
            cancel_in_progress,
        })
    }
}

impl From<Concurrency> for ConcurrencyDe {
    fn from(c: Concurrency) -> Self {
        let cancel = match c.cancel_in_progress {
            Cancel::No => return ConcurrencyDe::Group(c.group),
            Cancel::Yes => CancelDe::Flag(true),
            Cancel::When(text) => CancelDe::Expr(text),
        };
        ConcurrencyDe::Map(ConcurrencyMap {
            group: c.group,
            cancel_in_progress: Some(cancel),
        })
    }
}

/// A group, evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The group's name as evaluated.
    pub group: String,
    /// Whether the arrival cancels the member that runs.
    pub cancel_in_progress: bool,
}

/// Evaluates on the control plane, which has no files and no steps.
pub(crate) struct Plain;

impl Functions for Plain {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Err(ExprError {
            message: "hashFiles runs on the runner".to_owned(),
            offset: 0,
        })
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

impl Concurrency {
    /// Evaluates the group and `cancel-in-progress` against `ctx`, an
    /// object whose keys are context names.
    ///
    /// # Errors
    ///
    /// When an expression fails, or the group is empty or longer than
    /// [`MAX_GROUP`] bytes.
    pub fn resolve(&self, ctx: &Value) -> Result<Resolved, String> {
        let group = expr::interpolate(&self.group, ctx, &Plain)
            .map_err(|e| format!("concurrency group: {e}"))?
            .trim()
            .to_owned();
        if group.is_empty() {
            return Err("concurrency group is empty".into());
        }
        if group.len() > MAX_GROUP {
            return Err(format!(
                "concurrency group is {} bytes; the limit is {MAX_GROUP}",
                group.len()
            ));
        }
        let cancel_in_progress = match &self.cancel_in_progress {
            Cancel::No => false,
            Cancel::Yes => true,
            Cancel::When(text) => {
                let bad = |e: ExprError| format!("cancel-in-progress: {e}");
                if expr::find_open(text).is_some() {
                    let shown = expr::interpolate(text, ctx, &Plain).map_err(bad)?;
                    shown.trim().eq_ignore_ascii_case("true")
                } else {
                    expr::truthy(&expr::evaluate(text, ctx, &Plain).map_err(bad)?)
                }
            }
        };
        Ok(Resolved {
            group,
            cancel_in_progress,
        })
    }

    /// The texts the key holds: field name, text, and whether it is a
    /// bare expression rather than a template.
    pub(crate) fn texts(&self) -> Vec<(&'static str, &str, bool)> {
        let mut out = vec![("group", self.group.as_str(), false)];
        if let Cancel::When(text) = &self.cancel_in_progress {
            out.push(("cancel-in-progress", text.as_str(), true));
        }
        out
    }
}

impl fmt::Display for Concurrency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.group)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(yaml: &str) -> Concurrency {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn a_string_is_a_group_and_a_map_adds_cancel() {
        let c = parse("ci-${{ github.ref }}");
        assert_eq!(c.cancel_in_progress, Cancel::No);
        let c = parse("{ group: g, cancel-in-progress: true }");
        assert_eq!(c.cancel_in_progress, Cancel::Yes);
        let c = parse("{ group: g, cancel-in-progress: \"${{ github.ref != 'main' }}\" }");
        assert!(matches!(c.cancel_in_progress, Cancel::When(_)));
        assert!(serde_saphyr::from_str::<Concurrency>("{ group: g, nope: 1 }").is_err());
        assert!(serde_saphyr::from_str::<Concurrency>("''").is_err());
    }

    #[test]
    fn it_is_written_back_in_the_shortest_form() {
        let c = parse("{ group: g, cancel-in-progress: false }");
        assert_eq!(serde_json::to_value(&c).unwrap(), json!("g"));
        let c = parse("{ group: g, cancel-in-progress: true }");
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"group": "g", "cancel-in-progress": true})
        );
    }

    #[test]
    fn resolve_evaluates_group_and_cancel() {
        let ctx = json!({"github": {"workflow": "ci", "ref": "refs/heads/dev"}});
        let c = parse(
            "{ group: '${{ github.workflow }}-${{ github.ref }}', cancel-in-progress: \"${{ github.ref != 'refs/heads/main' }}\" }",
        );
        let r = c.resolve(&ctx).unwrap();
        assert_eq!(r.group, "ci-refs/heads/dev");
        assert!(r.cancel_in_progress);
        let c = parse("{ group: g, cancel-in-progress: github.ref == 'refs/heads/main' }");
        assert!(!c.resolve(&ctx).unwrap().cancel_in_progress);
    }

    #[test]
    fn resolve_refuses_an_empty_or_long_group() {
        let ctx = json!({"github": {"ref": ""}});
        assert!(parse("${{ github.ref }}").resolve(&ctx).is_err());
        let long = Concurrency {
            group: "a".repeat(MAX_GROUP + 1),
            cancel_in_progress: Cancel::No,
        };
        assert!(long.resolve(&ctx).is_err());
    }
}
