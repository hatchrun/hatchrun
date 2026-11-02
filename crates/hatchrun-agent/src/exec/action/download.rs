// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Downloading an action archive into the cache.

use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use hatchrun_workflow::job::Github;
use tokio::io::AsyncWriteExt;

use super::super::archive::extract_tar_gz;
use super::super::builtin::{self, mode_bits, remove_tree, to_mode};
use super::super::pack::{kind_of, open_dir};
use super::super::random_name;
use super::{
    ATTEMPT_TIMEOUT, COMPLETE, CONNECT_TIMEOUT, CacheDir, Limits, READ_TIMEOUT, RETRIES, TREE,
};

/// Downloads `owner/repo@git_ref` into the cache entry `key`. The
/// archive is downloaded and extracted in the stage, which only the
/// agent can write, and the extracted tree is moved into `key` by
/// descriptor, so a half-extracted action is never reused and no path
/// under the work directory is ever used to write. `key` is root's: no
/// directory above the tree is ever handed to the runner user.
pub(super) async fn download(
    github: &Github,
    owner: &str,
    repo: &str,
    git_ref: &str,
    key: &CacheDir,
    stage: &CacheDir,
    limits: Limits,
) -> Result<()> {
    let name = format!("action-{}", random_name().map_err(|e| anyhow!(e))?);
    nix::sys::stat::mkdirat(&*stage.fd, name.as_str(), to_mode(0o700))
        .with_context(|| format!("create a directory in {}", stage.path.display()))?;
    // Declared before the fetch, so that a dropped fetch has stopped its
    // extraction by the time this removes the entry.
    let cleanup = builtin::RemoveDir {
        stage: Arc::clone(&stage.fd),
        name: name.clone(),
        background: true,
    };
    let filled = fill(github, owner, repo, git_ref, key, stage, &name, limits).await;
    // Both outcomes clean up on a blocking thread. A dropped fetch falls
    // back to the guard's own drop, which also keeps off the worker.
    let _ = tokio::task::spawn_blocking(move || {
        let mut cleanup = cleanup;
        cleanup.background = false;
        drop(cleanup);
    })
    .await;
    filled
}

/// Downloads and extracts into the stage directory `name`, then moves
/// the tree to `key`.
#[allow(clippy::too_many_arguments)]
async fn fill(
    github: &Github,
    owner: &str,
    repo: &str,
    git_ref: &str,
    key: &CacheDir,
    stage: &CacheDir,
    name: &str,
    limits: Limits,
) -> Result<()> {
    let label = format!("{owner}/{repo}@{git_ref}");
    let work = stage.path.join(name);
    let held = open_dir(&*stage.fd, name).context("open the stage directory")?;
    let archive = work.join("download.tgz");
    let tree = work.join(TREE);

    let url = format!(
        "{}/repos/{owner}/{repo}/tarball/{git_ref}",
        github.api_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .user_agent("hatchrun-agent")
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(ATTEMPT_TIMEOUT)
        .build()
        .context("build http client")?;
    let mut pause = limits.backoff;
    let mut attempt = 0;
    loop {
        match try_download(&client, &url, &github.token, &archive, limits.compressed).await {
            Ok(()) => break,
            Err(Failure::Retry(e)) if attempt < RETRIES => {
                tracing::warn!("download of {label} failed, retrying: {e:#}");
                attempt += 1;
                tokio::time::sleep(pause).await;
                pause *= 2;
            }
            Err(Failure::Retry(e) | Failure::Fatal(e)) => {
                return Err(e.context(format!("download {label}")));
            }
        }
    }

    let (archive_path, expanded) = (archive.clone(), limits.expanded);
    let extract_into = tree.clone();
    // Dropping this future, as a cancelled job does, stops the blocking
    // extraction too.
    let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    let flag = Arc::clone(&stop.0);
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&archive_path)?;
        extract_tar_gz(file, &extract_into, 1, expanded, &flag)?;
        open_for_reading(&extract_into)?;
        anyhow::Ok(())
    })
    .await
    .context("extract task")?
    .with_context(|| format!("extract {label}"))?;
    std::fs::remove_file(&archive)?;

    // A leftover without the marker is incomplete: replace it. One with
    // the marker was finished by a concurrent job.
    if !key.is_complete() {
        remove_tree(&key.fd, OsStr::new(TREE));
        move_tree(&held, &key.fd).with_context(|| format!("move {label} into place"))?;
        // Only the tree goes to the runner user, never a directory above it.
        if let Some(owner) = limits.owner {
            let tree_fd = open_dir(&*key.fd, TREE)?;
            chown_tree(&tree_fd, owner).with_context(|| format!("chown {label}"))?;
        }
        match nix::fcntl::openat(
            &*key.fd,
            COMPLETE,
            nix::fcntl::OFlag::O_WRONLY
                | nix::fcntl::OFlag::O_CREAT
                | nix::fcntl::OFlag::O_EXCL
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            to_mode(0o644),
        ) {
            Ok(_) | Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => return Err(e).context("write the completion marker"),
        }
    }
    Ok(())
}

/// Moves the `tree` directory of `from` to `tree` in `to`. When the two
/// are on different filesystems it copies by descriptor instead.
pub(super) fn move_tree(from: &OwnedFd, to: &OwnedFd) -> Result<()> {
    match nix::fcntl::renameat(from, TREE, to, TREE) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EXDEV) => {
            let src = open_dir(from, TREE)?;
            nix::sys::stat::mkdirat(to, TREE, to_mode(0o755))?;
            let dst = open_dir(to, TREE)?;
            copy_tree(&src, &dst)
        }
        Err(e) => Err(e.into()),
    }
}

