// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Walks an expression tree against a context, with GitHub's loose typing.

use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use super::parser::{Expr, Op, Step};
use super::{CONTEXTS, ExprError, Functions, JobStatus, MAX_MEMORY, MAX_STRING};

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

/// The length of [`to_string`] of `v`, without copying a string.
fn text_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        other => to_string(other).len(),
    }
}

/// Passes [`to_string`] of `v` to `f`, without copying a string.
fn with_text<R>(v: &Value, f: impl FnOnce(&str) -> R) -> R {
    match v {
        Value::String(s) => f(s),
        other => f(&to_string(other)),
    }
}

/// Fails when a function would make a string longer than [`MAX_STRING`].
fn fits(len: usize, name: &str, offset: usize) -> Result<(), ExprError> {
    if len > MAX_STRING {
        return Err(ExprError::new(
            format!("{name}() would make a string of {len} bytes; the limit is {MAX_STRING}"),
            offset,
        ));
    }
    Ok(())
}

/// The bytes one `evaluate` or `interpolate` call has spent on the
/// strings it made, the values `fromJSON` parsed and the values it
/// copied out of the context. Every such
/// allocation is charged before it is made, and nothing is given back.
pub(crate) struct Budget {
    used: Cell<usize>,
    over: Cell<bool>,
}

impl Budget {
    pub(crate) fn new() -> Self {
        Self {
            used: Cell::new(0),
            over: Cell::new(false),
        }
    }

    /// Takes `bytes` from the budget, or fails with an error at `offset`
    /// that names `what` and the limit.
    pub(crate) fn charge(&self, bytes: usize, what: &str, offset: usize) -> Result<(), ExprError> {
        let used = self.used.get().saturating_add(bytes);
        if used > MAX_MEMORY {
            self.over.set(true);
            return Err(ExprError::new(
                format!(
                    "{what} would take the expression past its memory limit of {MAX_MEMORY} bytes"
                ),
                offset,
            ));
        }
        self.used.set(used);
        Ok(())
    }
}

/// Counts bytes written to it and fails past [`MAX_STRING`], so a value
/// too big to serialize is found without serializing all of it.
struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        if self.0 > MAX_STRING {
            return Err(std::io::Error::other("too long"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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

/// The characters of `s` in upper case, one by one, as
/// `str::to_uppercase` would make them, without making a string.
fn upper(s: &str) -> impl DoubleEndedIterator<Item = char> + '_ {
    s.chars().flat_map(char::to_uppercase)
}

/// GitHub's case-insensitive string equality.
fn eq_folded(x: &str, y: &str) -> bool {
    if x.is_ascii() && y.is_ascii() {
        return x.eq_ignore_ascii_case(y);
    }
    upper(x).eq(upper(y))
}

/// GitHub's case-insensitive string order.
fn cmp_folded(x: &str, y: &str) -> Ordering {
    if x.is_ascii() && y.is_ascii() {
        let up = u8::to_ascii_uppercase;
        return x.bytes().map(|b| up(&b)).cmp(y.bytes().map(|b| up(&b)));
    }
    upper(x).cmp(upper(y))
}

/// Whether `s` starts with `prefix`, ignoring case.
fn starts_folded(s: &str, prefix: &str) -> bool {
    let mut chars = upper(s);
    upper(prefix).all(|c| chars.next() == Some(c))
}

/// Whether `s` ends with `suffix`, ignoring case.
fn ends_folded(s: &str, suffix: &str) -> bool {
    let mut chars = upper(s).rev();
    upper(suffix).rev().all(|c| chars.next() == Some(c))
}

/// Whether `needle` occurs in `hay`, ignoring case. Only the needle is
/// copied; the haystack is read once, as Knuth-Morris-Pratt reads it.
/// The folded needle and its table are charged to `budget` before they
/// are made, and a needle longer than the haystack is not copied at all.
fn contains_folded(
    hay: &str,
    needle: &str,
    budget: &Budget,
    offset: usize,
) -> Result<bool, ExprError> {
    // Per needle character: the folded character and its KMP entry.
    const TABLE: usize = std::mem::size_of::<u32>();
    if hay.is_ascii() && needle.is_ascii() {
        if needle.len() > hay.len() {
            return Ok(false);
        }
        budget.charge(needle.len().saturating_mul(1 + TABLE), "contains()", offset)?;
        let needle: Vec<u8> = needle.bytes().map(|b| b.to_ascii_uppercase()).collect();
        return Ok(kmp(hay.bytes().map(|b| b.to_ascii_uppercase()), &needle));
    }
    let len = upper(needle).count();
    if len > upper(hay).count() {
        return Ok(false);
    }
    budget.charge(
        len.saturating_mul(std::mem::size_of::<char>() + TABLE),
        "contains()",
        offset,
    )?;
    let needle: Vec<char> = upper(needle).collect();
    Ok(kmp(upper(hay), &needle))
}

/// Whether `needle` occurs in `hay`, in time linear in both.
fn kmp<T: Copy + Eq>(hay: impl Iterator<Item = T>, needle: &[T]) -> bool {
    if needle.is_empty() {
        return true;
    }
    // fail[i]: the length of the longest proper prefix of needle[..=i]
    // that is also its suffix.
    let mut fail = vec![0u32; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle[i] != needle[k] {
            k = fail[k - 1] as usize;
        }
        if needle[i] == needle[k] {
            k += 1;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            fail[i] = k as u32;
        }
    }
    let mut k = 0;
    for c in hay {
        while k > 0 && c != needle[k] {
            k = fail[k - 1] as usize;
        }
        if c == needle[k] {
            k += 1;
            if k == needle.len() {
                return true;
            }
        }
    }
    false
}

/// Loose equality: same-typed values compare directly (strings ignoring
/// case, arrays and objects never equal), mixed types compare as numbers.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => eq_folded(x, y),
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => false,
        _ => to_number(a) == to_number(b),
    }
}

