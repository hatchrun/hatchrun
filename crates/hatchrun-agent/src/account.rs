// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The user steps run as, and the default `PATH`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

pub(crate) const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Whether `path` is a regular file the user the agent runs as may
/// execute. `access(X_OK)` answers it with the kernel's own rules for the
/// process's uid and groups, which mode bits alone do not (an owner or
/// group the agent is not, an ACL). A symlink counts by what it points
/// to.
pub(crate) fn is_executable(path: &Path) -> bool {
    use nix::unistd::{AccessFlags, access};
    path.metadata().is_ok_and(|meta| meta.is_file()) && access(path, AccessFlags::X_OK).is_ok()
}

/// The first of `dirs` that holds an executable file called `name`
/// (see [`is_executable`]): a directory that is not absolute, a file
/// without execute permission and a directory of that name are passed
/// over.
pub(crate) fn first_executable(name: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

pub(crate) fn home_of(name: &str) -> Result<String> {
    let user = nix::unistd::User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("user {name} does not exist"))?;
    Ok(user.dir.to_string_lossy().into_owned())
}

/// Switches the child to `name` with all its supplementary groups. std's
/// `uid()` drops supplementary groups, which would take away `docker`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn run_as(command: &mut Command, name: &str) -> Result<()> {
    use std::ffi::CString;

    use nix::unistd::{User, getgrouplist, setgid, setgroups, setuid};

    let user = User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("user {name} does not exist"))?;
    let groups = getgrouplist(&CString::new(name)?, user.gid)
        .with_context(|| format!("list groups of {name}"))?;
    let (uid, gid) = (user.uid, user.gid);

    // SAFETY: the closure runs in the forked child before exec. It only
    // calls setgroups, setgid and setuid, which are async-signal-safe, and
    // does not allocate: `?` turns a nix Errno into an io::Error with
    // from_raw_os_error, which does not allocate either. `groups` is moved
    // into the closure and only freed in the parent.
    unsafe {
        command.pre_exec(move || {
            setgroups(&groups)?;
            setgid(gid)?;
            setuid(uid)?;
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_as(_: &mut Command, _: &str) -> Result<()> {
    anyhow::bail!("running steps as another user is only supported on Linux")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn make(dir: &Path, mode: u32) {
        let file = dir.join("podman");
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_first_executable_on_the_path_is_the_command() {
        let plain = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let good = tempfile::tempdir().unwrap();
        let later = tempfile::tempdir().unwrap();
        make(plain.path(), 0o644);
        std::fs::create_dir(folder.path().join("podman")).unwrap();
        make(good.path(), 0o755);
        make(later.path(), 0o755);
        let dirs = [
            PathBuf::from("relative/dir"),
            plain.path().to_owned(),
            folder.path().to_owned(),
            good.path().to_owned(),
            later.path().to_owned(),
        ];
        assert_eq!(
            first_executable(Path::new("podman"), &dirs),
            Some(good.path().join("podman"))
        );
        assert_eq!(first_executable(Path::new("docker"), &dirs), None);
    }

    #[test]
    fn a_mode_for_somebody_else_is_not_executable_by_this_user() {
        let dir = tempfile::tempdir().unwrap();
        // Executable by nobody the owner is: the owner is this user.
        make(dir.path(), 0o011);
        assert!(!is_executable(&dir.path().join("podman")));
        make(dir.path(), 0o700);
        assert!(is_executable(&dir.path().join("podman")));
        assert!(!is_executable(dir.path()));
    }

    #[test]
    fn a_relative_directory_is_not_searched() {
        let dirs = [PathBuf::from(".")];
        assert_eq!(first_executable(Path::new("sh"), &dirs), None);
    }
}
