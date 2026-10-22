// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `cache`, `workspace` and `test-results` steps.
//!
//! Archives are made by jobs, so they are untrusted, and the workspace
//! is writable by steps and background processes while the agent works
//! on it as root. So:
//!
//! - Restores extract into a directory under `<state-dir>/stage`, which
//!   the runner user cannot write, then move entries into the workspace
//!   one path component at a time with `openat(O_NOFOLLOW)` from a
//!   descriptor of the workspace that was opened before any step ran.
//! - Saves walk descriptors too (see [`super::pack`]) and write the
//!   archive to an unlinked file in the same directory.
//! - The work runs on a blocking thread that checks a cancel flag, so a
//!   cancel or the grace period ends it promptly.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::anyhow;
use hatchrun_workflow::{CacheStep, Conclusion, JobStatus, WorkspaceStep};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AT_FDCWD, AtFlags, OFlag, openat, renameat};
use nix::sys::stat::{Mode, SFlag, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchownat, geteuid, unlinkat};

use super::archive::{ExtractLimits, extract_tar_gz_with};
use super::pack::{CancelRead, Filter, kind_of, open_dir, pack, walk};
use super::store::{DirStore, Kind, Store, StoreKey, entry_id, validate_key};
use super::{Common, Job, Line};

/// The most one stored archive may take, compressed.
const MAX_ARCHIVE_BYTES: u64 = 4 << 30;

/// The most a restored archive may expand to.
const MAX_EXPANDED_BYTES: u64 = 16 << 30;

type Owner = Option<(Uid, Gid)>;

/// Why a built-in step did not finish.
pub enum Stop {
    /// It failed, with this message.
    Failed(String),
    /// The job was cancelled.
    Cancelled,
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

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

/// Opens `<state_dir>/stage`, creating missing directories 0700. Every
/// directory from `/` down is opened with `O_NOFOLLOW` from the one
/// above it and must belong to the agent or root and be closed to
/// other writers, so the runner user cannot rename or replace any
/// part of the path. The stage itself must be closed to everyone else.
///
/// # Errors
///
/// When a directory cannot be created or opened, is a link, or is not
/// private; the message names it.
pub fn open_stage(state_dir: &Path) -> anyhow::Result<OwnedFd> {
    use anyhow::{Context, bail};

    if !state_dir.is_absolute() {
        bail!(
            "--state-dir {} must be an absolute path",
            state_dir.display()
        );
    }
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut shown = PathBuf::from("/");
    let mut dir = openat(AT_FDCWD, "/", flags, Mode::empty()).context("open /")?;
    check_private(&dir, &shown, false)?;
    let parts: Vec<&OsStr> = state_dir
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(Ok(name)),
            Component::RootDir => None,
            _ => Some(Err(anyhow::anyhow!(
                "--state-dir {} must not contain `.` or `..`",
                state_dir.display()
            ))),
        })
        .collect::<anyhow::Result<_>>()?;
    let total = parts.len() + 1;
    for (i, part) in parts.into_iter().chain([OsStr::new("stage")]).enumerate() {
        shown.push(part);
        match mkdirat(&dir, part, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(err) => return Err(err).with_context(|| format!("create {}", shown.display())),
        }
        dir = openat(&dir, part, flags, Mode::empty()).with_context(|| {
            format!(
                "open {}, which must be a directory and not a link",
                shown.display()
            )
        })?;
        check_private(&dir, &shown, i + 1 == total)?;
    }
    Ok(dir)
}

// `mode_t` is u32 on Linux and u16 on macOS, where the agent is only
// built for development.
#[allow(clippy::useless_conversion)]
fn mode_bits(mode: nix::libc::mode_t) -> u32 {
    u32::from(mode)
}

#[allow(clippy::useless_conversion)]
fn to_mode(bits: u32) -> Mode {
    Mode::from_bits_truncate(bits.try_into().unwrap_or(0o755))
}

