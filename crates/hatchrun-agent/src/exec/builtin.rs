// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `cache`, `workspace` and `test-results` steps.
//!
//! Archives are made by jobs, so they are untrusted. Restoring never
//! extracts into the live workspace: a background process may have
//! planted symlinks there. The archive is extracted into a fresh
//! directory under `RUNNER_TEMP`, then its entries are moved into place
//! one path component at a time with `openat(O_NOFOLLOW)` from a
//! descriptor of the workspace that was opened before any step ran.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use hatchrun_workflow::{CacheStep, Conclusion, JobStatus, WorkspaceStep};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AT_FDCWD, AtFlags, OFlag, openat, renameat};
use nix::sys::stat::{Mode, SFlag, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchownat, unlinkat};

use super::archive::{ExtractLimits, create_tar_gz, extract_tar_gz_with};
use super::store::{DirStore, Kind, Store, StoreKey, encode_name, validate_key};
use super::{Common, Job, Line};

/// The most one stored archive may take, compressed.
const MAX_ARCHIVE_BYTES: u64 = 4 << 30;

/// The most a restored archive may expand to.
const MAX_EXPANDED_BYTES: u64 = 16 << 30;

type Owner = Option<(Uid, Gid)>;

/// A cache that is saved when the job ends.
pub struct CacheSave {
    /// The step that asked for it.
    index: usize,
    /// The exact key, which missed at restore time.
    key: String,
    /// The paths to save, with expressions replaced.
    paths: Vec<String>,
}

fn common(name: &str) -> Common {
    Common {
        name: name.to_owned(),
        id: None,
        if_: None,
        continue_on_error: false,
    }
}

