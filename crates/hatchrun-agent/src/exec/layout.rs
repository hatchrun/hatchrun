// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The directory layout of GitHub's hosted runners, rooted at the work
//! directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where a job's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The work directory.
    pub work: PathBuf,
    /// `GITHUB_WORKSPACE`: `<work>/<repo>/<repo>`.
    pub workspace: PathBuf,
    /// `RUNNER_TEMP`: `<work>/_temp`.
    pub temp: PathBuf,
    /// Downloaded actions: `<work>/_actions`.
    pub actions: PathBuf,
    /// `RUNNER_TOOL_CACHE`.
    pub tool_cache: PathBuf,
}

impl Layout {
    /// The layout for `repository` (`owner/name`) under `work`.
    pub fn new(work: &Path, repository: &str, tool_cache: &Path) -> Self {
        let name = repository.rsplit('/').next().unwrap_or(repository);
        Self {
            work: work.to_owned(),
            workspace: work.join(name).join(name),
            temp: work.join("_temp"),
            actions: work.join("_actions"),
            tool_cache: tool_cache.to_owned(),
        }
    }

    /// Creates the workspace, temp and actions directories. The tool
    /// cache belongs to the image and is left alone.
    pub fn create(&self) -> Result<()> {
        for dir in [&self.workspace, &self.temp, &self.actions] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_hosted_layout() {
        let layout = Layout::new(
            Path::new("/w"),
            "acme/repo",
            Path::new("/opt/hostedtoolcache"),
        );
        assert_eq!(layout.workspace, Path::new("/w/repo/repo"));
        assert_eq!(layout.temp, Path::new("/w/_temp"));
        assert_eq!(layout.actions, Path::new("/w/_actions"));
        assert_eq!(layout.tool_cache, Path::new("/opt/hostedtoolcache"));
    }
}
