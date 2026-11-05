// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The user steps run as, and the default `PATH`.

use anyhow::{Context, Result};
use tokio::process::Command;

pub(crate) const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

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
