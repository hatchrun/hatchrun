// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fetching actions and reading their `action.yml`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hatchrun_workflow::job::Github;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use super::archive::extract_tar_gz;
use super::layout::Layout;

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
    Docker,
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
    if uses.starts_with("docker://") {
        return Err(format!(
            "`{uses}` is a Docker container action; Docker services and containers come in a later release"
        ));
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

/// Fetches the action, downloading it unless it is already cached.
pub async fn fetch_with(
    r: &ActionRef,
    github: &Github,
    layout: &Layout,
    limits: Limits,
) -> Result<Action> {
    let (root, rel);
    let dir = match r {
        ActionRef::Local(p) => {
            let dir = local_dir(&layout.workspace, p)?;
            root = layout.workspace.canonicalize()?;
            rel = dir.strip_prefix(&root).unwrap_or(Path::new("")).to_owned();
            dir
        }
        ActionRef::Remote {
            owner,
            repo,
            path,
            git_ref,
        } => {
            let cached = layout
                .actions
                .join(owner)
                .join(repo)
                .join(cache_key(git_ref));
            if !cached.join(COMPLETE).is_file() {
                download(github, owner, repo, git_ref, &cached, limits).await?;
            }
            root = cached.join(TREE);
            rel = PathBuf::from(path);
            root.join(path)
        }
    };
    let meta = read_metadata_in(&root, &rel)?;
    Ok(Action { dir, meta })
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

/// Downloads `owner/repo@git_ref` and extracts it to `root`. The
/// extraction happens in a fresh temp directory next to it that is
/// renamed into place, so a half-extracted action is never reused.
async fn download(
    github: &Github,
    owner: &str,
    repo: &str,
    git_ref: &str,
    root: &Path,
    limits: Limits,
) -> Result<()> {
    let parent = root.parent().context("action cache path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    // `<owner>/<repo>` is root's to create; the steps only need to pass through.
    for dir in [Some(parent), parent.parent()].into_iter().flatten() {
        // Open the directory itself, never through a link a step planted.
        let flags = nix::fcntl::OFlag::O_RDONLY
            | nix::fcntl::OFlag::O_DIRECTORY
            | nix::fcntl::OFlag::O_NOFOLLOW
            | nix::fcntl::OFlag::O_CLOEXEC;
        let Ok(fd) = nix::fcntl::open(dir, flags, nix::sys::stat::Mode::empty()) else {
            continue;
        };
        nix::sys::stat::fchmod(&fd, nix::sys::stat::Mode::from_bits_truncate(0o755))
            .with_context(|| format!("set mode on {}", dir.display()))?;
    }
    // A name no cache key can have: keys never start with `%tmp`.
    let tmp = tempfile::Builder::new()
        .prefix("%tmp-")
        .tempdir_in(parent)
        .context("create temp directory")?;
    // The steps run as another user and need to read the tree.
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
    let label = format!("{owner}/{repo}@{git_ref}");
    let archive = tmp.path().join("download.tgz");
    let tree = tmp.path().join(TREE);

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

    let (archive_path, expanded, owner) = (archive.clone(), limits.expanded, limits.owner);
    let extract_into = tree.clone();
    // Dropping this future, as a cancelled job does, stops the blocking
    // extraction too.
    let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    let flag = Arc::clone(&stop.0);
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&archive_path)?;
        extract_tar_gz(file, &extract_into, 1, expanded, &flag)?;
        open_for_reading(&extract_into)?;
        if let Some(owner) = owner {
            chown_tree(&extract_into, owner)?;
        }
        anyhow::Ok(())
    })
    .await
    .context("extract task")?
    .with_context(|| format!("extract {label}"))?;
    std::fs::remove_file(&archive)?;
    std::fs::write(tmp.path().join(COMPLETE), b"")?;

    // A leftover without the marker is incomplete: replace it. One with
    // the marker was finished by a concurrent job.
    if !root.join(COMPLETE).is_file() {
        match std::fs::remove_dir_all(root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("clear {}", root.display())),
        }
        std::fs::rename(tmp.path(), root).with_context(|| format!("move {label} into place"))?;
    }
    Ok(())
}

