// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Splits a byte stream into output lines.

use super::mask::Masker;

/// The longest line, in bytes, handed on. Longer lines are cut.
pub const MAX_LINE: usize = 64 << 10;

/// Collects bytes and returns whole lines. Masking runs on these lines,
/// so a secret written in several pieces is whole by then.
#[derive(Default)]
pub struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    /// Adds bytes and returns every line they complete. A line ends at
    /// `\n` (a preceding `\r` is dropped) or when it reaches
    /// [`MAX_LINE`] bytes. A multi-byte character is never cut.
    #[cfg(test)]
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        self.drain(None, false)
    }

    /// Like [`Lines::push`], but a cut at [`MAX_LINE`] never lands inside
    /// a value the masker hides, so each piece masks correctly.
    pub fn push_masked(&mut self, bytes: &[u8], masker: &Masker) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        self.drain(Some(masker), false)
    }

    /// Returns the unfinished last line, if there is one.
    #[cfg(test)]
    pub fn finish(self) -> Option<String> {
        if self.buf.is_empty() {
            None
        } else {
            Some(decode(&self.buf))
        }
    }

    /// Returns what is left, split like [`Lines::push_masked`] does.
    pub fn finish_masked(mut self, masker: &Masker) -> Vec<String> {
        self.drain(Some(masker), true)
    }

    fn drain(&mut self, masker: Option<&Masker>, flush: bool) -> Vec<String> {
        // With a masker, a value that starts before the cut may end up to
        // `max_len` bytes after it, so wait until those bytes are here.
        let ahead = if flush {
            0
        } else {
            masker.map_or(0, Masker::max_len)
        };
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            let rest = &self.buf[start..];
            let window = &rest[..rest.len().min(MAX_LINE + 1)];
            if let Some(pos) = window.iter().position(|&b| b == b'\n') {
                out.push(decode(&rest[..pos]));
                start += pos + 1;
            } else if rest.len() > MAX_LINE && rest.len() >= MAX_LINE + ahead {
                let mut cut = MAX_LINE;
                let mut back = 0;
                while back < 3 && is_continuation(rest[cut]) {
                    cut -= 1;
                    back += 1;
                }
                if is_continuation(rest[cut]) {
                    cut = MAX_LINE;
                }
                if let Some(m) = masker {
                    cut = m.safe_cut(rest, cut);
                }
                out.push(decode(&rest[..cut]));
                start += cut;
            } else if flush && !rest.is_empty() {
                out.push(decode(rest));
                start += rest.len();
            } else {
                break;
            }
        }
        self.buf.drain(..start);
        out
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_line_is_split() {
        let mut lines = Lines::default();
        let out = lines.push(&vec![b'a'; 200 << 10]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|l| l.len() == MAX_LINE));
        assert_eq!(lines.finish().map(|l| l.len()), Some(8 << 10));
    }

    #[test]
    fn secret_across_the_hard_cut_is_masked() {
        let mut m = Masker::default();
        m.add("XYZ123");
        let mut data = vec![b'a'; 200 << 10];
        data[MAX_LINE - 3..MAX_LINE + 3].copy_from_slice(b"XYZ123");
        let mut lines = Lines::default();
        let mut got = lines.push_masked(&data, &m);
        got.extend(lines.finish_masked(&m));
        let text: String = got.iter().map(|l| m.mask(l).into_owned()).collect();
        assert!(text.contains("***"));
        assert!(
            !text.contains("XYZ") && !text.contains("123"),
            "{}",
            &text[MAX_LINE - 10..MAX_LINE + 10]
        );
        assert!(got.iter().all(|l| l.len() <= MAX_LINE));
        assert_eq!(text.len(), (200 << 10) - 6 + 3);
    }

    #[test]
    fn a_secret_longer_than_a_line_is_cut_after_it() {
        let mut m = Masker::default();
        let secret = "S".repeat(70_000);
        m.add(&secret);
        let mut data = secret.clone().into_bytes();
        data.extend_from_slice(b" tail\n");
        let mut lines = Lines::default();
        let mut got = lines.push_masked(&data, &m);
        got.extend(lines.finish_masked(&m));
        let text: Vec<String> = got.iter().map(|l| m.mask(l).into_owned()).collect();
        assert_eq!(text, vec!["***".to_owned(), " tail".to_owned()]);
    }

    #[test]
    fn multibyte_character_is_never_cut() {
        let mut data = vec![b'a'; MAX_LINE - 1];
        data.extend_from_slice("é".as_bytes());
        data.extend_from_slice(b"tail");
        let mut lines = Lines::default();
        let out = lines.push(&data);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_LINE - 1);
        assert_eq!(lines.finish().as_deref(), Some("étail"));
    }

    #[test]
    fn invalid_utf8_becomes_replacement() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b"a\xFFb\n"), vec!["a\u{FFFD}b".to_owned()]);
    }

    #[test]
    fn crlf_and_empty_lines() {
        let mut lines = Lines::default();
        assert_eq!(
            lines.push(b"one\r\n\ntwo"),
            vec!["one".to_owned(), String::new()]
        );
        assert_eq!(lines.finish().as_deref(), Some("two"));
    }
}
