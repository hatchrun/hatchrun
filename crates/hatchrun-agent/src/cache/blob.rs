// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The part of Azure Blob storage the cache clients upload with, mapped
//! onto an S3 multipart upload.
//!
//! A client uploads to the `signed_upload_url` the proxy handed out,
//! either with one Put Blob or with Put Block calls and a Put Block List
//! that orders them. A Put Blob body is cut into [`PART_SIZE`] parts as
//! it streams in. Each block becomes one part, uploaded as soon as it
//! has arrived; its part number comes from its place in the upload, so
//! that the block list orders the parts the same way S3 does.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::{Bytes, BytesMut};

use super::cloud::Part;

/// Bytes per part of a Put Blob body.
pub const PART_SIZE: usize = 64 << 20;
/// The largest block taken. Blocks are held in memory until S3 has them.
pub const MAX_BLOCK: usize = 256 << 20;
/// The highest S3 part number.
pub const MAX_PART: u16 = 10_000;

/// What an Azure request on an upload URL asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    /// Put Blob: the whole blob in one body.
    Blob,
    /// Put Block: one block, named by its base64 id.
    Block {
        /// The block id as the client sent it.
        id: String,
    },
    /// Put Block List: the ordered list of blocks that makes the blob.
    BlockList,
}

/// An Azure error, sent as its status, `x-ms-error-code` and XML body.
#[derive(Debug, PartialEq, Eq)]
pub struct Error {
    /// HTTP status.
    pub status: u16,
    /// The Azure error code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl Error {
    /// A new error.
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// The XML body Azure clients parse.
    #[must_use]
    pub fn xml(&self) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{}</Code><Message>{}</Message></Error>",
            self.code,
            escape(&self.message)
        )
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The operation a `PUT` with this query asks for.
///
/// # Errors
///
/// For a query the proxy does not serve.
pub fn operation(query: Option<&str>) -> Result<Operation, Error> {
    let mut comp = None;
    let mut block_id = None;
    for (name, value) in reqwest::Url::parse(&format!("http://x/?{}", query.unwrap_or_default()))
        .map_err(|_| Error::new(400, "InvalidQueryParameterValue", "bad query"))?
        .query_pairs()
    {
        match name.as_ref() {
            "comp" => comp = Some(value.into_owned()),
            // Base64 has no spaces: a `+` the client left unescaped
            // decodes to one.
            "blockid" => block_id = Some(value.replace(' ', "+")),
            _ => {}
        }
    }
    match (comp.as_deref(), block_id) {
        (None, _) => Ok(Operation::Blob),
        (Some("block"), Some(id)) if !id.is_empty() => Ok(Operation::Block { id }),
        (Some("blocklist"), _) => Ok(Operation::BlockList),
        (Some(comp), _) => Err(Error::new(
            400,
            "InvalidQueryParameterValue",
            format!("comp={comp} is not served"),
        )),
    }
}

/// The index the JavaScript Azure SDK numbers a block id with. It
/// encodes a prefix of at most 42 characters, a UUID for the toolkit,
/// and the index zero-padded to the rest of 48, so the decoded id always
/// ends in six digits of the index. Only the last six are read: the
/// prefix may end in digits too.
#[must_use]
pub fn block_index(id: &str) -> Option<u32> {
    let decoded = STANDARD.decode(id).ok()?;
    let digits = decoded.get(decoded.len().checked_sub(6)?..)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// The block ids of a Put Block List body, in order. Azure takes
/// `Latest`, `Uncommitted` and `Committed`; with nothing committed
/// before, all three name an uploaded block.
///
/// # Errors
///
/// For a body that is not a block list.
pub fn block_list(xml: &str) -> Result<Vec<String>, Error> {
    let bad = || Error::new(400, "InvalidXmlDocument", "the block list is not valid XML");
    if !xml.contains("<BlockList") {
        return Err(bad());
    }
    let mut ids = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>').ok_or_else(bad)?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if matches!(tag, "Latest" | "Uncommitted" | "Committed") {
            let close = format!("</{tag}>");
            let end = rest.find(&close).ok_or_else(bad)?;
            ids.push(rest[..end].trim().to_owned());
            rest = &rest[end + close.len()..];
        }
    }
    Ok(ids)
}