impl Job<'_> {
    /// A `cache:` step: restores now, and queues the save for the end
    /// of the job when the exact key missed.
    pub(super) async fn cache_step(
        &mut self,
        index: usize,
        step: &CacheStep,
        sink: &mut dyn super::Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let common = common("Cache");
        let outcome = if let Some(outcome) = self.begin(index, &common.name, sink) {
            outcome
        } else {
            let result = self.restore_cache(index, step).await;
            self.conclude(result, sink)
        };
        (common, outcome, BTreeMap::new())
    }

    /// A `workspace:` step.
    pub(super) async fn workspace_step(
        &mut self,
        index: usize,
        step: &WorkspaceStep,
        sink: &mut dyn super::Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let common = common("Workspace");
        let outcome = if let Some(outcome) = self.begin(index, &common.name, sink) {
            outcome
        } else {
            let result = match step {
                WorkspaceStep::Persist { root, paths } => self.persist(root, paths).await,
                WorkspaceStep::Attach { at } => self.attach(at).await,
            };
            self.conclude(result, sink)
        };
        (common, outcome, BTreeMap::new())
    }

    /// A `test-results:` step.
    pub(super) async fn test_results_step(
        &mut self,
        index: usize,
        path: &str,
        sink: &mut dyn super::Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let common = common("Test results");
        let outcome = if let Some(outcome) = self.begin(index, &common.name, sink) {
            outcome
        } else {
            let result = self.test_results(path).await;
            self.conclude(result, sink)
        };
        (common, outcome, BTreeMap::new())
    }

    /// Announces a built-in step. Returns its outcome when it does not
    /// run: skipped by its condition, or failed by a bad one.
    fn begin(
        &mut self,
        index: usize,
        name: &str,
        sink: &mut dyn super::Sink,
    ) -> Option<Conclusion> {
        sink.step_started(index, name);
        match self.runs(None) {
            Ok(true) => None,
            Ok(false) => Some(Conclusion::Skipped),
            Err(err) => {
                self.say(sink, Line::Error(&format!("if: {err}")));
                Some(Conclusion::Failure)
            }
        }
    }

    /// Prints what a step reports and maps its result to an outcome.
    fn conclude(
        &mut self,
        result: Result<Vec<String>, String>,
        sink: &mut dyn super::Sink,
    ) -> Conclusion {
        match result {
            Ok(notes) => {
                for note in notes {
                    self.say(sink, Line::Note(&note));
                }
                Conclusion::Success
            }
            Err(message) => {
                self.say(sink, Line::Error(&message));
                Conclusion::Failure
            }
        }
    }

    fn store(&self, what: &str) -> Result<DirStore, String> {
        self.opts
            .store
            .as_ref()
            .map(|root| DirStore { root: root.clone() })
            .ok_or_else(|| format!("this agent has no store for {what} steps"))
    }

    fn expand(&self, text: &str, what: &str) -> Result<String, String> {
        super::interp(text, &self.ctx, &self.layout.workspace, self.status)
            .map_err(|err| format!("{what}: {err}"))
    }

    fn expand_all(&self, items: &[String], what: &str) -> Result<Vec<String>, String> {
        items.iter().map(|item| self.expand(item, what)).collect()
    }

    /// Fails when the workspace path no longer leads to the directory
    /// the job started with, which a step could cause by swapping a
    /// parent directory for a link.
    fn check_workspace(&self) -> Result<(), String> {
        let held = fstat(&self.workspace_fd).map_err(|e| format!("stat the workspace: {e}"))?;
        let now = fstatat(AT_FDCWD, &self.layout.workspace, AtFlags::empty())
            .map_err(|e| format!("stat {}: {e}", self.layout.workspace.display()))?;
        if (held.st_dev, held.st_ino) == (now.st_dev, now.st_ino) {
            Ok(())
        } else {
            Err("the workspace was replaced while the job ran".to_owned())
        }
    }

    /// `rel` under the workspace with links and `..` resolved. It must
    /// exist and stay inside the workspace.
    fn inside_workspace(&self, rel: &str, what: &str) -> Result<PathBuf, io::Error> {
        let root = self.layout.workspace.canonicalize()?;
        let full = self.layout.workspace.join(rel).canonicalize()?;
        if full.starts_with(&root) {
            Ok(full)
        } else {
            Err(io::Error::other(format!(
                "{what} `{rel}` is outside the workspace"
            )))
        }
    }

    // Caches.

    async fn restore_cache(
        &mut self,
        index: usize,
        step: &CacheStep,
    ) -> Result<Vec<String>, String> {
        let store = self.store("cache")?;
        let key = self.expand(&step.key, "key")?;
        validate_key(&key)?;
        let mut fallbacks = Vec::new();
        for text in &step.restore_keys {
            let prefix = self.expand(text, "restore-keys")?;
            validate_key(&prefix)?;
            fallbacks.push(prefix);
        }
        let paths = self.expand_all(&step.paths, "paths")?;
        let scope = cache_scope(&self.doc.github.repository);
        let entry = |name: &str| StoreKey {
            kind: Kind::Cache,
            scope: scope.clone(),
            name: name.to_owned(),
        };

        let fail = |e: anyhow::Error| format!("{e:#}");
        let mut note = format!("Cache not found for key: {key}");
        if let Some(reader) = store.get(&entry(&key)).await.map_err(fail)? {
            self.restore_archive(reader, &[])?;
            note = format!("Cache restored from key: {key}");
        } else {
            self.cache_saves.push(CacheSave {
                index,
                key: key.clone(),
                paths,
            });
            for prefix in &fallbacks {
                let Some(name) = store
                    .find_prefix(Kind::Cache, &scope, prefix)
                    .await
                    .map_err(fail)?
                else {
                    continue;
                };
                if let Some(reader) = store.get(&entry(&name)).await.map_err(fail)? {
                    self.restore_archive(reader, &[])?;
                    note = format!("Cache restored from key: {name}");
                    break;
                }
            }
        }
        Ok(vec![note])
    }

    /// Saves the caches whose exact key missed, in step order. Only a
    /// job that succeeded saves. A save that fails is a warning, as with
    /// `actions/cache`.
    pub(super) async fn save_caches(&mut self, sink: &mut dyn super::Sink) {
        let saves = std::mem::take(&mut self.cache_saves);
        if self.status != JobStatus::Success {
            return;
        }
        for save in saves {
            let name = self.masker.mask("Post Cache").into_owned();
            sink.step_started(save.index, &name);
            match self.save_cache(&save).await {
                Ok(note) => self.say(sink, Line::Note(&note)),
                Err(message) => {
                    let message = format!("Failed to save cache {}: {message}", save.key);
                    self.say(sink, Line::Warning(&message));
                }
            }
        }
    }

    async fn save_cache(&self, save: &CacheSave) -> Result<String, String> {
        let store = self.store("cache")?;
        let key = StoreKey {
            kind: Kind::Cache,
            scope: cache_scope(&self.doc.github.repository),
            name: save.key.clone(),
        };
        let fail = |e: anyhow::Error| format!("{e:#}");
        // Another job, or an earlier step of this one, may have saved it.
        if store.get(&key).await.map_err(fail)?.is_some() {
            return Ok(format!("Cache already exists for key: {}", save.key));
        }
        self.check_workspace()?;
        self.save_archive(&store, &key, &self.layout.workspace, &save.paths)
            .await?;
        Ok(format!("Cache saved with key: {}", save.key))
    }

    // Workspaces.

    async fn persist(&mut self, root: &str, paths: &[String]) -> Result<Vec<String>, String> {
        let store = self.store("workspace")?;
        let root = self.expand(root, "root")?;
        let paths = self.expand_all(paths, "paths")?;
        self.check_workspace()?;
        let root_dir = self
            .inside_workspace(&root, "root")
            .map_err(|e| format!("root `{root}`: {e}"))?;
        let key = self.run_key(Kind::Workspace);
        self.save_archive(&store, &key, &root_dir, &paths).await?;
        Ok(vec![format!(
            "Persisted {} for later jobs",
            paths.join(", ")
        )])
    }

    async fn attach(&mut self, at: &str) -> Result<Vec<String>, String> {
        let store = self.store("workspace")?;
        let at = self.expand(at, "at")?;
        let parts = self.inside_components(&at)?;
        let mut notes = Vec::new();
        for name in self.required_names() {
            let key = StoreKey {
                kind: Kind::Workspace,
                scope: self.doc.run_id.clone(),
                name: encode_name(&name),
            };
            let reader = store.get(&key).await.map_err(|e| format!("{e:#}"))?;
            if let Some(reader) = reader {
                self.restore_archive(reader, &parts)?;
                notes.push(format!("Attached the workspace of {name}"));
            }
        }
        if notes.is_empty() {
            notes.push("No required job persisted a workspace; nothing to attach".to_owned());
        }
        Ok(notes)
    }

    /// The names of the jobs this one requires, in `requires` order.
    /// A matrix or parallel expansion's id is its name plus a suffix.
    fn required_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for id in &self.doc.job.requires {
            let name = self
                .doc
                .needs
                .keys()
                .filter(|name| {
                    id == *name
                        || id
                            .strip_prefix(name.as_str())
                            .is_some_and(|rest| rest.starts_with(" (") || rest.starts_with(" ["))
                })
                .max_by_key(|name| name.len());
            if let Some(name) = name
                && !names.contains(name)
            {
                names.push(name.clone());
            }
        }
        names
    }

    /// The components of `at`, which must stay inside the workspace.
    fn inside_components(&self, at: &str) -> Result<Vec<std::ffi::OsString>, String> {
        let path = Path::new(at);
        let rel = if path.has_root() {
            path.strip_prefix(&self.layout.workspace)
                .map_err(|_| format!("`{at}` is outside the workspace"))?
        } else {
            path
        };
        let mut parts = Vec::new();
        for part in rel.components() {
            match part {
                Component::Normal(name) => parts.push(name.to_owned()),
                Component::CurDir => {}
                _ => return Err(format!("`{at}` must stay inside the workspace")),
            }
        }
        Ok(parts)
    }

    // Test results.

    async fn test_results(&mut self, path: &str) -> Result<Vec<String>, String> {
        let store = self.store("test-results")?;
        let path = self.expand(path, "path")?;
        self.check_workspace()?;
        let none = || format!("no test results found at {path}");
        let target = match self.inside_workspace(&path, "path") {
            Ok(target) => target,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(none()),
            Err(err) => return Err(format!("path `{path}`: {err}")),
        };
        let (root, files) = if target.is_dir() {
            let mut files = Vec::new();
            collect_xml(&target, Path::new(""), &mut files).map_err(|e| format!("{e:#}"))?;
            files.sort();
            (target, files)
        } else {
            let name = target
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(none)?
                .to_owned();
            let parent = target.parent().ok_or_else(none)?.to_owned();
            (parent, vec![name])
        };
        if files.is_empty() {
            return Err(none());
        }
        let key = self.run_key(Kind::TestResults);
        self.save_archive(&store, &key, &root, &files).await?;
        Ok(vec![format!(
            "Stored {} test result file(s) from {path}",
            files.len()
        )])
    }

    // Shared.

    /// This job's entry for the run.
    fn run_key(&self, kind: Kind) -> StoreKey {
        StoreKey {
            kind,
            scope: self.doc.run_id.clone(),
            name: encode_name(&self.doc.job.name),
        }
    }

    /// Archives `paths` under `root` in `RUNNER_TEMP`, then stores it.
    async fn save_archive(
        &self,
        store: &DirStore,
        key: &StoreKey,
        root: &Path,
        paths: &[String],
    ) -> Result<(), String> {
        let tmp = tempfile::Builder::new()
            .prefix("archive-")
            .tempfile_in(&self.layout.temp)
            .map_err(|e| format!("create a file in {}: {e}", self.layout.temp.display()))?;
        let tripped = Cell::new(false);
        let made = create_tar_gz(
            root,
            paths,
            CapWriter {
                inner: tmp.as_file(),
                written: 0,
                cap: MAX_ARCHIVE_BYTES,
                tripped: &tripped,
            },
        );
        if let Err(err) = made {
            return Err(if tripped.get() {
                "the archive is larger than 4 GiB".to_owned()
            } else {
                format!("{err:#}")
            });
        }
        store
            .put(key, tmp.path())
            .await
            .map_err(|e| format!("{e:#}"))
    }

    /// Restores an archive into the workspace at `at`.
    fn restore_archive(&self, reader: impl Read, at: &[std::ffi::OsString]) -> Result<(), String> {
        let tmp = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(&self.layout.temp)
            .map_err(|e| format!("create a directory in {}: {e}", self.layout.temp.display()))?;
        let held = open_dir(AT_FDCWD, tmp.path())
            .map_err(|e| format!("open the restore directory: {e}"))?;
        extract_tar_gz_with(
            reader,
            tmp.path(),
            0,
            ExtractLimits::new(MAX_EXPANDED_BYTES),
        )
        .map_err(|e| format!("extract the archive: {e:#}"))?;
        let held_stat = fstat(&held).map_err(|e| e.to_string())?;
        let path_stat = fstatat(AT_FDCWD, tmp.path(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|e| e.to_string())?;
        if (held_stat.st_dev, held_stat.st_ino) != (path_stat.st_dev, path_stat.st_ino) {
            return Err("the restore directory was replaced".to_owned());
        }
        self.check_workspace()?;

        let mut dir = self
            .workspace_fd
            .try_clone()
            .map_err(|e| format!("open the workspace: {e}"))?;
        let mut shown = PathBuf::new();
        for part in at {
            shown.push(part);
            dir = enter(&dir, part, 0o755, self.owner, &shown)?;
        }
        merge(&held, &dir, Path::new(""), self.owner)
    }
}

