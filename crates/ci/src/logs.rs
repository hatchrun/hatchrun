// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The text log of one job: how the run object collects lines into
//! chunks while the job runs, the index of its steps, and the gzip and
//! key layout of the finished log in R2.
//!
//! Nothing here touches storage or the network. The object keeps the
//! [`LogWriter`], stores each full chunk as its own value and, when the
//! job ends, calls [`LogWriter::finish`] and uploads the result.
//!
//! Memory bounds: a log is at most [`MAX_LOG_BYTES`] (later lines are
//! counted and dropped), a line is cut at [`MAX_LINE_BYTES`] and a
//! chunk is at most [`CHUNK_BYTES`] plus one line, so no value passes
//! the 128 KiB limit of one storage value.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

/// The most text one job's log keeps, in bytes. Lines after it are
/// counted and dropped.
pub const MAX_LOG_BYTES: u64 = 32 << 20;
/// A chunk ends at the first line end at or after this many bytes.
pub const CHUNK_BYTES: usize = 64 << 10;
/// The most stored text a new WebSocket gets first, in bytes.
pub const BACKLOG_BYTES: u64 = 2 << 20;
/// The longest line kept, in bytes. A longer one is cut.
pub const MAX_LINE_BYTES: usize = 8 << 10;
/// The longest step name kept in a header and in the index, in bytes.
pub const MAX_NAME_BYTES: usize = 96;
/// The most steps the index marks. A step after them has its header
/// and no mark.
pub const MAX_MARKS: usize = 256;

/// Where a step's header starts in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepMark {
    /// The step's position in the job, from 0.
    pub index: usize,
    /// The step's name.
    pub name: String,
    /// The byte offset of the step's `== name` line in the whole log.
    pub offset: u64,
}

/// What `<job>.index.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogIndex {
    /// The size of the whole log in bytes.
    pub bytes: u64,
    /// Where each step starts.
    pub steps: Vec<StepMark>,
    /// How many lines were dropped for the size cap.
    pub dropped_lines: u64,
}

/// Collects the lines of a job into chunks.
///
/// Everything but the open chunk serializes; the object stores the open
/// chunk as bytes in a value of its own (see [`open_text`] and
/// [`with_open`]), because a text value that holds one character
/// outside Latin-1 takes two bytes per character in storage.
///
/// [`open_text`]: LogWriter::open_text
/// [`with_open`]: LogWriter::with_open
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogWriter {
    bytes: u64,
    steps: Vec<StepMark>,
    dropped_lines: u64,
    /// The number the next chunk gets.
    chunks: u32,
    /// The size in bytes of each numbered chunk, in order.
    sizes: Vec<u32>,
    #[serde(skip)]
    open: String,
}

/// `text` cut to at most `max` bytes at a character boundary, with a
/// marker when something was cut.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [cut]", &text[..end])
}

impl LogWriter {
    /// A writer that continues with the open chunk `open`.
    #[must_use]
    pub fn with_open(mut self, open: String) -> Self {
        self.open = open;
        self
    }

    /// The text not yet in a numbered chunk.
    #[must_use]
    pub fn open_text(&self) -> &str {
        &self.open
    }

    /// How many bytes the log has so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The size in bytes of each numbered chunk.
    #[must_use]
    pub fn chunk_sizes(&self) -> &[u32] {
        &self.sizes
    }

    /// How many numbered chunks exist, which is also the number the
    /// next one gets.
    #[must_use]
    pub fn chunk_count(&self) -> u32 {
        self.chunks
    }

    fn push(&mut self, text: &str) {
        self.open.push_str(text);
        self.open.push('\n');
        self.bytes += text.len() as u64 + 1;
    }

    /// Starts step `index`: writes `== {name}` and records where it is.
    /// Past [`MAX_LOG_BYTES`] it only counts a drop, like a line.
    pub fn step_started(&mut self, index: usize, name: &str) {
        self.step_started_within(index, name, MAX_LOG_BYTES);
    }

    fn step_started_within(&mut self, index: usize, name: &str, cap: u64) {
        let name = clip(name, MAX_NAME_BYTES);
        let offset = self.bytes;
        if self.bytes + "== ".len() as u64 + name.len() as u64 + 1 > cap {
            self.dropped_lines += 1;
            return;
        }
        self.push(&format!("== {name}"));
        if self.steps.len() < MAX_MARKS {
            self.steps.push(StepMark {
                index,
                name,
                offset,
            });
        }
    }

