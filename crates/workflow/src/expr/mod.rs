// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! GitHub-style `${{ }}` expressions.
//!
//! The language and its loose typing follow GitHub Actions, with
//! nektos/act's `pkg/exprparser` as the reference for edge cases. An
//! expression is lexed, parsed into a tree and then walked against a
//! JSON context object whose keys are the context names.

mod eval;
mod lexer;
mod parser;

use std::borrow::Cow;
use std::fmt;

use serde_json::Value;

pub use eval::truthy;

use crate::secrets::SecretRefs;

/// Context names an expression may read even when the context object
/// leaves them out. Any other name must be a key of the context object.
///
/// `hatchrun` holds the [`HATCHRUN_KEYS`] and reads the same on every
/// host: each host adapter fills it from its own event. The GitHub and
/// Forgejo adapters also fill `github`, so converted workflows keep
/// working.
pub(crate) const CONTEXTS: &[&str] = &[
    "hatchrun",
    "github",
    "env",
    "matrix",
    "needs",
    "steps",
    "secrets",
    "parameters",
    "runner",
    "job",
    "vars",
    "inputs",
];

/// The keys of the `hatchrun` context, as GitHub names them in `github`:
/// the full ref, its short name, the commit, who caused the event, the
/// event's name, the repository as `owner/name`, the host's base URL
/// and the workflow's key.
pub const HATCHRUN_KEYS: &[&str] = &[
    "ref",
    "ref_name",
    "sha",
    "actor",
    "event_name",
    "repository",
    "server_url",
    "workflow",
];

/// The longest expression, in bytes, that [`evaluate`] accepts.
const MAX_LEN: usize = 4096;

/// The longest string an expression can produce, in bytes: the result
/// of `format`, `join`, `toJSON` or an [`interpolate`]. The length is
/// worked out before the string is made.
pub const MAX_STRING: usize = 1 << 20;

/// The most memory, in bytes, one [`evaluate`] or [`interpolate`] call
/// may spend on the strings it makes, the values `fromJSON` parses and
/// the values it copies out of the context, across all of its
/// expressions and calls. Each value is charged before it is made: a
/// string by its length, a parsed or copied value by
/// `size_of::<Value>()` per node plus its string and key bytes.
///
/// Reading a context does not copy it: the evaluator walks contexts by
/// reference, and only a value [`evaluate`] returns is copied out.
pub const MAX_MEMORY: usize = 8 << 20;

/// An expression that does not parse or fails to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    /// What went wrong.
    pub message: String,
    /// Byte offset of the token the error is about, in the string that
    /// was passed in.
    pub offset: usize,
}

impl ExprError {
    pub(crate) fn new(message: impl Into<String>, offset: usize) -> Self {
        Self {
            message: message.into(),
            offset,
        }
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

impl std::error::Error for ExprError {}

/// How the job is doing, as seen by `success()`, `failure()` and
/// `cancelled()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// No step has failed and the run was not cancelled.
    Success,
    /// A step has failed.
    Failure,
    /// The run was cancelled.
    Cancelled,
    /// A job this one depends on was skipped and none failed: neither
    /// `success()` nor `failure()` holds.
    Skipped,
}

/// The functions whose answer depends on where the expression runs.
pub trait Functions {
    /// Hashes the files matching `patterns`, for `hashFiles(...)`.
    ///
    /// # Errors
    ///
    /// When the files cannot be hashed here, for example on the control
    /// plane, which has no checkout.
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError>;

