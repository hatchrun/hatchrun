// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fetching actions and reading their `action.yml`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hatchrun_workflow::job::Github;

use super::builtin::{remove_tree, to_mode};
use super::layout::Layout;
use super::pack::{kind_of, open_dir};

/// Where an action comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRef {
    /// `owner/repo[/path]@ref` on the GitHub server.
    Remote {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The directory inside the repository, empty for the root.
        path: String,
        /// The tag, branch or commit.
        git_ref: String,
    },
    /// `docker://image`.
    Docker {
        /// The image, such as `alpine:3.20`.
        image: String,
    },
    /// `./path` inside the workspace.
    Local(PathBuf),
}

/// A fetched action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// The directory that holds `action.yml`.
    pub dir: PathBuf,
    /// The parsed `action.yml`.
    pub meta: Metadata,
}

/// The parts of `action.yml` the agent uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The action's name.
    pub name: String,
    /// The declared inputs.
    pub inputs: BTreeMap<String, Input>,
    /// The declared outputs.
    pub outputs: BTreeMap<String, Output>,
    /// How the action runs.
    pub runs: Runs,
}

/// One declared input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// The input's description.
    pub description: String,
    /// Whether the action needs it.
    pub required: bool,
    /// The default, raw: it may hold `${{ }}`.
    pub default: Option<String>,
}

/// One declared output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The output's description.
    pub description: String,
    /// A composite action's value expression, raw.
    pub value: Option<String>,
}

/// How an action runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runs {
    /// A JavaScript action.
    Node {
        /// The `using` value, `node12` to `node24`.
        version: String,
        /// The entry point.
        main: String,
        /// The script to run before the job's steps.
        pre: Option<String>,
        /// The condition for `pre`, raw.
        pre_if: Option<String>,
        /// The script to run after the job's steps.
        post: Option<String>,
        /// The condition for `post`, raw.
        post_if: Option<String>,
    },
    /// A composite action.
    Composite {
        /// The steps, raw.
        steps: Vec<CompositeStep>,
    },
    /// A Docker container action.
    Docker {
        /// `image`: `docker://name` to pull, or a Dockerfile in the
        /// action's directory.
        image: String,
        /// `entrypoint`, raw.
        entrypoint: Option<String>,
        /// `args`, raw, one argument each.
        args: Vec<String>,
        /// `env`, raw.
        env: BTreeMap<String, String>,
        /// `pre-entrypoint`, raw.
        pre_entrypoint: Option<String>,
        /// The condition for `pre-entrypoint`, raw.
        pre_if: Option<String>,
        /// `post-entrypoint`, raw.
        post_entrypoint: Option<String>,
        /// The condition for `post-entrypoint`, raw.
        post_if: Option<String>,
    },
}

/// One step of a composite action. Every field is the raw text, which
/// may hold `${{ }}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompositeStep {
    /// `id`.
    pub id: Option<String>,
    /// `name`.
    pub name: Option<String>,
    /// `if`.
    pub if_: Option<String>,
    /// `run`.
    pub run: Option<String>,
    /// `shell`.
    pub shell: Option<String>,
    /// `uses`.
    pub uses: Option<String>,
    /// `with`.
    pub with: BTreeMap<String, String>,
    /// `env`.
    pub env: BTreeMap<String, String>,
    /// `working-directory`.
    pub working_directory: Option<String>,
    /// `continue-on-error`.
    pub continue_on_error: Option<String>,
}

/// Size and retry limits for a download. Tests shrink them.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The most compressed bytes to read from the response.
    pub compressed: u64,
    /// The most bytes to write when extracting.
    pub expanded: u64,
    /// The pause before the first retry, doubled each time.
    pub backoff: Duration,
    /// Who owns the extracted tree, so a composite action can write in
    /// its own directory. `None` leaves it to the agent's user.
    pub owner: Option<(nix::unistd::Uid, nix::unistd::Gid)>,
}

/// The limits for real jobs: 1 GiB compressed, 4 GiB expanded.
pub const LIMITS: Limits = Limits {
    compressed: 1 << 30,
    expanded: 4 << 30,
    backoff: Duration::from_millis(500),
    owner: None,
};

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the download may go without receiving a byte.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one download attempt may take in all.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// Marks a finished extraction. It sits beside the extracted tree, so
/// an archive cannot ship its own.
const COMPLETE: &str = "complete";

