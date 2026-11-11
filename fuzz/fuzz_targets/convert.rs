// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fuzzes converting GitHub Actions workflows into a hatchrun config.
//!
//! A NUL byte splits the input into two workflow files, so a fuzzer can
//! reach the calls between them. The names match the fixtures: the
//! release workflow calls `./.github/workflows/docker-build.yml`.

#![no_main]

use hatchrun_workflow::convert;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let mut parts = source.splitn(2, '\0');
    let first = parts.next().unwrap_or_default();
    let mut files = vec![("docker-build.yml".to_owned(), first.to_owned())];
    if let Some(second) = parts.next() {
        files.push(("release.yml".to_owned(), second.to_owned()));
    }
    let _ = convert(&files);
});
