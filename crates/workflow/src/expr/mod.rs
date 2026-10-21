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

/// Context names an expression may read even when the context object
/// leaves them out. Any other name must be a key of the context object.
const CONTEXTS: &[&str] = &[
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
    while let Some(found) = template[rest..].find("${{") {
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
    while let Some(found) = template[rest..].find("${{") {
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