/// Lets everyone read the extracted tree and enter its directories.
/// The agent extracts as root, and an archive can ship 0700 entries,
/// but the steps run as another user. Execute bits are kept as they
/// are, except that a file the owner can execute stays executable for
/// everyone who can read it. Symlinks are left alone.
fn open_for_reading(dir: &Path) -> Result<()> {
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

/// Hands `dir` and everything under it to `owner`. Links are handed over
/// themselves, never what they point at, and are not walked into.
fn chown_tree(dir: &Path, owner: (nix::unistd::Uid, nix::unistd::Gid)) -> Result<()> {
    let give = |path: &Path| {
        nix::unistd::fchownat(
            nix::fcntl::AT_FDCWD,
            path,
            Some(owner.0),
            Some(owner.1),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .with_context(|| format!("chown {}", path.display()))
    };
    give(dir)?;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            chown_tree(&entry.path(), owner)?;
        } else {
            give(&entry.path())?;
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

/// Reads `action.yml` or `action.yaml` from `dir`.
#[cfg(test)]
pub fn read_metadata(dir: &Path) -> Result<Metadata> {
    read_metadata_in(dir, Path::new(""))
}

/// Reads `action.yml` or `action.yaml` from `root/rel`. The directory
/// is reached one component at a time with `O_NOFOLLOW` from `root`,
/// and the file is opened the same way and must be a regular file, so
/// a step that swaps either for a link or a FIFO after the action was
/// resolved cannot make the agent read or block on something else.
pub fn read_metadata_in(root: &Path, rel: &Path) -> Result<Metadata> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;

    let shown = root.join(rel);
    let mut dir = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open {}", root.display()))?;
    for part in rel.components() {
        match part {
            std::path::Component::Normal(name) => {
                dir = openat(
                    &dir,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| format!("open {}", shown.display()))?;
            }
            std::path::Component::CurDir => {}
            _ => bail!("action path {} leaves the action", rel.display()),
        }
    }
    for name in ["action.yml", "action.yaml"] {
        let path = shown.join(name);
        let fd = match openat(
            &dir,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => continue,
            Err(Errno::ELOOP) => bail!("{} is a symbolic link", path.display()),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let file = std::fs::File::from(fd);
        if !file.metadata()?.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let mut text = String::new();
        file.take(MAX_META + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
        if text.len() as u64 > MAX_META {
            bail!("{} is larger than 1 MiB", path.display());
        }
        return parse_metadata(&text).map_err(|e| anyhow!("parse {}: {e:#}", path.display()));
    }
    bail!("no action.yml or action.yaml in {}", shown.display())
}

fn parse_metadata(text: &str) -> Result<Metadata> {
    let raw: RawMeta = serde_saphyr::from_str_with_options(
        text,
        serde_saphyr::options! { strict_booleans: true, emit_comments: false },
    )?;
    let runs = match raw.runs.using.0.as_str() {
        using @ ("node12" | "node16" | "node20" | "node24") => Runs::Node {
            version: using.to_owned(),
            main: raw
                .runs
                .main
                .map(|v| v.0)
                .context("runs.main is required for a JavaScript action")?,
            pre: raw.runs.pre.map(|v| v.0),
            pre_if: raw.runs.pre_if.map(|v| v.0),
            post: raw.runs.post.map(|v| v.0),
            post_if: raw.runs.post_if.map(|v| v.0),
        },
        "composite" => Runs::Composite {
            steps: raw
                .runs
                .steps
                .context("runs.steps is required for a composite action")?
                .into_iter()
                .map(CompositeStep::from)
                .collect(),
        },
        "docker" => Runs::Docker,
        other => bail!("runs.using `{other}` is not supported"),
    };
    Ok(Metadata {
        name: raw.name.map(|v| v.0).unwrap_or_default(),
        inputs: raw
            .inputs
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Input {
                        description: v.description.map(|d| d.0).unwrap_or_default(),
                        required: v.required.is_some_and(|r| r.0 == "true"),
                        default: v.default.map(|d| d.0),
                    },
                )
            })
            .collect(),
        outputs: raw
            .outputs
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Output {
                        description: v.description.map(|d| d.0).unwrap_or_default(),
                        value: v.value.map(|d| d.0),
                    },
                )
            })
            .collect(),
        runs,
    })
}

