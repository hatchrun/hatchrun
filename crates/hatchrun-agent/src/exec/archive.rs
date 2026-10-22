// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Safe `tar.gz` extraction and creation. The agent may run as root, so
//! extraction never follows or creates a link that leaves the
//! destination and never takes ownership or setuid bits from the
//! archive.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

/// How many symlinks one path may pass through before it is treated as
/// a loop.
const MAX_LINK_DEPTH: usize = 40;

/// The most a GNU long-name, long-link or pax header body may hold. The
/// tar crate reads these fully into memory.
const MAX_EXTENSION: u64 = 64 << 10;

/// The longest symlink target, in bytes, that is accepted.
const MAX_LINK_TARGET: usize = 4096;

/// The most target components of all the symlinks in one archive that
/// are checked before the archive is refused.
const MAX_LINK_STEPS: usize = 1_000_000;

/// What one entry costs against the expanded budget, so that empty
/// files, directories and links are not free.
const ENTRY_COST: u64 = 512;

/// Limits for [`extract_tar_gz_with`].
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// The most the entries may expand to: file bytes plus 512 per entry.
    pub bytes: u64,
    /// The most decompressed tar bytes to read, headers and padding
    /// included.
    pub stream: u64,
    /// The most entries to read.
    pub entries: u64,
}

impl ExtractLimits {
    /// The defaults for an expanded cap of `max_bytes`: a stream of
    /// that plus 64 MiB, and a million entries.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            bytes: max_bytes,
            stream: max_bytes.saturating_add(64 << 20),
            entries: 1_000_000,
        }
    }
}

/// Extracts the gzipped tar in `src` into `dest`, dropping the first
/// `strip` path components of every entry. `max_bytes` caps the sum of
/// the entry sizes written plus 512 per entry.
///
/// Absolute paths, `..` components, hard links, sparse files, devices
/// and FIFOs are errors. Symlinks are kept when they resolve inside
/// `dest`. Files get the archive's permission bits minus setuid, setgid
/// and sticky, and are owned by the extracting user.
///
/// A symlink target over 4096 bytes is an error. Extraction ends with an
/// error soon after `cancel` is set.
///
/// On error `dest` may hold a partial tree (never a symlink). Callers
/// must discard it.
pub fn extract_tar_gz(
    src: impl Read,
    dest: &Path,
    strip: usize,
    max_bytes: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    extract_tar_gz_with(src, dest, strip, ExtractLimits::new(max_bytes), cancel)
}

/// [`extract_tar_gz`] with explicit limits.
pub fn extract_tar_gz_with(
    src: impl Read,
    dest: &Path,
    strip: usize,
    limits: ExtractLimits,
    cancel: &AtomicBool,
) -> Result<()> {
    let max_bytes = limits.bytes;
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let dest = dest
        .canonicalize()
        .with_context(|| format!("resolve {}", dest.display()))?;
    let guard = Guard {
        inner: GzDecoder::new(src),
        total: 0,
        cap: limits.stream,
        hdr: [0; 512],
        hdr_len: 0,
        body_left: 0,
    };
    let mut archive = Archive::new(guard);
    let mut written: u64 = 0;
    let mut entries: u64 = 0;
    // Symlinks are planned first and created after every file and
    // directory, so no later entry can be written through one.
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();

    for entry in archive.entries().context("read archive")? {
        let mut entry = entry.context("read archive entry")?;
        stop_if_cancelled(cancel)?;
        entries += 1;
        if entries > limits.entries {
            bail!("archive has more than {} entries", limits.entries);
        }
        written = written
            .checked_add(ENTRY_COST)
            .filter(|w| *w <= max_bytes)
            .with_context(|| format!("archive expands past the {max_bytes} byte limit"))?;
        let kind = entry.header().entry_type();
        let raw = entry.path().context("read entry path")?.into_owned();
        let Some(rel) = clean_entry_path(&raw, strip)? else {
            continue;
        };
        match kind {
            EntryType::XGlobalHeader | EntryType::XHeader => {}
            EntryType::Directory => {
                let path = make_parents(&dest, &rel)?;
                make_dir(&path)?;
                let mode = entry.header().mode().unwrap_or(0o755) & 0o777 | 0o700;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("set mode on {}", path.display()))?;
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.size();
                if size > max_bytes - written {
                    bail!("archive expands past the {max_bytes} byte limit");
                }
                let path = make_parents(&dest, &rel)?;
                if let Ok(meta) = fs::symlink_metadata(&path) {
                    if !meta.is_file() {
                        bail!("{} already exists and is not a file", rel.display());
                    }
                    fs::remove_file(&path)?;
                }
                let mode = entry.header().mode().unwrap_or(0o644) & 0o777 | 0o600;
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .open(&path)
                    .with_context(|| format!("create {}", path.display()))?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)
                    .with_context(|| format!("write {}", path.display()))?;
                written += copied;
                // The umask may have cleared bits; the archive decides.
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .context("read link target")?
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
                if target.as_os_str().len() > MAX_LINK_TARGET {
                    bail!(
                        "symlink {} has a target longer than {MAX_LINK_TARGET} bytes",
                        rel.display()
                    );
                }
                if target.as_os_str().is_empty() || target.has_root() {
                    bail!(
                        "symlink {} points outside the archive: {}",
                        rel.display(),
                        target.display()
                    );
                }
                make_parents(&dest, &rel)?;
                links.push((rel, target));
            }
            EntryType::Link => bail!("hard link {} is not allowed", rel.display()),
            EntryType::GNUSparse => bail!("sparse file {} is not allowed", rel.display()),
            other => bail!("entry {} has unsupported type {other:?}", rel.display()),
        }
    }

    create_links(&dest, &links, cancel)
}

