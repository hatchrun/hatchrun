// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-agent exec`: runs one job from a [`JobDocument`] on this
//! machine, without GitHub's runner.

pub mod commands;
pub mod layout;
pub mod lines;
pub mod mask;
pub mod sink;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;

use anyhow::{Result, bail};
use hatchrun_workflow::{Conclusion, JobDocument, JobKind, JobResult};

use self::layout::Layout;
use self::sink::Sink;

/// How the agent runs a job.
// The user, env file, node and store are read once steps run.
#[allow(dead_code)]
pub struct Options {
    /// The work directory, laid out like GitHub's hosted runners.
    pub work_dir: PathBuf,
    /// The user steps run as. `None` keeps the agent's user.
    pub runner_user: Option<String>,
    /// KEY=VALUE file that becomes the steps' environment.
    pub env_file: PathBuf,
    /// The Node binary that runs JavaScript actions.
    pub node: Option<PathBuf>,
    /// A directory of prefetched actions.
    pub store: Option<PathBuf>,
    /// `RUNNER_TOOL_CACHE`.
    pub tool_cache: PathBuf,
}

/// Runs the document's job and reports how it went. `cancel` resolves
/// when the agent is asked to stop.
// Nothing awaits until steps run.
#[allow(clippy::unused_async)]
pub async fn run_job(
    doc: JobDocument,
    opts: &Options,
    sink: &mut dyn Sink,
    cancel: impl Future<Output = ()>,
) -> Result<JobResult> {
    let _ = (sink, cancel);
    if matches!(doc.job.kind, JobKind::Approval) {
        bail!("approval jobs do not run on a runner");
    }
    let layout = Layout::new(&opts.work_dir, &doc.github.repository, &opts.tool_cache);
    layout.create()?;
    Ok(JobResult {
        conclusion: Conclusion::Success,
        outputs: BTreeMap::new(),
        steps: Vec::new(),
        annotations: Vec::new(),
    })
}