    /// The status of the job so far.
    fn status(&self) -> JobStatus;
}

/// Evaluates one expression, the text between `${{` and `}}`.
///
/// `ctx` is an object whose keys are context names such as `github`,
/// `matrix` or `needs`.
///
/// Expressions longer than 4096 bytes, or nesting brackets, parentheses,
/// calls and `!` deeper than 64 levels, are rejected so evaluation fits
/// a small stack.
///
/// # Errors
///
/// When the expression is too long or too deep, does not parse, names an unknown context or
/// function, a function fails, or it would use more than [`MAX_MEMORY`].
/// The offset is relative to `expr`.
pub fn evaluate(expr: &str, ctx: &Value, funcs: &dyn Functions) -> Result<Value, ExprError> {
    evaluate_in(expr, ctx, funcs, &eval::Budget::new())
}

/// Evaluates one expression, charging what it makes to `budget`.
fn evaluate_in(
    expr: &str,
    ctx: &Value,
    funcs: &dyn Functions,
    budget: &eval::Budget,
) -> Result<Value, ExprError> {
    let ast = tree(expr)?;
    eval::Evaluator { ctx, funcs, budget }.eval(&ast)
}

/// Whether an `if:` calls `success()`, `failure()`, `cancelled()` or
/// `always()`. GitHub runs a step or job whose condition calls none of
/// them only while everything before it succeeded, as if the condition
/// were `success() && (...)`.
///
/// `cond` is the `if:` as written: a bare expression, or text holding
/// `${{ }}` expressions. Calls are found in the parsed expression, so a
/// string literal or a property that spells `success(` is not one.
///
/// # Errors
///
/// When an expression does not parse or a `${{` is not closed.
pub fn checks_status(cond: &str) -> Result<bool, ExprError> {
    let found = if find_open(cond).is_some() {
        expressions(cond)?
    } else {
        vec![(0, cond)]
    };
    // Every segment must parse, so keep going after a status call.
    let mut calls = false;
    for (start, inner) in found {
        let ast = tree(inner).map_err(|e| ExprError {
            message: e.message,
            offset: e.offset + start,
        })?;
        calls |= eval::calls_status(&ast);
    }
    Ok(calls)
}

/// The secrets one text reads. `bare` reads the whole text as one
/// expression (an `if:`); otherwise only `${{ }}` segments count. A bare
/// text that holds `${{ }}` is read by its segments, as
/// [`checks_status`] does.
///
/// A name is read from `secrets.NAME` or `secrets['NAME']`, uppercased.
/// Any other use of the `secrets` context, such as a computed index or
/// `toJSON(secrets)`, needs every secret. `GITHUB_TOKEN` is the job
/// token and is never named.
///
/// # Errors
///
/// When a segment does not parse or a `${{` is not closed.
pub fn secret_refs(text: &str, bare: bool) -> Result<SecretRefs, ExprError> {
    let found = if !bare || find_open(text).is_some() {
        expressions(text)?
    } else {
        vec![(0, text)]
    };
    let mut refs = SecretRefs::default();
    for (start, inner) in found {
        let ast = tree(inner).map_err(|e| ExprError {
            message: e.message,
            offset: e.offset + start,
        })?;
        collect_secrets(&ast, &mut refs);
    }
    Ok(refs)
}

/// Adds what `expr` reads of the `secrets` context to `refs`.
fn collect_secrets(expr: &parser::Expr<'_>, refs: &mut SecretRefs) {
    use parser::{Expr, Step};
    match expr {
        Expr::Literal(_) => {}
        Expr::Context { name, .. } => {
            if name.eq_ignore_ascii_case("secrets") {
                refs.all = true;
            }
        }
        Expr::Not(inner) => collect_secrets(inner, refs),
        Expr::Access { base, path } => {
            let on_secrets = matches!(&**base, Expr::Context { name, .. } if name.eq_ignore_ascii_case("secrets"));
            if on_secrets {
                match path.first() {
                    Some(Step::Property(name)) => refs.add(name),
                    Some(Step::Index(Expr::Literal(Value::String(name)))) => refs.add(name),
                    _ => refs.all = true,
                }
            } else {
                collect_secrets(base, refs);
            }
            for step in path {
                if let Step::Index(index) = step
                    && !matches!(index, Expr::Literal(_))
                {
                    collect_secrets(index, refs);
                }
            }
        }
        Expr::Call { args, .. } | Expr::And(args) | Expr::Or(args) => {
            for arg in args {
                collect_secrets(arg, refs);
            }
        }
        Expr::Compare { first, rest } => {
            collect_secrets(first, refs);
            for (_, e) in rest {
                collect_secrets(e, refs);
            }
        }
    }
}

/// Whether a step's or job's `if:` holds. No condition means
/// `success()`. A condition that calls no status function runs as
/// `success() && (cond)`, as on GitHub. A `${{ }}` wrapper around the
/// whole condition is stripped. A condition that has `${{ }}` but is
/// not one whole expression, such as `${{ a }} && ${{ b }}`, is a string
/// template, which is truthy, as on GitHub; its status functions still
/// decide whether `success()` is implied.
///
/// # Errors
///
/// When the expression does not parse or fails to evaluate.
pub fn job_condition(
    cond: Option<&str>,
    ctx: &Value,
    funcs: &dyn Functions,
) -> Result<bool, ExprError> {
    let Some(cond) = cond else {
        return Ok(funcs.status() == JobStatus::Success);
    };
    let trimmed = cond.trim();
    // One `${{ }}` whose closing `}}` ends the condition is a whole
    // expression, even when a string inside it spells `${{`.
    let segments = if find_open(trimmed).is_some() {
        expressions(trimmed)?
    } else {
        Vec::new()
    };
    let whole = match segments.as_slice() {
        [(3, inner)] if trimmed.starts_with("${{") && 3 + inner.len() + 2 == trimmed.len() => {
            Some(*inner)
        }
        _ => None,
    };
    if whole.is_none() && !segments.is_empty() {
        // The same check as validation: every segment must parse.
        return Ok(checks_status(trimmed)? || funcs.status() == JobStatus::Success);
    }
    let inner = whole.map_or(trimmed, str::trim);
    // One expression: parse it as is, since a quoted `${{` in it is text.
    let calls = eval::calls_status(&tree(inner)?);
    let wrapped;
    let run = if calls {
        inner
    } else {
        wrapped = format!("success() && ({inner})");
        &wrapped
    };
    Ok(truthy(&evaluate(run, ctx, funcs)?))
}

/// Checks one expression without evaluating it: it must parse, call
/// only known functions with the right number of arguments and read
/// only known contexts.
///
/// # Errors
///
/// When the expression is too long or too deep, does not parse, or
/// names an unknown context or function. The offset is relative to
/// `expr`.
pub fn check(expr: &str) -> Result<(), ExprError> {
    checked_contexts(expr).map(|_| ())
}

/// [`check`], returning the contexts the expression reads, lowercased,
/// as [`contexts`] does.
///
/// # Errors
///
/// As [`check`].
pub(crate) fn checked_contexts(expr: &str) -> Result<Vec<(Cow<'_, str>, usize)>, ExprError> {
    let names = contexts(expr)?;
    for (name, offset) in &names {
        if !CONTEXTS.contains(&name.as_ref()) {
            return Err(ExprError::new(format!("unknown context '{name}'"), *offset));
        }
    }
    Ok(names)
}

/// The contexts an expression reads, lowercased, with the offset of
/// each read, in the order they appear. Unknown names are included.
///
/// # Errors
///
/// When the expression is too long or too deep, does not parse, or
/// calls an unknown function.
pub(crate) fn contexts(expr: &str) -> Result<Vec<(Cow<'_, str>, usize)>, ExprError> {
    let ast = tree(expr)?;
    let mut names = Vec::new();
    eval::visit_contexts(&ast, &mut |name, offset| {
        let name = if name.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(name.to_ascii_lowercase())
        } else {
            Cow::Borrowed(name)
        };
        names.push((name, offset));
        Ok(())
    })?;
    Ok(names)
}