/// Checks every link against the planned set before creating any.
fn create_links(dest: &Path, links: &[(PathBuf, PathBuf)], cancel: &AtomicBool) -> Result<()> {
    let mut plan = Plan::default();
    for (rel, target) in links {
        stop_if_cancelled(cancel)?;
        if fs::symlink_metadata(dest.join(rel)).is_ok() || !plan.add(rel, target) {
            bail!("symlink {} collides with another entry", rel.display());
        }
    }
    for (index, (rel, _)) in links.iter().enumerate() {
        stop_if_cancelled(cancel)?;
        plan.resolve(index, 0, cancel)
            .with_context(|| format!("symlink {}", rel.display()))?;
    }
    for (rel, target) in links {
        stop_if_cancelled(cancel)?;
        symlink(target, dest.join(rel))
            .with_context(|| format!("create symlink {}", rel.display()))?;
    }
    Ok(())
}

fn stop_if_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("extraction was cancelled");
    }
    Ok(())
}

/// Where a planned link has got to.
#[derive(Clone, Copy)]
enum Resolved {
    Pending,
    Working,
    /// The directory node the link ends at.
    Done(usize),
}

/// The planned links as a tree of path components, so that resolving a
/// link walks each component of its target once and a link met again
/// costs one lookup.
#[derive(Default)]
struct Plan<'a> {
    /// `(parent, component)` of each node. Node 0 is the destination.
    nodes: Vec<(usize, u32)>,
    names: HashMap<&'a std::ffi::OsStr, u32>,
    children: HashMap<(usize, u32), usize>,
    /// The planned link that sits at a node.
    link_at: HashMap<usize, usize>,
    /// The node and target of each planned link, in the order given.
    links: Vec<(usize, &'a Path)>,
    state: Vec<Resolved>,
    /// Target components walked so far, over all links.
    walked: usize,
}

impl<'a> Plan<'a> {
    fn child(&mut self, parent: usize, part: &'a std::ffi::OsStr) -> usize {
        let next_name = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        let name = *self.names.entry(part).or_insert(next_name);
        if self.nodes.is_empty() {
            self.nodes.push((0, 0));
        }
        let next_node = self.nodes.len();
        let node = *self.children.entry((parent, name)).or_insert(next_node);
        if node == next_node {
            self.nodes.push((parent, name));
        }
        node
    }

    /// Records the link at `rel`. False when something is there already.
    fn add(&mut self, rel: &'a Path, target: &'a Path) -> bool {
        let mut node = 0;
        for part in rel.components() {
            node = self.child(node, part.as_os_str());
        }
        let index = self.links.len();
        if self.link_at.insert(node, index).is_some() {
            return false;
        }
        self.links.push((node, target));
        self.state.push(Resolved::Pending);
        true
    }