fn compare(op: Op, a: &Value, b: &Value) -> bool {
    // Strings order ignoring case; anything else orders as numbers, and
    // NaN orders against nothing.
    let ordering = || match (a, b) {
        (Value::String(x), Value::String(y)) => Some(cmp_folded(x, y)),
        _ => to_number(a).partial_cmp(&to_number(b)),
    };
    match op {
        Op::Eq => loose_eq(a, b),
        Op::Ne => !loose_eq(a, b),
        Op::Lt => ordering() == Some(Ordering::Less),
        Op::Le => matches!(ordering(), Some(Ordering::Less | Ordering::Equal)),
        Op::Gt => ordering() == Some(Ordering::Greater),
        Op::Ge => matches!(ordering(), Some(Ordering::Greater | Ordering::Equal)),
    }
}

/// The `null` a known context reads as when the context object leaves
/// it out, and a missing property reads as.
static NULL: Value = Value::Null;

/// What a `*` filter's result is where only its type matters.
/// Comparisons, truthiness and text never look inside an array.
static ARRAY: Value = Value::Array(Vec::new());

/// A value borrowed from the context or the expression, or one made
/// while evaluating. Reads walk the context by reference; a borrowed
/// value is copied only when it is returned, and the copy is charged.
type Ref<'a> = Cow<'a, Value>;

