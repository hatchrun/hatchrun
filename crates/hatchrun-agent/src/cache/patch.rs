// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Points the runner's cache variables at the proxy.
//!
//! The runner sets `ACTIONS_RESULTS_URL` and friends on every JavaScript
//! and container action from the job message, after the step
//! environment is merged, so nothing a job or the agent sets can win.
//! Renaming the strings in `bin/Runner.Worker.dll` to `ZCTIONS_*`, which
//! have the same length, makes the runner set names nobody reads, and
//! the `ACTIONS_*` names come from the environment the agent starts the
//! runner with. .NET keeps string literals in UTF-16LE.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The variables the runner sets that the proxy takes over.
pub const VARIABLES: [&str; 3] = [
    "ACTIONS_CACHE_URL",
    "ACTIONS_RESULTS_URL",
    "ACTIONS_CACHE_SERVICE_V2",
];

/// The runner's worker, relative to the runner directory.
pub const WORKER: &str = "bin/Runner.Worker.dll";

/// Which way to rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `ACTIONS_*` to `ZCTIONS_*`: the proxy serves the cache.
    Redirect,
    /// `ZCTIONS_*` back to `ACTIONS_*`: the stock runner.
    Restore,
}

/// The renamed form of a variable.
fn renamed(name: &str) -> String {
    format!("Z{}", &name[1..])
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(at, _)| at)
        .collect()
}

/// Renames every variable in `dll` the given way. Each must occur
/// exactly once, in one form or the other; a runner release that moved
/// or duplicated one is not patched at all. Returns whether anything
/// changed: a worker already in the wanted state is left as it is.
///
/// # Errors
///
/// When a variable does not occur exactly once.
pub fn patch(dll: &mut [u8], direction: Direction) -> Result<bool> {
    let mut edits = Vec::with_capacity(VARIABLES.len());
    for name in VARIABLES {
        let (stock, ours) = (utf16(name), utf16(&renamed(name)));
        let stock_at = find_all(dll, &stock);
        let ours_at = find_all(dll, &ours);
        let (from, to, at) = match direction {
            Direction::Redirect => (&stock_at, &ours, &ours_at),
            Direction::Restore => (&ours_at, &stock, &stock_at),
        };
        match (from.as_slice(), at.len()) {
            ([offset], 0) => edits.push((*offset, to.clone())),
            ([], 1) => {}
            _ => bail!(
                "{name} occurs {} times and {} {} times in the runner worker, expected once",
                stock_at.len(),
                renamed(name),
                ours_at.len()
            ),
        }
    }
    let changed = !edits.is_empty();
    for (offset, bytes) in edits {
        dll[offset..offset + bytes.len()].copy_from_slice(&bytes);
    }
    Ok(changed)
}

/// Patches the worker under `runner_dir` in place. The new file is
/// written next to the old one with its owner and mode and renamed over
/// it, so a failed write leaves the old worker whole.
///
/// # Errors
///
/// When the worker cannot be read, patched or replaced.
pub fn patch_runner(runner_dir: &Path, direction: Direction) -> Result<()> {
    let path = runner_dir.join(WORKER);
    let mut dll = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if !patch(&mut dll, direction).with_context(|| path.display().to_string())? {
        return Ok(());
    }
    let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    let temp = path.with_extension("dll.hatchrun");
    std::fs::write(&temp, &dll).with_context(|| format!("write {}", temp.display()))?;
    std::fs::set_permissions(&temp, meta.permissions())
        .with_context(|| format!("set mode of {}", temp.display()))?;
    if let Err(err) = std::os::unix::fs::chown(&temp, Some(meta.uid()), Some(meta.gid()))
        && err.kind() != std::io::ErrorKind::PermissionDenied
    {
        return Err(err).with_context(|| format!("set owner of {}", temp.display()));
    }
    std::fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the worker: the three strings among other bytes.
    fn worker() -> Vec<u8> {
        let mut dll = b"MZ\x90\x00".to_vec();
        for name in VARIABLES {
            dll.extend(utf16("prefix "));
            dll.extend(utf16(name));
            dll.extend([0, 0]);
        }
        // The plain ASCII form is a different string and stays.
        dll.extend(b"ACTIONS_RESULTS_URL");
        dll
    }

    #[test]
    fn redirects_each_variable_once() {
        let mut dll = worker();
        assert!(patch(&mut dll, Direction::Redirect).unwrap());

        for name in VARIABLES {
            assert!(find_all(&dll, &utf16(name)).is_empty(), "{name}");
            assert_eq!(find_all(&dll, &utf16(&renamed(name))).len(), 1, "{name}");
        }
        assert_eq!(find_all(&dll, b"ACTIONS_RESULTS_URL").len(), 1);
        assert_eq!(dll.len(), worker().len());
    }

    #[test]
    fn restores_a_redirected_worker() {
        let mut dll = worker();
        patch(&mut dll, Direction::Redirect).unwrap();
        assert!(patch(&mut dll, Direction::Restore).unwrap());
        assert_eq!(dll, worker());
    }

    #[test]
    fn a_patched_worker_is_left_alone() {
        let mut dll = worker();
        patch(&mut dll, Direction::Redirect).unwrap();
        let patched = dll.clone();
        assert!(!patch(&mut dll, Direction::Redirect).unwrap());
        assert_eq!(dll, patched);

        let mut stock = worker();
        assert!(!patch(&mut stock, Direction::Restore).unwrap());
        assert_eq!(stock, worker());
    }

    #[test]
    fn a_missing_variable_patches_nothing() {
        let mut dll = worker();
        let at = find_all(&dll, &utf16("ACTIONS_CACHE_SERVICE_V2"))[0];
        dll[at] = b'B';
        let before = dll.clone();

        let err = patch(&mut dll, Direction::Redirect).unwrap_err();
        assert!(
            err.to_string().contains("ACTIONS_CACHE_SERVICE_V2"),
            "{err}"
        );
        assert_eq!(dll, before);
    }

    #[test]
    fn a_duplicated_variable_patches_nothing() {
        let mut dll = worker();
        dll.extend(utf16("ACTIONS_RESULTS_URL"));
        let before = dll.clone();

        assert!(patch(&mut dll, Direction::Redirect).is_err());
        assert_eq!(dll, before);
    }

    #[test]
    fn a_worker_with_both_forms_patches_nothing() {
        let mut dll = worker();
        dll.extend(utf16("ZCTIONS_CACHE_URL"));
        let before = dll.clone();

        assert!(patch(&mut dll, Direction::Redirect).is_err());
        assert!(patch(&mut dll, Direction::Restore).is_err());
        assert_eq!(dll, before);
    }

    #[test]
    fn patches_the_file_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        let path = dir.path().join(WORKER);
        std::fs::write(&path, worker()).unwrap();

        patch_runner(dir.path(), Direction::Redirect).unwrap();
        let dll = std::fs::read(&path).unwrap();
        assert_eq!(find_all(&dll, &utf16("ZCTIONS_RESULTS_URL")).len(), 1);
        assert!(!path.with_extension("dll.hatchrun").exists());
    }
}
