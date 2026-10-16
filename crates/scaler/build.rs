// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Sets `HATCHRUN_VERSION` for `--version`. Release builds pass the tag
//! version in the environment; local builds use the crate version.

fn main() {
    println!("cargo:rerun-if-env-changed=HATCHRUN_VERSION");
    let version = std::env::var("HATCHRUN_VERSION").unwrap_or_else(|_| {
        std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
    });
    println!("cargo:rustc-env=HATCHRUN_VERSION={version}");
}
