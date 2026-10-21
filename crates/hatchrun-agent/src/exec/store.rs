// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Where the cache, workspace and test-results steps keep their
//! archives.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

/// The longest key or restore key, in bytes.
pub const MAX_KEY_BYTES: usize = 512;

/// The file name suffix of a stored archive.
const SUFFIX: &str = ".tar.gz";

/// What an archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A `cache:` entry.
    Cache,
    /// Files a job passed to later jobs.
    Workspace,
    /// `JUnit` XML a job stored.
    TestResults,
}

impl Kind {
    /// The directory name under the store's root.
    fn dir(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Workspace => "workspace",
            Self::TestResults => "test-results",
        }
    }
}

/// Where one archive lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreKey {
    /// What the archive holds.
    pub kind: Kind,
    /// The repository (caches) or the run id (workspaces and results).
    pub scope: String,
    /// The cache key, or the job's name.
    pub name: String,
}

/// A place to keep archives.
pub trait Store {
    /// Opens the archive stored under `key`, if there is one.
    async fn get(&self, key: &StoreKey) -> Result<Option<Box<dyn Read + Send>>>;

    /// Stores the file at `file` under `key`, replacing what was there.
    async fn put(&self, key: &StoreKey, file: &Path) -> Result<()>;

    /// The name of the newest archive of `kind` in `scope` whose name
    /// starts with `prefix`.
    async fn find_prefix(&self, kind: Kind, scope: &str, prefix: &str) -> Result<Option<String>>;
}

/// Archives as files: `<root>/<kind>/<scope>/<name>.tar.gz`.
pub struct DirStore {
    /// The directory everything is kept under.
    pub root: PathBuf,
}

impl DirStore {
    fn scope_dir(&self, kind: Kind, scope: &str) -> Result<PathBuf> {
        check_component(scope).context("scope")?;
        Ok(self.root.join(kind.dir()).join(scope))
    }

    fn file(&self, key: &StoreKey) -> Result<PathBuf> {
        check_component(&key.name).context("name")?;
        Ok(self
            .scope_dir(key.kind, &key.scope)?
            .join(format!("{}{SUFFIX}", key.name)))
    }
}

/// A file name the store can use as it is: not empty, no path
/// separator or NUL, and not a way to walk up.
fn check_component(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        bail!("`{name}` cannot be a file name");
    }
    Ok(())
}

// The directory store does blocking file work; the trait is async for
// stores that talk to a network.
#[allow(clippy::unused_async_trait_impl)]
impl Store for DirStore {
    async fn get(&self, key: &StoreKey) -> Result<Option<Box<dyn Read + Send>>> {
        let path = self.file(key)?;
        match File::open(&path) {
            Ok(file) => Ok(Some(Box::new(io::BufReader::new(file)))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("open {}", path.display())),
        }
    }

    async fn put(&self, key: &StoreKey, file: &Path) -> Result<()> {
        let dest = self.file(key)?;
        let dir = dest.parent().context("no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // The file sits in a directory the runner user can write to, so
        // it is never followed if it was swapped for a link.
        let mut src = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(file)
            .with_context(|| format!("open {}", file.display()))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".put-")
            .tempfile_in(dir)
            .with_context(|| format!("create a file in {}", dir.display()))?;
        io::copy(&mut src, tmp.as_file_mut()).context("copy the archive")?;
        tmp.persist(&dest)
            .map_err(|e| e.error)
            .with_context(|| format!("write {}", dest.display()))?;
        Ok(())
    }

    async fn find_prefix(&self, kind: Kind, scope: &str, prefix: &str) -> Result<Option<String>> {
        let dir = self.scope_dir(kind, scope)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
        };
        let mut best: Option<(SystemTime, String)> = None;
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", dir.display()))?;
            let file_name = entry.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|n| n.strip_suffix(SUFFIX))
                .filter(|n| n.starts_with(prefix))
            else {
                continue;
            };
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified()?;
            if best
                .as_ref()
                .is_none_or(|(t, n)| (mtime, name) > (*t, n.as_str()))
            {
                best = Some((mtime, name.to_owned()));
            }
        }
        Ok(best.map(|(_, name)| name))
    }
}

