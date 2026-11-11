// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads chat commands out of a comment.
//!
//! A command is a line of the comment that starts, after optional
//! leading whitespace, with `/<name>` and then ends or goes on after a
//! space. [`parse_comment`] finds them; it does no I/O and knows nothing
//! of who wrote the comment. The caller checks the commenter's
//! permission and the pull request's head before it runs anything, and
//! replies to a command that came back with an error, quoting
//! [`Found::line`].
//!
//! Rules:
//!
//! - Lines inside a fenced code block (three or more backticks or tildes, closed by a fence of
//!   the same character that is at least as long, or by the end of the
//!   comment) and lines of a block quote (`>`) never match.
//! - The first line of each name wins; a later line of the same name is
//!   ignored. At most [`MAX_COMMANDS`] commands are returned, the first
//!   ones in the order of the comment.
//! - The word `--sha=<40 hex digits>` is the sha pin and not an
//!   argument. A short value, a value that is not hex and a second
//!   `--sha=` word are errors of that command ([`ArgsError`]).
//! - Every other word is an argument, unchanged: a word that starts with
//!   `@` or `-` is one too. At most [`MAX_WORDS`] words of at most
//!   [`MAX_WORD_CHARS`] characters each; more is an error.
//! - Names are matched as written: `/Retest` is not `/retest`.

use std::fmt;

/// The built-in command that approves a fork's run. It needs no workflow
/// and its name is reserved: `on.command` cannot define it.
pub const APPROVE_RUN: &str = "approve-run";

/// The most commands read from one comment.
pub const MAX_COMMANDS: usize = 3;

/// The most argument words of one command.
pub const MAX_WORDS: usize = 20;

/// The most characters of one argument word.
pub const MAX_WORD_CHARS: usize = 200;

/// The length of a full commit sha, in hex digits.
pub const SHA_LEN: usize = 40;

/// The prefix of the word that pins a sha.
const SHA_PREFIX: &str = "--sha=";

/// One command found in a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The command's name, without the `/`.
    pub name: String,
    /// The line as written, without the whitespace around it, for a
    /// reply to quote.
    pub line: String,
    /// The pin and the arguments, or why the line is refused.
    pub args: Result<Args, ArgsError>,
}

/// The pin and the arguments of a command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    /// The sha the comment pins, 40 lowercase hex digits.
    pub sha: Option<String>,
    /// The words that are not the pin, in order and unchanged.
    pub words: Vec<String>,
}

/// Why a command line is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgsError {
    /// `--sha=` has fewer than [`SHA_LEN`] characters. Only the full sha
    /// is accepted.
    ShortSha,
    /// `--sha=` has more than [`SHA_LEN`] characters, or one that is not
    /// a hex digit.
    BadSha,
    /// The line has a second `--sha=` word.
    DuplicateSha,
    /// The line has more than [`MAX_WORDS`] argument words.
    TooManyWords,
    /// A word has more than [`MAX_WORD_CHARS`] characters.
    WordTooLong,
}

impl fmt::Display for ArgsError {
    /// Says what is wrong with the arguments, for the reply to the comment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortSha => f.write_str("`--sha=` needs the full 40 character commit sha"),
            Self::BadSha => f.write_str("`--sha=` is not a 40 digit hex commit sha"),
            Self::DuplicateSha => f.write_str("the command has more than one `--sha=`"),
            Self::TooManyWords => write!(f, "the command has more than {MAX_WORDS} arguments"),
            Self::WordTooLong => {
                write!(f, "an argument is longer than {MAX_WORD_CHARS} characters")
            }
        }
    }
}

impl std::error::Error for ArgsError {}