/// The store scope of a repository's caches.
fn cache_scope(repository: &str) -> String {
    repository.replace('/', "%2F")
}

/// `*.xml` files under `dir`, as paths relative to it. Links are not
/// followed.
fn collect_xml(dir: &Path, rel: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let rel = rel.join(entry.file_name());
        if kind.is_dir() {
            collect_xml(&entry.path(), &rel, out)?;
        } else if kind.is_file()
            && rel
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
            && let Some(rel) = rel.to_str()
        {
            out.push(rel.to_owned());
        }
    }
    Ok(())
}

/// Fails once more than [`MAX_ARCHIVE_BYTES`] are written.
struct CapWriter<'a> {
    inner: &'a File,
    written: u64,
    cap: u64,
    tripped: &'a Cell<bool>,
}

impl Write for CapWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.written.saturating_add(len) > self.cap {
            self.tripped.set(true);
            return Err(io::Error::other("archive is larger than 4 GiB"));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// Placing restored files.

fn open_dir(parent: impl std::os::fd::AsFd, name: impl AsRef<Path>) -> nix::Result<OwnedFd> {
    openat(
        parent,
        name.as_ref(),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

fn kind_of(mode: nix::libc::mode_t) -> SFlag {
    SFlag::from_bits_truncate(mode) & SFlag::S_IFMT
}

fn chown_link(dir: &OwnedFd, name: &OsStr, owner: Owner, shown: &Path) -> Result<(), String> {
    if let Some((uid, gid)) = owner {
        fchownat(
            dir,
            name,
            Some(uid),
            Some(gid),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(|e| format!("chown {}: {e}", shown.display()))?;
    }
    Ok(())
}

/// Opens the directory `name` in `parent`, creating it when missing. A
/// link, or anything that is not a directory, is refused.
fn enter(
    parent: &OwnedFd,
    name: &OsStr,
    mode: u32,
    owner: Owner,
    shown: &Path,
) -> Result<OwnedFd, String> {
    match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => match kind_of(stat.st_mode) {
            SFlag::S_IFDIR => {}
            SFlag::S_IFLNK => return Err(format!("{} is a symlink", shown.display())),
            _ => return Err(format!("{} is not a directory", shown.display())),
        },
        Err(Errno::ENOENT) => {
            let mode = Mode::from_bits_truncate(mode.try_into().unwrap_or(0o755));
            mkdirat(parent, name, mode).map_err(|e| format!("create {}: {e}", shown.display()))?;
            chown_link(parent, name, owner, shown)?;
        }
        Err(e) => return Err(format!("stat {}: {e}", shown.display())),
    }
    open_dir(parent, name)
        .map_err(|_| format!("{} is a symlink or not a directory", shown.display()))
}

/// Moves everything in `src` into `dst`, which are the directory `rel`
/// of the extracted tree and of the workspace. Directories are merged;
/// a file or link replaces what is there.
fn merge(src: &OwnedFd, dst: &OwnedFd, rel: &Path, owner: Owner) -> Result<(), String> {
    let mut listing = Dir::openat(
        src,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("read {}: {e}", rel.display()))?;
    let mut names = Vec::new();
    for entry in listing.iter() {
        let entry = entry.map_err(|e| format!("read {}: {e}", rel.display()))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    for name in names {
        let shown = rel.join(&name);
        let stat = fstatat(src, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|e| format!("stat {}: {e}", shown.display()))?;
        match kind_of(stat.st_mode) {
            SFlag::S_IFDIR => {
                let child_dst = enter(dst, &name, u32::from(stat.st_mode & 0o777), owner, &shown)?;
                let child_src =
                    open_dir(src, &name).map_err(|e| format!("open {}: {e}", shown.display()))?;
                merge(&child_src, &child_dst, &shown, owner)?;
            }
            SFlag::S_IFREG | SFlag::S_IFLNK => {
                match fstatat(dst, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(old) if kind_of(old.st_mode) == SFlag::S_IFDIR => {
                        return Err(format!(
                            "{} is a directory in the workspace and a file in the archive",
                            shown.display()
                        ));
                    }
                    Ok(_) => unlinkat(dst, name.as_os_str(), UnlinkatFlags::NoRemoveDir)
                        .map_err(|e| format!("remove {}: {e}", shown.display()))?,
                    Err(Errno::ENOENT) => {}
                    Err(e) => return Err(format!("stat {}: {e}", shown.display())),
                }
                renameat(src, name.as_os_str(), dst, name.as_os_str())
                    .map_err(|e| format!("move {}: {e}", shown.display()))?;
                chown_link(dst, &name, owner, &shown)?;
            }
            _ => return Err(format!("{} is not a file or directory", shown.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archive_cap_stops_writes() {
        let file = tempfile::tempfile().unwrap();
        let tripped = Cell::new(false);
        let mut out = CapWriter {
            inner: &file,
            written: 0,
            cap: 10,
            tripped: &tripped,
        };
        out.write_all(b"0123456789").unwrap();
        assert!(!tripped.get());
        assert!(out.write_all(b"x").is_err());
        assert!(tripped.get());
        assert_eq!(file.metadata().unwrap().len(), 10);
    }
}