/// Lexes and parses one expression, within the length limit.
fn tree(expr: &str) -> Result<parser::Expr<'_>, ExprError> {
    if expr.len() > MAX_LEN {
        return Err(ExprError::new(
            format!("expression is longer than {MAX_LEN} bytes"),
            0,
        ));
    }
    parser::parse(lexer::lex(expr)?)
}

/// The byte offset of the first `${{` in `text`.
///
/// `str::find` with a string sets up a two-way searcher on every call,
/// which costs more than the search on the short strings of a workflow.
/// A short text is searched for the needle's first character, which
/// `memchr` finds, and the rest is checked; after a few false starts
/// the rest of the text goes to `str::find`, so text full of `$` stays
/// linear. A long text is first checked with `str::contains`, which is
/// faster than both on long text.
pub(crate) fn find_open(text: &str) -> Option<usize> {
    find(text, '$', "${{")
}

/// The byte offset of the first `}}` in `text`.
pub(crate) fn find_close(text: &str) -> Option<usize> {
    find(text, '}', "}}")
}

/// Text up to this long is searched by its first character.
const SHORT: usize = 256;

fn find(text: &str, first: char, needle: &str) -> Option<usize> {
    if text.len() > SHORT {
        return if text.contains(needle) {
            text.find(needle)
        } else {
            None
        };
    }
    let mut from = 0;
    for _ in 0..8 {
        let at = from + text[from..].find(first)?;
        if text[at..].starts_with(needle) {
            return Some(at);
        }
        from = at + 1;
    }
    text[from..].find(needle).map(|i| from + i)
}

