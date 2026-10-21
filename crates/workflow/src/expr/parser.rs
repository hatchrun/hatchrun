// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Builds an expression tree from tokens by precedence climbing.
//!
//! Precedence, low to high: `||`, `&&`, `==` `!=`, `<` `<=` `>` `>=`,
//! `!`, then property access and calls.
//!
//! Chains of one operator (`a || b || c`, `a.b[0].c`) are parsed by a
//! loop and kept flat in one node, so parsing, walking and dropping the
//! tree recurse only per nesting level: brackets, parentheses, call
//! arguments and `!`. [`MAX_DEPTH`] caps those levels, which bounds every
//! recursion over the tree.

use serde_json::Value;

use super::ExprError;
use super::eval::number;
use super::lexer::{Kind, Token};

/// A node of the expression tree. Names are borrowed from the source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr<'s> {
    Literal(Value),
    /// A context name such as `github`, at `offset`.
    Context {
        name: &'s str,
        offset: usize,
    },
    /// `base` followed by `.name`, `[index]`, `.*` or `[*]` steps.
    Access {
        base: Box<Expr<'s>>,
        path: Vec<Step<'s>>,
    },
    /// A function call; `name` is lowercased, `offset` is where it starts.
    Call {
        name: &'static str,
        args: Vec<Expr<'s>>,
        offset: usize,
    },
    Not(Box<Expr<'s>>),
    /// Operands of a `&&` chain, at least two.
    And(Vec<Expr<'s>>),
    /// Operands of a `||` chain, at least two.
    Or(Vec<Expr<'s>>),
    /// `first`, then each operator and right operand, applied left to
    /// right.
    Compare {
        first: Box<Expr<'s>>,
        rest: Vec<(Op, Expr<'s>)>,
    },
}

/// One step of a property access.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step<'s> {
    /// `.name`.
    Property(&'s str),
    /// `[expr]`.
    Index(Expr<'s>),
    /// `.*` or `[*]`.
    Filter,
}

/// How deeply brackets, parentheses, calls and `!` may nest.
pub(crate) const MAX_DEPTH: usize = 64;

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The functions the language knows, with their lowest and highest
/// argument counts.
const FUNCTIONS: &[(&str, usize, usize)] = &[
    ("contains", 2, 2),
    ("startswith", 2, 2),
    ("endswith", 2, 2),
    ("format", 1, usize::MAX),
    ("join", 1, 2),
    ("tojson", 1, 1),
    ("fromjson", 1, 1),
    ("hashfiles", 1, usize::MAX),
    ("success", 0, 0),
    ("always", 0, 0),
    ("failure", 0, 0),
    ("cancelled", 0, 0),
];

/// Parses a whole token list, which must end with [`Kind::Eof`].
pub(crate) fn parse(tokens: Vec<Token<'_>>) -> Result<Expr<'_>, ExprError> {
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    if p.peek().kind == Kind::Eof {
        return Err(ExprError::new("expected an expression", p.peek().offset));
    }
    let expr = p.expr()?;
    let next = p.peek();
    if next.kind != Kind::Eof {
        return Err(ExprError::new(
            format!("unexpected {}", describe(&next.kind)),
            next.offset,
        ));
    }
    Ok(expr)
}

struct Parser<'s> {
    tokens: Vec<Token<'s>>,
    pos: usize,
    depth: usize,
}

impl<'s> Parser<'s> {
    fn peek(&self) -> &Token<'s> {
        &self.tokens[self.pos]
    }

    /// Takes the next token. The parser never looks back, so the token
    /// is moved out, not copied; the end stays where it is.
    fn bump(&mut self) -> Token<'s> {
        let token = &mut self.tokens[self.pos];
        if token.kind == Kind::Eof {
            return token.clone();
        }
        self.pos += 1;
        let offset = token.offset;
        std::mem::replace(
            token,
            Token {
                kind: Kind::Eof,
                offset,
            },
        )
    }

    /// Checks that an operand follows the operator `op`, so a missing
    /// right side is reported at the operator.
    fn operand_after(&self, op: &Token<'s>) -> Result<(), ExprError> {
        if matches!(
            self.peek().kind,
            Kind::Ident(_) | Kind::Number(_) | Kind::Str(_) | Kind::LParen | Kind::Not
        ) {
            Ok(())
        } else {
            Err(ExprError::new(
                format!("expected an expression after {}", describe(&op.kind)),
                op.offset,
            ))
        }
    }

    /// Goes one nesting level deeper at `token`, failing past
    /// [`MAX_DEPTH`]. Pair with [`Parser::leave`].
    fn enter(&mut self, token: &Token<'s>) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExprError::new(
                format!("expression nests deeper than {MAX_DEPTH} levels"),
                token.offset,
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Parses a full expression.
    fn expr(&mut self) -> Result<Expr<'s>, ExprError> {
        self.climb(0)
    }

    /// Precedence climbing: parses operands joined by binary operators
    /// binding at least as tightly as `min`. It recurses only when
    /// precedence rises, so a chain of one operator is a loop.
    fn climb(&mut self, min: u8) -> Result<Expr<'s>, ExprError> {
        let mut lhs = self.unary()?;
        while let Some((level, op)) = binary(&self.peek().kind) {
            if level < min {
                break;
            }
            let token = self.bump();
            self.operand_after(&token)?;
            let rhs = self.climb(level + 1)?;
            lhs = join(lhs, op, rhs);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr<'s>, ExprError> {
        if self.peek().kind == Kind::Not {
            let op = self.bump();
            self.operand_after(&op)?;
            self.enter(&op)?;
            let inner = self.unary()?;
            self.leave();
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr<'s>, ExprError> {
        let base = self.primary()?;
        let mut path = Vec::new();
        loop {
            match self.peek().kind {
                Kind::Dot => {
                    let dot = self.bump();
                    let next = self.bump();
                    path.push(match next.kind {
                        Kind::Ident(name) => Step::Property(name),
                        Kind::Star => Step::Filter,
                        _ => {
                            return Err(ExprError::new(
                                "expected a property name or '*' after '.'",
                                dot.offset,
                            ));
                        }
                    });
                }
                Kind::LBracket => {
                    let open = self.bump();
                    if self.peek().kind == Kind::Star {
                        self.bump();
                        path.push(Step::Filter);
                    } else {
                        self.enter(&open)?;
                        path.push(Step::Index(self.expr()?));
                        self.leave();
                    }
                    if self.bump().kind != Kind::RBracket {
                        return Err(ExprError::new("'[' is not closed", open.offset));
                    }
                }
                _ if path.is_empty() => return Ok(base),
                _ => {
                    return Ok(Expr::Access {
                        base: Box::new(base),
                        path,
                    });
                }
            }
        }
    }

    fn primary(&mut self) -> Result<Expr<'s>, ExprError> {
        let token = self.bump();
        match token.kind {
            Kind::Number(n) => Ok(Expr::Literal(number(n))),
            Kind::Str(s) => Ok(Expr::Literal(Value::String(s))),
            Kind::Ident(name) => match name {
                "null" => Ok(Expr::Literal(Value::Null)),
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                _ if self.peek().kind == Kind::LParen => self.call(name, token.offset),
                _ => Ok(Expr::Context {
                    name,
                    offset: token.offset,
                }),
            },
            Kind::LParen => {
                self.enter(&token)?;
                let inner = self.expr()?;
                self.leave();
                if self.bump().kind != Kind::RParen {
                    return Err(ExprError::new("'(' is not closed", token.offset));
                }
                Ok(inner)
            }
            kind => Err(ExprError::new(
                format!("expected an expression, found {}", describe(&kind)),
                token.offset,
            )),
        }
    }

    fn call(&mut self, name: &str, offset: usize) -> Result<Expr<'s>, ExprError> {
        let open = self.bump();
        let mut args = Vec::new();
        if self.peek().kind == Kind::RParen {
            self.bump();
        } else {
            self.enter(&open)?;
            loop {
                args.push(self.expr()?);
                match self.bump().kind {
                    Kind::Comma => {}
                    Kind::RParen => break,
                    _ => return Err(ExprError::new("'(' is not closed", open.offset)),
                }
            }
            self.leave();
        }
        let Some(&(lower, min, max)) = FUNCTIONS
            .iter()
            .find(|(f, _, _)| f.eq_ignore_ascii_case(name))
        else {
            return Err(ExprError::new(format!("unknown function '{name}'"), offset));
        };
        if args.len() < min || args.len() > max {
            let want = match (min, max) {
                (min, max) if min == max => format!("{min}"),
                (min, usize::MAX) => format!("at least {min}"),
                (min, max) => format!("{min} or {max}"),
            };
            return Err(ExprError::new(
                format!("{name}() takes {want} arguments, got {}", args.len()),
                offset,
            ));
        }
        Ok(Expr::Call {
            name: lower,
            args,
            offset,
        })
    }
}

/// A binary operator: its precedence level (higher binds tighter) and
/// what it does.
#[derive(Clone, Copy)]
enum Binary {
    Or,
    And,
    Compare(Op),
}

fn binary(kind: &Kind<'_>) -> Option<(u8, Binary)> {
    Some(match kind {
        Kind::Or => (0, Binary::Or),
        Kind::And => (1, Binary::And),
        Kind::Eq => (2, Binary::Compare(Op::Eq)),
        Kind::Ne => (2, Binary::Compare(Op::Ne)),
        Kind::Lt => (3, Binary::Compare(Op::Lt)),
        Kind::Le => (3, Binary::Compare(Op::Le)),
        Kind::Gt => (3, Binary::Compare(Op::Gt)),
        Kind::Ge => (3, Binary::Compare(Op::Ge)),
        _ => return None,
    })
}

/// Whether two comparison operators share a precedence level.
fn same_level(a: Op, b: Op) -> bool {
    matches!(a, Op::Eq | Op::Ne) == matches!(b, Op::Eq | Op::Ne)
}

/// Builds `lhs op rhs`, appending to `lhs` when it is already a chain of
/// the same level, so chains stay flat and are walked by loops.
fn join<'s>(lhs: Expr<'s>, op: Binary, rhs: Expr<'s>) -> Expr<'s> {
    match (op, lhs) {
        (Binary::Or, Expr::Or(mut operands)) => {
            operands.push(rhs);
            Expr::Or(operands)
        }
        (Binary::Or, lhs) => Expr::Or(vec![lhs, rhs]),
        (Binary::And, Expr::And(mut operands)) => {
            operands.push(rhs);
            Expr::And(operands)
        }
        (Binary::And, lhs) => Expr::And(vec![lhs, rhs]),
        (Binary::Compare(op), Expr::Compare { first, mut rest })
            if rest.first().is_some_and(|(o, _)| same_level(*o, op)) =>
        {
            rest.push((op, rhs));
            Expr::Compare { first, rest }
        }
        (Binary::Compare(op), lhs) => Expr::Compare {
            first: Box::new(lhs),
            rest: vec![(op, rhs)],
        },
    }
}

/// How a token reads in an error message.
fn describe(kind: &Kind<'_>) -> String {
    let s = match kind {
        Kind::Ident(name) => return format!("'{name}'"),
        Kind::Number(_) => "a number",
        Kind::Str(_) => "a string",
        Kind::Dot => "'.'",
        Kind::Star => "'*'",
        Kind::Comma => "','",
        Kind::LParen => "'('",
        Kind::RParen => "')'",
        Kind::LBracket => "'['",
        Kind::RBracket => "']'",
        Kind::Not => "'!'",
        Kind::Lt => "'<'",
        Kind::Le => "'<='",
        Kind::Gt => "'>'",
        Kind::Ge => "'>='",
        Kind::Eq => "'=='",
        Kind::Ne => "'!='",
        Kind::And => "'&&'",
        Kind::Or => "'||'",
        Kind::Eof => "the end of the expression",
    };
    s.to_owned()
}
