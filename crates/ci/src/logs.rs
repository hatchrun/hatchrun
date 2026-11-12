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

use std::io::{self, BufRead, BufReader, Read, Write};

use flate2::Compression;
use flate2::read::MultiGzDecoder;
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

/// The most error lines the index lists.
pub const MAX_ERRORS: usize = 200;
/// The most characters of an error line the index keeps.
pub const MAX_ERROR_CHARS: usize = 300;

/// Where a step's header starts in the log. Indexes stored before
/// `line` and `lines` existed read them as 0.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepMark {
    /// The step's position in the job, from 0.
    pub index: usize,
    /// The step's name.
    pub name: String,
    /// The byte offset of the step's `== name` line in the whole log.
    pub offset: u64,
    /// The step's header line, counted from 0.
    pub line: u64,
    /// The lines of the step, its header included.
    pub lines: u64,
}

/// A line that reports an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMark {
    /// The byte offset of the line in the whole log.
    pub offset: u64,
    /// The line, counted from 0.
    pub line: u64,
    /// The position of the step the line is in; `None` before the
    /// first step.
    pub step: Option<usize>,
    /// The line, cut to [`MAX_ERROR_CHARS`] characters.
    pub text: String,
}

/// What `<job>.index.json` holds. The writer's index has the first
/// three fields; the upload adds the rest while it reads the chunks
/// (see [`Scanner`]). An index stored before that reads `lines` as 0
/// and `errors` as empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogIndex {
    /// The size of the whole log in bytes.
    pub bytes: u64,
    /// Where each step starts.
    pub steps: Vec<StepMark>,
    /// How many lines were dropped for the size cap.
    pub dropped_lines: u64,
    /// How many lines the log has.
    pub lines: u64,
    /// The first [`MAX_ERRORS`] error lines.
    pub errors: Vec<ErrorMark>,
    /// The gzip members the stored log is made of. Empty for a log
    /// stored before members existed, which is one member.
    pub chunks: Vec<ChunkMark>,
}

/// One gzip member of a stored log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkMark {
    /// Where its text starts in the whole log, in bytes.
    pub offset: u64,
    /// The length of its text, in bytes.
    pub bytes: u64,
    /// Where the member starts in the stored object.
    pub at: u64,
    /// The length of the member in the stored object.
    pub len: u64,
}

/// The text of a member, about: a member ends at the first chunk end
/// at or after this many bytes.
pub const MEMBER_BYTES: u64 = 1 << 20;

/// Builds the stored log as concatenated gzip members of about
/// [`MEMBER_BYTES`] of text each: still one valid `.gz`, and a reader
/// that knows where a member lies decompresses only that one. Members
/// end where the text pushed to them ends, which is at a line end.
#[derive(Default)]
pub struct Members {
    out: Vec<u8>,
    current: Compressor,
    text: u64,
    done: u64,
    chunks: Vec<ChunkMark>,
}

impl Members {
    /// Adds `text`, which ends at a line end.
    pub fn push(&mut self, text: &str) {
        self.current.push(text);
        self.text += text.len() as u64;
        if self.text >= MEMBER_BYTES {
            self.close();
        }
    }

    fn close(&mut self) {
        if self.text == 0 {
            return;
        }
        let member = std::mem::take(&mut self.current).finish();
        self.chunks.push(ChunkMark {
            offset: self.done,
            bytes: self.text,
            at: self.out.len() as u64,
            len: member.len() as u64,
        });
        self.out.extend_from_slice(&member);
        self.done += self.text;
        self.text = 0;
    }

    /// The stored bytes and where each member lies. A log with no
    /// text is one empty member, and no marks.
    #[must_use]
    pub fn finish(mut self) -> (Vec<u8>, Vec<ChunkMark>) {
        self.close();
        if self.out.is_empty() {
            self.out = Compressor::new().finish();
        }
        (self.out, self.chunks)
    }
}

/// What to read of the stored object for a range of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// The whole object, which starts at text offset 0.
    Whole,
    /// Bytes `at` to `at + len` of the object, which hold members whose
    /// text starts at `start`.
    Range {
        /// Where in the object.
        at: u64,
        /// How many bytes.
        len: u64,
        /// The text offset of the first member.
        start: u64,
    },
    /// The range lies past the end of the text.
    Empty,
}

