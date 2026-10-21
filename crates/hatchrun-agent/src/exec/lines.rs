// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Splits a byte stream into output lines.

// Steps are not wired to the splitter yet.
#![allow(dead_code)]

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
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            let rest = &self.buf[start..];
            let window = &rest[..rest.len().min(MAX_LINE + 1)];
            if let Some(pos) = window.iter().position(|&b| b == b'\n') {
                out.push(decode(&rest[..pos]));
                start += pos + 1;
            } else if rest.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                let mut back = 0;
                while back < 3 && is_continuation(rest[cut]) {
                    cut -= 1;
                    back += 1;
                }
                if is_continuation(rest[cut]) {
                    cut = MAX_LINE;
                }
                out.push(decode(&rest[..cut]));
                start += cut;
            } else {
                break;
            }
        }
        self.buf.drain(..start);
        out
    }

    /// Returns the unfinished last line, if there is one.
    pub fn finish(self) -> Option<String> {
        if self.buf.is_empty() {
            None
        } else {
            Some(decode(&self.buf))
        }
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