    /// Writes one line. Past [`MAX_LOG_BYTES`] it only counts a drop.
    pub fn line(&mut self, text: &str) {
        self.line_within(text, MAX_LOG_BYTES);
    }

    fn line_within(&mut self, text: &str, cap: u64) {
        let text = clip(text, MAX_LINE_BYTES);
        if self.bytes + text.len() as u64 + 1 > cap {
            self.dropped_lines += 1;
            return;
        }
        self.push(&text);
    }

    /// Takes the text that reached [`CHUNK_BYTES`] as numbered chunks.
    /// Each chunk ends at a line end.
    pub fn take_full_chunks(&mut self) -> Vec<(u32, String)> {
        let mut chunks = Vec::new();
        let mut start = 0;
        while self.open.len() - start >= CHUNK_BYTES {
            let Some(newline) = self.open.as_bytes()[start + CHUNK_BYTES - 1..]
                .iter()
                .position(|b| *b == b'\n')
            else {
                break;
            };
            let end = start + CHUNK_BYTES + newline;
            chunks.push((self.chunks, self.open[start..end].to_owned()));
            self.chunks += 1;
            self.sizes
                .push(u32::try_from(end - start).unwrap_or(u32::MAX));
            start = end;
        }
        self.open.drain(..start);
        chunks
    }

    /// Ends the log: adds a line about dropped lines when there were
    /// any, and returns the last chunk (none when the log has nothing
    /// left) with the index. Call it once.
    pub fn finish(&mut self) -> (Option<(u32, String)>, LogIndex) {
        if self.dropped_lines > 0 {
            let note = format!(
                "[hatchrun] {} lines were dropped: the log is over 32 MiB",
                self.dropped_lines
            );
            self.push(&note);
        }
        let last = if self.open.is_empty() {
            None
        } else {
            let number = self.chunks;
            self.chunks += 1;
            self.sizes
                .push(u32::try_from(self.open.len()).unwrap_or(u32::MAX));
            Some((number, std::mem::take(&mut self.open)))
        };
        let index = LogIndex {
            bytes: self.bytes,
            steps: self.steps.clone(),
            dropped_lines: self.dropped_lines,
        };
        (last, index)
    }
}

/// Whether `days` is a retention tier: 30, or 90 for enterprise tenants.
/// The R2 lifecycle rules of `r2-lifecycle.json` are one per tier.
#[must_use]
pub fn is_tier(days: u32) -> bool {
    matches!(days, 30 | 90)
}

/// The key of the log of attempt `attempt` of a job in the logs bucket,
/// under the tier of the tenant's retention `days`.
#[must_use]
pub fn log_key(
    days: u32,
    tenant: &str,
    repo_id: u64,
    run_id: u64,
    job_id: u64,
    attempt: u32,
) -> String {
    format!("logs/{days}d/{tenant}/{repo_id}/{run_id}/{job_id}/{attempt}.log.gz")
}

/// The key of the index of that log.
#[must_use]
pub fn index_key(
    days: u32,
    tenant: &str,
    repo_id: u64,
    run_id: u64,
    job_id: u64,
    attempt: u32,
) -> String {
    format!("logs/{days}d/{tenant}/{repo_id}/{run_id}/{job_id}/{attempt}.index.json")
}

/// Gzips text as it comes, so a log of many chunks is never held whole
/// next to its compressed form.
pub struct Compressor {
    encoder: GzEncoder<Vec<u8>>,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self {
            encoder: GzEncoder::new(Vec::new(), Compression::default()),
        }
    }

    /// Adds `text` to the stream.
    pub fn push(&mut self, text: &str) {
        // Writing to a Vec does not fail.
        let _ = self.encoder.write_all(text.as_bytes());
    }

    /// Ends the stream and returns its bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.encoder.finish().unwrap_or_default()
    }
}

/// The chunks joined, as one gzip stream.
#[must_use]
pub fn gzip(chunks: impl IntoIterator<Item = String>) -> Vec<u8> {
    let mut compressor = Compressor::new();
    for chunk in chunks {
        compressor.push(&chunk);
    }
    compressor.finish()
}

