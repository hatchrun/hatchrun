// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Walks an expression tree against a context, with GitHub's loose typing.

use std::cmp::Ordering;

use serde_json::{Number, Value};

use super::parser::{Expr, Op};
use super::{CONTEXTS, ExprError, Functions, JobStatus};

/// Whether `v` counts as true in a condition: everything but `null`,
/// `false`, `0`, `NaN` and the empty string.
#[must_use]
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Builds a JSON number, integral when the value has no fraction, so `2`
/// stays `2` rather than `2.0`. Non-finite values become `null`.
pub(crate) fn number(n: f64) -> Value {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if n.fract() == 0.0 && n.abs() < EXACT {
        #[allow(clippy::cast_possible_truncation)]
        return Value::Number(Number::from(n as i64));
    }
    Number::from_f64(n).map_or(Value::Null, Value::Number)
}

/// Converts a value to a string the way GitHub does when it formats,
/// joins or interpolates it.
pub(crate) fn to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() => format_float(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) => "Array".to_owned(),
        Value::Object(_) => "Object".to_owned(),
    }
}

fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        // Display prints whole floats without a decimal point.
        f.to_string()
    }
}

/// Converts a value to a number: `null` is 0, booleans are 0 or 1, a
/// string is parsed (empty is 0) and anything else is `NaN`.
fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => parse_number(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn parse_number(s: &str) -> f64 {
    let s = s.trim();
    if s.is_empty() {
        return 0.0;
    }
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        #[allow(clippy::cast_precision_loss)]
        return u64::from_str_radix(hex, 16)
            .map_or(f64::NAN, |n| if negative { -(n as f64) } else { n as f64 });
    }
    // Only plain decimal forms; Rust would also accept "inf" and "nan".
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return f64::NAN;
    }
    s.parse().unwrap_or(f64::NAN)
}

/// GitHub's case-insensitive string comparison.
fn fold(s: &str) -> String {
    s.to_uppercase()
}

/// Loose equality: same-typed values compare directly (strings ignoring
/// case, arrays and objects never equal), mixed types compare as numbers.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => fold(x) == fold(y),
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => false,
        _ => to_number(a) == to_number(b),
    }
}

fn compare(op: Op, a: &Value, b: &Value) -> bool {
    let ordering = match (op, a, b) {
        (Op::Eq, ..) => return loose_eq(a, b),
        (Op::Ne, ..) => return !loose_eq(a, b),
        (_, Value::String(x), Value::String(y)) => Some(fold(x).cmp(&fold(y))),
        _ => to_number(a).partial_cmp(&to_number(b)),
    };
    let Some(ordering) = ordering else {
        return false;
    };
    match op {
        Op::Lt => ordering == Ordering::Less,
        Op::Le => ordering != Ordering::Greater,
        Op::Gt => ordering == Ordering::Greater,
        Op::Ge => ordering != Ordering::Less,
        Op::Eq | Op::Ne => unreachable!("handled above"),
    }
}

/// A value, or the array a `*` filter produced. Property access on a
/// filtered array applies to each element and drops what is missing.
enum Val {
    One(Value),
    Many(Vec<Value>),
}

impl Val {
    fn into_value(self) -> Value {
        match self {
            Val::One(v) => v,
            Val::Many(vs) => Value::Array(vs),
        }
    }

    fn deref(self, key: &Value) -> Val {
        match self {
            Val::One(v) => Val::One(lookup(&v, key)),
            Val::Many(vs) => Val::Many(
                vs.iter()
                    .map(|v| lookup(v, key))
                    .filter(|v| !v.is_null())
                    .collect(),
            ),
        }
    }

    fn filter(self) -> Val {
        let children = |v: Value| match v {
            Value::Array(items) => items,
            Value::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
            _ => Vec::new(),
        };
        match self {
            Val::One(v) => Val::Many(children(v)),
            Val::Many(vs) => Val::Many(vs.into_iter().flat_map(children).collect()),
        }
    }
}

/// Reads `v[key]`: an object key (exact, then ignoring case) or an array
/// index. Anything missing is `null`.
fn lookup(v: &Value, key: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let key = to_string(key);
            map.get(&key)
                .or_else(|| {
                    map.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                        .map(|(_, v)| v)
                })
                .cloned()
                .unwrap_or(Value::Null)
        }
        Value::Array(items) => match key {
            Value::Number(n) => n
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .and_then(|f| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    items.get(f as usize)
                })
                .cloned()
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

/// Evaluates expression trees against one context.
pub(crate) struct Evaluator<'a> {
    pub ctx: &'a Value,
    pub funcs: &'a dyn Functions,
}

