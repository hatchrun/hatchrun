// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Workflow commands: `::name key=value::message` lines a step prints.

use hatchrun_workflow::{Annotation, AnnotationLevel};

/// A workflow command found in a line of step output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `::add-mask::value`. The line itself is never printed.
    AddMask(String),
    /// `::error::`, `::warning::` or `::notice::`.
    Annotation(Annotation),
    /// `::debug::message`.
    Debug(String),
    /// `::group::title`.
    Group(String),
    /// `::endgroup::`.
    EndGroup,
    /// `::stop-commands::token`. Commands are plain text until `::token::`.
    StopCommands(String),
    /// `::token::` after `::stop-commands::token`.
    Resume(String),
    /// `::echo::on` or `::echo::off`.
    Echo(bool),
    /// `::set-output name=n::value`.
    SetOutput {
        /// The output name.
        name: String,
        /// The output value.
        value: String,
    },
    /// A command that is turned off. The text says what to use instead.
    Refused(&'static str),
}

const SET_ENV_REFUSED: &str =
    "The `set-env` command is disabled. Write NAME=value to the file named by $GITHUB_ENV instead.";
const ADD_PATH_REFUSED: &str = "The `add-path` command is disabled. Write the directory to the file named by $GITHUB_PATH instead.";

/// Reads commands line by line and tracks `::stop-commands::`.
#[derive(Default)]
pub struct Parser {
    stop_token: Option<String>,
}

impl Parser {
    /// Parses one line. While commands are stopped, only the resume
    /// token is recognised.
    pub fn parse(&mut self, line: &str) -> Option<Command> {
        if let Some(token) = &self.stop_token {
            let (name, _, _) = split(line)?;
            if name == *token {
                let token = self.stop_token.take()?;
                return Some(Command::Resume(token));
            }
            return None;
        }
        let command = parse(line)?;
        if let Command::StopCommands(token) = &command {
            self.stop_token = Some(token.clone());
        }
        Some(command)
    }
}

/// Splits `::name props::message` into its three parts.
fn split(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.trim_start().strip_prefix("::")?;
    let end = rest.find("::")?;
    let (head, message) = (&rest[..end], &rest[end + 2..]);
    let (name, props) = head.split_once(' ').unwrap_or((head, ""));
    if name.is_empty() {
        return None;
    }
    Some((name, props, message))
}

/// Parses one line without any stop-commands state. Returns `None` for
/// plain text and for commands this agent does not know.
pub fn parse(line: &str) -> Option<Command> {
    let (name, props, message) = split(line)?;
    let name = name.to_ascii_lowercase();
    let level = match name.as_str() {
        "error" => Some(AnnotationLevel::Error),
        "warning" => Some(AnnotationLevel::Warning),
        "notice" => Some(AnnotationLevel::Notice),
        _ => None,
    };
    if let Some(level) = level {
        return Some(Command::Annotation(annotation(level, props, message)));
    }
    match name.as_str() {
        "add-mask" => Some(Command::AddMask(unescape_data(message))),
        "debug" => Some(Command::Debug(unescape_data(message))),
        "group" => Some(Command::Group(unescape_data(message))),
        "endgroup" => Some(Command::EndGroup),
        "stop-commands" => Some(Command::StopCommands(unescape_data(message))),
        "echo" => match message.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Command::Echo(true)),
            "off" => Some(Command::Echo(false)),
            _ => None,
        },
        "set-output" => {
            let name = properties(props)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("name"))?
                .1;
            if name.is_empty() {
                return None;
            }
            Some(Command::SetOutput {
                name,
                value: unescape_data(message),
            })
        }
        "set-env" => Some(Command::Refused(SET_ENV_REFUSED)),
        "add-path" => Some(Command::Refused(ADD_PATH_REFUSED)),
        _ => None,
    }
}