/// A value; the array a `*` filter produced; or such an array passed on
/// by `&&` or `||`, which is an ordinary array again. Property access on
/// a filtered array applies to each element and drops what is missing.
/// The lists hold references, so neither copies the context.
enum Val<'a> {
    One(Ref<'a>),
    Many(Vec<Ref<'a>>),
    List(Vec<Ref<'a>>),
}

/// The bytes a list of `n` references takes, as charged to the budget.
fn list_bytes(n: usize) -> usize {
    n.saturating_mul(std::mem::size_of::<Ref<'_>>())
}

impl<'a> Val<'a> {
    /// The value, with a list standing in as an empty array.
    fn scalar(&self) -> &Value {
        match self {
            Val::One(v) => v,
            Val::Many(_) | Val::List(_) => &ARRAY,
        }
    }

    /// The items of a list, or `None` for one value.
    fn items(&self) -> Option<&[Ref<'a>]> {
        match self {
            Val::One(_) => None,
            Val::Many(items) | Val::List(items) => Some(items),
        }
    }

    fn deref(self, key: Key<'_>, budget: &Budget) -> Result<Val<'a>, ExprError> {
        Ok(match self {
            Val::One(v) => Val::One(lookup(v, key)),
            Val::Many(vs) => {
                budget.charge(list_bytes(vs.len()), "a `*` filter", 0)?;
                Val::Many(
                    vs.into_iter()
                        .map(|v| lookup(v, key))
                        .filter(|v| !v.is_null())
                        .collect(),
                )
            }
            // An array: only a whole-number index finds an item.
            Val::List(mut items) => Val::One(match key.index() {
                Some(i) if i < items.len() => items.swap_remove(i),
                _ => Cow::Borrowed(&NULL),
            }),
        })
    }

    fn filter(self, budget: &Budget) -> Result<Val<'a>, ExprError> {
        Ok(match self {
            Val::One(v) => {
                budget.charge(list_bytes(child_count(&v)), "a `*` filter", 0)?;
                let mut out = Vec::new();
                children(v, &mut out);
                Val::Many(out)
            }
            Val::Many(vs) => {
                let n = vs.iter().map(|v| child_count(v)).sum();
                budget.charge(list_bytes(n), "a `*` filter", 0)?;
                let mut out = Vec::with_capacity(n);
                for v in vs {
                    children(v, &mut out);
                }
                Val::Many(out)
            }
            // The items of an array are already a list.
            Val::List(items) => Val::Many(items),
        })
    }
}

/// How many items an array or values an object has; 0 for anything else.
fn child_count(v: &Value) -> usize {
    match v {
        Value::Array(items) => items.len(),
        Value::Object(map) => map.len(),
        _ => 0,
    }
}

/// Adds the items of an array or the values of an object to `out`,
/// borrowing what was borrowed and moving what was owned.
fn children<'a>(v: Ref<'a>, out: &mut Vec<Ref<'a>>) {
    match v {
        Cow::Borrowed(Value::Array(items)) => out.extend(items.iter().map(Cow::Borrowed)),
        Cow::Borrowed(Value::Object(map)) => out.extend(map.values().map(Cow::Borrowed)),
        Cow::Owned(Value::Array(items)) => out.extend(items.into_iter().map(Cow::Owned)),
        Cow::Owned(Value::Object(map)) => out.extend(map.into_iter().map(|(_, v)| Cow::Owned(v))),
        Cow::Borrowed(_) | Cow::Owned(_) => {}
    }
}

/// A property name or an index value.
#[derive(Clone, Copy)]
enum Key<'k> {
    Name(&'k str),
    Value(&'k Value),
}

impl Key<'_> {
    /// The key as an array index: a whole, non-negative number.
    fn index(self) -> Option<usize> {
        let Key::Value(Value::Number(n)) = self else {
            return None;
        };
        n.as_f64()
            .filter(|f| f.fract() == 0.0 && *f >= 0.0)
            .map(|f| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    f as usize
                }
            })
    }
}

/// Reads `v[key]`: an object key (exact, then ignoring case) or an array
/// index. Anything missing is `null`. A borrowed value gives a borrowed
/// child; an owned one gives up its child without copying it.
fn lookup<'a>(v: Ref<'a>, key: Key<'_>) -> Ref<'a> {
    match v {
        Cow::Borrowed(v) => Cow::Borrowed(find(v, key).unwrap_or(&NULL)),
        Cow::Owned(v) => Cow::Owned(take(v, key)),
    }
}

