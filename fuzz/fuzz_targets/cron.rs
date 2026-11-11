// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fuzzes cron schedules and container option splitting.

#![no_main]

use hatchrun_workflow::Cron;
use hatchrun_workflow::container::split_options;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = Cron::parse(text);
    let _ = split_options(text);
});