/// Requires `dir` to belong to the agent or root, and to be closed to
/// group and others (a sticky directory may be writable: others cannot
/// rename what they do not own). `strict` closes it to reading as well.
fn check_private(dir: &OwnedFd, path: &Path, strict: bool) -> anyhow::Result<()> {
    let stat = fstat(dir)?;
    let mode = mode_bits(stat.st_mode);
    let sticky = mode & 0o1000 != 0;
    let writable_by_others = mode & 0o022 != 0 && !sticky;
    let owner_ok = stat.st_uid == geteuid().as_raw() || stat.st_uid == 0;
    if !owner_ok || writable_by_others || (strict && mode & 0o077 != 0) {
        anyhow::bail!(
            "{} must belong to the agent or root and not be writable by others",
            path.display()
        );
    }
    Ok(())
}

/// What a blocking thread needs of the job.
#[derive(Clone)]
struct Fs {
    workspace_fd: Arc<OwnedFd>,
    workspace: PathBuf,
    stage_fd: Arc<OwnedFd>,
    stage: PathBuf,
    owner: Owner,
}

impl Fs {
    /// Fails when the workspace path no longer leads to the directory
    /// the job started with, which a step could cause by swapping a
    /// parent directory for a link.
    fn check_workspace(&self) -> Result<(), String> {
        let held = fstat(&*self.workspace_fd).map_err(|e| format!("stat the workspace: {e}"))?;
        let now = fstatat(AT_FDCWD, &self.workspace, AtFlags::empty())
            .map_err(|e| format!("stat {}: {e}", self.workspace.display()))?;
        if (held.st_dev, held.st_ino) == (now.st_dev, now.st_ino) {
            Ok(())
        } else {
            Err("the workspace was replaced while the job ran".to_owned())
        }
    }

    fn check_stage(&self) -> Result<(), String> {
        let held = fstat(&*self.stage_fd).map_err(|e| format!("stat the stage: {e}"))?;
        let now = fstatat(AT_FDCWD, &self.stage, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|e| format!("stat {}: {e}", self.stage.display()))?;
        if (held.st_dev, held.st_ino) == (now.st_dev, now.st_ino) {
            Ok(())
        } else {
            Err(format!(
                "{} was replaced while the job ran",
                self.stage.display()
            ))
        }
    }