/// The members that hold text bytes `offset` to `offset + length`.
#[must_use]
pub fn span(chunks: &[ChunkMark], offset: u64, length: u64) -> Span {
    if chunks.is_empty() {
        return Span::Whole;
    }
    let end = offset.saturating_add(length);
    let covering: Vec<&ChunkMark> = chunks
        .iter()
        .filter(|c| c.offset + c.bytes > offset && c.offset < end.max(offset + 1))
        .collect();
    match (covering.first(), covering.last()) {
        (Some(first), Some(last)) => Span::Range {
            at: first.at,
            len: last.at + last.len - first.at,
            start: first.offset,
        },
        _ => Span::Empty,
    }
}

/// Whether a log line reports an error: a `##[error]` line, or the
/// agent's `Error: ` line for an `::error::` command.
fn is_error(line: &str) -> bool {
    line.contains("##[error]") || line.starts_with("Error: ")
}

/// Reads a finished log chunk by chunk and fills in what the writer
/// does not know: the line counts and the error lines. Chunks end at
/// line ends, and arrive in order.
#[derive(Debug)]
pub struct Scanner {
    index: LogIndex,
    offset: u64,
    line: u64,
    step: Option<usize>,
    next_mark: usize,
}

impl Scanner {
    /// A scanner for the log the writer's `index` describes.
    #[must_use]
    pub fn new(mut index: LogIndex) -> Self {
        index.errors.clear();
        Self {
            index,
            offset: 0,
            line: 0,
            step: None,
            next_mark: 0,
        }
    }

    /// Reads the next chunk.
    pub fn push(&mut self, chunk: &str) {
        for raw in chunk.split_inclusive('\n') {
            if let Some(mark) = self.index.steps.get_mut(self.next_mark)
                && mark.offset == self.offset
            {
                mark.line = self.line;
                self.step = Some(mark.index);
                self.next_mark += 1;
            }
            let text = raw.strip_suffix('\n').unwrap_or(raw);
            if self.index.errors.len() < MAX_ERRORS && is_error(text) {
                self.index.errors.push(ErrorMark {
                    offset: self.offset,
                    line: self.line,
                    step: self.step,
                    text: text.chars().take(MAX_ERROR_CHARS).collect(),
                });
            }
            self.offset += raw.len() as u64;
            self.line += 1;
        }
    }

    /// The index, with the lines counted.
    #[must_use]
    pub fn finish(mut self) -> LogIndex {
        self.index.lines = self.line;
        let firsts: Vec<u64> = self.index.steps.iter().map(|mark| mark.line).collect();
        for (at, mark) in self.index.steps.iter_mut().enumerate() {
            let end = firsts.get(at + 1).copied().unwrap_or(self.line);
            mark.lines = end.saturating_sub(mark.line);
        }
        self.index
    }
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
                ..StepMark::default()
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
            ..LogIndex::default()
        };
        (last, index)
    }
}

/// Whether `days` is a retention tier: 30, or 90 for enterprise tenants.
/// The logs bucket has one R2 lifecycle rule per tier.
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

/// The most bytes one range read of a stored log answers.
pub const MAX_RANGE_BYTES: u64 = 1 << 20;
/// The most matches one search page lists.
pub const MAX_MATCHES: usize = 1000;
/// The most matches a search counts before it stops reading the log.
pub const MAX_COUNTED: u64 = 100_000;

/// A range of a stored log, as bytes of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Where the bytes start in the whole log.
    pub offset: u64,
    /// The text.
    pub bytes: Vec<u8>,
}

/// Up to `length` bytes of the text of the gzip members `gz`, which
/// start at text offset `start`, from byte `offset` of the text. The
/// bytes before `offset` are read and dropped; with members only the
/// ones that hold the range are given. The range moves to whole
/// characters: it starts after a character that `offset` splits and
/// ends before one that the end splits.
///
/// # Errors
///
/// The stream is not valid gzip.
pub fn window(gz: &[u8], start: u64, offset: u64, length: u64) -> io::Result<Window> {
    let mut text = MultiGzDecoder::new(gz);
    io::copy(
        &mut text.by_ref().take(offset.saturating_sub(start)),
        &mut io::sink(),
    )?;
    let mut bytes = Vec::new();
    text.take(length.min(MAX_RANGE_BYTES))
        .read_to_end(&mut bytes)?;
    let skip = bytes
        .iter()
        .take(3)
        .take_while(|b| (**b & 0b1100_0000) == 0b1000_0000)
        .count();
    // Only when the range starts inside a character: at 0, or after a
    // line end, the first byte starts one.
    let skip = if offset == 0 { 0 } else { skip };
    bytes.drain(..skip.min(bytes.len()));
    if let Err(err) = std::str::from_utf8(&bytes)
        && err.error_len().is_none()
    {
        bytes.truncate(err.valid_up_to());
    }
    Ok(Window {
        offset: offset + skip as u64,
        bytes,
    })
}