/// Checks a cache key or restore key before it becomes a file name:
/// printable ASCII except `/` and `\`, at most [`MAX_KEY_BYTES`].
///
/// # Errors
///
/// A message that says what is wrong.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("a key cannot be empty".to_owned());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!("a key is at most {MAX_KEY_BYTES} bytes"));
    }
    if let Some(bad) = key
        .chars()
        .find(|c| !c.is_ascii() || c.is_ascii_control() || *c == '/' || *c == '\\')
    {
        return Err(format!(
            "key `{key}` has {bad:?}; keys are printable ASCII without `/` or `\\`"
        ));
    }
    Ok(())
}

/// Turns a job name into a file name: everything but letters, digits,
/// and `. _ -` and the space becomes `%XX`, so that two names never
/// share a file.
pub fn encode_name(name: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b' ') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: Kind, scope: &str, name: &str) -> StoreKey {
        StoreKey {
            kind,
            scope: scope.to_owned(),
            name: name.to_owned(),
        }
    }

    fn source(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("src.tar.gz");
        fs::write(&path, text).unwrap();
        path
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore {
            root: dir.path().join("store"),
        };
        let k = key(Kind::Cache, "acme%2Frepo", "k1");
        assert!(store.get(&k).await.unwrap().is_none());
        store.put(&k, &source(dir.path(), "one")).await.unwrap();
        store.put(&k, &source(dir.path(), "two")).await.unwrap();
        let mut text = String::new();
        store
            .get(&k)
            .await
            .unwrap()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "two");
        assert!(
            dir.path()
                .join("store/cache/acme%2Frepo/k1.tar.gz")
                .is_file()
        );
        let left: Vec<_> = fs::read_dir(dir.path().join("store/cache/acme%2Frepo"))
            .unwrap()
            .collect();
        assert_eq!(left.len(), 1, "no temp file stays behind");
    }

    #[tokio::test]
    async fn find_prefix_takes_the_newest_in_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore {
            root: dir.path().to_owned(),
        };
        let src = source(dir.path(), "x");
        for name in ["deps-a", "deps-b", "other"] {
            store.put(&key(Kind::Cache, "s", name), &src).await.unwrap();
        }
        store
            .put(&key(Kind::Cache, "t", "deps-z"), &src)
            .await
            .unwrap();
        let old = File::options()
            .write(true)
            .open(dir.path().join("cache/s/deps-b.tar.gz"))
            .unwrap();
        old.set_modified(SystemTime::now() - std::time::Duration::from_secs(1000))
            .unwrap();
        let found = store.find_prefix(Kind::Cache, "s", "deps-").await.unwrap();
        assert_eq!(found.as_deref(), Some("deps-a"));
        let none = store.find_prefix(Kind::Cache, "s", "nope").await.unwrap();
        assert_eq!(none, None);
        let other_kind = store
            .find_prefix(Kind::Workspace, "s", "deps-")
            .await
            .unwrap();
        assert_eq!(other_kind, None);
    }

    #[tokio::test]
    async fn names_cannot_leave_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore {
            root: dir.path().to_owned(),
        };
        let src = source(dir.path(), "x");
        for (scope, name) in [("..", "a"), ("s", "../a"), ("a/b", "k"), ("", "k")] {
            let err = store
                .put(&key(Kind::Cache, scope, name), &src)
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("cannot be a file name"),
                "{err:#}"
            );
        }
    }

    #[tokio::test]
    async fn put_does_not_follow_a_link_as_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore {
            root: dir.path().join("store"),
        };
        let secret = dir.path().join("secret");
        fs::write(&secret, "s").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let err = store
            .put(&key(Kind::Cache, "s", "k"), &link)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("open"), "{err:#}");
    }

    #[test]
    fn keys_are_checked() {
        assert!(validate_key("deps-v1.2_x y!#").is_ok());
        assert!(validate_key(&"a".repeat(512)).is_ok());
        for bad in ["", "a/b", "a\\b", "caf\u{e9}", "a\nb", "a\u{0}b"] {
            assert!(validate_key(bad).is_err(), "{bad:?}");
        }
        assert!(validate_key(&"a".repeat(513)).is_err());
    }

    #[test]
    fn job_names_encode_without_collisions() {
        assert_eq!(encode_name("build"), "build");
        assert_eq!(encode_name("a/b"), "a%2Fb");
        assert_eq!(encode_name("a%2Fb"), "a%252Fb");
        assert_eq!(encode_name("t (x)"), "t %28x%29");
    }
}
