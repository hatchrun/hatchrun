// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Expands a planned job that reads its matrix or its `runs-on` when it
//! becomes ready.
//!
//! The planner cannot expand `matrix: ${{ fromJSON(needs.setup.outputs.m) }}`
//! or resolve `runs-on: ${{ matrix.os }}` before the jobs a workflow
//! requires have run. It plans one stand-in job that holds a
//! [`Deferred`]; the host calls [`expand`] with the job's context once
//! the stand-in can start, and gets back the jobs to run.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::config::Matrix;
use crate::expr::{self, ExprError, Functions, JobStatus, evaluate, interpolate};
use crate::plan::{
    Deferred, JobKind, MAX_COMBINATIONS, MAX_MATRIX, PlannedJob, PlannedStrategy, render,
};
use crate::templates::{template, template_labels};

/// One job an expansion makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    /// The job. Its `deferred` is empty, unless the job failed to
    /// resolve (see `error`): then it keeps the `runs-on` expression, so
    /// a rerun reads it again.
    pub job: PlannedJob,
    /// Why the job cannot run: its `runs-on` failed to evaluate or named
    /// no runner template. The host ends the job as failed with this
    /// message, and the other jobs go on.
    pub error: Option<String>,
}

/// What an expression sees on the control plane: no files, and a job
/// that has not failed.
struct Plain;