    /// Resolves the planned link `index` by hand, through the other
    /// planned links it meets, and fails when the result leaves the
    /// destination. Returns the node it ends at.
    fn resolve(&mut self, index: usize, depth: usize, cancel: &AtomicBool) -> Result<usize> {
        match self.state[index] {
            Resolved::Done(node) => return Ok(node),
            Resolved::Working => bail!("loops"),
            Resolved::Pending => {}
        }
        if depth > MAX_LINK_DEPTH {
            bail!("loops");
        }
        self.state[index] = Resolved::Working;
        let (at, target) = self.links[index];
        let mut dir = self.nodes[at].0;
        for part in target.components() {
            self.walked += 1;
            if self.walked > MAX_LINK_STEPS {
                bail!("the symlinks in the archive are too complex to check");
            }
            if self.walked.is_multiple_of(4096) {
                stop_if_cancelled(cancel)?;
            }
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    if dir == 0 {
                        bail!("points outside the archive");
                    }
                    dir = self.nodes[dir].0;
                }
                Component::Normal(name) => {
                    dir = self.child(dir, name);
                    if let Some(&other) = self.link_at.get(&dir) {
                        dir = self.resolve(other, depth + 1, cancel)?;
                    }
                }
                Component::RootDir | Component::Prefix(_) => bail!("points outside the archive"),
            }
        }
        self.state[index] = Resolved::Done(dir);
        Ok(dir)
    }
}

/// Passes the decompressed tar through while enforcing a byte cap and
/// refusing oversized extension headers before the tar crate reads
/// their bodies into memory.
struct Guard<R> {
    inner: R,
    total: u64,
    cap: u64,
    hdr: [u8; 512],
    hdr_len: usize,
    /// Body and padding bytes of the current entry still to pass.
    body_left: u64,
}

impl<R> Guard<R> {
    /// Parses the finished header block the way the tar crate does and
    /// returns its body size. Anything the crate would allocate for
    /// before the extractor sees it is refused here.
    fn inspect(&self) -> io::Result<u64> {
        if self.hdr.iter().all(|b| *b == 0) {
            return Ok(0);
        }
        let header = tar::Header::from_byte_slice(&self.hdr);
        let size = header.entry_size()?;
        let kind = header.entry_type();
        let extension = matches!(
            kind,
            EntryType::GNULongName
                | EntryType::GNULongLink
                | EntryType::XHeader
                | EntryType::XGlobalHeader
        );
        if extension && size > MAX_EXTENSION {
            return Err(io::Error::other(format!(
                "archive has an extension header of {size} bytes, more than {MAX_EXTENSION}"
            )));
        }
        if kind == EntryType::GNUSparse || header.as_gnu().is_some_and(tar::GnuHeader::is_extended)
        {
            return Err(io::Error::other("archive has a sparse entry"));
        }
        Ok(size)
    }
}

impl<R: Read> Read for Guard<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        if self.total > self.cap {
            return Err(io::Error::other(format!(
                "archive stream is larger than the {} byte limit",
                self.cap
            )));
        }
        let mut data = &buf[..n];
        while !data.is_empty() {
            if self.body_left > 0 {
                let k = usize::try_from(self.body_left).map_or(data.len(), |b| b.min(data.len()));
                self.body_left -= k as u64;
                data = &data[k..];
                continue;
            }
            let k = (512 - self.hdr_len).min(data.len());
            self.hdr[self.hdr_len..self.hdr_len + k].copy_from_slice(&data[..k]);
            self.hdr_len += k;
            data = &data[k..];
            if self.hdr_len == 512 {
                self.hdr_len = 0;
                let size = self.inspect()?;
                self.body_left = size.div_ceil(512).saturating_mul(512);
            }
        }
        Ok(n)
    }
}

/// Validates an entry path and drops `strip` leading components.
/// Returns `None` when nothing is left.
fn clean_entry_path(raw: &Path, strip: usize) -> Result<Option<PathBuf>> {
    let mut parts = Vec::new();
    for c in raw.components() {
        match c {
            Component::Normal(p) => parts.push(p),
            Component::CurDir => {}
            _ => bail!("entry path {} leaves the destination", raw.display()),
        }
    }
    if parts.len() <= strip {
        return Ok(None);
    }
    Ok(Some(parts[strip..].iter().collect()))
}

