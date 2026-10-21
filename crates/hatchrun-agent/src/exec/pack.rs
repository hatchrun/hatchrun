// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Archives workspace files by walking directory descriptors. The agent
//! may run as root over a tree that steps and background processes can
//! change, so nothing is opened by path: every component is opened with
//! `O_NOFOLLOW` from the directory above it, regular files are checked
//! with `fstat` after opening, and symlinks are stored as links.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use tar::{Builder, EntryType, Header};

/// What a walk stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Everything: files, directories and links.
    All,
    /// `*.xml` files only, in any directory below. Links are skipped.
    TestResults,
}

/// Opens the directory `name` in `parent`, never through a link.
pub fn open_dir(parent: &impl std::os::fd::AsFd, name: impl AsRef<Path>) -> nix::Result<OwnedFd> {
    openat(
        parent,
        name.as_ref(),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// The file type bits of a `st_mode`.
pub fn kind_of(mode: nix::libc::mode_t) -> SFlag {
    SFlag::from_bits_truncate(mode) & SFlag::S_IFMT
}

/// Opens the directory at `parts` below `root`, one component at a time.
///
/// # Errors
///
/// When a component is missing, is a link, or is not a directory.
pub fn walk(root: &OwnedFd, parts: &[OsString]) -> Result<OwnedFd> {
    let mut dir = root
        .try_clone()
        .context("duplicate a directory descriptor")?;
    let mut shown = PathBuf::new();
    for part in parts {
        shown.push(part);
        dir = open_dir(&dir, part).map_err(|err| match err {
            Errno::ENOENT => anyhow!("{} does not exist", shown.display()),
            _ => anyhow!("{} is a symlink or not a directory", shown.display()),
        })?;
    }
    Ok(dir)
}

/// Fails reads once `cancel` is set.
pub struct CancelRead<'a, R> {
    /// The reader underneath.
    pub inner: R,
    /// Set when the step was cancelled.
    pub cancel: &'a AtomicBool,
}

impl<R: Read> Read for CancelRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

/// Writes a gzipped tar of `paths` below `root` to `dest` and returns
/// how many files and links it holds. A path with no components means
/// everything in `root`. Archive names are the paths as given.
///
/// # Errors
///
/// When a path is missing, a component is a link, something is not a
/// file, directory or link, or `cancel` is set.
pub fn pack(
    root: &OwnedFd,
    paths: &[Vec<OsString>],
    filter: Filter,
    dest: impl Write,
    cancel: &AtomicBool,
) -> Result<u64> {
    let mut builder = Builder::new(GzEncoder::new(dest, Compression::default()));
    builder.follow_symlinks(false);
    let mut packer = Packer {
        builder,
        cancel,
        filter,
        files: 0,
    };
    for parts in paths {
        packer.add(root, parts)?;
    }
    let Packer { builder, files, .. } = packer;
    builder
        .into_inner()
        .context("finish the archive")?
        .finish()
        .context("finish compression")?;
    Ok(files)
}

struct Packer<'a, W: Write> {
    builder: Builder<W>,
    cancel: &'a AtomicBool,
    filter: Filter,
    files: u64,
}