/// The extracted repository inside a cache entry.
const TREE: &str = "tree";

/// The most `action.yml` may hold.
const MAX_META: u64 = 1 << 20;

/// Extra tries after the first, for 5xx answers and connection errors.
const RETRIES: u32 = 3;

/// Splits a `uses:` value.
pub fn parse_ref(uses: &str) -> Result<ActionRef, String> {
    if let Some(image) = uses.strip_prefix("docker://") {
        if image.trim().is_empty() || image.contains(char::is_whitespace) {
            return Err(format!("`{uses}` has no valid image: write docker://image"));
        }
        return Ok(ActionRef::Docker {
            image: image.to_owned(),
        });
    }
    if uses.starts_with("./") {
        return Ok(ActionRef::Local(PathBuf::from(uses)));
    }
    let Some((spec, git_ref)) = uses.rsplit_once('@') else {
        return Err(format!(
            "`{uses}` has no version: write it as owner/repo@ref"
        ));
    };
    let mut parts = spec.splitn(3, '/');
    let owner = parts.next().unwrap_or_default();
    let Some(repo) = parts.next() else {
        return Err(format!("`{uses}` is not owner/repo[/path]@ref"));
    };
    let path = parts.next().unwrap_or_default();
    if !is_name(owner) || !is_name(repo) {
        return Err(format!("`{uses}` has an invalid owner or repository name"));
    }
    if !path.is_empty() && !path.split('/').all(is_name) {
        return Err(format!("`{uses}` has an invalid action path"));
    }
    if !git_ref.split('/').all(is_name) {
        return Err(format!("`{uses}` has an invalid ref"));
    }
    Ok(ActionRef::Remote {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path: path.to_owned(),
        git_ref: git_ref.to_owned(),
    })
}

/// A path segment: `[A-Za-z0-9._-]+`, and not `.` or `..`.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The action cache, `<work>/_actions`, opened once when the job starts
/// and held for the whole job.
///
/// The work directory belongs to the runner user, so a step can rename
/// `_actions` or plant links next to it. Nothing under `_actions` is
/// handed to the runner except each extracted `tree`, and every
/// cache operation goes through descriptors with `O_NOFOLLOW` instead of
/// paths, so a link a step plants cannot send a later fetch elsewhere.
pub struct ActionCache {
    root: CacheDir,
    /// `<state dir>/stage`, where archives are downloaded and extracted.
    /// Every directory down to it is closed to the runner user.
    stage: CacheDir,
}

/// A cache directory: its descriptor, and its path for display and for
/// the steps, which run in it.
#[derive(Clone)]
struct CacheDir {
    fd: Arc<OwnedFd>,
    path: PathBuf,
}

impl CacheDir {
    /// Opens the root-owned directory `name` below this one, creating
    /// it when missing. A link or file in its place is removed, never
    /// followed. The second value says whether it was created.
    fn child(&self, name: &str) -> Result<(CacheDir, bool)> {
        use nix::errno::Errno;
        let path = self.path.join(name);
        for _ in 0..2 {
            let created = match nix::sys::stat::mkdirat(&*self.fd, name, to_mode(0o755)) {
                Ok(()) => true,
                Err(Errno::EEXIST) => false,
                Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
            };
            match open_dir(&*self.fd, name) {
                Ok(fd) => {
                    // The steps run as another user and need to enter it.
                    nix::sys::stat::fchmod(&fd, to_mode(0o755))
                        .with_context(|| format!("set mode on {}", path.display()))?;
                    return Ok((
                        CacheDir {
                            fd: Arc::new(fd),
                            path,
                        },
                        created,
                    ));
                }
                Err(Errno::ELOOP | Errno::ENOTDIR) if !created => {
                    nix::unistd::unlinkat(&*self.fd, name, nix::unistd::UnlinkatFlags::NoRemoveDir)
                        .with_context(|| format!("replace {}", path.display()))?;
                }
                Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
            }
        }
        bail!("{} keeps changing", path.display())
    }