/// The size of the text of the gzip stream `gz`, in bytes.
///
/// # Errors
///
/// The stream is not valid gzip.
pub fn text_len(gz: &[u8]) -> io::Result<u64> {
    io::copy(&mut MultiGzDecoder::new(gz), &mut io::sink())
}

/// A line of a log that holds the text searched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The byte offset of the line.
    pub offset: u64,
    /// The line, counted from 0.
    pub line: u64,
    /// The position of the step the line is in.
    pub step: Option<usize>,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The first [`MAX_MATCHES`] hits from the cursor on.
    pub hits: Vec<Hit>,
    /// How many lines hold the text, in the whole log.
    pub total: u64,
    /// Where the next page starts, when there is one.
    pub next: Option<u64>,
    /// Whether the count stopped at [`MAX_COUNTED`].
    pub capped: bool,
}

/// A search over a log, fed one gzip stream at a time: the members of
/// a log in order, or the whole stream. Lines do not cross members.
pub struct Searcher<'a> {
    needle: String,
    steps: &'a [StepMark],
    from: u64,
    found: Found,
    offset: u64,
    number: u64,
    step: usize,
    current: Option<usize>,
}

impl<'a> Searcher<'a> {
    /// A search for `needle`, ignoring case, that lists the hits at
    /// byte offset `from` or later. `steps` name the step of each hit.
    #[must_use]
    pub fn new(needle: &str, steps: &'a [StepMark], from: u64) -> Self {
        Self {
            needle: needle.to_lowercase(),
            steps,
            from,
            found: Found {
                hits: Vec::new(),
                total: 0,
                next: None,
                capped: false,
            },
            offset: 0,
            number: 0,
            step: 0,
            current: None,
        }
    }

    /// Whether the count stopped at [`MAX_COUNTED`]: more input would
    /// change nothing.
    #[must_use]
    pub fn done(&self) -> bool {
        self.found.capped
    }

    /// Reads the next stream, line by line.
    ///
    /// # Errors
    ///
    /// The stream is not valid gzip.
    pub fn scan(&mut self, gz: &[u8]) -> io::Result<()> {
        let mut reader = BufReader::with_capacity(CHUNK_BYTES, MultiGzDecoder::new(gz));
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                return Ok(());
            }
            while let Some(mark) = self.steps.get(self.step)
                && mark.offset <= self.offset
            {
                self.current = Some(mark.index);
                self.step += 1;
            }
            if String::from_utf8_lossy(&line)
                .to_lowercase()
                .contains(&self.needle)
            {
                self.found.total += 1;
                if self.offset >= self.from {
                    if self.found.hits.len() < MAX_MATCHES {
                        self.found.hits.push(Hit {
                            offset: self.offset,
                            line: self.number,
                            step: self.current,
                        });
                    } else if self.found.next.is_none() {
                        self.found.next = Some(self.offset);
                    }
                }
                if self.found.total >= MAX_COUNTED {
                    self.found.capped = true;
                    return Ok(());
                }
            }
            self.offset += read as u64;
            self.number += 1;
        }
    }

    /// What was found.
    #[must_use]
    pub fn finish(self) -> Found {
        self.found
    }
}

