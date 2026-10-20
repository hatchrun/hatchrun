// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads and plans hatchrun workflow files (`.hatchrun/config.yml`).
//!
//! The crate does no I/O and never reads the clock, so the same code runs
//! in a Cloudflare Worker, the runner agent and the CLI.

pub mod templates;

pub use templates::{Arch, TEMPLATES, Template, template, template_labels};