/// Cuts a stream into parts of [`PART_SIZE`] bytes.
pub struct Chunker {
    buffer: BytesMut,
    size: usize,
}

impl Chunker {
    /// A chunker for parts of `size` bytes.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            size,
        }
    }

    /// Adds `data` and returns the parts it completed.
    pub fn push(&mut self, mut data: Bytes) -> Vec<Bytes> {
        let mut parts = Vec::new();
        while !data.is_empty() {
            if self.buffer.capacity() == 0 {
                self.buffer.reserve(self.size);
            }
            let take = data.len().min(self.size - self.buffer.len());
            self.buffer.extend_from_slice(&data.split_to(take));
            if self.buffer.len() == self.size {
                parts.push(self.buffer.split().freeze());
            }
        }
        parts
    }

    /// The last part, shorter than the rest. An empty stream is one
    /// empty part, since a multipart upload needs at least one.
    pub fn finish(self, first: bool) -> Option<Bytes> {
        (first || !self.buffer.is_empty()).then(|| self.buffer.freeze())
    }
}

/// How the blocks of an upload become parts, settled by its first
/// block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The block ids carry their index, as the JavaScript Azure SDK's
    /// do. The toolkit sends 64 MiB blocks, eight at a time and out of
    /// order; each becomes the part numbered after its index.
    Indexed,
    /// The ids carry no index, as the Go Azure SDK's. `BuildKit` sends
    /// 1 MiB blocks, below the S3 part minimum, one at a time and in
    /// order; they are joined in arrival order into [`PART_SIZE`]
    /// parts.
    Sequential,
}

/// The blocks of one upload and the parts they became.
pub struct Blocks {
    mode: Option<Mode>,
    /// Indexed: the part each block was uploaded as.
    parts: HashMap<String, Part>,
    numbers: HashMap<u16, String>,
    /// Sequential: the blocks in arrival order, the bytes not yet in a
    /// part, and the parts so far.
    order: Vec<String>,
    chunker: Chunker,
    joined: Vec<Part>,
    next: u16,
}

impl Default for Blocks {
    fn default() -> Self {
        Self::with_part_size(PART_SIZE)
    }
}

/// A committed block list.
#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    /// The parts uploaded so far, in order.
    pub parts: Vec<Part>,
    /// For sequential blocks, the last part, still to upload. Its
    /// [`Part`] goes after the others.
    pub tail: Option<(u16, Bytes)>,
}

/// Where a block goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Placement {
    /// Upload it as this part, then call [`Blocks::uploaded`].
    Part(u16),
    /// Append it with [`Blocks::append`].
    Append,
}

impl Blocks {
    /// Blocks joined into parts of `size` bytes when they arrive in
    /// order.
    #[must_use]
    pub fn with_part_size(size: usize) -> Self {
        Self {
            mode: None,
            parts: HashMap::new(),
            numbers: HashMap::new(),
            order: Vec::new(),
            chunker: Chunker::new(size),
            joined: Vec::new(),
            next: 0,
        }
    }

    /// Where block `id` goes. An indexed block is the part numbered its
    /// index plus one.
    ///
    /// # Errors
    ///
    /// When the number is past [`MAX_PART`], another block has it, or
    /// the id does not match the upload's other ids.
    pub fn place(&mut self, id: &str) -> Result<Placement, Error> {
        let index = block_index(id);
        let mode = if index.is_some() {
            Mode::Indexed
        } else {
            Mode::Sequential
        };
        if *self.mode.get_or_insert(mode) != mode {
            return Err(Error::new(
                400,
                "InvalidBlockId",
                "the upload mixes block ids with and without an index",
            ));
        }
        let Some(index) = index else {
            return Ok(Placement::Append);
        };
        if let Some(part) = self.parts.get(id) {
            return Ok(Placement::Part(part.number));
        }
        let number = u16::try_from(index + 1)
            .ok()
            .filter(|number| *number <= MAX_PART)
            .ok_or_else(too_many)?;
        match self.numbers.get(&number) {
            Some(other) if other != id => Err(Error::new(
                400,
                "InvalidBlockId",
                format!("blocks {other} and {id} both map to part {number}"),
            )),
            _ => {
                self.numbers.insert(number, id.to_owned());
                Ok(Placement::Part(number))
            }
        }
    }

