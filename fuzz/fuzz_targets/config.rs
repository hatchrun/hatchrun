// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Fuzzes reading and planning a `.hatchrun/config.yml`.

#![no_main]

use hatchrun_workflow::{parse, validate};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(config) = parse(source) else {
        return;
    };
    // validate compiles every workflow under one shared budget, as
    // production does. Compiling each again would cost N fresh budgets.
    let _ = validate(&config);
});
