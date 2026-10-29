// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Expression checks and rewriting of `${{ }}` text inside jobs.

use super::{BTreeSet, ExprError, Job, Step, UNSUPPORTED_CONTEXTS, Value, expr};

/// Why a job that had errors is missing from those that need it.
/// What happened to a job that had errors, after "which".
pub(super) const FAILED: &str = "did not convert";

/// What happened to a reusable workflow call, after "which".
pub(super) const REUSABLE: &str = "calls a reusable workflow";

/// What in a converted job's expressions `validate` would reject: the
/// GitHub contexts it reads that hatchrun does not provide, in
/// [`UNSUPPORTED_CONTEXTS`] order, and, as error messages, each
/// expression that does not parse, reads an unknown context or reads
/// `parameters`.
pub(super) fn expression_problems(job: &Job) -> (Vec<&'static str>, Vec<String>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut conds: Vec<&str> = job.if_.iter().map(String::as_str).collect();
    texts.extend(job.env.values().map(String::as_str));
    texts.extend(job.outputs.values().map(String::as_str));
    for step in &job.steps {
        match step {
            Step::Run(run) => {
                texts.push(&run.run);
                texts.extend(run.name.as_deref());
                texts.extend(run.shell.as_deref());
                texts.extend(run.working_directory.as_deref());
                texts.extend(run.env.values().map(String::as_str));
                conds.extend(run.if_.as_deref());
            }
            Step::Uses(uses) => {
                texts.push(&uses.uses);
                texts.extend(uses.name.as_deref());
                for value in uses.with.values() {
                    strings(value, &mut texts);
                }
                texts.extend(uses.env.values().map(String::as_str));
                conds.extend(uses.if_.as_deref());
            }
            Step::Checkout(checkout) => {
                texts.extend(checkout.r#ref.as_deref());
                texts.extend(checkout.path.as_deref());
            }
            _ => {}
        }
    }
    let mut bad = Vec::new();
    let mut exprs: Vec<(&str, usize)> = Vec::new();
    for cond in conds {
        if cond.contains("${{") {
            texts.push(cond);
        } else {
            exprs.push((cond, 0));
        }
    }
    for text in texts {
        match expr::expressions(text) {
            Ok(found) => exprs.extend(found.into_iter().map(|(start, inner)| (inner, start))),
            Err(e) => bad.push(format!("has a bad expression: {e}")),
        }
    }
    let mut read = BTreeSet::new();
    let shifted = |e: ExprError, start: usize| ExprError {
        message: e.message,
        offset: e.offset + start,
    };
    for (inner, start) in exprs {
        let names = match expr::contexts(inner) {
            Ok(names) => names,
            Err(e) => {
                bad.push(format!("has a bad expression: {}", shifted(e, start)));
                continue;
            }
        };
        for (name, offset) in names {
            if let Some(&c) = UNSUPPORTED_CONTEXTS.iter().find(|c| **c == name) {
                read.insert(c);
            } else if name == "parameters" {
                bad.push("reads `parameters`, which a converted job does not declare".to_owned());
            } else if !expr::CONTEXTS.contains(&name.as_ref()) {
                let e = ExprError::new(format!("unknown context '{name}'"), offset);
                bad.push(format!("has a bad expression: {}", shifted(e, start)));
                break;
            }
        }
    }
    let read = UNSUPPORTED_CONTEXTS
        .iter()
        .copied()
        .filter(|c| read.contains(*c))
        .collect();
    (read, bad)
}

/// Adds every string inside `value` to `out`.
pub(super) fn strings<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// The reason Docker-phase keys are dropped.
pub(super) const LATER: &str = "; Docker services and containers come in a later release";

/// What `on.workflow_call` is in an `on:` value, when it has one: its
/// map, or null.
pub(super) fn workflow_call(on: &Value) -> Option<Value> {
    match on {
        Value::String(name) if name == "workflow_call" => Some(Value::Null),
        Value::Array(names) if names.iter().any(|n| n.as_str() == Some("workflow_call")) => {
            Some(Value::Null)
        }
        Value::Object(events) => events.get("workflow_call").cloned(),
        _ => None,
    }
}

/// Whether an `on:` value has `workflow_call`.
pub(super) fn calls_workflow(on: &Value) -> bool {
    workflow_call(on).is_some()
}

/// A reader of the dotted names in an expression: given the parts of
/// one, such as `["inputs", "target"]`, it returns how many parts it
/// replaces and what with.
pub(super) type Names<'a> = dyn FnMut(&[&str]) -> Option<(usize, String)> + 'a;

/// The inside of a text that is one `${{ }}` and nothing else.
pub(super) fn whole(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("${{")?.strip_suffix("}}")?;
    (!inner.contains("${{") && !inner.contains("}}")).then_some(inner.trim())
}

/// Whether an expression is a plain name, which needs no parentheses.
pub(super) fn simple(expr: &str) -> bool {
    !expr.is_empty()
        && expr
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// A value as an expression: an expression that was its whole text
/// stays, anything else is a literal.
pub(super) fn expr_text(value: &Value) -> String {
    match value {
        Value::String(text) => match whole(text) {
            Some(expr) if simple(expr) => expr.to_owned(),
            Some(expr) => format!("({expr})"),
            None => format!("'{}'", text.replace('\'', "''")),
        },
        Value::Null => "''".to_owned(),
        other => other.to_string(),
    }
}

/// A text with each dotted name `names` answers replaced. Quoted strings
/// and the parts of longer names are left alone. `bare` says the text is
/// an expression; otherwise the expressions are the `${{ }}` in it.
pub(super) fn rewrite(text: &str, bare: bool, names: &mut Names<'_>) -> String {
    if bare {
        return rewrite_expr(text, names);
    }
    let Ok(found) = expr::expressions(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for (start, inner) in found.into_iter().rev() {
        out.replace_range(start..start + inner.len(), &rewrite_expr(inner, names));
    }
    out
}

/// [`rewrite`] of one expression.
pub(super) fn rewrite_expr(expr: &str, names: &mut Names<'_>) -> String {
    let bytes = expr.as_bytes();
    let part = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let mut out = String::with_capacity(expr.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == b'\'' {
                    if bytes.get(end + 1) == Some(&b'\'') {
                        end += 2;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
            let end = (end + 1).min(bytes.len());
            out.push_str(&expr[i..end]);
            i = end;
        } else if start(b) && (i == 0 || !(part(bytes[i - 1]) || bytes[i - 1] == b'.')) {
            let mut ends = Vec::new();
            let mut end = i;
            loop {
                while end < bytes.len() && part(bytes[end]) {
                    end += 1;
                }
                ends.push(end);
                if bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(|b| start(*b)) {
                    end += 1;
                } else {
                    break;
                }
            }
            let mut from = i;
            let parts: Vec<&str> = ends
                .iter()
                .map(|&to| {
                    let name = &expr[from..to];
                    from = to + 1;
                    name
                })
                .collect();
            let call = bytes.get(end) == Some(&b'(');
            if let Some((n, replacement)) = names(&parts).filter(|_| !call) {
                out.push_str(&replacement);
                i = ends[n - 1];
            } else {
                out.push_str(&expr[i..end]);
                i = end;
            }
        } else {
            let ch = expr[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8().max(1);
        }
    }
    out
}

/// Rewrites one text, which is a bare expression when the flag is set.
pub(super) type Texts<'a> = dyn FnMut(&str, bool) -> String + 'a;

/// Rewrites the expressions of a value's strings, at any depth.
pub(super) fn rewrite_value(value: &mut Value, names: &mut Names<'_>) {
    walk_value(value, &mut |text, bare| rewrite(text, bare, names));
}

/// [`rewrite_value`] with a rewriter of whole texts.
pub(super) fn walk_value(value: &mut Value, texts: &mut Texts<'_>) {
    match value {
        Value::String(text) => *text = texts(text, false),
        Value::Array(items) => items.iter_mut().for_each(|v| walk_value(v, texts)),
        Value::Object(map) => map.values_mut().for_each(|v| walk_value(v, texts)),
        _ => {}
    }
}

/// Rewrites every expression of a converted job.
pub(super) fn map_job(job: &mut Job, names: &mut Names<'_>) {
    walk_job(job, &mut |text, bare| rewrite(text, bare, names));
}

/// [`map_job`] with a rewriter of whole texts.
pub(super) fn walk_job(job: &mut Job, texts: &mut Texts<'_>) {
    fn text(text: &mut String, texts: &mut Texts<'_>) {
        *text = texts(text, false);
    }
    fn opt(text: &mut Option<String>, texts: &mut Texts<'_>) {
        if let Some(t) = text {
            *t = texts(t, false);
        }
    }
    fn cond(text: &mut Option<String>, texts: &mut Texts<'_>) {
        if let Some(t) = text {
            *t = texts(t, !t.contains("${{"));
        }
    }
    job.env.values_mut().for_each(|v| text(v, texts));
    job.outputs.values_mut().for_each(|v| text(v, texts));
    opt(&mut job.runs_on, texts);
    cond(&mut job.if_, texts);
    for step in &mut job.steps {
        match step {
            Step::Run(run) => {
                opt(&mut run.name, texts);
                text(&mut run.run, texts);
                opt(&mut run.shell, texts);
                opt(&mut run.working_directory, texts);
                run.env.values_mut().for_each(|v| text(v, texts));
                cond(&mut run.if_, texts);
            }
            Step::Uses(uses) => {
                opt(&mut uses.name, texts);
                uses.with.values_mut().for_each(|v| walk_value(v, texts));
                uses.env.values_mut().for_each(|v| text(v, texts));
                cond(&mut uses.if_, texts);
            }
            _ => {}
        }
    }
}
