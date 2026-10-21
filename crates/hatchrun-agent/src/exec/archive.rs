// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Safe `tar.gz` extraction and creation. The agent may run as root, so
//! extraction never follows or creates a link that leaves the
//! destination and never takes ownership or setuid bits from the
//! archive.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tar::{Archive, Builder, EntryType};

/// How many symlinks one path may pass through before it is treated as
/// a loop.
const MAX_LINK_DEPTH: usize = 40;

/// Extracts the gzipped tar in `src` into `dest`, dropping the first
/// `strip` path components of every entry. `max_bytes` caps the sum of
/// the entry sizes written.
///
/// Absolute paths, `..` components, hard links, devices and FIFOs are
/// errors. Symlinks are kept when they resolve inside `dest`. Files
/// get the archive's permission bits minus setuid, setgid and sticky,
/// and are owned by the extracting user.
pub fn extract_tar_gz(src: impl Read, dest: &Path, strip: usize, max_bytes: u64) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let dest = dest
        .canonicalize()
        .with_context(|| format!("resolve {}", dest.display()))?;
    let mut archive = Archive::new(GzDecoder::new(src));
    let mut written: u64 = 0;
    // Symlinks are created after every file and directory, so no later
    // entry can be written through one.
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();

    for entry in archive.entries().context("read archive")? {
        let mut entry = entry.context("read archive entry")?;
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
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                let size = entry.header().size().context("read entry size")?;
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
            other => bail!("entry {} has unsupported type {other:?}", rel.display()),
        }
    }

    for (rel, target) in &links {
        let path = dest.join(rel);
        if fs::symlink_metadata(&path).is_ok() {
            bail!("symlink {} collides with another entry", rel.display());
        }
        symlink(target, &path).with_context(|| format!("create symlink {}", rel.display()))?;
    }
    for (rel, _) in &links {
        check_link(&dest, rel)?;
    }
    Ok(())
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

/// Resolves the symlink at `rel` by hand, through every other symlink
/// it meets, and fails when the result leaves `dest`.
fn check_link(dest: &Path, rel: &Path) -> Result<()> {
    // Components still to walk, relative to the link's directory.
    let mut dir: Vec<std::ffi::OsString> = rel
        .parent()
        .map(|p| p.components().map(|c| c.as_os_str().to_owned()).collect())
        .unwrap_or_default();
    let target = fs::read_link(dest.join(rel))?;
    let mut todo: Vec<std::ffi::OsString> = target
        .components()
        .rev()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut hops = 0;
    while let Some(part) = todo.pop() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if dir.pop().is_none() {
                bail!("symlink {} points outside the archive", rel.display());
            }
            continue;
        }
        dir.push(part);
        let mut full = dest.to_owned();
        full.extend(&dir);
        if let Ok(meta) = fs::symlink_metadata(&full)
            && meta.file_type().is_symlink()
        {
            hops += 1;
            if hops > MAX_LINK_DEPTH {
                bail!("symlink {} loops", rel.display());
            }
            let next = fs::read_link(&full)?;
            if next.has_root() {
                bail!("symlink {} points outside the archive", rel.display());
            }
            dir.pop();
            todo.extend(next.components().rev().map(|c| c.as_os_str().to_owned()));
        }
    }
    Ok(())
}

/// Writes a gzipped tar of `paths` (relative to `root`) to `dest` and
/// returns the number of bytes written. Directories are added with
/// their contents. Symlinks are stored as symlinks. Every path must
/// resolve inside `root`.
// Used by the cache and workspace steps in a later task.
#[allow(dead_code)]
pub fn create_tar_gz(root: &Path, paths: &[String], dest: impl Write) -> Result<u64> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolve {}", root.display()))?;
    let counter = Counter {
        inner: dest,
        count: 0,
    };
    let mut builder = Builder::new(GzEncoder::new(counter, Compression::default()));
    builder.follow_symlinks(false);
    for p in paths {
        let rel = Path::new(p);
        if rel.has_root() {
            bail!("{p} is an absolute path");
        }
        let full = resolve_inside(&root, rel).with_context(|| format!("path {p}"))?;
        let name = full.strip_prefix(&root).expect("checked by resolve_inside");
        if name.as_os_str().is_empty() {
            add_dir(&mut builder, &full, Path::new(""))?;
        } else {
            add_tree(&mut builder, &full, name)?;
        }
    }
    let counter = builder
        .into_inner()
        .context("finish archive")?
        .finish()
        .context("finish compression")?;
    Ok(counter.count)
}