impl<W: Write> Packer<'_, W> {
    fn add(&mut self, root: &OwnedFd, parts: &[OsString]) -> Result<()> {
        let Some((last, parents)) = parts.split_last() else {
            return self.children(root, Path::new(""));
        };
        let dir = walk(root, parents)?;
        let name: PathBuf = parts.iter().collect();
        self.entry(&dir, last, &name)
    }

    fn children(&mut self, dir: &OwnedFd, prefix: &Path) -> Result<()> {
        let mut listing = Dir::openat(
            dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("read {}", prefix.display()))?;
        let mut names = Vec::new();
        for entry in listing.iter() {
            let entry = entry.with_context(|| format!("read {}", prefix.display()))?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
        names.sort();
        for name in names {
            self.entry(dir, &name, &prefix.join(&name))?;
        }
        Ok(())
    }

    fn entry(&mut self, dir: &OwnedFd, name: &OsStr, archive: &Path) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let stat = fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)
            .with_context(|| format!("stat {}", archive.display()))?;
        match kind_of(stat.st_mode) {
            SFlag::S_IFDIR => {
                let fd = open_dir(dir, name)
                    .map_err(|_| anyhow!("{} changed while it was archived", archive.display()))?;
                if self.filter == Filter::All {
                    let file = File::from(fd.try_clone()?);
                    let mut header = Header::new_gnu();
                    header.set_metadata(&file.metadata()?);
                    self.builder
                        .append_data(&mut header, archive, io::empty())
                        .with_context(|| format!("add {}", archive.display()))?;
                }
                self.children(&fd, archive)
            }
            SFlag::S_IFREG => {
                if self.filter == Filter::TestResults && !is_xml(archive) {
                    return Ok(());
                }
                // Not blocking, so that a file swapped for a FIFO cannot
                // hang the open; it fails the regular file check below.
                let fd = openat(
                    dir,
                    name,
                    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| anyhow!("{} changed while it was archived", archive.display()))?;
                let file = File::from(fd);
                let meta = file.metadata()?;
                if !meta.is_file() {
                    bail!("{} changed while it was archived", archive.display());
                }
                let mut header = Header::new_gnu();
                header.set_metadata(&meta);
                let data = CancelRead {
                    inner: file.take(meta.len()),
                    cancel: self.cancel,
                };
                self.builder
                    .append_data(&mut header, archive, data)
                    .with_context(|| format!("add {}", archive.display()))?;
                self.files += 1;
                Ok(())
            }
            SFlag::S_IFLNK if self.filter == Filter::All => {
                let target = readlinkat(dir, name)
                    .with_context(|| format!("read link {}", archive.display()))?;
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_mtime(u64::try_from(stat.st_mtime).unwrap_or(0));
                self.builder
                    .append_link(&mut header, archive, &target)
                    .with_context(|| format!("add {}", archive.display()))?;
                self.files += 1;
                Ok(())
            }
            _ if self.filter == Filter::TestResults => Ok(()),
            _ => bail!("{} is not a file, directory or symlink", archive.display()),
        }
    }
}

fn is_xml(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use flate2::read::GzDecoder;
    use tar::Archive;

    use super::*;
    use crate::exec::archive::extract_tar_gz;

    fn root_fd(path: &Path) -> OwnedFd {
        open_dir(&nix::fcntl::AT_FDCWD, path).unwrap()
    }

    fn parts(path: &str) -> Vec<OsString> {
        Path::new(path)
            .components()
            .map(|c| c.as_os_str().to_owned())
            .collect()
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
        let none = AtomicBool::new(false);
        let n = pack(
            &root_fd(&root),
            &[parts("d"), parts("top.txt")],
            Filter::All,
            &mut out,
            &none,
        )
        .unwrap();
        assert_eq!(n, 3);
        let dest = t.path().join("out");
        extract_tar_gz(&out[..], &dest, 0, 1 << 20).unwrap();
        assert_eq!(fs::read(dest.join("d/e/f.txt")).unwrap(), b"data");
        assert_eq!(fs::read(dest.join("top.txt")).unwrap(), b"top");
        assert_eq!(
            fs::read_link(dest.join("d/link")).unwrap(),
            Path::new("e/f.txt")
        );
    }

    #[test]
    fn components_are_never_followed() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let outside = t.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("f"), "o").unwrap();
        symlink(&outside, root.join("via")).unwrap();
        let fd = root_fd(&root);
        let none = AtomicBool::new(false);
        for p in ["via/f", "missing"] {
            let r = pack(&fd, &[parts(p)], Filter::All, Vec::new(), &none);
            assert!(r.is_err(), "{p} should fail");
        }
        // A link at the end is stored as a link.
        let mut out = Vec::new();
        pack(&fd, &[parts("via")], Filter::All, &mut out, &none).unwrap();
        let mut a = Archive::new(GzDecoder::new(&out[..]));
        let e = a.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(e.header().entry_type(), EntryType::Symlink);
    }

    #[test]
    fn a_set_cancel_flag_stops_the_walk() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("f"), "x").unwrap();
        let yes = AtomicBool::new(true);
        let r = pack(
            &root_fd(t.path()),
            &[Vec::new()],
            Filter::All,
            Vec::new(),
            &yes,
        );
        assert!(r.is_err());
    }
}
