// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Secret masking for step output.

// Steps are not wired to the masker yet.
#![allow(dead_code)]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// What a masked value prints as.
const REPLACEMENT: &str = "***";

/// Shortest partial base64 form worth masking. Shorter ones match too
/// much ordinary text.
const MIN_PARTIAL_B64: usize = 4;

/// Everything except the RFC 3986 unreserved characters.
const STRICT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// What `encodeURIComponent` leaves alone.
const URI_COMPONENT: &AsciiSet = &STRICT
    .remove(b'!')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// The set of values to hide, matched in one pass over each line.
#[derive(Default)]
pub struct Masker {
    patterns: BTreeSet<String>,
    matcher: OnceLock<Option<AhoCorasick>>,
}

impl Masker {
    /// Hides `value` and its base64, JSON and URL encoded forms. A
    /// multi-line value is hidden one line at a time, because output is
    /// masked per line. Empty and whitespace-only lines are ignored.
    pub fn add(&mut self, value: &str) {
        for line in value.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() {
                continue;
            }
            self.insert(line.to_owned());
            self.insert_base64(line.as_bytes());
            if let Ok(json) = serde_json::to_string(line) {
                self.insert(json[1..json.len() - 1].to_owned());
            }
            self.insert(utf8_percent_encode(line, STRICT).to_string());
            self.insert(utf8_percent_encode(line, URI_COMPONENT).to_string());
        }
        self.matcher = OnceLock::new();
    }

    fn insert(&mut self, pattern: String) {
        if !pattern.trim().is_empty() {
            self.patterns.insert(pattern);
        }
    }

    /// Adds the base64 text of `bytes` as it appears at each of the three
    /// alignments inside a longer base64 string. Only the characters that
    /// do not depend on neighbouring bytes are kept.
    fn insert_base64(&mut self, bytes: &[u8]) {
        self.insert(STANDARD.encode(bytes));
        self.insert(STANDARD_NO_PAD.encode(bytes));
        for (pad, skip) in [(0usize, 0usize), (1, 2), (2, 3)] {
            let mut buf = vec![0u8; pad];
            buf.extend_from_slice(bytes);
            let mut text = STANDARD_NO_PAD.encode(&buf);
            if buf.len() % 3 != 0 {
                text.pop();
            }
            let stable = &text[skip.min(text.len())..];
            if pad == 0 || stable.len() >= MIN_PARTIAL_B64 {
                self.insert(stable.to_owned());
            }
        }
    }

    /// Replaces every hidden value in `line` with `***`. At each position
    /// the longest match wins.
    pub fn mask<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let matcher = self.matcher.get_or_init(|| {
            if self.patterns.is_empty() {
                return None;
            }
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&self.patterns)
                .ok()
        });
        let Some(matcher) = matcher else {
            return Cow::Borrowed(line);
        };
        let mut hits = matcher.find_iter(line).peekable();
        if hits.peek().is_none() {
            return Cow::Borrowed(line);
        }
        let mut out = String::with_capacity(line.len());
        let mut last = 0;
        for m in hits {
            out.push_str(&line[last..m.start()]);
            out.push_str(REPLACEMENT);
            last = m.end();
        }
        out.push_str(&line[last..]);
        Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::lines::Lines;

    #[test]
    fn masks_across_writes() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b"tok"), Vec::<String>::new());
        assert_eq!(lines.push(b"en=s3c"), Vec::<String>::new());
        let out = lines.push(b"ret\n");
        assert_eq!(out, vec!["token=s3cret".to_owned()]);
        let mut m = Masker::default();
        m.add("s3cret");
        assert_eq!(m.mask(&out[0]), "token=***");
    }

    #[test]
    fn masks_each_line_of_a_multiline_secret() {
        let mut m = Masker::default();
        m.add("a1b2\nc3d4");
        assert_eq!(m.mask("x a1b2 y"), "x *** y");
        assert_eq!(m.mask("x c3d4 y"), "x *** y");
    }

    #[test]
    fn masks_encoded_forms() {
        let mut m = Masker::default();
        m.add("p@ss\"w");
        assert_eq!(m.mask("cEBzcyJ3"), "***");
        assert_eq!(m.mask("p@ss\\\"w"), "***");
        assert_eq!(m.mask("p%40ss%22w"), "***");
    }

    #[test]
    fn masks_base64_inside_a_longer_string() {
        let mut m = Masker::default();
        m.add("hunter2-secret");
        for prefix in ["", "a", "ab"] {
            let text = STANDARD.encode(format!("{prefix}hunter2-secret and more"));
            assert!(m.mask(&text).contains("***"), "prefix {prefix:?}: {text}");
        }
        assert_eq!(m.mask(&STANDARD_NO_PAD.encode("hunter2-secret")), "***");
    }

    #[test]
    fn longest_secret_wins() {
        let mut m = Masker::default();
        m.add("abc");
        m.add("abcdef");
        assert_eq!(m.mask("xabcdefx"), "x***x");
    }

    #[test]
    fn ignores_blank_values_and_leaves_clean_lines_borrowed() {
        let mut m = Masker::default();
        m.add("");
        m.add("  \n\t");
        assert!(matches!(m.mask("plain"), Cow::Borrowed("plain")));
        m.add("zzzz");
        assert!(matches!(m.mask("plain"), Cow::Borrowed("plain")));
    }

    #[test]
    fn thousand_secrets_mask_in_one_pass() {
        let mut m = Masker::default();
        for i in 0..1000 {
            m.add(&format!("secret-value-{i:04}"));
        }
        let line = "secret-value-0999 ".repeat(1000);
        let out = m.mask(&line);
        assert_eq!(out, "*** ".repeat(1000));
    }
}