fn find<'v>(v: &'v Value, key: Key<'_>) -> Option<&'v Value> {
    match v {
        Value::Object(map) => with_key(key, |name| {
            map.get(name).or_else(|| {
                map.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        }),
        Value::Array(items) => key.index().and_then(|i| items.get(i)),
        _ => None,
    }
}

fn take(v: Value, key: Key<'_>) -> Value {
    match v {
        Value::Object(mut map) => with_key(key, |name| {
            if let Some(found) = map.remove(name) {
                return found;
            }
            map.into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map_or(Value::Null, |(_, v)| v)
        }),
        Value::Array(mut items) => match key.index() {
            Some(i) if i < items.len() => items.swap_remove(i),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

/// Passes the key as an object key to `f`, copying it only when it is
/// not already a string.
fn with_key<R>(key: Key<'_>, f: impl FnOnce(&str) -> R) -> R {
    match key {
        Key::Name(name) => f(name),
        Key::Value(Value::String(name)) => f(name),
        Key::Value(other) => f(&to_string(other)),
    }
}

/// The bytes a copy of `v` holds, counted the way `fromJSON` charges a
/// parsed value. Counting stops once it passes `limit`.
fn copy_bytes(v: &Value, total: &mut usize, limit: usize) {
    let own = match v {
        Value::String(s) => s.len(),
        _ => 0,
    };
    *total = total.saturating_add(std::mem::size_of::<Value>() + own);
    match v {
        Value::Array(items) => {
            for item in items {
                if *total > limit {
                    return;
                }
                copy_bytes(item, total, limit);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                if *total > limit {
                    return;
                }
                *total = total.saturating_add(std::mem::size_of::<String>() + key.len());
                copy_bytes(item, total, limit);
            }
        }
        _ => {}
    }
}

/// Calls `f` with the name and offset of every context `expr` reads,
/// including ones a short circuit would skip, stopping at its first
/// error.
pub(crate) fn visit_contexts<'s>(
    expr: &Expr<'s>,
    f: &mut dyn FnMut(&'s str, usize) -> Result<(), ExprError>,
) -> Result<(), ExprError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Context { name, offset } => f(name, *offset),
        Expr::Not(inner) => visit_contexts(inner, f),
        Expr::Access { base, path } => {
            visit_contexts(base, f)?;
            path.iter().try_for_each(|step| match step {
                Step::Index(index) => visit_contexts(index, f),
                Step::Property(_) | Step::Filter => Ok(()),
            })
        }
        Expr::Call { args, .. } | Expr::And(args) | Expr::Or(args) => {
            args.iter().try_for_each(|a| visit_contexts(a, f))
        }
        Expr::Compare { first, rest } => {
            visit_contexts(first, f)?;
            rest.iter().try_for_each(|(_, e)| visit_contexts(e, f))
        }
    }
}

/// Whether `expr` calls a function whose answer is the job status.
pub(crate) fn calls_status(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Context { .. } => false,
        Expr::Not(inner) => calls_status(inner),
        Expr::Access { base, path } => {
            calls_status(base)
                || path.iter().any(|step| match step {
                    Step::Index(index) => calls_status(index),
                    Step::Property(_) | Step::Filter => false,
                })
        }
        Expr::Call { name, args, .. } => {
            matches!(*name, "success" | "failure" | "cancelled" | "always")
                || args.iter().any(calls_status)
        }
        Expr::And(args) | Expr::Or(args) => args.iter().any(calls_status),
        Expr::Compare { first, rest } => {
            calls_status(first) || rest.iter().any(|(_, e)| calls_status(e))
        }
    }
}

/// Evaluates expression trees against one context.
pub(crate) struct Evaluator<'a> {
    pub ctx: &'a Value,
    pub funcs: &'a dyn Functions,
    pub budget: &'a Budget,
}

impl<'a> Evaluator<'a> {
    /// Checks every context name, including ones a short circuit would
    /// skip, then evaluates. The result is copied out of the context
    /// only if it was read from there, and the copy is charged.
    pub(crate) fn eval(&self, expr: &'a Expr<'_>) -> Result<Value, ExprError> {
        self.check(expr)?;
        match self.walk(expr)? {
            Val::One(v) => self.own(v),
            Val::Many(vs) | Val::List(vs) => self.own_all(vs).map(Value::Array),
        }
    }

    /// Like [`Evaluator::eval`], but passes the result as text to `f`
    /// without copying it.
    pub(crate) fn eval_text<R>(
        &self,
        expr: &'a Expr<'_>,
        f: impl FnOnce(&str) -> R,
    ) -> Result<R, ExprError> {
        self.check(expr)?;
        Ok(with_text(self.walk(expr)?.scalar(), f))
    }

    /// A value the caller keeps: borrowed values are copied, after the
    /// copy is charged.
    fn own(&self, v: Ref<'a>) -> Result<Value, ExprError> {
        match v {
            Cow::Owned(v) => Ok(v),
            Cow::Borrowed(v) => {
                let mut bytes = 0;
                copy_bytes(v, &mut bytes, MAX_MEMORY);
                self.budget.charge(bytes, "copying a value", 0)?;
                Ok(v.clone())
            }
        }
    }

    fn own_all(&self, vs: Vec<Ref<'a>>) -> Result<Vec<Value>, ExprError> {
        self.budget
            .charge(std::mem::size_of::<Value>(), "copying a value", 0)?;
        vs.into_iter().map(|v| self.own(v)).collect()
    }

    fn context(&self, name: &str) -> Option<&'a Value> {
        let given = self.ctx.as_object().and_then(|m| {
            m.get(name).or_else(|| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        });
        if given.is_some() {
            return given;
        }
        CONTEXTS
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
            .then_some(&NULL)
    }

    fn check(&self, expr: &Expr<'_>) -> Result<(), ExprError> {
        visit_contexts(expr, &mut |name, offset| match self.context(name) {
            Some(_) => Ok(()),
            None => Err(ExprError::new(format!("unknown context '{name}'"), offset)),
        })
    }

    fn walk(&self, expr: &'a Expr<'_>) -> Result<Val<'a>, ExprError> {
        let v = match expr {
            Expr::Literal(v) => Cow::Borrowed(v),
            Expr::Context { name, .. } => Cow::Borrowed(self.context(name).unwrap_or(&NULL)),
            Expr::Access { base, path } => {
                let mut val = self.walk(base)?;
                for step in path {
                    val = match step {
                        Step::Property(name) => val.deref(Key::Name(name), self.budget)?,
                        Step::Index(index) => {
                            let key = self.walk(index)?;
                            val.deref(Key::Value(key.scalar()), self.budget)?
                        }
                        Step::Filter => val.filter(self.budget)?,
                    };
                }
                return Ok(val);
            }
            Expr::Not(inner) => Cow::Owned(Value::Bool(!truthy(self.walk(inner)?.scalar()))),
            // `&&` returns the first falsy operand, `||` the first truthy
            // one, and each returns its last operand when none qualifies.
            Expr::And(operands) => return self.first(operands, false),
            Expr::Or(operands) => return self.first(operands, true),
            Expr::Compare { first, rest } => {
                let mut acc = self.walk(first)?;
                for (op, e) in rest {
                    let rhs = self.walk(e)?;
                    acc = Val::One(Cow::Owned(Value::Bool(compare(
                        *op,
                        acc.scalar(),
                        rhs.scalar(),
                    ))));
                }
                // The parser gives a comparison at least one operator.
                return Ok(acc);
            }
            Expr::Call { name, args, offset } => {
                let args = args
                    .iter()
                    .map(|a| self.walk(a))
                    .collect::<Result<Vec<_>, _>>()?;
                Cow::Owned(self.call(name, &args, *offset)?)
            }
        };
        Ok(Val::One(v))
    }

    /// Evaluates `operands` in order and returns the first whose
    /// truthiness is `stop_when`, or the last one.
    /// A filtered array comes out as an ordinary array, still borrowed.
    fn first(&self, operands: &'a [Expr<'_>], stop_when: bool) -> Result<Val<'a>, ExprError> {
        let mut last = Val::One(Cow::Borrowed(&NULL));
        for e in operands {
            last = self.walk(e)?;
            if truthy(last.scalar()) == stop_when {
                break;
            }
        }
        Ok(match last {
            Val::Many(items) => Val::List(items),
            other => other,
        })
    }

    fn call(&self, name: &str, args: &[Val<'a>], offset: usize) -> Result<Value, ExprError> {
        let arg = |i: usize| args.get(i).map_or(&NULL, Val::scalar);
        let v = match name {
            "contains" => Value::Bool(contains(&args[0], args[1].scalar(), offset, self.budget)?),
            "startswith" => Value::Bool(with_text(arg(0), |s| {
                with_text(arg(1), |prefix| starts_folded(s, prefix))
            })),
            "endswith" => Value::Bool(with_text(arg(0), |s| {
                with_text(arg(1), |suffix| ends_folded(s, suffix))
            })),
            "format" => Value::String(with_text(arg(0), |fmt| {
                format(fmt, &args[1..], offset, self.budget)
            })?),
            "join" => Value::String(join(args, offset, self.budget)?),
            "tojson" => Value::String(match &args[0] {
                Val::One(v) => to_json(&**v, offset, self.budget)?,
                Val::Many(vs) | Val::List(vs) => to_json(vs, offset, self.budget)?,
            }),
            "fromjson" => with_text(arg(0), |t| from_json(t, offset, self.budget))?,
            "hashfiles" => {
                let patterns: Vec<String> = args.iter().map(|a| to_string(a.scalar())).collect();
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

/// `contains(search, item)`: an array holds an item loosely equal to
/// `item`, or the text of `search` holds the text of `item`, ignoring
/// case. An object holds nothing.
fn contains(
    search: &Val<'_>,
    item: &Value,
    offset: usize,
    budget: &Budget,
) -> Result<bool, ExprError> {
    if let Some(items) = search.items() {
        return Ok(items.iter().any(|v| loose_eq(v, item)));
    }
    Ok(match search.scalar() {
        Value::Array(items) => items.iter().any(|v| loose_eq(v, item)),
        Value::Object(_) => false,
        text => with_text(text, |hay| {
            with_text(item, |needle| contains_folded(hay, needle, budget, offset))
        })?,
    })
}

/// `join(array, sep)`: the items as text with `sep` (`,` by default)
/// between them. Anything but an array is returned as text.
fn join(args: &[Val<'_>], offset: usize, budget: &Budget) -> Result<String, ExprError> {
    let sep = args
        .get(1)
        .map_or_else(|| ",".to_owned(), |a| to_string(a.scalar()));
    match &args[0] {
        Val::Many(items) | Val::List(items) => {
            join_items(items.iter().map(|v| &**v), &sep, offset, budget)
        }
        Val::One(v) => match &**v {
            Value::Array(items) => join_items(items.iter(), &sep, offset, budget),
            other => {
                let len = text_len(other);
                fits(len, "join", offset)?;
                budget.charge(len, "join()", offset)?;
                Ok(to_string(other))
            }
        },
    }
}

fn join_items<'v>(
    items: impl Iterator<Item = &'v Value> + Clone,
    sep: &str,
    offset: usize,
    budget: &Budget,
) -> Result<String, ExprError> {
    let (count, text) = items.clone().fold((0usize, 0usize), |(n, len), v| {
        (n + 1, len.saturating_add(text_len(v)))
    });
    let len = text.saturating_add(sep.len().saturating_mul(count.saturating_sub(1)));
    fits(len, "join", offset)?;
    budget.charge(len, "join()", offset)?;
    let mut out = String::with_capacity(len);
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        with_text(item, |t| out.push_str(t));
    }
    Ok(out)
}

/// `toJSON(value)`: the value as indented JSON.
fn to_json<T: serde::Serialize + ?Sized>(
    value: &T,
    offset: usize,
    budget: &Budget,
) -> Result<String, ExprError> {
    let mut count = Counter(0);
    if serde_json::to_writer_pretty(&mut count, value).is_err() {
        fits(count.0, "toJSON", offset)?;
    }
    budget.charge(count.0, "toJSON()", offset)?;
    serde_json::to_string_pretty(value).map_err(|e| ExprError::new(format!("toJSON: {e}"), offset))
}

/// `format(fmt, args...)`: `{N}` takes argument N, `{{` and `}}` are
/// literal braces. The length is worked out first, then the string is
/// made.
fn format(
    fmt: &str,
    args: &[Val<'_>],
    offset: usize,
    budget: &Budget,
) -> Result<String, ExprError> {
    let mut len = 0usize;
    format_parts(fmt, args, offset, &mut |part| {
        len = len.saturating_add(part.len());
    })?;
    fits(len, "format", offset)?;
    budget.charge(len, "format()", offset)?;
    let mut out = String::with_capacity(len);
    format_parts(fmt, args, offset, &mut |part| out.push_str(part))?;
    Ok(out)
}

/// Passes each piece of a `format` result to `emit`, in order.
fn format_parts(
    fmt: &str,
    args: &[Val<'_>],
    offset: usize,
    emit: &mut dyn FnMut(&str),
) -> Result<(), ExprError> {
    let mut buf = [0; 4];
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                emit("{");
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
                with_text(arg.scalar(), |t| emit(t));
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                emit("}");
            }
            '}' => {
                return Err(ExprError::new(
                    format!("format: unmatched '}}' in '{fmt}'"),
                    offset,
                ));
            }
            c => emit(c.encode_utf8(&mut buf)),
        }
    }
    Ok(())
}

/// `fromJSON(text)`: parses `text`, charging each value to `budget` as
/// it is built, so a large document fails before it is all in memory.
fn from_json(text: &str, offset: usize, budget: &Budget) -> Result<Value, ExprError> {
    let mut de = serde_json::Deserializer::from_str(text);
    let parsed = Parse { budget, offset }
        .deserialize(&mut de)
        .and_then(|v| de.end().map(|()| v));
    parsed.map_err(|e| {
        if budget.over.get() {
            ExprError::new(
                format!("fromJSON() would take the expression past its memory limit of {MAX_MEMORY} bytes"),
                offset,
            )
        } else {
            ExprError::new(format!("fromJSON: {e}"), offset)
        }
    })
}

/// Builds a [`Value`] the way `serde_json` does, charging every node
/// (`size_of::<Value>()`) and every string and key byte first.
#[derive(Clone, Copy)]
struct Parse<'b> {
    budget: &'b Budget,
    offset: usize,
}

impl Parse<'_> {
    fn charge<E: de::Error>(self, bytes: usize) -> Result<(), E> {
        self.budget
            .charge(bytes, "fromJSON()", self.offset)
            .map_err(|e| E::custom(e.message))
    }

    fn node<E: de::Error>(self, v: Value) -> Result<Value, E> {
        self.charge(std::mem::size_of::<Value>())?;
        Ok(v)
    }
}