    /// Whether the finished-extraction marker is here as a regular file.
    fn is_complete(&self) -> bool {
        nix::sys::stat::fstatat(
            &*self.fd,
            COMPLETE,
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .is_ok_and(|st| kind_of(st.st_mode) == nix::sys::stat::SFlag::S_IFREG)
    }
}

impl ActionCache {
    /// Opens `path`, creating it when missing, and the stage below
    /// `state_dir`. `path` must be a directory the agent owns that no one
    /// else can write, which it makes sure of.
    ///
    /// # Errors
    ///
    /// When `path` is a link, is not a directory, or belongs to someone
    /// else, or the stage cannot be opened.
    pub fn open(path: &Path, state_dir: &Path) -> Result<Self> {
        let stage = CacheDir {
            fd: Arc::new(super::builtin::open_stage(state_dir)?),
            path: state_dir.join("stage"),
        };
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        let fd = open_dir(&nix::fcntl::AT_FDCWD, path).with_context(|| {
            format!(
                "open {}, which must be a directory and not a link",
                path.display()
            )
        })?;
        let stat = nix::sys::stat::fstat(&fd)?;
        if stat.st_uid != nix::unistd::geteuid().as_raw() {
            bail!("{} must belong to the agent", path.display());
        }
        nix::sys::stat::fchmod(&fd, to_mode(0o755))
            .with_context(|| format!("set mode on {}", path.display()))?;
        Ok(Self {
            root: CacheDir {
                fd: Arc::new(fd),
                path: path.to_owned(),
            },
            stage,
        })
    }
}

/// Fetches the action, downloading it unless it is already cached.
pub async fn fetch_with(
    r: &ActionRef,
    github: &Github,
    layout: &Layout,
    cache: &ActionCache,
    limits: Limits,
) -> Result<Action> {
    match r {
        ActionRef::Docker { .. } => bail!("a docker:// step has no action.yml"),
        ActionRef::Local(p) => {
            let dir = local_dir(&layout.workspace, p)?;
            let root = layout.workspace.canonicalize()?;
            let rel = dir.strip_prefix(&root).unwrap_or(Path::new("")).to_owned();
            let meta = read_metadata_in(&root, &rel)?;
            Ok(Action { dir, meta })
        }
        ActionRef::Remote {
            owner,
            repo,
            path,
            git_ref,
        } => {
            let key = cache_key(git_ref);
            let (owner_dir, _) = cache.root.child(owner)?;
            let (repo_dir, _) = owner_dir.child(repo)?;
            let (key_dir, created) = repo_dir.child(&key)?;
            if !key_dir.is_complete()
                && let Err(e) =
                    download(github, owner, repo, git_ref, &key_dir, &cache.stage, limits).await
            {
                if created {
                    remove_tree(&repo_dir.fd, OsStr::new(&key));
                }
                return Err(e);
            }
            let root = key_dir.path.join(TREE);
            let tree =
                open_dir(&*key_dir.fd, TREE).with_context(|| format!("open {}", root.display()))?;
            let rel = PathBuf::from(path);
            let meta = read_metadata_at(tree, &rel, &root.join(&rel))?;
            Ok(Action {
                dir: root.join(path),
                meta,
            })
        }
    }
}

/// The directory of a local action, which must be inside the workspace.
fn local_dir(workspace: &Path, p: &Path) -> Result<PathBuf> {
    let root = workspace
        .canonicalize()
        .with_context(|| format!("resolve {}", workspace.display()))?;
    let dir = workspace
        .join(p)
        .canonicalize()
        .with_context(|| format!("resolve local action {}", p.display()))?;
    if !dir.starts_with(&root) {
        bail!("local action {} is outside the workspace", p.display());
    }
    Ok(dir)
}

/// The cache directory name for a ref: one path component, with every
/// byte outside `[A-Za-z0-9._-]` percent-encoded, so `a/b` and `a` can
/// never nest.
fn cache_key(git_ref: &str) -> String {
    let mut key = String::new();
    for b in git_ref.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            key.push(b as char);
        } else {
            let _ = write!(key, "%{b:02X}");
        }
    }
    key
}

mod download;
mod metadata;

use self::download::download;
#[cfg(test)]
use self::download::{chown_tree, copy_tree};
#[cfg(test)]
pub use self::metadata::read_metadata;
use self::metadata::read_metadata_at;
pub use self::metadata::read_metadata_in;

#[cfg(test)]
mod tests;
