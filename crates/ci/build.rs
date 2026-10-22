// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Declares the `hatchrun_local_launcher` cfg, which `RUSTFLAGS` sets
//! to build the local launcher backend.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(hatchrun_local_launcher)");
}