/// Creates the parent directories of `rel` under `dest` one at a time,
/// refusing to pass through anything that is not a plain directory.
/// Returns the full path.
fn make_parents(dest: &Path, rel: &Path) -> Result<PathBuf> {
    let mut cur = dest.to_owned();
    if let Some(parent) = rel.parent() {
        for c in parent.components() {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("{} is not a directory", cur.display()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&cur).with_context(|| format!("create {}", cur.display()))?;
                    fs::set_permissions(&cur, fs::Permissions::from_mode(0o755))?;
                }
                Err(e) => return Err(e).with_context(|| format!("stat {}", cur.display())),
            }
        }
    }
    if let Some(name) = rel.file_name() {
        cur.push(name);
    }
    Ok(cur)
}

fn make_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => bail!("{} is not a directory", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("create {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use std::io::Write;

    use flate2::write::GzEncoder;

    use super::*;

    /// A raw tar entry, so tests can build paths `tar::Builder` refuses.
    struct Raw {
        path: &'static str,
        kind: u8,
        link: &'static str,
        body: &'static [u8],
        mode: u32,
        claim: Option<u64>,
    }

    fn raw(path: &'static str, kind: u8, link: &'static str, body: &'static [u8]) -> Raw {
        Raw {
            path,
            kind,
            link,
            body,
            mode: 0o644,
            claim: None,
        }
    }

    fn tgz(entries: &[Raw]) -> Vec<u8> {
        regzip(&tar_bytes(entries))
    }

    fn regzip(tar: &[u8]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(tar).unwrap();
        gz.finish().unwrap()
    }

    fn tar_bytes(entries: &[Raw]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            let mut h = [0u8; 512];
            h[..e.path.len()].copy_from_slice(e.path.as_bytes());
            h[100..107].copy_from_slice(format!("{:07o}", e.mode).as_bytes());
            h[108..115].copy_from_slice(b"0000000");
            h[116..123].copy_from_slice(b"0000000");
            h[124..135].copy_from_slice(
                format!("{:011o}", e.claim.unwrap_or(e.body.len() as u64)).as_bytes(),
            );
            h[136..147].copy_from_slice(b"00000000000");
            h[148..156].copy_from_slice(b"        ");
            h[156] = e.kind;
            h[157..157 + e.link.len()].copy_from_slice(e.link.as_bytes());
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
            h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            h[155] = b' ';
            out.extend_from_slice(&h);
            out.extend_from_slice(e.body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    fn extract(bytes: &[u8], dest: &Path, strip: usize, max: u64) -> Result<()> {
        extract_tar_gz(bytes, dest, strip, max, &AtomicBool::new(false))
    }

    fn outer() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        (t, dest)
    }

    #[test]
    fn extracts_and_strips_the_top_directory() {
        let (_t, dest) = outer();
        let bytes = tgz(&[
            raw("top/", b'5', "", b""),
            raw("top/a.txt", b'0', "", b"hi"),
            raw("top/sub/b.txt", b'0', "", b"yo"),
            raw("top/l", b'2', "sub/b.txt", b""),
        ]);
        extract(&bytes, &dest, 1, 1 << 20).unwrap();
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hi");
        assert_eq!(fs::read(dest.join("sub/b.txt")).unwrap(), b"yo");
        assert_eq!(
            fs::read_link(dest.join("l")).unwrap(),
            Path::new("sub/b.txt")
        );
    }

    #[test]
    fn rejects_entries_outside_the_root() {
        for entries in [
            vec![raw("../x", b'0', "", b"x")],
            vec![raw("/etc/x", b'0', "", b"x")],
            vec![raw("a/../../x", b'0', "", b"x")],
            vec![raw("l", b'2', "/etc", b""), raw("l/passwd", b'0', "", b"x")],
            vec![raw("l", b'2', "..", b""), raw("l/passwd", b'0', "", b"x")],
            vec![raw("h", b'1', "/etc/passwd", b"")],
            vec![raw("f", b'6', "", b"")],
            vec![raw("c", b'3', "", b"")],
        ] {
            let (t, dest) = outer();
            let err = extract(&tgz(&entries), &dest, 0, 1 << 20);
            assert!(err.is_err(), "{:?} should fail", entries[0].path);
            // Nothing was written next to dest.
            let siblings: Vec<_> = fs::read_dir(t.path()).unwrap().collect();
            assert_eq!(siblings.len(), 1, "only dest exists");
            assert!(!Path::new("/etc/x").exists());
        }
    }

    #[test]
    fn rejects_a_chain_of_links_that_escapes() {
        let (_t, dest) = outer();
        // q -> p, and s -> p/q/../.., which only resolves outside once q
        // exists.
        let bytes = tgz(&[
            raw("p/", b'5', "", b""),
            raw("p/q", b'2', "../p", b""),
            raw("s", b'2', "p/q/../..", b""),
        ]);
        assert!(extract(&bytes, &dest, 0, 1 << 20).is_err());
    }

    /// A symlink entry whose target can be longer than a tar header
    /// holds, through a GNU long-link entry.
    fn long_link(path: &str, target: &str) -> Vec<Raw> {
        let body: &'static [u8] = format!("{target}\0").into_bytes().leak();
        let path: &'static str = path.to_owned().leak();
        vec![
            raw("././@LongLink", b'K', "", body),
            raw(path, b'2', "", b""),
        ]
    }

    #[test]
    fn many_links_through_a_long_link_check_in_linear_time() {
        let (_t, dest) = outer();
        // `big` walks 400 components and ends at the root. Each other
        // link passes through it 39 times.
        let mut entries = long_link("big", &"d/../".repeat(200));
        let hops = ["big"; 39].join("/");
        for i in 0..2000 {
            entries.extend(long_link(&format!("l{i}"), &hops));
        }
        let bytes = tgz(&entries);
        let began = std::time::Instant::now();
        extract(&bytes, &dest, 0, 1 << 30).unwrap();
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
        assert!(fs::symlink_metadata(dest.join("l1999")).is_ok());
    }

    #[test]
    fn a_link_target_over_4096_bytes_is_refused() {
        let (_t, dest) = outer();
        let bytes = tgz(&long_link("l", &"a/".repeat(2049)));
        let msg = err_text(extract(&bytes, &dest, 0, 1 << 20));
        assert!(msg.contains("longer than 4096"), "{msg}");
    }

    #[test]
    fn links_with_too_many_components_in_all_are_refused() {
        let (_t, dest) = outer();
        let target = "a/".repeat(500);
        let mut entries = Vec::new();
        for i in 0..2100 {
            entries.extend(long_link(&format!("l{i}"), &target));
        }
        let msg = err_text(extract(&tgz(&entries), &dest, 0, 1 << 30));
        assert!(msg.contains("too complex"), "{msg}");
    }

    #[test]
    fn a_cancelled_extract_stops() {
        let (_t, dest) = outer();
        let bytes = tgz(&[raw("a", b'0', "", b"x"), raw("l", b'2', "a", b"")]);
        let cancel = AtomicBool::new(true);
        let msg = err_text(extract_tar_gz(&bytes[..], &dest, 0, 1 << 20, &cancel));
        assert!(msg.contains("cancelled"), "{msg}");
        assert!(fs::symlink_metadata(dest.join("l")).is_err());
    }

    fn err_text(r: Result<()>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    #[test]
    fn refuses_a_huge_extension_header_before_reading_it() {
        for kind in *b"LKxg" {
            let (_t, dest) = outer();
            let mut e = raw("././@LongLink", kind, "", b"");
            e.claim = Some(1 << 30);
            let msg = err_text(extract(&tgz(&[e]), &dest, 0, u64::MAX / 4));
            assert!(msg.contains("extension header"), "{msg}");
        }
    }

    #[test]
    fn parses_sizes_the_way_tar_does() {
        // A `+` prefix reads as octal for tar; the guard must see it too.
        let (_t, dest) = outer();
        let mut e = raw("long", b'L', "", b"");
        e.claim = Some(1 << 30);
        let mut bytes = tar_bytes(&[e]);
        let at = 124;
        bytes[at..at + 12].copy_from_slice(b"+77777777777");
        let msg = err_text(extract(&regzip(&bytes), &dest, 0, u64::MAX / 4));
        assert!(msg.contains("extension header"), "{msg}");

        // Base-256 with high bytes set must not switch inspection off.
        let (_t, dest) = outer();
        let mut first = tar_bytes(&[raw("f", b'0', "", b"")]);
        first[124] = 0x80;
        first[125] = 0xff;
        first[148..156].copy_from_slice(b"        ");
        let sum: u32 = first[..512].iter().map(|b| u32::from(*b)).sum();
        first[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut long = raw("long", b'L', "", b"");
        long.claim = Some(1 << 30);
        let second = tar_bytes(&[long]);
        let mut all = first[..512].to_vec();
        all.extend_from_slice(&second[..512]);
        // tar reads the high bytes as a huge size and skips past the end
        // of the stream; the guard sees the same size, so the extension
        // header behind it is never left unchecked and never allocated.
        assert!(extract(&regzip(&all), &dest, 0, u64::MAX / 4).is_err());
    }

    #[test]
    fn rejects_sparse_headers_in_the_guard() {
        let (_t, dest) = outer();
        let mut bytes = tar_bytes(&[raw("s", b'0', "", b"")]);
        // GNU magic and the isextended flag at offset 482.
        bytes[257..265].copy_from_slice(b"ustar  \0");
        bytes[482] = 1;
        let msg = err_text(extract(&regzip(&bytes), &dest, 0, 1 << 20));
        assert!(msg.contains("sparse"), "{msg}");
    }

    #[test]
    fn stops_at_the_stream_cap() {
        let (_t, dest) = outer();
        let bytes = tgz(&[raw("f", b'0', "", &[0u8; 4096])]);
        let limits = ExtractLimits {
            stream: 1000,
            ..ExtractLimits::new(1 << 30)
        };
        let msg = err_text(extract_tar_gz_with(
            &bytes[..],
            &dest,
            0,
            limits,
            &AtomicBool::new(false),
        ));
        assert!(msg.contains("1000 byte limit"), "{msg}");
    }

    #[test]
    fn caps_entry_count_and_charges_each_entry() {
        let (_t, dest) = outer();
        let dirs: Vec<Raw> = ["a/", "b/", "c/", "d/", "e/"]
            .iter()
            .map(|p| raw(p, b'5', "", b""))
            .collect();
        let limits = ExtractLimits {
            entries: 3,
            ..ExtractLimits::new(1 << 30)
        };
        let msg = err_text(extract_tar_gz_with(
            &tgz(&dirs)[..],
            &dest,
            0,
            limits,
            &AtomicBool::new(false),
        ));
        assert!(msg.contains("more than 3 entries"), "{msg}");

        let (_t, dest) = outer();
        let empties: Vec<Raw> = ["a", "b", "c"]
            .iter()
            .map(|p| raw(p, b'0', "", b""))
            .collect();
        let msg = err_text(extract(&tgz(&empties), &dest, 0, 1000));
        assert!(msg.contains("1000 byte limit"), "{msg}");
    }

    #[test]
    fn a_bad_link_leaves_no_links_behind() {
        let (_t, dest) = outer();
        let bytes = tgz(&[
            raw("a", b'0', "", b"x"),
            raw("ok", b'2', "a", b""),
            raw("bad", b'2', "../x", b""),
        ]);
        assert!(extract(&bytes, &dest, 0, 1 << 20).is_err());
        assert!(fs::symlink_metadata(dest.join("ok")).is_err());
        assert!(fs::symlink_metadata(dest.join("bad")).is_err());
    }

    #[test]
    fn rejects_sparse_entries() {
        let (_t, dest) = outer();
        let msg = err_text(extract(&tgz(&[raw("s", b'S', "", b"")]), &dest, 0, 1 << 20));
        assert!(msg.contains("sparse"), "{msg}");
    }

    #[test]
    fn clears_setuid_bits() {
        let (_t, dest) = outer();
        let mut e = raw("bin", b'0', "", b"x");
        e.mode = 0o4755;
        extract(&tgz(&[e]), &dest, 0, 1 << 20).unwrap();
        let mode = fs::metadata(dest.join("bin")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn stops_at_the_size_cap() {
        let (_t, dest) = outer();
        let bytes = tgz(&[raw("big", b'0', "", &[7u8; 4096])]);
        let err = extract(&bytes, &dest, 0, 1000).unwrap_err();
        assert!(err.to_string().contains("1000 byte limit"), "{err}");
        assert!(!dest.join("big").exists());
    }
}
