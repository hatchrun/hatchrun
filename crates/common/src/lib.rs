// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Small helpers shared by the hatchrun crates.
//!
//! With no features it holds only [`now_secs`]. The `worker` feature adds
//! [`error`], the JSON error response the Workers send.

#[cfg(not(target_arch = "wasm32"))]
mod clock;
#[cfg(feature = "worker")]
mod response;

#[cfg(not(target_arch = "wasm32"))]
pub use clock::now_secs;
#[cfg(feature = "worker")]
pub use response::error;
