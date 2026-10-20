// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Problems found in a workflow file, with the place they were found.

use std::fmt;

use serde_saphyr::{DefaultMessageFormatter, MessageFormatter};

/// One problem in a workflow file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What is wrong, without the position.
    pub message: String,
    /// The 1-based line, when known.
    pub line: Option<u64>,
    /// The 1-based column, when known.
    pub column: Option<u64>,
}

impl Diagnostic {
    /// A problem with no known position.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: None,
            column: None,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(f, "{line}:{column}: {}", self.message),
            (Some(line), None) => write!(f, "{line}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl From<&serde_saphyr::Error> for Diagnostic {
    fn from(err: &serde_saphyr::Error) -> Self {
        let err = err.without_snippet();
        let message = DefaultMessageFormatter.format_message(err).into_owned();
        let location = err.location();
        Self {
            message,
            line: location.map(|l| l.line()),
            column: location.map(|l| l.column()),
        }
    }
}