impl<'de> DeserializeSeed<'de> for Parse<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Parse<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        self.node(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        self.node(Value::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        self.node(Value::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        self.node(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.charge(std::mem::size_of::<Value>().saturating_add(v.len()))?;
        Ok(Value::String(v.to_owned()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        self.node(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        self.node(Value::Null)
    }

    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        self.charge(std::mem::size_of::<Value>())?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.charge(std::mem::size_of::<Value>())?;
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            self.charge(std::mem::size_of::<String>().saturating_add(key.len()))?;
            let value = map.next_value_seed(self)?;
            out.insert(key, value);
        }
        Ok(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strings whose upper case is longer, shorter or special, mixed
    /// with ASCII.
    const WORDS: &[&str] = &[
        "", "a", "A", "ab", "AB", "abc", "ss", "SS", "ß", "aß", "ßa", "straße", "STRASSE", "é",
        "É", "ﬁ", "FI", "ﬁx", "İ", "i", "ς", "Σ", "aaab", "aab", "abab", "ababab", "x", "ẞ",
    ];

    #[test]
    fn folding_matches_to_uppercase() {
        let fold = |s: &str| s.to_uppercase();
        for x in WORDS {
            for y in WORDS {
                let (fx, fy) = (fold(x), fold(y));
                assert_eq!(eq_folded(x, y), fx == fy, "{x:?} == {y:?}");
                assert_eq!(cmp_folded(x, y), fx.cmp(&fy), "{x:?} cmp {y:?}");
                assert_eq!(
                    starts_folded(x, y),
                    fx.starts_with(&fy),
                    "{x:?} starts {y:?}"
                );
                assert_eq!(ends_folded(x, y), fx.ends_with(&fy), "{x:?} ends {y:?}");
                assert_eq!(
                    contains_folded(x, y, &Budget::new(), 0).unwrap(),
                    fx.contains(&fy),
                    "{x:?} contains {y:?}"
                );
                let (xx, yy) = (format!("{x}{y}{x}"), format!("{y}{x}"));
                assert_eq!(
                    contains_folded(&xx, &yy, &Budget::new(), 0).unwrap(),
                    fold(&xx).contains(&fold(&yy)),
                    "{xx:?} contains {yy:?}"
                );
            }
        }
    }
}