/// Resolves `rel` under `root` through `..` and symlinks, except for a
/// final symlink, which is stored as it is. The result is inside `root`.
// Used by the cache and workspace steps in a later task.
#[allow(dead_code)]
fn resolve_inside(root: &Path, rel: &Path) -> Result<PathBuf> {
    let joined = root.join(rel);
    let last_is_name = matches!(rel.components().next_back(), Some(Component::Normal(_)));
    let resolved = if last_is_name {
        let parent = joined.parent().context("no parent")?;
        let parent = parent
            .canonicalize()
            .with_context(|| format!("resolve {}", parent.display()))?;
        parent.join(joined.file_name().context("no file name")?)
    } else {
        joined
            .canonicalize()
            .with_context(|| format!("resolve {}", joined.display()))?
    };
    if !resolved.starts_with(root) {
        bail!("leaves {}", root.display());
    }
    fs::symlink_metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
    Ok(resolved)
}

// Used by the cache and workspace steps in a later task.
#[allow(dead_code)]
fn add_dir<W: Write>(b: &mut Builder<W>, full: &Path, name: &Path) -> Result<()> {
    if !name.as_os_str().is_empty() {
        b.append_dir(name, full)
            .with_context(|| format!("add {}", name.display()))?;
    }
    let mut kids: Vec<_> = fs::read_dir(full)
        .with_context(|| format!("read {}", full.display()))?
        .collect::<io::Result<_>>()?;
    kids.sort_by_key(std::fs::DirEntry::file_name);
    for kid in kids {
        add_tree(b, &kid.path(), &name.join(kid.file_name()))?;
    }
    Ok(())
}

// Used by the cache and workspace steps in a later task.
#[allow(dead_code)]
fn add_tree<W: Write>(b: &mut Builder<W>, full: &Path, name: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(full)?;
    if meta.is_dir() {
        add_dir(b, full, name)
    } else if meta.file_type().is_symlink() || meta.is_file() {
        b.append_path_with_name(full, name)
            .with_context(|| format!("add {}", name.display()))
    } else {
        bail!("{} is not a file, directory or symlink", name.display())
    }
}

/// Counts the bytes that pass through.
// Used by the cache and workspace steps in a later task.
#[allow(dead_code)]
struct Counter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for Counter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw tar entry, so tests can build paths `tar::Builder` refuses.
    struct Raw {
        path: &'static str,
        kind: u8,
        link: &'static str,
        body: &'static [u8],
        mode: u32,
    }

    fn raw(path: &'static str, kind: u8, link: &'static str, body: &'static [u8]) -> Raw {
        Raw {
            path,
            kind,
            link,
            body,
            mode: 0o644,
        }
    }

    fn tgz(entries: &[Raw]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            let mut h = [0u8; 512];
            h[..e.path.len()].copy_from_slice(e.path.as_bytes());
            h[100..107].copy_from_slice(format!("{:07o}", e.mode).as_bytes());
            h[108..115].copy_from_slice(b"0000000");
            h[116..123].copy_from_slice(b"0000000");
            h[124..135].copy_from_slice(format!("{:011o}", e.body.len()).as_bytes());
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
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&out).unwrap();
        gz.finish().unwrap()
    }

    fn extract(bytes: &[u8], dest: &Path, strip: usize, max: u64) -> Result<()> {
        extract_tar_gz(bytes, dest, strip, max)
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

    #[test]
    fn round_trips_files_dirs_and_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        fs::create_dir_all(root.join("d/e")).unwrap();
        fs::write(root.join("d/e/f.txt"), "data").unwrap();
        fs::write(root.join("top.txt"), "top").unwrap();
        symlink("e/f.txt", root.join("d/link")).unwrap();
        let mut out = Vec::new();
        let n = create_tar_gz(&root, &["d".into(), "top.txt".into()], &mut out).unwrap();
        assert_eq!(n, out.len() as u64);
        let dest = t.path().join("out");
        extract(&out, &dest, 0, 1 << 20).unwrap();
        assert_eq!(fs::read(dest.join("d/e/f.txt")).unwrap(), b"data");
        assert_eq!(fs::read(dest.join("top.txt")).unwrap(), b"top");
        assert_eq!(
            fs::read_link(dest.join("d/link")).unwrap(),
            Path::new("e/f.txt")
        );
    }

    #[test]
    fn create_refuses_paths_outside_the_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        fs::create_dir_all(root.join("in")).unwrap();
        fs::write(t.path().join("secret"), "s").unwrap();
        let outside = t.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("f"), "o").unwrap();
        symlink(&outside, root.join("via")).unwrap();
        for p in [
            "../secret",
            "in/../../secret",
            "via/f",
            "/etc/passwd",
            "missing",
        ] {
            let r = create_tar_gz(&root, &[p.to_string()], Vec::new());
            assert!(r.is_err(), "{p} should fail");
        }
        // `a/../in` stays inside.
        create_tar_gz(&root, &["in/../in".to_string()], Vec::new()).unwrap();
        // A symlink that points outside is stored as a link, not followed.
        let mut out = Vec::new();
        create_tar_gz(&root, &["via".to_string()], &mut out).unwrap();
        let mut a = Archive::new(GzDecoder::new(&out[..]));
        let e = a.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(e.header().entry_type(), EntryType::Symlink);
    }
}
