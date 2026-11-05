// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The warm instance pool: types and the pool state machine.
//!
//! [`pool::Pool`] does no I/O and never reads the clock. The caller feeds
//! it the instances it finds in the cloud and the time, carries out the
//! [`Action`]s it returns and reports back how each one went.
//!
//! A warm pool keeps instances that booted once and were then stopped.
//! Their first boot read the image's files off the snapshot, so a job
//! started from the pool skips both the launch and the slow first read. A
//! pool instance moves through [`PoolState::Warming`],
//! [`PoolState::Stopping`] and [`PoolState::Ready`].

pub mod pool;

/// Something the caller has to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Launch a pool instance with this name. Report
    /// [`pool::Pool::launched`] or [`pool::Pool::launch_failed`].
    LaunchPool {
        /// The instance name.
        name: String,
    },
    /// Stop the instance. Nothing to report: the inventory shows it
    /// stopped.
    Stop {
        /// The instance id.
        instance_id: String,
    },
    /// Terminate the instance. Nothing to report: an instance that
    /// survives shows up in the next inventory.
    Terminate {
        /// The instance id.
        instance_id: String,
    },
}

/// An instance found in the cloud.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    /// The instance id.
    pub id: String,
    /// The runner name from the instance tags, `None` for a pool instance.
    pub runner_name: Option<String>,
    /// The runner id from the instance tags.
    pub runner_id: Option<u64>,
    /// When the instance launched.
    pub launched_at: u64,
    /// Whether the instance is stopped.
    pub stopped: bool,
}

/// Where a pool instance is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolState {
    /// Booting for the first time, reading its files.
    Warming,
    /// Asked to stop.
    Stopping,
    /// Stopped and ready for a runner.
    Ready,
}

/// A pool instance the scaler tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolInstance {
    /// Where the instance is.
    pub state: PoolState,
    /// When the instance launched.
    pub launched_at: u64,
    /// When the instance entered its state.
    pub since: u64,
}