/// The last `lines` lines of `text` from byte offset `from` on.
#[must_use]
pub fn tail(text: &str, from: u64, lines: usize) -> Vec<String> {
    let mut from = usize::try_from(from).unwrap_or(usize::MAX).min(text.len());
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let all: Vec<&str> = text[from..].lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;

    fn whole(writer: &mut LogWriter) -> (String, LogIndex) {
        let mut text = String::new();
        for (_, chunk) in writer.take_full_chunks() {
            text.push_str(&chunk);
        }
        let (last, index) = writer.finish();
        if let Some((_, chunk)) = last {
            text.push_str(&chunk);
        }
        (text, index)
    }

    #[test]
    fn index_points_at_step_headers() {
        let mut w = LogWriter::default();
        w.step_started(0, "a");
        w.line("one");
        w.step_started(1, "b");
        w.line("two");
        let (text, index) = whole(&mut w);
        assert_eq!(text, "== a\none\n== b\ntwo\n");
        assert_eq!(index.bytes, text.len() as u64);
        assert_eq!(index.steps.len(), 2);
        for mark in &index.steps {
            let at = usize::try_from(mark.offset).unwrap();
            assert!(text[at..].starts_with(&format!("== {}\n", mark.name)));
        }
        assert_eq!(index.steps[1].index, 1);
    }

    #[test]
    fn chunks_split_at_the_chunk_size() {
        let mut w = LogWriter::default();
        let line = "x".repeat(999);
        for _ in 0..200 {
            w.line(&line);
        }
        let chunks = w.take_full_chunks();
        assert_eq!(chunks.len(), 3);
        for (i, (n, chunk)) in chunks.iter().enumerate() {
            assert_eq!(*n as usize, i);
            assert!(chunk.len() >= CHUNK_BYTES);
            assert!(chunk.len() < CHUNK_BYTES + 1000);
            assert!(chunk.ends_with('\n'));
        }
        assert_eq!(w.chunk_count(), 3);
        let (last, index) = w.finish();
        let (n, rest) = last.unwrap();
        assert_eq!(n, 3);
        assert!(rest.len() < CHUNK_BYTES);
        let total: usize = chunks.iter().map(|(_, c)| c.len()).sum::<usize>() + rest.len();
        assert_eq!(total as u64, index.bytes);
        assert_eq!(total, 200 * 1000);
    }

    #[test]
    fn a_short_log_has_one_last_chunk_and_an_empty_one_none() {
        let mut w = LogWriter::default();
        w.line("hi");
        assert_eq!(w.take_full_chunks(), Vec::<(u32, String)>::new());
        let (last, _) = w.finish();
        assert_eq!(last, Some((0, "hi\n".to_owned())));
        let (last, index) = LogWriter::default().finish();
        assert_eq!(last, None);
        assert_eq!(index.bytes, 0);
    }

    #[test]
    fn drops_past_the_cap_and_says_so() {
        let mut w = LogWriter::default();
        w.line_within("aaaa", 10);
        w.line_within("bbbb", 10);
        w.line_within("cccc", 10);
        w.line_within("d", 10);
        let (text, index) = whole(&mut w);
        assert_eq!(index.dropped_lines, 2);
        assert_eq!(
            text,
            "aaaa\nbbbb\n[hatchrun] 2 lines were dropped: the log is over 32 MiB\n"
        );
        assert_eq!(index.bytes, text.len() as u64);
    }

    #[test]
    fn step_headers_count_against_the_cap() {
        let mut w = LogWriter::default();
        w.step_started_within(0, "a", 11);
        w.step_started_within(1, "b", 11);
        w.step_started_within(2, "c", 11);
        w.line_within("x", 11);
        let (text, index) = whole(&mut w);
        assert_eq!(index.steps.len(), 2);
        assert_eq!(index.dropped_lines, 2);
        assert_eq!(
            text,
            "== a\n== b\n[hatchrun] 2 lines were dropped: the log is over 32 MiB\n"
        );
    }

    #[test]
    fn a_flood_of_headers_stays_under_the_cap_and_the_note() {
        let mut w = LogWriter::default();
        let mut taken = 0_u64;
        for i in 0..400_000 {
            w.step_started(i, &"n".repeat(MAX_NAME_BYTES));
            for (_, chunk) in w.take_full_chunks() {
                taken += chunk.len() as u64;
            }
        }
        let (last, index) = w.finish();
        taken += last.map_or(0, |(_, c)| c.len() as u64);
        assert!(index.dropped_lines > 0);
        assert!(taken <= MAX_LOG_BYTES + 100);
        assert_eq!(taken, index.bytes);
    }

    #[test]
    fn chunk_sizes_add_up_to_the_stored_text() {
        let mut w = LogWriter::default();
        for _ in 0..200 {
            w.line(&"x".repeat(999));
        }
        let mut total = 0;
        for (_, chunk) in w.take_full_chunks() {
            total += chunk.len();
        }
        let (last, index) = w.finish();
        total += last.unwrap().1.len();
        let sizes: Vec<usize> = w.chunk_sizes().iter().map(|s| *s as usize).collect();
        assert_eq!(u32::try_from(sizes.len()).unwrap(), w.chunk_count());
        assert_eq!(sizes.iter().sum::<usize>(), total);
        assert_eq!(total as u64, index.bytes);
    }

    #[test]
    fn a_log_under_the_cap_has_no_note() {
        let mut w = LogWriter::default();
        w.line("ok");
        let (text, index) = whole(&mut w);
        assert_eq!(text, "ok\n");
        assert_eq!(index.dropped_lines, 0);
    }

    #[test]
    fn long_lines_and_names_are_cut_at_a_character_boundary() {
        let mut w = LogWriter::default();
        w.step_started(0, &"\u{e9}".repeat(200));
        w.line(&"\u{e9}".repeat(MAX_LINE_BYTES));
        let (text, index) = whole(&mut w);
        assert!(index.steps[0].name.len() <= MAX_NAME_BYTES + " [cut]".len());
        assert!(text.len() < MAX_LINE_BYTES + 200);
        assert!(text.contains("[cut]"));
    }

    #[test]
    fn marks_stop_at_the_limit_but_headers_do_not() {
        let mut w = LogWriter::default();
        for i in 0..MAX_MARKS + 5 {
            w.step_started(i, "s");
        }
        let (text, index) = whole(&mut w);
        assert_eq!(index.steps.len(), MAX_MARKS);
        assert_eq!(text.lines().count(), MAX_MARKS + 5);
    }

    #[test]
    fn writer_state_survives_serde_without_the_open_chunk() {
        let mut w = LogWriter::default();
        w.step_started(0, "a");
        for _ in 0..100 {
            w.line(&"y".repeat(1000));
        }
        let taken = w.take_full_chunks().len();
        let json = serde_json::to_string(&w).unwrap();
        let back: LogWriter = serde_json::from_str(&json).unwrap();
        let back = back.with_open(w.open_text().to_owned());
        assert_eq!(back, w);
        assert_eq!(back.chunk_count() as usize, taken);
    }

    #[test]
    fn gzip_round_trips() {
        let packed = gzip(["== a\n".to_owned(), "line\n".to_owned()]);
        let mut text = String::new();
        GzDecoder::new(packed.as_slice())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "== a\nline\n");
    }

    #[test]
    fn keys_follow_the_layout() {
        assert_eq!(log_key(30, "t1", 9, 7, 3, 2), "logs/30d/t1/9/7/3/2.log.gz");
        assert_eq!(
            index_key(90, "t1", 9, 7, 3, 2),
            "logs/90d/t1/9/7/3/2.index.json"
        );
        assert_ne!(log_key(30, "t1", 9, 7, 3, 1), log_key(30, "t1", 9, 7, 3, 2));
    }

    #[test]
    fn tail_takes_the_last_lines_of_a_step() {
        let text = "== a\n1\n2\n== b\n3\n4\n5\n";
        let b = text.find("== b").unwrap() as u64;
        assert_eq!(tail(text, b, 2), vec!["4", "5"]);
        assert_eq!(tail(text, b, 10), vec!["== b", "3", "4", "5"]);
        assert_eq!(tail(text, 0, 1), vec!["5"]);
        assert_eq!(tail(text, 10_000, 3), Vec::<String>::new());
    }
}
