// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Builds an expression tree from tokens by precedence climbing.
//!
//! Precedence, low to high: `||`, `&&`, `==` `!=`, `<` `<=` `>` `>=`,
//! `!`, then property access and calls.

use serde_json::Value;

use super::ExprError;
use super::eval::number;
use super::lexer::{Kind, Token};

/// A node of the expression tree.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    /// A context name such as `github`, at `offset`.
    Context {
        name: String,
        offset: usize,
    },
    /// `target.name`.
    Property {
        target: Box<Expr>,
        name: String,
    },
    /// `target[index]`.
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    /// `target.*` or `target[*]`.
    Filter(Box<Expr>),
    /// A function call; `name` is lowercased, `offset` is where it starts.
    Call {
        name: String,
        args: Vec<Expr>,
        offset: usize,
    },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(Op, Box<Expr>, Box<Expr>),
}

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
pub(crate) fn parse(tokens: &[Token]) -> Result<Expr, ExprError> {
    let mut p = Parser { tokens, pos: 0 };
    if p.peek().kind == Kind::Eof {
        return Err(ExprError::new("expected an expression", p.peek().offset));
    }
    let expr = p.or()?;
    let next = p.peek();
    if next.kind != Kind::Eof {
        return Err(ExprError::new(
            format!("unexpected {}", describe(&next.kind)),
            next.offset,
        ));
    }
    Ok(expr)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if token.kind != Kind::Eof {
            self.pos += 1;
        }
        token
    }

    /// Checks that an operand follows the operator `op`, so a missing
    /// right side is reported at the operator.
    fn operand_after(&self, op: &Token) -> Result<(), ExprError> {
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

    fn or(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.and()?;
        while self.peek().kind == Kind::Or {
            let op = self.bump();
            self.operand_after(&op)?;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.equality()?;
        while self.peek().kind == Kind::And {
            let op = self.bump();
            self.operand_after(&op)?;
            left = Expr::And(Box::new(left), Box::new(self.equality()?));
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.relational()?;
        loop {
            let op = match self.peek().kind {
                Kind::Eq => Op::Eq,
                Kind::Ne => Op::Ne,
                _ => return Ok(left),
            };
            let token = self.bump();
            self.operand_after(&token)?;
            left = Expr::Compare(op, Box::new(left), Box::new(self.relational()?));
        }
    }

    fn relational(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek().kind {
                Kind::Lt => Op::Lt,
                Kind::Le => Op::Le,
                Kind::Gt => Op::Gt,
                Kind::Ge => Op::Ge,
                _ => return Ok(left),
            };
            let token = self.bump();
            self.operand_after(&token)?;
            left = Expr::Compare(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, ExprError> {
        if self.peek().kind == Kind::Not {
            let op = self.bump();
            self.operand_after(&op)?;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ExprError> {
        let mut expr = self.primary()?;
        loop {
            match self.peek().kind {
                Kind::Dot => {
                    let dot = self.bump();
                    let next = self.bump();
                    expr = match next.kind {
                        Kind::Ident(name) => Expr::Property {
                            target: Box::new(expr),
                            name,
                        },
                        Kind::Star => Expr::Filter(Box::new(expr)),
                        _ => {
                            return Err(ExprError::new(
                                "expected a property name or '*' after '.'",
                                dot.offset,
                            ));
                        }
                    };
                }
                Kind::LBracket => {
                    let open = self.bump();
                    if self.peek().kind == Kind::Star {
                        self.bump();
                        expr = Expr::Filter(Box::new(expr));
                    } else {
                        let index = self.or()?;
                        expr = Expr::Index {
                            target: Box::new(expr),
                            index: Box::new(index),
                        };
                    }
                    if self.bump().kind != Kind::RBracket {
                        return Err(ExprError::new("'[' is not closed", open.offset));
                    }
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, ExprError> {
        let token = self.bump();
        match token.kind {
            Kind::Number(n) => Ok(Expr::Literal(number(n))),
            Kind::Str(s) => Ok(Expr::Literal(Value::String(s))),
            Kind::Ident(name) => match name.as_str() {
                "null" => Ok(Expr::Literal(Value::Null)),
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                _ if self.peek().kind == Kind::LParen => self.call(&name, token.offset),
                _ => Ok(Expr::Context {
                    name,
                    offset: token.offset,
                }),
            },
            Kind::LParen => {
                let inner = self.or()?;
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

    fn call(&mut self, name: &str, offset: usize) -> Result<Expr, ExprError> {
        let open = self.bump();
        let mut args = Vec::new();
        if self.peek().kind == Kind::RParen {
            self.bump();
        } else {
            loop {
                args.push(self.or()?);
                match self.bump().kind {
                    Kind::Comma => {}
                    Kind::RParen => break,
                    _ => return Err(ExprError::new("'(' is not closed", open.offset)),
                }
            }
        }
        let lower = name.to_ascii_lowercase();
        let Some(&(_, min, max)) = FUNCTIONS.iter().find(|(f, _, _)| *f == lower) else {
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

/// How a token reads in an error message.
fn describe(kind: &Kind) -> String {
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
