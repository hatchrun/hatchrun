// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Secret masking for step output.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
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

/// Finds the hidden values in a byte string.
enum Matcher {
    /// One Aho-Corasick pass.
    Fast(AhoCorasick),
    /// Longest pattern first at each position. Used when the automaton
    /// cannot be built, so output is still masked.
    Plain(Vec<String>),
}

impl Matcher {
    fn build(patterns: &BTreeSet<String>) -> Self {
        AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns)
            .map_or_else(|_| Self::plain(patterns), Self::Fast)
    }

    fn plain(patterns: &BTreeSet<String>) -> Self {
        let mut sorted: Vec<String> = patterns.iter().cloned().collect();
        sorted.sort_by_key(|p| std::cmp::Reverse(p.len()));
        Self::Plain(sorted)
    }

    /// Non-overlapping `(start, end)` spans, left to right, longest first.
    fn spans(&self, hay: &[u8]) -> Vec<(usize, usize)> {
        match self {
            Self::Fast(ac) => ac.find_iter(hay).map(|m| (m.start(), m.end())).collect(),
            Self::Plain(sorted) => {
                let mut out = Vec::new();
                let mut i = 0;
                while i < hay.len() {
                    let rest = &hay[i..];
                    if let Some(p) = sorted.iter().find(|p| rest.starts_with(p.as_bytes())) {
                        out.push((i, i + p.len()));
                        i += p.len();
                    } else {
                        i += 1;
                    }
                }
                out
            }
        }
    }
}

/// The set of values to hide, matched in one pass over each line.
#[derive(Default)]
pub struct Masker {
    patterns: BTreeSet<String>,
    max_len: usize,
    matcher: OnceLock<Option<Matcher>>,
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
            for set in [STRICT, URI_COMPONENT] {
                let upper = utf8_percent_encode(line, set).to_string();
                self.insert(lower_hex(&upper));
                self.insert(upper);
            }
        }
        self.matcher = OnceLock::new();
    }

    fn insert(&mut self, pattern: String) {
        if !pattern.trim().is_empty() {
            self.max_len = self.max_len.max(pattern.len());
            self.patterns.insert(pattern);
        }
    }

    /// Adds the base64 text of `bytes` (standard and URL-safe alphabets)
    /// as it appears at each of the three alignments inside a longer
    /// base64 string. Only the characters that do not depend on
    /// neighbouring bytes are kept.
    fn insert_base64(&mut self, bytes: &[u8]) {
        self.insert(STANDARD.encode(bytes));
        self.insert(URL_SAFE.encode(bytes));
        for (pad, skip) in [(0usize, 0usize), (1, 2), (2, 3)] {
            let mut buf = vec![0u8; pad];
            buf.extend_from_slice(bytes);
            for engine in [STANDARD_NO_PAD, URL_SAFE_NO_PAD] {
                let mut text = engine.encode(&buf);
                if buf.len() % 3 != 0 {
                    text.pop();
                }
                let stable = &text[skip.min(text.len())..];
                if pad == 0 && skip == 0 {
                    self.insert(engine.encode(bytes));
                }
                if pad == 0 || stable.len() >= MIN_PARTIAL_B64 {
                    self.insert(stable.to_owned());
                }
            }
        }
    }

    fn matcher(&self) -> Option<&Matcher> {
        self.matcher
            .get_or_init(|| {
                if self.patterns.is_empty() {
                    None
                } else {
                    Some(Matcher::build(&self.patterns))
                }
            })
            .as_ref()
    }

    /// Replaces every hidden value in `line` with `***`. At each position
    /// the longest match wins.
    pub fn mask<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let Some(matcher) = self.matcher() else {
            return Cow::Borrowed(line);
        };
        let spans = matcher.spans(line.as_bytes());
        if spans.is_empty() {
            return Cow::Borrowed(line);
        }
        let mut out = String::with_capacity(line.len());
        let mut last = 0;
        for (start, end) in spans {
            out.push_str(&line[last..start]);
            out.push_str(REPLACEMENT);
            last = end;
        }
        out.push_str(&line[last..]);
        Cow::Owned(out)
    }

    /// The length in bytes of the longest value that can be hidden.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// The largest cut point at or below `max` that is not inside a
    /// hidden value in `buf`. Returns `max` when no value spans it.
    pub fn safe_cut(&self, buf: &[u8], max: usize) -> usize {
        let Some(matcher) = self.matcher() else {
            return max;
        };
        let end = buf.len().min(max.saturating_add(self.max_len));
        for (start, stop) in matcher.spans(&buf[..end]) {
            if start < max && max < stop {
                return start;
            }
        }
        max
    }
}

/// Lowercases the hex digits of every `%XX` in `s`.
fn lower_hex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut keep = 0;
    for c in s.chars() {
        if c == '%' {
            keep = 2;
            out.push(c);
        } else if keep > 0 {
            keep -= 1;
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
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
    fn masks_url_safe_base64_and_lowercase_percent() {
        let mut m = Masker::default();
        m.add("???>>>");
        assert_eq!(m.mask("Pz8_Pj4-"), "***");
        let mut m = Masker::default();
        m.add("a/b");
        assert_eq!(m.mask("a%2Fb"), "***");
        assert_eq!(m.mask("a%2fb"), "***");
    }

    #[test]
    fn plain_fallback_masks_longest_first() {
        let matcher = Matcher::Plain(vec!["abcdef".into(), "abc".into(), "zz".into()]);
        let spans = matcher.spans(b"xabcdefx abc zz");
        assert_eq!(spans, vec![(1, 7), (9, 12), (13, 15)]);
    }

    #[test]
    fn cut_never_splits_a_match() {
        let mut m = Masker::default();
        m.add("XYZ123");
        let mut buf = vec![b'a'; 100];
        buf[48..54].copy_from_slice(b"XYZ123");
        assert_eq!(m.safe_cut(&buf, 50), 48);
        assert_eq!(m.safe_cut(&buf, 54), 54);
        assert_eq!(m.safe_cut(&buf, 40), 40);
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