/// Copies everything in `src` into `dst`, both open directories, one
/// name at a time with `O_NOFOLLOW`. Links are copied as links.
pub(super) fn copy_tree(src: &OwnedFd, dst: &OwnedFd) -> Result<()> {
    use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
    use nix::sys::stat::{SFlag, fchmod, fstat, fstatat, mkdirat};

    fchmod(dst, to_mode(mode_bits(fstat(src)?.st_mode & 0o7777)))?;
    let mut listing = nix::dir::Dir::openat(
        src,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )?;
    let mut names = Vec::new();
    for entry in listing.iter() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(&name).to_owned());
        }
    }
    for name in names {
        let stat = fstatat(src, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
        match kind_of(stat.st_mode) {
            SFlag::S_IFDIR => {
                mkdirat(dst, name.as_os_str(), to_mode(0o755))?;
                copy_tree(&open_dir(src, &name)?, &open_dir(dst, &name)?)?;
            }
            SFlag::S_IFREG => {
                let from = openat(
                    src,
                    name.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                    nix::sys::stat::Mode::empty(),
                )?;
                let to = openat(
                    dst,
                    name.as_os_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    to_mode(0o600),
                )?;
                fchmod(&to, to_mode(mode_bits(stat.st_mode & 0o7777)))?;
                std::io::copy(&mut std::fs::File::from(from), &mut std::fs::File::from(to))?;
            }
            SFlag::S_IFLNK => {
                let target = readlinkat(src, name.as_os_str())?;
                nix::unistd::symlinkat(target.as_os_str(), dst, name.as_os_str())?;
            }
            _ => bail!(
                "{} is not a file, directory or link",
                Path::new(&name).display()
            ),
        }
    }
    Ok(())
}

/// Lets everyone read the extracted tree and enter its directories.
/// The agent extracts as root, and an archive can ship 0700 entries,
/// but the steps run as another user. Execute bits are kept as they
/// are, except that a file the owner can execute stays executable for
/// everyone who can read it. Symlinks are left alone.
pub(super) fn open_for_reading(dir: &Path) -> Result<()> {
    let widen = |path: &Path, bits: u32| -> Result<()> {
        let mode = std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | bits))
            .with_context(|| format!("set mode on {}", path.display()))
    };
    widen(dir, 0o055)?;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            open_for_reading(&entry.path())?;
        } else if kind.is_file() {
            let owner_exec = entry.metadata()?.permissions().mode() & 0o100 != 0;
            widen(&entry.path(), if owner_exec { 0o055 } else { 0o044 })?;
        }
    }
    Ok(())
}

/// Hands the open directory `dir` and everything under it to `owner`.
/// The walk goes one name at a time through descriptors with
/// `O_NOFOLLOW`, so a runner process that swaps a subdirectory for a
/// link while it runs cannot redirect it. Links are handed over
/// themselves, never what they point at, and are not walked into.
pub(super) fn chown_tree(dir: &OwnedFd, owner: (nix::unistd::Uid, nix::unistd::Gid)) -> Result<()> {
    use nix::fcntl::{AtFlags, OFlag};
    use nix::sys::stat::{Mode, SFlag, fstatat};

    let give = |parent: &OwnedFd, name: &OsStr| {
        nix::unistd::fchownat(
            parent,
            name,
            Some(owner.0),
            Some(owner.1),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .with_context(|| format!("chown {}", Path::new(name).display()))
    };
    nix::unistd::fchown(dir, Some(owner.0), Some(owner.1)).context("chown the tree")?;
    let mut listing = nix::dir::Dir::openat(
        dir,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .context("read the tree")?;
    let mut names = Vec::new();
    for entry in listing.iter() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(&name).to_owned());
        }
    }
    for name in names {
        let stat = fstatat(dir, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
        if kind_of(stat.st_mode) == SFlag::S_IFDIR {
            // Fails when the name no longer is a directory, which is
            // all a swap can achieve.
            let child = open_dir(dir, &name)
                .with_context(|| format!("open {}", Path::new(&name).display()))?;
            chown_tree(&child, owner)?;
        } else {
            give(dir, &name)?;
        }
    }
    Ok(())
}

/// Sets its flag when dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

enum Failure {
    /// Worth another try: 5xx, connection or body errors.
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

/// One attempt: streams the response body to `archive`, counting bytes.
async fn try_download(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    archive: &Path,
    cap: u64,
) -> Result<(), Failure> {
    let mut req = client.get(url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    let mut resp = req.send().await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            Failure::Retry(e.into())
        } else {
            Failure::Fatal(e.into())
        }
    })?;
    let status = resp.status();
    if status.is_server_error() {
        return Err(Failure::Retry(anyhow!("server answered {status}")));
    }
    if !status.is_success() {
        return Err(Failure::Fatal(anyhow!("server answered {status}")));
    }
    let mut file = tokio::fs::File::create(archive)
        .await
        .map_err(|e| Failure::Fatal(e.into()))?;
    let mut read: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| Failure::Retry(e.into()))? {
        read += chunk.len() as u64;
        if read > cap {
            return Err(Failure::Fatal(anyhow!(
                "response is larger than the {cap} byte limit"
            )));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| Failure::Fatal(e.into()))?;
    }
    file.flush().await.map_err(|e| Failure::Fatal(e.into()))?;
    Ok(())
}