impl Evaluator<'_> {
    /// Checks every context name, including ones a short circuit would
    /// skip, then evaluates.
    pub(crate) fn eval(&self, expr: &Expr) -> Result<Value, ExprError> {
        self.check(expr)?;
        Ok(self.walk(expr)?.into_value())
    }

    fn context(&self, name: &str) -> Option<Value> {
        let given = self.ctx.as_object().and_then(|m| {
            m.get(name).or_else(|| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        });
        if let Some(v) = given {
            return Some(v.clone());
        }
        CONTEXTS
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
            .then_some(Value::Null)
    }

    fn check(&self, expr: &Expr) -> Result<(), ExprError> {
        match expr {
            Expr::Literal(_) => Ok(()),
            Expr::Context { name, offset } => match self.context(name) {
                Some(_) => Ok(()),
                None => Err(ExprError::new(format!("unknown context '{name}'"), *offset)),
            },
            Expr::Property { target, .. } | Expr::Filter(target) | Expr::Not(target) => {
                self.check(target)
            }
            Expr::Index { target, index } => {
                self.check(target)?;
                self.check(index)
            }
            Expr::Call { args, .. } => args.iter().try_for_each(|a| self.check(a)),
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Compare(_, l, r) => {
                self.check(l)?;
                self.check(r)
            }
        }
    }

    fn value(&self, expr: &Expr) -> Result<Value, ExprError> {
        Ok(self.walk(expr)?.into_value())
    }

    fn walk(&self, expr: &Expr) -> Result<Val, ExprError> {
        let v = match expr {
            Expr::Literal(v) => v.clone(),
            Expr::Context { name, .. } => self.context(name).unwrap_or(Value::Null),
            Expr::Property { target, name } => {
                return Ok(self.walk(target)?.deref(&Value::String(name.clone())));
            }
            Expr::Index { target, index } => {
                let target = self.walk(target)?;
                return Ok(target.deref(&self.value(index)?));
            }
            Expr::Filter(target) => return Ok(self.walk(target)?.filter()),
            Expr::Not(inner) => Value::Bool(!truthy(&self.value(inner)?)),
            Expr::And(l, r) => {
                let l = self.value(l)?;
                if truthy(&l) { self.value(r)? } else { l }
            }
            Expr::Or(l, r) => {
                let l = self.value(l)?;
                if truthy(&l) { l } else { self.value(r)? }
            }
            Expr::Compare(op, l, r) => Value::Bool(compare(*op, &self.value(l)?, &self.value(r)?)),
            Expr::Call { name, args, offset } => {
                let args = args
                    .iter()
                    .map(|a| self.value(a))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call(name, &args, *offset)?
            }
        };
        Ok(Val::One(v))
    }

    fn call(&self, name: &str, args: &[Value], offset: usize) -> Result<Value, ExprError> {
        let string = |i: usize| args.get(i).map(to_string).unwrap_or_default();
        let v = match name {
            "contains" => Value::Bool(match &args[0] {
                Value::Array(items) => items.iter().any(|item| loose_eq(item, &args[1])),
                Value::Object(_) => false,
                search => fold(&to_string(search)).contains(&fold(&string(1))),
            }),
            "startswith" => Value::Bool(fold(&string(0)).starts_with(&fold(&string(1)))),
            "endswith" => Value::Bool(fold(&string(0)).ends_with(&fold(&string(1)))),
            "format" => Value::String(format(&string(0), &args[1..], offset)?),
            "join" => Value::String(match &args[0] {
                Value::Array(items) => {
                    let sep = if args.len() > 1 {
                        string(1)
                    } else {
                        ",".to_owned()
                    };
                    items.iter().map(to_string).collect::<Vec<_>>().join(&sep)
                }
                other => to_string(other),
            }),
            "tojson" => Value::String(
                serde_json::to_string_pretty(&args[0])
                    .map_err(|e| ExprError::new(format!("toJSON: {e}"), offset))?,
            ),
            "fromjson" => serde_json::from_str(&string(0))
                .map_err(|e| ExprError::new(format!("fromJSON: {e}"), offset))?,
            "hashfiles" => {
                let patterns: Vec<String> = args.iter().map(to_string).collect();
                let hash = self
                    .funcs
                    .hash_files(&patterns)
                    .map_err(|e| ExprError::new(e.message, offset))?;
                Value::String(hash)
            }
            "success" => Value::Bool(self.funcs.status() == JobStatus::Success),
            "failure" => Value::Bool(self.funcs.status() == JobStatus::Failure),
            "cancelled" => Value::Bool(self.funcs.status() == JobStatus::Cancelled),
            "always" => Value::Bool(true),
            _ => return Err(ExprError::new(format!("unknown function '{name}'"), offset)),
        };
        Ok(v)
    }
}

/// `format(fmt, args...)`: `{N}` takes argument N, `{{` and `}}` are
/// literal braces.
fn format(fmt: &str, args: &[Value], offset: usize) -> Result<String, ExprError> {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                let mut digits = String::new();
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    digits.push(d);
                }
                if digits.is_empty() || chars.next() != Some('}') {
                    return Err(ExprError::new(
                        format!("format: invalid placeholder in '{fmt}'"),
                        offset,
                    ));
                }
                let arg = digits
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| args.get(i))
                    .ok_or_else(|| {
                        ExprError::new(format!("format: no argument for {{{digits}}}"), offset)
                    })?;
                out.push_str(&to_string(arg));
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '}' => {
                return Err(ExprError::new(
                    format!("format: unmatched '}}' in '{fmt}'"),
                    offset,
                ));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}
