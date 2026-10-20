// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Splits an expression into tokens.

use super::ExprError;

/// What a token is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// A name: a context, property, function or `true`/`false`/`null`.
    Ident(String),
    Number(f64),
    Str(String),
    Dot,
    Star,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Not,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Eof,
}

/// A token and the byte offset where it starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: Kind,
    pub offset: usize,
}

/// Lexes `src`. The last token is always [`Kind::Eof`].
pub(crate) fn lex(src: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let two = |next: u8| bytes.get(i + 1) == Some(&next);
        let kind = match c {
            b'.' => Kind::Dot,
            b'*' => Kind::Star,
            b',' => Kind::Comma,
            b'(' => Kind::LParen,
            b')' => Kind::RParen,
            b'[' => Kind::LBracket,
            b']' => Kind::RBracket,
            b'<' if two(b'=') => Kind::Le,
            b'<' => Kind::Lt,
            b'>' if two(b'=') => Kind::Ge,
            b'>' => Kind::Gt,
            b'=' if two(b'=') => Kind::Eq,
            b'!' if two(b'=') => Kind::Ne,
            b'!' => Kind::Not,
            b'&' if two(b'&') => Kind::And,
            b'|' if two(b'|') => Kind::Or,
            b'\'' => {
                let (s, end) = string(src, start)?;
                i = end;
                tokens.push(Token {
                    kind: Kind::Str(s),
                    offset: start,
                });
                continue;
            }
            b'0'..=b'9' | b'-' => {
                let (n, end) = number(src, start)?;
                i = end;
                tokens.push(Token {
                    kind: Kind::Number(n),
                    offset: start,
                });
                continue;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let mut end = i + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-'))
                {
                    end += 1;
                }
                i = end;
                tokens.push(Token {
                    kind: Kind::Ident(src[start..end].to_owned()),
                    offset: start,
                });
                continue;
            }
            _ => {
                let ch = src[start..].chars().next().unwrap_or_default();
                return Err(ExprError::new(
                    format!("unexpected character '{ch}'"),
                    start,
                ));
            }
        };
        i += match kind {
            Kind::Le | Kind::Ge | Kind::Eq | Kind::Ne | Kind::And | Kind::Or => 2,
            _ => 1,
        };
        tokens.push(Token {
            kind,
            offset: start,
        });
    }
    tokens.push(Token {
        kind: Kind::Eof,
        offset: src.len(),
    });
    Ok(tokens)
}

/// Reads a single-quoted string starting at `start`, where `''` stands for
/// one quote. Returns the text and the offset after the closing quote.
fn string(src: &str, start: usize) -> Result<(String, usize), ExprError> {
    let bytes = src.as_bytes();
    let mut out = String::new();
    let mut i = start + 1;
    let mut chunk = i;
    loop {
        match bytes.get(i) {
            None => return Err(ExprError::new("string is not closed", start)),
            Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                out.push_str(&src[chunk..=i]);
                i += 2;
                chunk = i;
            }
            Some(b'\'') => {
                out.push_str(&src[chunk..i]);
                return Ok((out, i + 1));
            }
            Some(_) => i += 1,
        }
    }
}

/// Reads a number starting at `start`: an optional `-`, then hex
/// (`0xff`) or decimal with an optional fraction and exponent.
fn number(src: &str, start: usize) -> Result<(f64, usize), ExprError> {
    let bytes = src.as_bytes();
    let digits = |mut i: usize, hex: bool| {
        while i < bytes.len()
            && (bytes[i].is_ascii_digit() || (hex && bytes[i].is_ascii_hexdigit()))
        {
            i += 1;
        }
        i
    };
    let invalid = || ExprError::new("invalid number", start);
    let mut i = start;
    let negative = bytes[i] == b'-';
    if negative {
        i += 1;
    }
    let value;
    if bytes.get(i) == Some(&b'0') && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
        let end = digits(i + 2, true);
        let parsed = u64::from_str_radix(&src[i + 2..end], 16).map_err(|_| invalid())?;
        #[allow(clippy::cast_precision_loss)]
        let magnitude = parsed as f64;
        value = if negative { -magnitude } else { magnitude };
        i = end;
    } else {
        let int_end = digits(i, false);
        if int_end == i {
            return Err(invalid());
        }
        i = int_end;
        if bytes.get(i) == Some(&b'.') {
            let frac_end = digits(i + 1, false);
            if frac_end == i + 1 {
                return Err(invalid());
            }
            i = frac_end;
        }
        if matches!(bytes.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(bytes.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            let exp_end = digits(j, false);
            if exp_end == j {
                return Err(invalid());
            }
            i = exp_end;
        }
        value = src[start..i].parse::<f64>().map_err(|_| invalid())?;
    }
    if bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(invalid());
    }
    if !value.is_finite() {
        return Err(ExprError::new("number is out of range", start));
    }
    Ok((value, i))
}