/// The commands in `body`, in the order of the comment. `known` says
/// whether a name is a command of the repository's workflows;
/// [`APPROVE_RUN`] is always known.
#[must_use]
pub fn parse_comment(body: &str, known: impl Fn(&str) -> bool) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    // The fence being read: its character and how many it opened with.
    let mut fence: Option<(char, usize)> = None;
    // A carriage return alone ends a line in Markdown, as `\r\n` does.
    for raw in body.split(['\n', '\r']) {
        if let Some((mark, open)) = fence {
            if closes_fence(raw, mark, open) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = opens_fence(raw) {
            fence = Some(opened);
            continue;
        }
        let line = raw.trim();
        if line.starts_with('>') {
            continue;
        }
        let Some(rest) = line.strip_prefix('/') else {
            continue;
        };
        let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (name, words) = rest.split_at(name_end);
        if name != APPROVE_RUN && !known(name) {
            continue;
        }
        if found.iter().any(|f| f.name == name) {
            continue;
        }
        found.push(Found {
            name: name.to_owned(),
            line: line.to_owned(),
            args: read_args(words),
        });
        if found.len() == MAX_COMMANDS {
            break;
        }
    }
    found
}

/// The pin and the argument words of what follows a command's name.
fn read_args(words: &str) -> Result<Args, ArgsError> {
    let mut args = Args::default();
    for word in words.split_whitespace() {
        if word.chars().count() > MAX_WORD_CHARS {
            return Err(ArgsError::WordTooLong);
        }
        let Some(value) = word.strip_prefix(SHA_PREFIX) else {
            args.words.push(word.to_owned());
            continue;
        };
        if args.sha.is_some() {
            return Err(ArgsError::DuplicateSha);
        }
        if !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ArgsError::BadSha);
        }
        match value.len() {
            n if n < SHA_LEN => return Err(ArgsError::ShortSha),
            n if n > SHA_LEN => return Err(ArgsError::BadSha),
            _ => args.sha = Some(value.to_ascii_lowercase()),
        }
    }
    if args.words.len() > MAX_WORDS {
        return Err(ArgsError::TooManyWords);
    }
    Ok(args)
}

/// The spaces before a fence, which may be at most three, and what
/// follows them, `None` when more (an indented code block, not a fence).
fn fence_indent(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    (line.len() - rest.len() <= 3).then_some(rest)
}

/// The character and length of a fence that `line` opens: three or more
/// backticks or tildes. The info string after a backtick fence has no
/// backtick.
fn opens_fence(line: &str) -> Option<(char, usize)> {
    let rest = fence_indent(line)?;
    let mark = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = rest.chars().take_while(|c| *c == mark).count();
    if run < 3 || (mark == '`' && rest[run..].contains('`')) {
        return None;
    }
    Some((mark, run))
}

