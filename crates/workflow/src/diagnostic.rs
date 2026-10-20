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
    /// The file the position is in, when there are several.
    pub file: Option<String>,
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
            file: None,
            line: None,
            column: None,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{file}:")?;
            if self.line.is_none() {
                f.write_str(" ")?;
            }
        }
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
        // Two of the library's messages name its own options and types.
        let message = match err {
            serde_saphyr::Error::DuplicateMappingKey { key: Some(key), .. } => {
                format!("duplicate key `{key}`")
            }
            serde_saphyr::Error::DuplicateMappingKey { key: None, .. } => "duplicate key".into(),
            serde_saphyr::Error::Budget { .. } => {
                "the file is too large or nests too deeply to read".into()
            }
            other => DefaultMessageFormatter.format_message(other).into_owned(),
        };
        let location = err.location();
        Self {
            message,
            file: None,
            line: location.map(|l| l.line()),
            column: location.map(|l| l.column()),
        }
    }
}