impl Functions for Plain {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Err(ExprError::new("hashFiles runs on the runner", 0))
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

/// One matrix entry: its values, and the values its job's name shows.
struct Combo {
    values: BTreeMap<String, Value>,
    shown: Vec<String>,
}

/// Makes the jobs of `job` from `ctx`, the contexts of the job as the
/// host builds them: `hatchrun`, `github`, `needs`, `vars` and `inputs`.
/// A job with nothing deferred is its own only job.
///
/// The matrix is made as on GitHub Actions: the lists written out and
/// the ones the expressions read, every combination of them, then
/// `exclude`, then `include`. Each job is named `<name> (<values>)` and
/// reads its entry as `matrix.KEY`. Each job's `runs-on` is then read
/// with that entry in `matrix` and must name a runner template; one that
/// does not makes that job fail, not the others.
///
/// # Errors
///
/// A message when the matrix cannot be made: an expression fails or
/// reads the wrong kind of value, a key has no values, the matrix has
/// no combination or more than 256, or it would make a job id twice.
pub fn expand(job: &PlannedJob, ctx: &Value) -> Result<Vec<Leg>, String> {
    let Some(deferred) = &job.deferred else {
        return Ok(vec![Leg {
            job: job.clone(),
            error: None,
        }]);
    };
    // A job that is one matrix entry already keeps its id and matrix.
    let fixed = deferred.matrix.is_empty();
    let combos = if fixed {
        vec![Combo {
            values: job.matrix.clone(),
            shown: Vec::new(),
        }]
    } else {
        combinations(&deferred.matrix, ctx)?
    };
    let nodes = deferred.nodes.max(1);
    let total = combos.len() * nodes as usize;
    let mut ids = BTreeSet::new();
    let mut scope = ctx.clone();
    let mut legs = Vec::with_capacity(total);
    for combo in combos {
        let base = if fixed {
            job.id.clone()
        } else if combo.shown.is_empty() {
            job.name.clone()
        } else {
            format!("{} ({})", job.name, combo.shown.join(", "))
        };
        for node in 0..nodes {
            let id = if fixed {
                base.clone()
            } else if nodes > 1 {
                format!("{base} [{}/{nodes}]", node + 1)
            } else {
                base.clone()
            };
            let id = unique(&ids, id);
            ids.insert(id.clone());
            let mut leg = job.clone();
            leg.id = id;
            if !fixed {
                leg.matrix.clone_from(&combo.values);
                leg.node = (nodes > 1).then_some((node, nodes));
            }
            leg.strategy = job.strategy.as_ref().map(|s| PlannedStrategy {
                job_index: u32::try_from(legs.len()).unwrap_or(u32::MAX),
                job_total: u32::try_from(total).unwrap_or(u32::MAX),
                ..s.clone()
            });
            leg.deferred = None;
            let error = match &deferred.runs_on {
                None => None,
                Some(text) => {
                    scope["matrix"] = json_map(&leg.matrix);
                    scope["strategy"] = leg.strategy_context();
                    match runner(text, &scope, &leg.id) {
                        Ok(template) => {
                            leg.kind = JobKind::Run { template };
                            None
                        }
                        Err(message) => {
                            leg.deferred = Some(Deferred {
                                matrix: Matrix::default(),
                                runs_on: Some(text.clone()),
                                nodes: 1,
                            });
                            Some(message)
                        }
                    }
                }
            };
            legs.push(Leg { job: leg, error });
        }
    }
    Ok(legs)
}

fn json_map(values: &BTreeMap<String, Value>) -> Value {
    Value::Object(values.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `id`, or `id` with a number when another job has it.
fn unique(taken: &BTreeSet<String>, id: String) -> String {
    if !taken.contains(&id) {
        return id;
    }
    (2..u32::MAX)
        .map(|n| format!("{id} #{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(id)
}

/// The template `runs-on` selects for the job `id`.
fn runner(
    text: &str,
    ctx: &Value,
    id: &str,
) -> Result<&'static crate::templates::Template, String> {
    let label = interpolate(text, ctx, &Plain)
        .map_err(|e| format!("`runs-on` of `{id}` could not be read: {e}"))?;
    let label = label.trim();
    template(label).ok_or_else(|| {
        format!(
            "`runs-on` of `{id}` is `{label}`, which names no runner template; use one of {}",
            template_labels().join(", ")
        )
    })
}

/// Reads an expression that must make a JSON value. A text that is one
/// `${{ }}` is evaluated; a string result is read as JSON, which is what
/// an output holds.
fn read(text: &str, ctx: &Value, what: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    let whole = expr::expressions(trimmed)
        .ok()
        .and_then(|found| match found.as_slice() {
            [(3, inner)] if 3 + inner.len() + 2 == trimmed.len() => Some(*inner),
            _ => None,
        });
    let value = match whole {
        Some(inner) => evaluate(inner, ctx, &Plain).map_err(|e| format!("{what}: {e}"))?,
        None => {
            Value::String(interpolate(trimmed, ctx, &Plain).map_err(|e| format!("{what}: {e}"))?)
        }
    };
    match value {
        Value::String(json) => serde_json::from_str(&json)
            .map_err(|e| format!("{what} is not JSON ({e}); read it with fromJSON(...)")),
        other => Ok(other),
    }
}

fn list(value: Value, what: &str) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(format!("{what} must be a list, found {other}")),
    }
}

fn maps(value: Value, what: &str) -> Result<Vec<BTreeMap<String, Value>>, String> {
    list(value, what)?
        .into_iter()
        .map(|item| match item {
            Value::Object(map) => Ok(map.into_iter().collect()),
            other => Err(format!("{what} entries must be maps, found {other}")),
        })
        .collect()
}

/// The entries of a matrix, as GitHub makes them.
#[allow(clippy::too_many_lines)] // Reads each part of a matrix, then combines them.
fn combinations(matrix: &Matrix, ctx: &Value) -> Result<Vec<Combo>, String> {
    let mut values = matrix.values.clone();
    let mut include = matrix.include.clone();
    let mut exclude = matrix.exclude.clone();
    if let Some(text) = &matrix.from {
        let value = read(text, ctx, "matrix")?;
        let Value::Object(map) = value else {
            return Err(format!("matrix must read as a map of lists, found {value}"));
        };
        for (key, value) in map {
            match key.as_str() {
                "include" => include.extend(maps(value, "matrix include")?),
                "exclude" => exclude.extend(maps(value, "matrix exclude")?),
                _ => {
                    let what = format!("matrix key `{key}`");
                    values.insert(key, list(value, &what)?);
                }
            }
        }
    }
    for (key, text) in &matrix.lists {
        let what = format!("matrix key `{key}`");
        let items = list(read(text, ctx, &what)?, &what)?;
        values.insert(key.clone(), items);
    }
    if let Some(text) = &matrix.include_from {
        include.extend(maps(read(text, ctx, "matrix include")?, "matrix include")?);
    }
    for (key, items) in &values {
        if items.is_empty() {
            return Err(format!("matrix key `{key}` has no values"));
        }
    }
    let size = values
        .values()
        .fold(1usize, |n, items| n.saturating_mul(items.len()));
    if !values.is_empty() && size > MAX_COMBINATIONS {
        return Err(format!(
            "matrix has {size} combinations before `exclude`; the limit is {MAX_COMBINATIONS}"
        ));
    }
    let mut combos: Vec<Combo> = Vec::new();
    if !values.is_empty() {
        for index in 0..size {
            let mut rest = index;
            let mut picked = BTreeMap::new();
            for (key, items) in values.iter().rev() {
                picked.insert(key.clone(), items[rest % items.len()].clone());
                rest /= items.len();
            }
            if excluded_by(&exclude, &picked) {
                continue;
            }
            let shown = picked.values().map(render).collect();
            combos.push(Combo {
                values: picked,
                shown,
            });
        }
    }
    let mut added: Vec<Combo> = Vec::new();
    for entry in include {
        let mut matched = false;
        for combo in &mut combos {
            // Original values stay; keys an earlier entry added can change.
            let fits = entry
                .iter()
                .all(|(k, v)| !values.contains_key(k) || combo.values.get(k) == Some(v));
            if fits {
                matched = true;
                for (k, v) in &entry {
                    combo.values.insert(k.clone(), v.clone());
                }
            }
        }
        if !matched {
            let shown = entry.values().map(render).collect();
            added.push(Combo {
                values: entry,
                shown,
            });
        }
    }
    combos.extend(added);
    let mut seen = BTreeSet::new();
    combos.retain(|c| {
        seen.insert(
            Value::Object(
                c.values
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map<_, _>>(),
            )
            .to_string(),
        )
    });
    if combos.is_empty() {
        return Err("matrix has no combination to run".to_owned());
    }
    if combos.len() > MAX_MATRIX {
        return Err(format!(
            "matrix has {} jobs; the limit is {MAX_MATRIX}",
            combos.len()
        ));
    }
    Ok(combos)
}

/// Whether `exclude` leaves out the entry: one of its maps matches
/// every one of its keys.
fn excluded_by(exclude: &[BTreeMap<String, Value>], combo: &BTreeMap<String, Value>) -> bool {
    exclude.iter().any(|entry| {
        entry.iter().all(|(k, want)| {
            combo
                .get(k)
                .is_some_and(|v| v == want || render(v) == render(want))
        })
    })
}