fn annotation(level: AnnotationLevel, props: &str, message: &str) -> Annotation {
    let mut a = Annotation {
        level,
        message: unescape_data(message),
        title: None,
        file: None,
        line: None,
        end_line: None,
        col: None,
        end_column: None,
    };
    for (key, value) in properties(props) {
        match key.to_ascii_lowercase().as_str() {
            "title" => a.title = Some(value),
            "file" => a.file = Some(value),
            "line" => a.line = value.parse().ok(),
            "endline" => a.end_line = value.parse().ok(),
            "col" => a.col = value.parse().ok(),
            "endcolumn" => a.end_column = value.parse().ok(),
            _ => {}
        }
    }
    a
}

/// Reads `k=v,k=v`. Pieces without `=` are skipped.
fn properties(props: &str) -> Vec<(String, String)> {
    props
        .split(',')
        .filter_map(|piece| {
            let (k, v) = piece.trim().split_once('=')?;
            Some((k.trim().to_owned(), unescape(v, true)))
        })
        .collect()
}

fn unescape_data(s: &str) -> String {
    unescape(s, false)
}

/// Undoes the runner's escaping: `%25 %0D %0A` everywhere, plus `%3A %2C`
/// in property values.
fn unescape(s: &str, property: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = match tail.get(..3) {
            Some("%25") => Some('%'),
            Some("%0D") => Some('\r'),
            Some("%0A") => Some('\n'),
            Some("%3A") if property => Some(':'),
            Some("%2C") if property => Some(','),
            _ => None,
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &tail[3..];
        } else {
            out.push('%');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_mask_line_is_not_printed() {
        assert_eq!(parse("::add-mask::v"), Some(Command::AddMask("v".into())));
    }

    #[test]
    fn parses_annotation_properties() {
        let got = parse("::error file=a.rs,line=3,col=7,title=Bad%2C really::boom%0Anext");
        assert_eq!(
            got,
            Some(Command::Annotation(Annotation {
                level: AnnotationLevel::Error,
                message: "boom\nnext".into(),
                title: Some("Bad, really".into()),
                file: Some("a.rs".into()),
                line: Some(3),
                end_line: None,
                col: Some(7),
                end_column: None,
            }))
        );
    }

    #[test]
    fn stop_commands_until_token() {
        let mut p = Parser::default();
        assert_eq!(
            p.parse("::stop-commands::tok"),
            Some(Command::StopCommands("tok".into()))
        );
        assert_eq!(p.parse("::add-mask::x"), None);
        assert_eq!(p.parse("::tok::"), Some(Command::Resume("tok".into())));
        assert_eq!(p.parse("::add-mask::x"), Some(Command::AddMask("x".into())));
    }

    #[test]
    fn set_env_and_add_path_are_refused() {
        match parse("::set-env name=A::b") {
            Some(Command::Refused(m)) => {
                assert!(m.contains("`set-env` command is disabled"));
                assert!(m.contains("$GITHUB_ENV"));
            }
            other => panic!("{other:?}"),
        }
        match parse("::add-path::/x") {
            Some(Command::Refused(m)) => {
                assert!(m.contains("`add-path` command is disabled"));
                assert!(m.contains("$GITHUB_PATH"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_output_group_echo_debug() {
        assert_eq!(
            parse("::set-output name=out::a%25b"),
            Some(Command::SetOutput {
                name: "out".into(),
                value: "a%b".into()
            })
        );
        assert_eq!(
            parse("::group::Build"),
            Some(Command::Group("Build".into()))
        );
        assert_eq!(parse("::endgroup::"), Some(Command::EndGroup));
        assert_eq!(parse("::echo::ON"), Some(Command::Echo(true)));
        assert_eq!(parse("::debug::hi"), Some(Command::Debug("hi".into())));
    }

    #[test]
    fn plain_text_and_unknown_commands_are_not_commands() {
        assert_eq!(parse("hello ::error::x"), None);
        assert_eq!(parse("::bogus::x"), None);
        assert_eq!(parse("::error no terminator"), None);
        assert_eq!(parse("::set-output::v"), None);
    }
}