/// Whether `line` closes a fence of `open` characters `mark`: a fence at
/// least as long and nothing else on the line.
fn closes_fence(line: &str, mark: char, open: usize) -> bool {
    let Some(rest) = fence_indent(line) else {
        return false;
    };
    let run = rest.chars().take_while(|c| *c == mark).count();
    run >= open && rest[run..].trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [&str; 0] = [];

    const SHA: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";

    /// Every command `body` holds, with `retest`, `deploy` and `ok` known.
    fn all(body: &str) -> Vec<Found> {
        parse_comment(body, |name| matches!(name, "retest" | "deploy" | "ok"))
    }

    /// The names of the commands found in `body`.
    fn names(body: &str) -> Vec<String> {
        all(body).into_iter().map(|f| f.name).collect()
    }

    /// The argument words of the first command found in `body`.
    fn words(body: &str) -> Vec<String> {
        all(body)[0].args.clone().unwrap().words
    }

    #[test]
    fn a_command_line_is_found_with_its_arguments() {
        let found = all("/deploy staging eu");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "deploy");
        assert_eq!(found[0].line, "/deploy staging eu");
        let args = found[0].args.clone().unwrap();
        assert_eq!(args.words, ["staging", "eu"]);
        assert_eq!(args.sha, None);
    }

    #[test]
    fn a_name_alone_or_before_a_space_matches() {
        assert_eq!(names("/retest"), ["retest"]);
        assert_eq!(names("/retest please"), ["retest"]);
        assert_eq!(names("  \t /retest"), ["retest"]);
        assert_eq!(names("/retest\t now"), ["retest"]);
    }

    #[test]
    fn a_longer_or_other_name_does_not_match() {
        assert_eq!(names("/retestx"), NONE);
        assert_eq!(names("/retest-all"), NONE);
        assert_eq!(names("/retest_"), NONE);
        assert_eq!(names("/retes"), NONE);
        assert_eq!(names("/Retest"), NONE);
        assert_eq!(names("/RETEST"), NONE);
        assert_eq!(names("//retest"), NONE);
        assert_eq!(names("/ retest"), NONE);
        assert_eq!(names("/unknown"), NONE);
        assert_eq!(names("/retest/more"), NONE);
        assert_eq!(names("/retest:now"), NONE);
    }

    #[test]
    fn the_name_is_not_found_later_in_the_line() {
        assert_eq!(names("please /retest"), NONE);
        assert_eq!(names("see /retest"), NONE);
        assert_eq!(names("a/retest"), NONE);
        assert_eq!(names("`/retest`"), NONE);
        assert_eq!(names("**/retest**"), NONE);
        assert_eq!(names("- /retest"), NONE);
        assert_eq!(names("1. /retest"), NONE);
    }

    #[test]
    fn approve_run_is_always_known() {
        let found = parse_comment(&format!("/approve-run --sha={SHA}"), |_| false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, APPROVE_RUN);
        assert_eq!(found[0].args.clone().unwrap().sha.as_deref(), Some(SHA));
        assert_eq!(parse_comment("/approve-runs", |_| false).len(), 0);
    }

    #[test]
    fn crlf_and_carriage_returns_end_a_line() {
        assert_eq!(names("hello\r\n/retest\r\n"), ["retest"]);
        assert_eq!(names("hello\r/retest\r"), ["retest"]);
        assert_eq!(all("/retest\r\n")[0].line, "/retest");
        assert_eq!(words("/deploy a\r\nb"), ["a"]);
        // A fence closes on a line that ends in a carriage return.
        assert_eq!(names("```\r\n/retest\r\n```\r\n/deploy"), ["deploy"]);
    }

    #[test]
    fn the_first_line_of_a_name_wins() {
        let found = all("/deploy one\n/retest\n/deploy two");
        let order: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(order, ["deploy", "retest"]);
        assert_eq!(found[0].line, "/deploy one");
        // A repeat does not count against the limit.
        assert_eq!(
            names("/deploy\n/deploy\n/deploy\n/retest\n/ok"),
            ["deploy", "retest", "ok"]
        );
    }

    #[test]
    fn at_most_three_commands_are_read() {
        let found = parse_comment("/a\n/b\n/c\n/d\n/e", |_| true);
        let order: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(order, ["a", "b", "c"]);
        assert_eq!(MAX_COMMANDS, 3);
    }

    #[test]
    fn fenced_code_never_matches() {
        assert_eq!(names("```\n/retest\n```"), NONE);
        assert_eq!(names("~~~\n/retest\n~~~"), NONE);
        assert_eq!(names("```sh\n/retest\n```"), NONE);
        assert_eq!(names("  ```\n  /retest\n  ```"), NONE);
        // After the fence it matches again.
        assert_eq!(names("```\n/retest\n```\n/deploy"), ["deploy"]);
        // An unclosed fence runs to the end.
        assert_eq!(names("```\n/retest"), NONE);
        assert_eq!(names("/retest\n```\n/deploy"), ["retest"]);
    }

    #[test]
    fn a_fence_needs_its_own_character_and_length_to_close() {
        // A tilde fence is not closed by backticks, nor the reverse.
        assert_eq!(names("~~~\n```\n/retest\n```"), NONE);
        assert_eq!(names("```\n~~~\n/retest\n~~~"), NONE);
        // A longer fence holds a shorter one.
        assert_eq!(names("````\n```\n/retest\n```\n````"), NONE);
        assert_eq!(names("````\n```\n/retest\n````\n/deploy"), ["deploy"]);
        // A shorter fence does not close a longer one.
        assert_eq!(names("~~~~\n~~~\n/retest"), NONE);
        // A closing fence has no text after it.
        assert_eq!(names("```\n``` x\n/retest"), NONE);
        // Two backticks are not a fence.
        assert_eq!(names("``\n/retest"), ["retest"]);
        // A backtick fence's info string has no backtick: inline code.
        assert_eq!(names("```code``` \n/retest"), ["retest"]);
        // A fence indented four spaces is a code block, not a fence.
        assert_eq!(names("    ```\n/retest"), ["retest"]);
    }

    #[test]
    fn a_fence_inside_a_fence_of_the_other_kind_is_text() {
        assert_eq!(
            names("~~~\n```\n~~~\n/retest\n```\n"),
            ["retest"],
            "the tilde fence closed on the second ~~~"
        );
    }

    #[test]
    fn block_quotes_never_match() {
        assert_eq!(names("> /retest"), NONE);
        assert_eq!(names(">/retest"), NONE);
        assert_eq!(names("  > /retest"), NONE);
        assert_eq!(names("> > /retest"), NONE);
        assert_eq!(names("> quote\n> /deploy"), NONE);
        // The line after the quote is its own.
        assert_eq!(names("> /deploy\n/retest"), ["retest"]);
        // A quote of a quote reply does not hide the real command.
        assert_eq!(names("> /deploy\n\n/deploy staging"), ["deploy"]);
    }

    #[test]
    fn a_blank_line_or_other_text_between_is_fine() {
        assert_eq!(names("Looks good.\n\n/retest\n\nThanks"), ["retest"]);
    }

    #[test]
    fn the_sha_pin_is_not_an_argument() {
        let found = all(&format!("/deploy staging --sha={SHA} eu"));
        let args = found[0].args.clone().unwrap();
        assert_eq!(args.sha.as_deref(), Some(SHA));
        assert_eq!(args.words, ["staging", "eu"]);
        // The line is as written.
        assert_eq!(found[0].line, format!("/deploy staging --sha={SHA} eu"));
        // The pin may come first.
        let args = all(&format!("/deploy --sha={SHA} staging"))[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(args.sha.as_deref(), Some(SHA));
        assert_eq!(args.words, ["staging"]);
    }

    #[test]
    fn an_upper_case_sha_is_kept_in_lower_case() {
        let upper = SHA.to_ascii_uppercase();
        let args = all(&format!("/retest --sha={upper}"))[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(args.sha.as_deref(), Some(SHA));
    }

    #[test]
    fn a_bad_sha_refuses_the_command() {
        let err = |body: &str| all(body)[0].args.clone().unwrap_err();
        assert_eq!(err("/retest --sha="), ArgsError::ShortSha);
        assert_eq!(err("/retest --sha=1a2b3c4"), ArgsError::ShortSha);
        assert_eq!(
            err(&format!("/retest --sha={}", &SHA[..39])),
            ArgsError::ShortSha
        );
        assert_eq!(err(&format!("/retest --sha={SHA}0")), ArgsError::BadSha);
        assert_eq!(
            err(&format!("/retest --sha={}g", &SHA[..39])),
            ArgsError::BadSha
        );
        assert_eq!(err("/retest --sha=main"), ArgsError::BadSha);
        assert_eq!(err("/retest --sha=HEAD"), ArgsError::BadSha);
        assert_eq!(err(&format!("/retest --sha={SHA},")), ArgsError::BadSha);
        // A digit that looks like hex in another script is not hex.
        let wide = SHA.replacen('1', "\u{ff11}", 1);
        assert_eq!(err(&format!("/retest --sha={wide}")), ArgsError::BadSha);
        assert_eq!(
            err(&format!("/retest --sha={SHA} --sha={SHA}")),
            ArgsError::DuplicateSha
        );
        assert_eq!(
            err(&format!("/retest --sha={SHA} --sha=abc")),
            ArgsError::DuplicateSha
        );
        // The command is still found, so the caller can reply.
        assert_eq!(names("/retest --sha=abc"), ["retest"]);
    }

    #[test]
    fn words_like_a_sha_flag_but_not_it_are_arguments() {
        assert_eq!(words("/retest --sha"), ["--sha"]);
        assert_eq!(words("/retest --SHA=abc"), ["--SHA=abc"]);
        assert_eq!(words("/retest x--sha=abc"), ["x--sha=abc"]);
        assert_eq!(words("/retest -sha=abc"), ["-sha=abc"]);
        assert_eq!(words("/retest ---sha=abc"), ["---sha=abc"]);
    }

    #[test]
    fn other_words_are_arguments_unchanged() {
        assert_eq!(
            words("/deploy @alice -f --force $HOME `x` \"quoted\" a=b ${{ x }}"),
            [
                "@alice",
                "-f",
                "--force",
                "$HOME",
                "`x`",
                "\"quoted\"",
                "a=b",
                "${{",
                "x",
                "}}"
            ]
        );
        assert_eq!(words("/deploy   a \t  b  "), ["a", "b"]);
    }

    #[test]
    fn unicode_is_kept_and_does_not_make_a_name() {
        assert_eq!(
            words("/deploy h\u{e9}llo \u{1f680} \u{4e16}\u{754c}"),
            ["h\u{e9}llo", "\u{1f680}", "\u{4e16}\u{754c}"]
        );
        // A zero width space after the name is part of the name.
        assert_eq!(names("/retest\u{200b}"), NONE);
        // A no-break space is whitespace.
        assert_eq!(names("/retest\u{a0}now"), ["retest"]);
        assert_eq!(names("\u{a0}/retest"), ["retest"]);
        // A full width slash is not a slash.
        assert_eq!(names("\u{ff0f}retest"), NONE);
        // A name with an accent is another name.
        assert_eq!(names("/retest\u{301}"), NONE);
    }

    #[test]
    fn the_word_limits_are_enforced() {
        let twenty = (0..MAX_WORDS).map(|i| i.to_string()).collect::<Vec<_>>();
        let args = all(&format!("/retest {}", twenty.join(" ")))[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(args.words, twenty);
        let err = all(&format!("/retest {} x", twenty.join(" ")))[0]
            .args
            .clone()
            .unwrap_err();
        assert_eq!(err, ArgsError::TooManyWords);
        // The pin is not a word.
        let with_pin = format!("/retest {} --sha={SHA}", twenty.join(" "));
        assert!(all(&with_pin)[0].args.is_ok());

        let long = "x".repeat(MAX_WORD_CHARS);
        assert!(all(&format!("/retest {long}"))[0].args.is_ok());
        let err = all(&format!("/retest {long}x"))[0]
            .args
            .clone()
            .unwrap_err();
        assert_eq!(err, ArgsError::WordTooLong);
        // Characters, not bytes.
        let wide = "\u{e9}".repeat(MAX_WORD_CHARS);
        assert!(all(&format!("/retest {wide}"))[0].args.is_ok());
        let err = all(&format!("/retest {wide}\u{e9}"))[0]
            .args
            .clone()
            .unwrap_err();
        assert_eq!(err, ArgsError::WordTooLong);
    }

    #[test]
    fn a_refused_command_still_counts_toward_the_limit_and_the_name() {
        let found = all("/deploy --sha=abc\n/deploy staging\n/retest");
        assert_eq!(found.len(), 2);
        assert!(found[0].args.is_err());
        assert_eq!(found[0].line, "/deploy --sha=abc");
    }

    #[test]
    fn the_line_is_trimmed_but_otherwise_as_written() {
        let found = all("  \t/deploy   a \t b \t \n");
        assert_eq!(found[0].line, "/deploy   a \t b");
    }

    #[test]
    fn empty_and_odd_bodies_find_nothing() {
        assert_eq!(all("").len(), 0);
        assert_eq!(all("\n\n\r\n").len(), 0);
        assert_eq!(all("/").len(), 0);
        assert_eq!(all("//").len(), 0);
        assert_eq!(all("   ").len(), 0);
        assert_eq!(all("\0/retest").len(), 0);
    }

    #[test]
    fn a_huge_comment_is_read_in_one_pass() {
        let body = "x\n".repeat(100_000) + "/retest";
        assert_eq!(names(&body), ["retest"]);
    }

    #[test]
    fn errors_say_what_is_wrong() {
        for err in [
            ArgsError::ShortSha,
            ArgsError::BadSha,
            ArgsError::DuplicateSha,
            ArgsError::TooManyWords,
            ArgsError::WordTooLong,
        ] {
            assert_ne!(err.to_string(), "");
        }
    }
}