    /// An unlinked file in the stage.
    fn anon_file(&self) -> Result<File, String> {
        let name = format!("archive-{}", super::random_name()?);
        let fd = openat(
            &*self.stage_fd,
            name.as_str(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| format!("create a file in {}: {e}", self.stage.display()))?;
        unlinkat(&*self.stage_fd, name.as_str(), UnlinkatFlags::NoRemoveDir)
            .map_err(|e| format!("remove {name}: {e}"))?;
        Ok(File::from(fd))
    }

    /// Builds an archive of the workspace with `build`, which gets the
    /// workspace descriptor and where to write, then stores it. Returns
    /// what `build` returns.
    fn save<F>(
        &self,
        store: &DirStore,
        key: &StoreKey,
        cancel: &AtomicBool,
        build: F,
    ) -> Result<u64, Stop>
    where
        F: FnOnce(&OwnedFd, &mut dyn Write, &AtomicBool) -> anyhow::Result<u64>,
    {
        self.check_workspace()?;
        self.check_stage()?;
        let mut file = self.anon_file()?;
        let tripped = Cell::new(false);
        let made = build(
            &self.workspace_fd,
            &mut CapWriter {
                inner: &file,
                written: 0,
                cap: MAX_ARCHIVE_BYTES,
                tripped: &tripped,
            },
            cancel,
        );
        let count = made.map_err(|err| {
            if cancel.load(Ordering::Relaxed) {
                Stop::Cancelled
            } else if tripped.get() {
                Stop::Failed("the archive is larger than 4 GiB".to_owned())
            } else {
                Stop::Failed(format!("{err:#}"))
            }
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| format!("rewind the archive: {e}"))?;
        let mut src = CancelRead {
            inner: file,
            cancel,
        };
        tokio::runtime::Handle::current()
            .block_on(store.put(key, &mut src))
            .map_err(|err| {
                if cancel.load(Ordering::Relaxed) {
                    Stop::Cancelled
                } else {
                    Stop::Failed(format!("{err:#}"))
                }
            })?;
        Ok(count)
    }

    /// Restores an archive into the workspace at `at`.
    fn restore(&self, reader: impl Read, at: &[OsString], cancel: &AtomicBool) -> Result<(), Stop> {
        self.check_stage()?;
        let name = format!("restore-{}", super::random_name()?);
        mkdirat(
            &*self.stage_fd,
            name.as_str(),
            Mode::from_bits_truncate(0o700),
        )
        .map_err(|e| format!("create a directory in {}: {e}", self.stage.display()))?;
        let path = self.stage.join(&name);
        let _cleanup = RemoveDir {
            stage: self.stage_fd.clone(),
            name: name.clone(),
        };
        let held = open_dir(&*self.stage_fd, &name)
            .map_err(|e| format!("open the restore directory: {e}"))?;
        extract_tar_gz_with(
            CancelRead {
                inner: reader,
                cancel,
            },
            &path,
            0,
            ExtractLimits::new(MAX_EXPANDED_BYTES),
            cancel,
        )
        .map_err(|err| {
            if cancel.load(Ordering::Relaxed) {
                Stop::Cancelled
            } else {
                Stop::Failed(format!("extract the archive: {err:#}"))
            }
        })?;
        let held_stat = fstat(&held).map_err(|e| e.to_string())?;
        let path_stat =
            fstatat(AT_FDCWD, &path, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| e.to_string())?;
        if (held_stat.st_dev, held_stat.st_ino) != (path_stat.st_dev, path_stat.st_ino) {
            return Err("the restore directory was replaced".to_owned().into());
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
        merge(&held, &dir, Path::new(""), self.owner, cancel)
    }
}

/// Removes a directory of the stage when dropped, relative to the
/// stage descriptor. Links are removed, never followed.
struct RemoveDir {
    stage: Arc<OwnedFd>,
    name: String,
}

impl Drop for RemoveDir {
    fn drop(&mut self) {
        remove_tree(&self.stage, OsStr::new(&self.name));
    }
}

fn remove_tree(parent: &OwnedFd, name: &OsStr) {
    if let Ok(dir) = open_dir(parent, name)
        && let Ok(mut listing) = Dir::openat(
            &dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    {
        let names: Vec<OsString> = listing
            .iter()
            .filter_map(Result::ok)
            .map(|e| OsStr::from_bytes(e.file_name().to_bytes()).to_owned())
            .filter(|n| n != "." && n != "..")
            .collect();
        for child in names {
            match fstatat(&dir, child.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(st) if kind_of(st.st_mode) == SFlag::S_IFDIR => remove_tree(&dir, &child),
                _ => {
                    let _ = unlinkat(&dir, child.as_os_str(), UnlinkatFlags::NoRemoveDir);
                }
            }
        }
    }
    let _ = unlinkat(parent, name, UnlinkatFlags::RemoveDir);
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
                WorkspaceStep::Persist { root, paths } => self.persist(index, root, paths).await,
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
            let result = self.test_results(index, path).await;
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
        result: Result<Vec<String>, Stop>,
        sink: &mut dyn super::Sink,
    ) -> Conclusion {
        match result {
            Ok(notes) => {
                for note in notes {
                    self.say(sink, Line::Note(&note));
                }
                Conclusion::Success
            }
            Err(Stop::Failed(message)) => {
                self.say(sink, Line::Error(&message));
                Conclusion::Failure
            }
            Err(Stop::Cancelled) => {
                self.say(sink, Line::Error("The operation was canceled."));
                Conclusion::Cancelled
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

    fn fs(&self) -> Fs {
        Fs {
            workspace_fd: self.workspace_fd.clone(),
            workspace: self.layout.workspace.clone(),
            stage_fd: self.stage_fd.clone(),
            stage: self.stage.clone(),
            owner: self.owner,
        }
    }

    /// Runs `work` on a blocking thread. A cancel request, or the end
    /// of the grace period after one, sets the flag `work` is given,
    /// and `work` is expected to stop soon after.
    async fn blocking<T, F>(&mut self, work: F) -> Result<T, Stop>
    where
        T: Send + 'static,
        F: FnOnce(&AtomicBool) -> Result<T, Stop> + Send + 'static,
    {
        let flag = Arc::new(AtomicBool::new(false));
        let theirs = flag.clone();
        let mut handle = tokio::task::spawn_blocking(move || work(&theirs));
        let mut cancel: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> =
            if self.grace_until.is_some() {
                Box::pin(std::future::pending())
            } else {
                super::wait_for(self.cancel_requests.clone(), 1)
            };
        let mut timer: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> =
            match self.grace_until {
                Some(end) => Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                    end,
                ))),
                None => Box::pin(std::future::pending()),
            };
        let mut stopped = false;
        let joined = loop {
            tokio::select! {
                joined = &mut handle => break joined,
                () = &mut cancel, if !stopped => {
                    flag.store(true, Ordering::Relaxed);
                    stopped = true;
                }
                () = &mut timer, if !stopped => {
                    flag.store(true, Ordering::Relaxed);
                    stopped = true;
                }
            }
        };
        let result = match joined {
            Ok(result) => result,
            Err(err) => Err(Stop::Failed(format!("the archive task failed: {err}"))),
        };
        if stopped || matches!(result, Err(Stop::Cancelled)) {
            if *self.cancel_requests.borrow() >= 2 {
                self.aborted = true;
            }
            if self.grace_until.is_none() {
                self.grace_until = Some(std::time::Instant::now() + self.opts.minute * 5);
            }
            if result.is_ok() {
                return result;
            }
            return Err(Stop::Cancelled);
        }
        result
    }

    // Caches.

    async fn restore_cache(&mut self, index: usize, step: &CacheStep) -> Result<Vec<String>, Stop> {
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
        let exact = store.get(&entry(&key)).await.map_err(fail)?;
        let exact_hit = exact.is_some();
        let mut hit = exact.map(|reader| (key.clone(), reader));
        if hit.is_none() {
            for prefix in &fallbacks {
                let Some(name) = store
                    .find_prefix(Kind::Cache, &scope, prefix)
                    .await
                    .map_err(fail)?
                else {
                    continue;
                };
                if let Some(reader) = store.get(&entry(&name)).await.map_err(fail)? {
                    hit = Some((name, reader));
                    break;
                }
            }
        }
        let note = if let Some((name, reader)) = hit {
            self.restore_archive(reader, Vec::new()).await?;
            format!("Cache restored from key: {name}")
        } else {
            format!("Cache not found for key: {key}")
        };
        // Queued only once the restore went through.
        if !exact_hit {
            self.cache_saves.push(CacheSave { index, key, paths });
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
            if self.aborted {
                break;
            }
            let name = self.masker.mask("Post Cache").into_owned();
            sink.step_started(save.index, &name);
            match self.save_cache(&save).await {
                Ok(note) => self.say(sink, Line::Note(&note)),
                Err(Stop::Failed(message)) => {
                    let message = format!("Failed to save cache {}: {message}", save.key);
                    self.say(sink, Line::Warning(&message));
                }
                Err(Stop::Cancelled) => {
                    self.say(sink, Line::Error("The operation was canceled."));
                    self.settle(Conclusion::Cancelled, false);
                    break;
                }
            }
        }
    }

    async fn save_cache(&mut self, save: &CacheSave) -> Result<String, Stop> {
        let store = self.store("cache")?;
        let key = StoreKey {
            kind: Kind::Cache,
            scope: cache_scope(&self.doc.github.repository),
            name: save.key.clone(),
        };
        // Another job, or an earlier step of this one, may have saved it.
        let existing = store.get(&key).await.map_err(|e| format!("{e:#}"))?;
        if existing.is_some() {
            return Ok(format!("Cache already exists for key: {}", save.key));
        }
        let paths = relative_all(&save.paths)?;
        let fs = self.fs();
        self.blocking(move |cancel| {
            fs.save(&store, &key, cancel, |ws, w, cancel| {
                pack(ws, &paths, Filter::All, w, cancel)
            })
        })
        .await?;
        Ok(format!("Cache saved with key: {}", save.key))
    }

    // Workspaces.

    async fn persist(
        &mut self,
        index: usize,
        root: &str,
        paths: &[String],
    ) -> Result<Vec<String>, Stop> {
        let store = self.store("workspace")?;
        let root = self.expand(root, "root")?;
        let paths = self.expand_all(paths, "paths")?;
        let root_parts = inside_parts(&self.layout.workspace, &root)?;
        let path_parts = relative_all(&paths)?;
        let key = self.run_key(Kind::Workspace, index);
        let fs = self.fs();
        self.blocking(move |cancel| {
            fs.save(&store, &key, cancel, |ws, w, cancel| {
                let root = walk(ws, &root_parts)?;
                pack(&root, &path_parts, Filter::All, w, cancel)
            })
        })
        .await?;
        Ok(vec![format!(
            "Persisted {} for later jobs",
            paths.join(", ")
        )])
    }

    async fn attach(&mut self, at: &str) -> Result<Vec<String>, Stop> {
        let store = self.store("workspace")?;
        let at = self.expand(at, "at")?;
        let parts = inside_parts(&self.layout.workspace, &at)?;
        let fail = |e: anyhow::Error| format!("{e:#}");
        let mut notes = Vec::new();
        for id in self.doc.job.requires.clone() {
            let prefix = format!("{}.", entry_id(&id));
            let mut found: Vec<(u64, String)> = store
                .list_prefix(Kind::Workspace, &self.doc.run_id, &prefix)
                .await
                .map_err(fail)?
                .into_iter()
                .filter_map(|name| {
                    let step = name.strip_prefix(&prefix)?.parse().ok()?;
                    Some((step, name))
                })
                .collect();
            found.sort();
            for (step, name) in found {
                let key = StoreKey {
                    kind: Kind::Workspace,
                    scope: self.doc.run_id.clone(),
                    name,
                };
                if let Some(reader) = store.get(&key).await.map_err(fail)? {
                    self.restore_archive(reader, parts.clone()).await?;
                    notes.push(format!("Attached what {id} persisted in step {}", step + 1));
                }
            }
        }
        if notes.is_empty() {
            notes.push("No required job persisted a workspace; nothing to attach".to_owned());
        }
        Ok(notes)
    }

    // Test results.

    async fn test_results(&mut self, index: usize, path: &str) -> Result<Vec<String>, Stop> {
        let store = self.store("test-results")?;
        let path = self.expand(path, "path")?;
        let parts = inside_parts(&self.layout.workspace, &path)?;
        let key = self.run_key(Kind::TestResults, index);
        let fs = self.fs();
        let shown = path.clone();
        let count = self
            .blocking(move |cancel| {
                fs.save(&store, &key, cancel, |ws, w, cancel| {
                    let none = || anyhow!("no test results found at {shown}");
                    let Some((last, parents)) = parts.split_last() else {
                        let found = pack(ws, &[Vec::new()], Filter::TestResults, w, cancel)?;
                        return if found == 0 { Err(none()) } else { Ok(found) };
                    };
                    let parent = walk(ws, parents).map_err(|_| none())?;
                    let stat = fstatat(&parent, last.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
                        .map_err(|_| none())?;
                    let found = match kind_of(stat.st_mode) {
                        SFlag::S_IFDIR => {
                            let dir = open_dir(&parent, last).map_err(|_| none())?;
                            pack(&dir, &[Vec::new()], Filter::TestResults, w, cancel)?
                        }
                        SFlag::S_IFREG => {
                            pack(&parent, &[vec![last.clone()]], Filter::All, w, cancel)?
                        }
                        _ => 0,
                    };
                    if found == 0 { Err(none()) } else { Ok(found) }
                })
            })
            .await?;
        Ok(vec![format!(
            "Stored {count} test result file(s) from {path}"
        )])
    }

    // Shared.

    /// This job's entry for the run: its id and the step that made it.
    fn run_key(&self, kind: Kind, index: usize) -> StoreKey {
        StoreKey {
            kind,
            scope: self.doc.run_id.clone(),
            name: format!("{}.{index}", entry_id(&self.doc.job.id)),
        }
    }

    /// Restores an archive into the workspace at `at`.
    async fn restore_archive(
        &mut self,
        reader: Box<dyn Read + Send>,
        at: Vec<OsString>,
    ) -> Result<(), Stop> {
        let fs = self.fs();
        self.blocking(move |cancel| fs.restore(reader, &at, cancel))
            .await
    }
}

/// The store scope of a repository's caches.
fn cache_scope(repository: &str) -> String {
    repository.replace('/', "%2F")
}

/// The components of a path below the workspace: `text` is relative, or
/// absolute inside `workspace`, and never goes up.
fn inside_parts(workspace: &Path, text: &str) -> Result<Vec<OsString>, String> {
    let path = Path::new(text);
    let rel = if path.has_root() {
        path.strip_prefix(workspace)
            .map_err(|_| format!("`{text}` is outside the workspace"))?
    } else {
        path
    };
    parts_of(rel, text)
}

/// The components of a relative path that never goes up.
fn relative_parts(text: &str) -> Result<Vec<OsString>, String> {
    let path = Path::new(text);
    if path.has_root() {
        return Err(format!("`{text}` is an absolute path"));
    }
    parts_of(path, text)
}

fn relative_all(paths: &[String]) -> Result<Vec<Vec<OsString>>, String> {
    paths.iter().map(|p| relative_parts(p)).collect()
}

fn parts_of(rel: &Path, text: &str) -> Result<Vec<OsString>, String> {
    let mut parts = Vec::new();
    for part in rel.components() {
        match part {
            Component::Normal(name) => parts.push(name.to_owned()),
            Component::CurDir => {}
            _ => return Err(format!("`{text}` must stay inside the workspace")),
        }
    }
    Ok(parts)
}

/// Fails once more than `cap` bytes are written.
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
        let n = (&*self.inner).write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.inner).flush()
    }
}

// Placing restored files.

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
            let mode = to_mode(mode);
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
fn merge(
    src: &OwnedFd,
    dst: &OwnedFd,
    rel: &Path,
    owner: Owner,
    cancel: &AtomicBool,
) -> Result<(), Stop> {
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
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let shown = rel.join(&name);
        let stat = fstatat(src, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|e| format!("stat {}: {e}", shown.display()))?;
        match kind_of(stat.st_mode) {
            SFlag::S_IFDIR => {
                let child_dst = enter(dst, &name, mode_bits(stat.st_mode & 0o777), owner, &shown)?;
                let child_src =
                    open_dir(src, &name).map_err(|e| format!("open {}: {e}", shown.display()))?;
                merge(&child_src, &child_dst, &shown, owner, cancel)?;
            }
            SFlag::S_IFREG | SFlag::S_IFLNK => {
                match fstatat(dst, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(old) if kind_of(old.st_mode) == SFlag::S_IFDIR => {
                        return Err(format!(
                            "{} is a directory in the workspace and a file in the archive",
                            shown.display()
                        )
                        .into());
                    }
                    Ok(_) => unlinkat(dst, name.as_os_str(), UnlinkatFlags::NoRemoveDir)
                        .map_err(|e| format!("remove {}: {e}", shown.display()))?,
                    Err(Errno::ENOENT) => {}
                    Err(e) => return Err(format!("stat {}: {e}", shown.display()).into()),
                }
                renameat(src, name.as_os_str(), dst, name.as_os_str())
                    .map_err(|e| format!("move {}: {e}", shown.display()))?;
                chown_link(dst, &name, owner, &shown)?;
            }
            _ => return Err(format!("{} is not a file or directory", shown.display()).into()),
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

    #[test]
    fn paths_stay_inside() {
        let ws = Path::new("/w/repo/repo");
        assert!(inside_parts(ws, "a/b").is_ok());
        assert!(inside_parts(ws, "/w/repo/repo/a").is_ok());
        for bad in ["../x", "a/../../x", "/etc"] {
            assert!(inside_parts(ws, bad).is_err(), "{bad}");
        }
        assert!(relative_parts("/etc").is_err());
        assert_eq!(relative_parts(".").unwrap(), Vec::<OsString>::new());
    }
}