/// The `${{ }}` expressions in `template`: the byte offset of the text
/// between the braces and that text.
///
/// # Errors
///
/// When a `${{` has no closing `}}`. The offset is relative to
/// `template`.
pub(crate) fn expressions(template: &str) -> Result<Vec<(usize, &str)>, ExprError> {
    let mut out = Vec::new();
    let mut rest = 0;
    while let Some(found) = find_open(&template[rest..]) {
        let open = rest + found;
        let start = open + 3;
        let end = closing(template, start)
            .ok_or_else(|| ExprError::new("'${{' has no closing '}}'", open))?;
        out.push((start, &template[start..end]));
        rest = end + 2;
    }
    Ok(out)
}

/// Replaces every `${{ }}` in `template` with its value as a string.
///
/// # Errors
///
/// When a `${{` has no closing `}}`, an expression fails, the result
/// would be longer than [`MAX_STRING`], or the expressions and the
/// result together would use more than [`MAX_MEMORY`]. The offset is
/// relative to `template`.
pub fn interpolate(
    template: &str,
    ctx: &Value,
    funcs: &dyn Functions,
) -> Result<String, ExprError> {
    let budget = eval::Budget::new();
    let mut out = String::with_capacity(template.len());
    let mut rest = 0;
    while let Some(found) = find_open(&template[rest..]) {
        let open = rest + found;
        let start = open + 3;
        let end = closing(template, start)
            .ok_or_else(|| ExprError::new("'${{' has no closing '}}'", open))?;
        let shift = |e: ExprError| ExprError {
            message: e.message,
            offset: e.offset + start,
        };
        let ast = tree(&template[start..end]).map_err(shift)?;
        let evaluator = eval::Evaluator {
            ctx,
            funcs,
            budget: &budget,
        };
        evaluator
            .eval_text(&ast, |text| {
                let len = out.len() + (open - rest) + text.len() + (template.len() - end - 2);
                if len > MAX_STRING {
                    return Err(ExprError::new(
                        format!(
                            "the value would be {len} bytes or more; the limit is {MAX_STRING}"
                        ),
                        open,
                    ));
                }
                budget.charge((open - rest) + text.len(), "interpolation", open)?;
                out.push_str(&template[rest..open]);
                out.push_str(text);
                Ok(())
            })
            .map_err(shift)??;
        rest = end + 2;
    }
    out.push_str(&template[rest..]);
    Ok(out)
}

/// Finds the `}}` that closes an expression starting at `start`, skipping
/// any inside string literals.
fn closing(template: &str, start: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            // A doubled quote inside a string toggles twice, so it needs
            // no special case.
            b'\'' => in_string = !in_string,
            b'}' if !in_string && bytes.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}