/// The lines of the gzip stream `gz` that hold `needle`, ignoring case,
/// from byte offset `from` on: [`Searcher`] over one stream.
///
/// # Errors
///
/// The stream is not valid gzip.
pub fn search(gz: &[u8], needle: &str, steps: &[StepMark], from: u64) -> io::Result<Found> {
    let mut searcher = Searcher::new(needle, steps, from);
    searcher.scan(gz)?;
    Ok(searcher.finish())
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
    use std::fmt::Write as _;
    use std::io::Read;

    use flate2::read::{GzDecoder, MultiGzDecoder};

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
    fn scanner_counts_lines_and_finds_errors() {
        let mut w = LogWriter::default();
        w.line("before");
        w.step_started(0, "a");
        w.line("one");
        w.step_started(1, "b");
        w.line("##[error]boom");
        w.line("Error: \u{e9}");
        w.line(&format!("Error: {}", "x".repeat(400)));
        w.line("not an Error: line");
        let (text, index) = whole(&mut w);
        // Feed the text in two chunks that split at a line end.
        let split = text.find("one\n").unwrap() + 4;
        let mut scanner = Scanner::new(index);
        scanner.push(&text[..split]);
        scanner.push(&text[split..]);
        let index = scanner.finish();
        assert_eq!(index.lines, 8);
        assert_eq!(index.steps[0].line, 1);
        assert_eq!(index.steps[0].lines, 2);
        assert_eq!(index.steps[1].line, 3);
        assert_eq!(index.steps[1].lines, 5);
        assert_eq!(index.errors.len(), 3);
        let first = &index.errors[0];
        assert_eq!((first.line, first.step), (4, Some(1)));
        assert_eq!(first.text, "##[error]boom");
        assert_eq!(
            &text[usize::try_from(first.offset).unwrap()..][..5],
            "##[er"
        );
        assert_eq!(index.errors[2].text.chars().count(), MAX_ERROR_CHARS);
    }

    #[test]
    fn scanner_caps_the_errors() {
        let mut w = LogWriter::default();
        for _ in 0..MAX_ERRORS + 10 {
            w.line("##[error]x");
        }
        let (text, index) = whole(&mut w);
        let mut scanner = Scanner::new(index);
        scanner.push(&text);
        let index = scanner.finish();
        assert_eq!(index.errors.len(), MAX_ERRORS);
        assert_eq!(index.lines, MAX_ERRORS as u64 + 10);
    }

    #[test]
    fn an_old_index_reads_with_defaults() {
        let old = r#"{"bytes":9,"steps":[{"index":0,"name":"a","offset":0}],"dropped_lines":0}"#;
        let index: LogIndex = serde_json::from_str(old).unwrap();
        assert_eq!(index.lines, 0);
        assert_eq!(index.errors.len(), 0);
        assert_eq!((index.steps[0].line, index.steps[0].lines), (0, 0));
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

    fn stored() -> (Vec<u8>, LogIndex) {
        let mut w = LogWriter::default();
        w.line("before \u{e9}");
        w.step_started(0, "build");
        w.line("compiling Foo");
        w.line("nothing here");
        w.step_started(1, "test");
        w.line("FOO failed");
        let (text, index) = whole(&mut w);
        (gzip([text]), index)
    }

    #[test]
    fn a_window_reads_bytes_of_the_text_and_keeps_characters_whole() {
        let (gz, index) = stored();
        let all = window(&gz, 0, 0, MAX_RANGE_BYTES).unwrap();
        assert_eq!(all.bytes.len() as u64, index.bytes);
        let two = window(&gz, 0, 3, 6).unwrap();
        assert_eq!(
            (two.offset, two.bytes.as_slice()),
            (3, "ore \u{e9}".as_bytes())
        );
        // Byte 8 is the second byte of the e-acute: the range starts after it.
        let split = window(&gz, 0, 8, 4).unwrap();
        assert_eq!((split.offset, split.bytes.as_slice()), (9, &b"\n=="[..]));
        // An end inside a character leaves it out.
        let cut = window(&gz, 0, 0, 8).unwrap();
        assert_eq!(cut.bytes, b"before ");
        assert_eq!(window(&gz, 0, 10_000, 10).unwrap().bytes, b"");
        assert!(window(b"not gzip", 0, 0, 10).is_err());
    }

    /// A log of `lines` lines of `width` bytes, in members of
    /// `per_member` lines.
    fn members_of(lines: usize, per_member: usize) -> (Vec<u8>, Vec<ChunkMark>, String) {
        let mut members = Members::default();
        let mut text = String::new();
        for first in (0..lines).step_by(per_member) {
            let mut chunk = String::new();
            for n in first..(first + per_member).min(lines) {
                let _ = writeln!(chunk, "line {n:05} x");
            }
            text.push_str(&chunk);
            members.push(&chunk);
            // Members close at the size, not at the chunk: force it.
            members.close();
        }
        let (gz, chunks) = members.finish();
        (gz, chunks, text)
    }

    #[test]
    fn members_are_one_valid_gzip_and_can_be_read_alone() {
        let (gz, chunks, text) = members_of(30, 10);
        assert_eq!(chunks.len(), 3);
        let mut whole = String::new();
        MultiGzDecoder::new(gz.as_slice())
            .read_to_string(&mut whole)
            .unwrap();
        assert_eq!(whole, text);
        assert_eq!(chunks[1].offset, 130);
        assert_eq!(chunks[1].bytes, 130);
        // A range inside the middle member reads only that member.
        let Span::Range { at, len, start } = span(&chunks, 140, 20) else {
            panic!("not a range");
        };
        assert_eq!(start, 130);
        let part = &gz[usize::try_from(at).unwrap()..usize::try_from(at + len).unwrap()];
        let got = window(part, start, 140, 20).unwrap();
        assert_eq!(got.bytes, text.as_bytes()[140..160].to_vec());
        // A range over two members reads both.
        let Span::Range { at, len, start } = span(&chunks, 120, 20) else {
            panic!("not a range");
        };
        assert_eq!(start, 0);
        let part = &gz[usize::try_from(at).unwrap()..usize::try_from(at + len).unwrap()];
        let got = window(part, start, 120, 20).unwrap();
        assert_eq!(got.bytes, text.as_bytes()[120..140].to_vec());
        assert_eq!(span(&chunks, 390, 5), Span::Empty);
        assert_eq!(span(&[], 5, 5), Span::Whole);
    }

    #[test]
    fn members_close_at_the_size_and_an_empty_log_is_valid() {
        let mut members = Members::default();
        let line = "y".repeat(1000) + "\n";
        for _ in 0..(usize::try_from(MEMBER_BYTES).unwrap() / 1001) + 2 {
            members.push(&line);
        }
        let (_, chunks) = members.finish();
        assert_eq!(chunks.len(), 2);
        let (gz, chunks) = Members::default().finish();
        assert_eq!(chunks.len(), 0);
        assert_eq!(text_len(&gz).unwrap(), 0);
    }

    #[test]
    fn a_search_streams_members_one_at_a_time() {
        let (gz, chunks, _) = members_of(30, 10);
        let mut searcher = Searcher::new("LINE 00015", &[], 0);
        for chunk in &chunks {
            let at = usize::try_from(chunk.at).unwrap();
            let end = at + usize::try_from(chunk.len).unwrap();
            searcher.scan(&gz[at..end]).unwrap();
        }
        let found = searcher.finish();
        assert_eq!(found.total, 1);
        assert_eq!(
            found.hits[0],
            Hit {
                offset: 195,
                line: 15,
                step: None
            }
        );
    }

    #[test]
    fn search_finds_lines_ignoring_case_with_their_steps() {
        let (gz, index) = stored();
        let found = search(&gz, "foo", &index.steps, 0).unwrap();
        assert_eq!(found.total, 2);
        assert_eq!(
            found.hits,
            [
                Hit {
                    offset: 19,
                    line: 2,
                    step: Some(0)
                },
                Hit {
                    offset: 54,
                    line: 5,
                    step: Some(1)
                },
            ]
        );
        assert_eq!(found.next, None);
        let rest = search(&gz, "foo", &index.steps, 20).unwrap();
        assert_eq!((rest.total, rest.hits.len()), (2, 1));
        let none = search(&gz, "absent", &index.steps, 0).unwrap();
        assert_eq!((none.total, none.hits.len()), (0, 0));
        let before = search(&gz, "BEFORE", &index.steps, 0).unwrap();
        assert_eq!(before.hits[0].step, None);
    }

    #[test]
    fn a_search_page_ends_at_the_match_limit() {
        let mut w = LogWriter::default();
        for _ in 0..MAX_MATCHES + 5 {
            w.line("hit");
        }
        let (text, index) = whole(&mut w);
        let gz = gzip([text]);
        let first = search(&gz, "hit", &index.steps, 0).unwrap();
        assert_eq!(first.hits.len(), MAX_MATCHES);
        assert_eq!(first.total, MAX_MATCHES as u64 + 5);
        let next = first.next.unwrap();
        assert_eq!(next, 4 * MAX_MATCHES as u64);
        let second = search(&gz, "hit", &index.steps, next).unwrap();
        assert_eq!(second.hits.len(), 5);
        assert_eq!(second.next, None);
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