/// A YAML scalar kept as its text: strings, booleans and numbers all
/// read as the string a workflow would see.
#[derive(Debug)]
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Scalar;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string, number or boolean")
            }

            fn visit_str<E>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_owned()))
            }

            fn visit_bool<E>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
struct RawMeta {
    name: Option<Scalar>,
    #[serde(default)]
    inputs: BTreeMap<String, RawInput>,
    #[serde(default)]
    outputs: BTreeMap<String, RawOutput>,
    runs: RawRuns,
}

#[derive(Deserialize)]
struct RawInput {
    description: Option<Scalar>,
    required: Option<Scalar>,
    default: Option<Scalar>,
}

#[derive(Deserialize)]
struct RawOutput {
    description: Option<Scalar>,
    value: Option<Scalar>,
}

#[derive(Deserialize)]
struct RawRuns {
    using: Scalar,
    main: Option<Scalar>,
    pre: Option<Scalar>,
    #[serde(rename = "pre-if")]
    pre_if: Option<Scalar>,
    post: Option<Scalar>,
    #[serde(rename = "post-if")]
    post_if: Option<Scalar>,
    steps: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
struct RawStep {
    id: Option<Scalar>,
    name: Option<Scalar>,
    #[serde(rename = "if")]
    if_: Option<Scalar>,
    run: Option<Scalar>,
    shell: Option<Scalar>,
    uses: Option<Scalar>,
    #[serde(default)]
    with: BTreeMap<String, Scalar>,
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    #[serde(rename = "working-directory")]
    working_directory: Option<Scalar>,
    #[serde(rename = "continue-on-error")]
    continue_on_error: Option<Scalar>,
}

impl From<RawStep> for CompositeStep {
    fn from(s: RawStep) -> Self {
        let map = |m: BTreeMap<String, Scalar>| m.into_iter().map(|(k, v)| (k, v.0)).collect();
        Self {
            id: s.id.map(|v| v.0),
            name: s.name.map(|v| v.0),
            if_: s.if_.map(|v| v.0),
            run: s.run.map(|v| v.0),
            shell: s.shell.map(|v| v.0),
            uses: s.uses.map(|v| v.0),
            with: map(s.with),
            env: map(s.env),
            working_directory: s.working_directory.map(|v| v.0),
            continue_on_error: s.continue_on_error.map(|v| v.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn github(api_url: &str) -> Github {
        Github {
            repository: "o/r".into(),
            repository_owner: "o".into(),
            sha: "0".into(),
            ref_: "refs/heads/main".into(),
            ref_name: "main".into(),
            ref_type: "branch".into(),
            head_ref: String::new(),
            base_ref: String::new(),
            actor: "a".into(),
            server_url: "https://github.com".into(),
            api_url: api_url.into(),
            graphql_url: String::new(),
            token: "tok".into(),
        }
    }

    #[test]
    fn chown_tree_walks_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/f"), "x").unwrap();
        // A link that points nowhere fails if it is followed.
        std::os::unix::fs::symlink("/nonexistent/target", tree.join("dangling")).unwrap();
        std::os::unix::fs::symlink(dir.path(), tree.join("a/up")).unwrap();
        let me = (nix::unistd::getuid(), nix::unistd::getgid());
        chown_tree(&tree, me).unwrap();
        // Still the same tree, links intact.
        assert!(
            fs::symlink_metadata(tree.join("dangling"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(tree.join("a/b/f")).unwrap(), "x");
    }

    fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        let raw = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    fn remote(owner: &str, repo: &str, path: &str, r: &str) -> ActionRef {
        ActionRef::Remote {
            owner: owner.into(),
            repo: repo.into(),
            path: path.into(),
            git_ref: r.into(),
        }
    }

    fn fast() -> Limits {
        Limits {
            backoff: Duration::from_millis(1),
            ..LIMITS
        }
    }

    const NODE_YML: &str = "name: n\nruns:\n  using: node20\n  main: dist/index.js\n";

    #[test]
    fn parses_refs() {
        assert_eq!(
            parse_ref("actions/checkout@v5").unwrap(),
            remote("actions", "checkout", "", "v5")
        );
        assert_eq!(
            parse_ref("github/codeql-action/init@v3").unwrap(),
            remote("github", "codeql-action", "init", "v3")
        );
        assert_eq!(
            parse_ref("./.github/act").unwrap(),
            ActionRef::Local(PathBuf::from("./.github/act"))
        );
        let err = parse_ref("actions/checkout").unwrap_err();
        assert!(err.contains("actions/checkout"), "{err}");
        assert!(parse_ref("a/b@../x").is_err());
        assert!(parse_ref("a/b/../c@v1").is_err());
        assert!(parse_ref("a b/c@v1").is_err());
        assert!(parse_ref("lonely@v1").is_err());
        assert!(
            parse_ref("docker://alpine:3")
                .unwrap_err()
                .contains("Docker container action")
        );
    }

    #[tokio::test]
    async fn downloads_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/tarball/v1"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball(&[
                ("o-r-abc/action.yml", NODE_YML),
                ("o-r-abc/dist/index.js", "x"),
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let gh = github(&server.uri());
        let r = remote("o", "r", "", "v1");

        let a = fetch_with(&r, &gh, &layout, fast()).await.unwrap();
        assert_eq!(a.dir, layout.actions.join("o/r/v1/tree"));
        assert!(a.dir.join("dist/index.js").is_file());
        assert!(matches!(a.meta.runs, Runs::Node { .. }));

        // The second fetch hits the cache (the mock expects one request).
        let b = fetch_with(&r, &gh, &layout, fast()).await.unwrap();
        assert_eq!(a, b);
        // No temp leftovers beside the cache entry.
        let names: Vec<_> = fs::read_dir(layout.actions.join("o/r"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("v1")]);
    }

    #[tokio::test]
    async fn fetches_a_subdirectory_action_and_retries_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/tarball/v2"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/tarball/v2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(tarball(&[("o-r-abc/init/action.yml", NODE_YML)])),
            )
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let a = fetch_with(
            &remote("o", "r", "init", "v2"),
            &github(&server.uri()),
            &layout,
            fast(),
        )
        .await
        .unwrap();
        assert_eq!(a.dir, layout.actions.join("o/r/v2/tree/init"));
    }

    #[tokio::test]
    async fn slashed_refs_get_one_component_and_stale_entries_are_replaced() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/tarball/feat/x"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball(&[
                ("o-r-abc/action.yml", NODE_YML),
                ("o-r-abc/complete", "shipped"),
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        // A crashed run left a tree but no marker.
        let stale = layout.actions.join("o/r/feat%2Fx");
        fs::create_dir_all(stale.join("tree")).unwrap();
        fs::write(stale.join("tree/old"), "old").unwrap();
        let a = fetch_with(
            &remote("o", "r", "", "feat/x"),
            &github(&server.uri()),
            &layout,
            fast(),
        )
        .await
        .unwrap();
        assert_eq!(a.dir, stale.join("tree"));
        assert!(!stale.join("tree/old").exists());
        // The archive's own `complete` file is inside the tree only.
        assert!(stale.join("tree/complete").is_file());
        assert!(stale.join("complete").is_file());
        assert_eq!(cache_key("a/b%"), "a%2Fb%25");
    }