    /// Records that indexed block `id` was uploaded as `part`. A block
    /// sent again replaces the first.
    pub fn uploaded(&mut self, id: &str, part: Part) {
        self.parts.insert(id.to_owned(), part);
    }

    /// Appends sequential block `id` and returns the parts it filled,
    /// numbered, to upload and then pass to [`Blocks::joined`]. A block
    /// sent twice in a row, as a retry, is appended once.
    ///
    /// # Errors
    ///
    /// When the parts run past [`MAX_PART`].
    pub fn append(&mut self, id: &str, data: Bytes) -> Result<Vec<(u16, Bytes)>, Error> {
        if self.order.last().is_some_and(|last| last == id) {
            return Ok(Vec::new());
        }
        self.order.push(id.to_owned());
        self.chunker
            .push(data)
            .into_iter()
            .map(|chunk| Ok((self.number()?, chunk)))
            .collect()
    }

    fn number(&mut self) -> Result<u16, Error> {
        if self.next >= MAX_PART {
            return Err(too_many());
        }
        self.next += 1;
        Ok(self.next)
    }

    /// Records a part [`Blocks::append`] or [`Blocks::commit`] handed
    /// out.
    pub fn joined(&mut self, part: Part) {
        self.joined.push(part);
    }

    /// Commits the block list.
    ///
    /// S3 joins parts by number, so indexed blocks must be listed in
    /// increasing order, and sequential blocks exactly in the order
    /// they arrived, each once.
    ///
    /// # Errors
    ///
    /// When a block was never uploaded or the order differs from the
    /// parts' order.
    pub fn commit(&mut self, list: &[String]) -> Result<Commit, Error> {
        let reordered = || {
            Error::new(
                400,
                "InvalidBlockList",
                "the block list orders blocks differently from their upload",
            )
        };
        if list.is_empty() {
            return Err(Error::new(
                400,
                "InvalidBlockList",
                "the block list is empty",
            ));
        }
        if self.mode == Some(Mode::Sequential) {
            if list != self.order.as_slice() {
                return Err(reordered());
            }
            let chunker = std::mem::replace(&mut self.chunker, Chunker::new(0));
            let tail = match chunker.finish(self.next == 0) {
                Some(chunk) => Some((self.number()?, chunk)),
                None => None,
            };
            let mut parts = std::mem::take(&mut self.joined);
            parts.sort_by_key(|part| part.number);
            return Ok(Commit { parts, tail });
        }
        let mut parts: Vec<Part> = Vec::with_capacity(list.len());
        for id in list {
            let part = self.parts.get(id).ok_or_else(|| {
                Error::new(
                    400,
                    "InvalidBlockList",
                    format!("block {id} was not uploaded"),
                )
            })?;
            if parts.last().is_some_and(|last| last.number >= part.number) {
                return Err(reordered());
            }
            parts.push(part.clone());
        }
        Ok(Commit { parts, tail: None })
    }
}

