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
];

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
/// # Errors
///
/// When the expression does not parse, names an unknown context or
/// function, or a function fails. The offset is relative to `expr`.
pub fn evaluate(expr: &str, ctx: &Value, funcs: &dyn Functions) -> Result<Value, ExprError> {
    let tokens = lexer::lex(expr)?;
    let ast = parser::parse(&tokens)?;
    eval::Evaluator { ctx, funcs }.eval(&ast)
}

/// Replaces every `${{ }}` in `template` with its value as a string.
///
/// # Errors
///
/// When a `${{` has no closing `}}`, or an expression fails. The offset
/// is relative to `template`.
pub fn interpolate(
    template: &str,
    ctx: &Value,
    funcs: &dyn Functions,
) -> Result<String, ExprError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = 0;
    while let Some(found) = template[rest..].find("${{") {
        let open = rest + found;
        let start = open + 3;
        let end = closing(template, start)
            .ok_or_else(|| ExprError::new("'${{' has no closing '}}'", open))?;
        let value = evaluate(&template[start..end], ctx, funcs).map_err(|e| ExprError {
            message: e.message,
            offset: e.offset + start,
        })?;
        out.push_str(&template[rest..open]);
        out.push_str(&eval::to_string(&value));
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