    #[tokio::test]
    async fn a_shipped_marker_does_not_make_a_failed_fetch_cached() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(7);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "o-r-abc/complete", &b"shipped"[..])
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_size(0);
        h.set_cksum();
        b.append_link(&mut h, "o-r-abc/hard", "/etc/passwd")
            .unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&b.into_inner().unwrap()).unwrap();
        let body = gz.finish().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let r = remote("o", "r", "", "bad");
        assert!(
            fetch_with(&r, &github(&server.uri()), &layout, fast())
                .await
                .is_err()
        );
        assert!(!layout.actions.join("o/r/bad").exists());
        let leftovers: Vec<_> = fs::read_dir(layout.actions.join("o/r")).unwrap().collect();
        assert!(leftovers.is_empty(), "temp dir cleaned up");
    }

    #[tokio::test]
    async fn a_404_fails_without_caching() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let err = fetch_with(
            &remote("o", "r", "", "nope"),
            &github(&server.uri()),
            &layout,
            fast(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(!layout.actions.join("o/r/nope").exists());
    }

    #[tokio::test]
    async fn stops_at_the_compressed_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 4096]))
            .mount(&server)
            .await;
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let limits = Limits {
            compressed: 100,
            ..fast()
        };
        let err = fetch_with(
            &remote("o", "r", "", "big"),
            &github(&server.uri()),
            &layout,
            limits,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("100 byte limit"), "{err:#}");
        assert!(!layout.actions.join("o/r/big").exists());
    }

    #[tokio::test]
    async fn local_actions_stay_inside_the_workspace() {
        let work = tempfile::tempdir().unwrap();
        let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
        let act = layout.workspace.join(".github/act");
        fs::create_dir_all(&act).unwrap();
        fs::write(act.join("action.yml"), NODE_YML).unwrap();
        let gh = github("http://unused");
        let a = fetch_with(
            &ActionRef::Local("./.github/act".into()),
            &gh,
            &layout,
            fast(),
        )
        .await
        .unwrap();
        assert_eq!(a.dir, act.canonicalize().unwrap());
        let err = fetch_with(&ActionRef::Local("./../../..".into()), &gh, &layout, fast())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside the workspace"), "{err}");
    }

    #[test]
    fn reads_action_yaml_or_yml() {
        for name in ["action.yml", "action.yaml"] {
            let d = tempfile::tempdir().unwrap();
            fs::write(d.path().join(name), NODE_YML).unwrap();
            let m = read_metadata(d.path()).unwrap();
            assert_eq!(m.name, "n");
            assert_eq!(
                m.runs,
                Runs::Node {
                    version: "node20".into(),
                    main: "dist/index.js".into(),
                    pre: None,
                    pre_if: None,
                    post: None,
                    post_if: None,
                }
            );
        }
        let d = tempfile::tempdir().unwrap();
        let err = read_metadata(d.path()).unwrap_err().to_string();
        assert!(
            err.contains("action.yml") && err.contains("action.yaml"),
            "{err}"
        );
    }

    #[test]
    fn a_linked_or_fifo_action_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let elsewhere = d.path().join("elsewhere.yml");
        fs::write(&elsewhere, NODE_YML).unwrap();
        let act = d.path().join("act");
        fs::create_dir(&act).unwrap();
        std::os::unix::fs::symlink(&elsewhere, act.join("action.yml")).unwrap();
        let err = read_metadata(&act).unwrap_err().to_string();
        assert!(err.contains("symbolic link"), "{err}");

        let fifo = d.path().join("fifo");
        fs::create_dir(&fifo).unwrap();
        nix::unistd::mkfifo(
            &fifo.join("action.yml"),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_metadata(&fifo).map(|_| ()).map_err(|e| e.to_string()));
        });
        let done = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reading a FIFO must not block");
        assert!(done.unwrap_err().contains("not a regular file"));
    }

    #[test]
    fn a_linked_directory_on_the_way_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("action.yml"), NODE_YML).unwrap();
        let root = d.path().join("root");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&real, root.join("sub")).unwrap();
        assert!(read_metadata_in(&root, Path::new("sub")).is_err());
        fs::create_dir(root.join("plain")).unwrap();
        fs::write(root.join("plain/action.yml"), NODE_YML).unwrap();
        assert!(read_metadata_in(&root, Path::new("plain")).is_ok());
        assert!(read_metadata_in(&root, Path::new("../real")).is_err());
    }

    #[test]
    fn caps_the_action_file() {
        let d = tempfile::tempdir().unwrap();
        let mut big = NODE_YML.to_string();
        big.push_str(&"#".repeat(2 << 20));
        fs::write(d.path().join("action.yml"), big).unwrap();
        let err = read_metadata(d.path()).unwrap_err().to_string();
        assert!(err.contains("1 MiB"), "{err}");
    }

    fn parse(yml: &str) -> Result<Metadata> {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("action.yml"), yml).unwrap();
        read_metadata(d.path())
    }

    #[test]
    fn docker_action_is_runs_docker() {
        let m = parse("name: d\nruns:\n  using: docker\n  image: Dockerfile\n").unwrap();
        assert_eq!(m.runs, Runs::Docker);
    }

    #[test]
    fn maps_every_node_version_and_rejects_others() {
        for v in ["node12", "node16", "node20", "node24"] {
            let m = parse(&format!("name: n\nruns:\n  using: {v}\n  main: m.js\n  pre: p.js\n  pre-if: always()\n  post: q.js\n  post-if: failure()\n")).unwrap();
            assert_eq!(
                m.runs,
                Runs::Node {
                    version: v.into(),
                    main: "m.js".into(),
                    pre: Some("p.js".into()),
                    pre_if: Some("always()".into()),
                    post: Some("q.js".into()),
                    post_if: Some("failure()".into()),
                }
            );
        }
        let err = parse("name: n\nruns:\n  using: python3\n  main: m.py\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("python3"), "{err}");
    }

    #[test]
    fn keeps_inputs_outputs_and_composite_steps_raw() {
        let m = parse(
            r"
name: c
inputs:
  a:
    description: first
    required: true
  b:
    default: ${{ github.token }}
  n:
    default: 5
  t:
    default: true
outputs:
  o:
    description: out
    value: ${{ steps.s.outputs.x }}
runs:
  using: composite
  steps:
    - id: s
      name: say
      if: ${{ inputs.a == 'x' }}
      run: echo hi
      shell: bash
      working-directory: sub
      continue-on-error: true
      env:
        K: v
        N: 3
    - uses: actions/checkout@v5
      with:
        fetch-depth: 0
",
        )
        .unwrap();
        assert!(m.inputs["a"].required);
        assert_eq!(m.inputs["a"].description, "first");
        assert_eq!(
            m.inputs["b"].default.as_deref(),
            Some("${{ github.token }}")
        );
        assert_eq!(m.inputs["n"].default.as_deref(), Some("5"));
        assert_eq!(m.inputs["t"].default.as_deref(), Some("true"));
        assert_eq!(
            m.outputs["o"].value.as_deref(),
            Some("${{ steps.s.outputs.x }}")
        );
        let Runs::Composite { steps } = m.runs else {
            panic!("composite");
        };
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id.as_deref(), Some("s"));
        assert_eq!(steps[0].if_.as_deref(), Some("${{ inputs.a == 'x' }}"));
        assert_eq!(steps[0].run.as_deref(), Some("echo hi"));
        assert_eq!(steps[0].shell.as_deref(), Some("bash"));
        assert_eq!(steps[0].working_directory.as_deref(), Some("sub"));
        assert_eq!(steps[0].continue_on_error.as_deref(), Some("true"));
        assert_eq!(steps[0].env["N"], "3");
        assert_eq!(steps[1].uses.as_deref(), Some("actions/checkout@v5"));
        assert_eq!(steps[1].with["fetch-depth"], "0");
    }
}