fn too_many() -> Error {
    Error::new(
        400,
        "BlockCountExceedsLimit",
        format!("an upload holds at most {MAX_PART} parts"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block id the way @azure/storage-blob's `generateBlockID` makes
    /// one: a prefix and the index padded to 48 characters, base64.
    fn azure_id(index: u32) -> String {
        let prefix = "9f6c1a52-7b3e-4f7e-9d1f-3c2b1a0f9e8d";
        STANDARD.encode(format!(
            "{prefix}{index:0>width$}",
            width = 48 - prefix.len()
        ))
    }

    fn part(number: u16) -> Part {
        Part {
            number,
            etag: format!("\"e{number}\""),
        }
    }

    #[test]
    fn reads_operations_from_the_query() {
        assert_eq!(operation(None), Ok(Operation::Blob));
        assert_eq!(operation(Some("")), Ok(Operation::Blob));
        assert_eq!(
            operation(Some("comp=block&blockid=YWJj%3D%3D")),
            Ok(Operation::Block {
                id: "YWJj==".into()
            })
        );
        assert_eq!(
            operation(Some("comp=block&blockid=a+b%2Bc")),
            Ok(Operation::Block { id: "a+b+c".into() })
        );
        assert_eq!(operation(Some("comp=blocklist")), Ok(Operation::BlockList));
        assert_eq!(operation(Some("comp=block")).unwrap_err().status, 400);
        assert_eq!(operation(Some("comp=appendblock")).unwrap_err().status, 400);
    }

    #[test]
    fn azure_block_ids_carry_their_index() {
        assert_eq!(block_index(&azure_id(0)), Some(0));
        assert_eq!(block_index(&azure_id(41)), Some(41));
        assert_eq!(block_index(&STANDARD.encode("no-index")), None);
        assert_eq!(block_index(&STANDARD.encode("12345")), None);
        // A UUID prefix that ends in digits.
        let prefix = "9f6c1a52-7b3e-4f7e-9d1f-3c2b1a0f9e87";
        assert_eq!(
            block_index(&STANDARD.encode(format!("{prefix}{:012}", 3))),
            Some(3)
        );
        assert_eq!(block_index("not base64!"), None);
    }

    #[test]
    fn parses_a_block_list() {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<BlockList>\
             <Latest>{}</Latest><Latest>{}</Latest><Uncommitted> {} </Uncommitted></BlockList>",
            azure_id(0),
            azure_id(1),
            azure_id(2)
        );
        assert_eq!(
            block_list(&xml).unwrap(),
            [azure_id(0), azure_id(1), azure_id(2)]
        );
        assert!(block_list("<BlockList><Latest>x</BlockList>").is_err());
        assert!(block_list("not xml").is_err());
    }

    #[test]
    fn cuts_a_stream_into_parts() {
        let mut chunker = Chunker::new(4);
        assert_eq!(chunker.push(Bytes::from_static(b"ab")), Vec::<Bytes>::new());
        assert_eq!(
            chunker.push(Bytes::from_static(b"cdefghij")),
            [Bytes::from_static(b"abcd"), Bytes::from_static(b"efgh")]
        );
        assert_eq!(chunker.finish(false), Some(Bytes::from_static(b"ij")));

        let mut chunker = Chunker::new(4);
        assert_eq!(chunker.push(Bytes::from_static(b"abcd")).len(), 1);
        assert_eq!(chunker.finish(false), None);
        assert_eq!(Chunker::new(4).finish(true), Some(Bytes::new()));
    }

    /// Places an indexed block and records it as uploaded.
    fn upload(blocks: &mut Blocks, id: &str) -> u16 {
        let Placement::Part(number) = blocks.place(id).unwrap() else {
            panic!("{id} is not indexed");
        };
        blocks.uploaded(id, part(number));
        number
    }

    /// A block id the way the Go Azure SDK makes one: a UUID, the block
    /// number big-endian, zero padding to 64 bytes, base64.
    fn go_id(number: u32) -> String {
        let mut raw = [0u8; 64];
        raw[..16].copy_from_slice(&[0xab; 16]);
        raw[16..20].copy_from_slice(&number.to_be_bytes());
        STANDARD.encode(raw)
    }

    #[test]
    fn indexed_blocks_that_arrive_out_of_order_join_in_list_order() {
        let mut blocks = Blocks::default();
        for index in [2, 0, 1] {
            upload(&mut blocks, &azure_id(index));
        }
        let list = [azure_id(0), azure_id(1), azure_id(2)];
        assert_eq!(
            blocks.commit(&list).unwrap(),
            Commit {
                parts: vec![part(1), part(2), part(3)],
                tail: None
            }
        );
    }

    #[test]
    fn indexed_blocks_listed_out_of_order_fail() {
        let mut blocks = Blocks::default();
        upload(&mut blocks, &azure_id(0));
        upload(&mut blocks, &azure_id(1));
        assert_eq!(
            blocks.commit(&[azure_id(1), azure_id(0)]).unwrap_err().code,
            "InvalidBlockList"
        );
    }

    #[test]
    fn an_indexed_block_sent_again_keeps_its_part() {
        let mut blocks = Blocks::default();
        assert_eq!(upload(&mut blocks, &azure_id(0)), 1);
        assert_eq!(blocks.place(&azure_id(0)).unwrap(), Placement::Part(1));
    }

    #[test]
    fn a_block_list_naming_a_missing_block_fails() {
        let mut blocks = Blocks::default();
        upload(&mut blocks, &azure_id(0));
        assert!(blocks.commit(&[azure_id(0), azure_id(1)]).is_err());
        assert!(blocks.commit(&[]).is_err());
    }

    #[test]
    fn too_many_indexed_blocks_fail() {
        let mut blocks = Blocks::default();
        assert_eq!(
            blocks.place(&azure_id(9_999)).unwrap(),
            Placement::Part(MAX_PART)
        );
        assert_eq!(
            blocks.place(&azure_id(10_000)).unwrap_err().code,
            "BlockCountExceedsLimit"
        );
    }

    #[test]
    fn go_block_ids_carry_no_index() {
        assert_eq!(block_index(&go_id(7)), None);
        assert_eq!(go_id(7).len(), 88);
    }

    #[test]
    fn sequential_blocks_join_into_parts() {
        let mut blocks = Blocks::with_part_size(4);
        let ids: Vec<String> = (0..4).map(go_id).collect();
        for id in &ids {
            assert_eq!(blocks.place(id).unwrap(), Placement::Append);
        }
        assert_eq!(
            blocks.append(&ids[0], Bytes::from_static(b"ab")).unwrap(),
            []
        );
        let full = blocks.append(&ids[1], Bytes::from_static(b"cd")).unwrap();
        assert_eq!(full, [(1, Bytes::from_static(b"abcd"))]);
        blocks.joined(part(1));
        // A retry of the block just appended adds nothing.
        assert_eq!(
            blocks.append(&ids[1], Bytes::from_static(b"cd")).unwrap(),
            []
        );
        let full = blocks
            .append(&ids[2], Bytes::from_static(b"efghi"))
            .unwrap();
        assert_eq!(full, [(2, Bytes::from_static(b"efgh"))]);
        blocks.joined(part(2));
        assert_eq!(
            blocks.append(&ids[3], Bytes::from_static(b"j")).unwrap(),
            []
        );

        assert_eq!(
            blocks.commit(&ids).unwrap(),
            Commit {
                parts: vec![part(1), part(2)],
                tail: Some((3, Bytes::from_static(b"ij"))),
            }
        );
    }

    #[test]
    fn sequential_blocks_that_fill_their_parts_leave_no_last_part() {
        let mut blocks = Blocks::with_part_size(2);
        let id = go_id(0);
        blocks.place(&id).unwrap();
        assert_eq!(
            blocks.append(&id, Bytes::from_static(b"ab")).unwrap().len(),
            1
        );
        blocks.joined(part(1));
        assert_eq!(
            blocks.commit(&[id]).unwrap(),
            Commit {
                parts: vec![part(1)],
                tail: None
            }
        );
    }

    #[test]
    fn sequential_blocks_listed_in_another_order_fail() {
        let mut blocks = Blocks::with_part_size(4);
        for id in [go_id(0), go_id(1)] {
            blocks.place(&id).unwrap();
            blocks.append(&id, Bytes::from_static(b"x")).unwrap();
        }
        assert!(blocks.commit(&[go_id(1), go_id(0)]).is_err());
        assert!(blocks.commit(&[go_id(0)]).is_err());
    }

    #[test]
    fn an_upload_cannot_mix_id_kinds() {
        let mut blocks = Blocks::default();
        upload(&mut blocks, &azure_id(0));
        assert_eq!(blocks.place(&go_id(1)).unwrap_err().code, "InvalidBlockId");
    }

    #[test]
    fn errors_are_azure_xml() {
        let err = Error::new(400, "InvalidBlockList", "a < b");
        assert_eq!(
            err.xml(),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>InvalidBlockList</Code>\
             <Message>a &lt; b</Message></Error>"
        );
    }
}
