// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What parsing a config keeps in memory. A config lives for a whole
//! webhook, so it must not hold much more than its own text: filter
//! patterns are kept as text and compiled only when matched.
//!
//! The allocator is counted, so this file holds one test: the test
//! harness would otherwise count other tests' threads too.

use std::alloc::System;
use std::fmt::Write as _;

use hatchrun_workflow::{Event, parse, triggered};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn many_filter_patterns_cost_about_their_text() {
    let mut yaml = String::from("workflows:\n  ci:\n    on:\n      push:\n        paths:\n");
    let mut n = 0;
    while yaml.len() < 1_000_000 {
        writeln!(yaml, "          - 'src/module{n}/deep/path/to/a/**/*.rs'").unwrap();
        n += 1;
    }
    yaml.push_str("    jobs: []\n");
    let patterns: usize = (0..n)
        .map(|i| format!("src/module{i}/deep/path/to/a/**/*.rs").len())
        .sum();

    let region = Region::new(GLOBAL);
    let config = parse(&yaml).unwrap();
    let stats = region.change();
    let kept = stats.bytes_allocated - stats.bytes_deallocated;
    // The pattern text, a String and a shape byte per pattern, and the
    // list's spare capacity. Compiled tokens took about 40 times the
    // text.
    assert!(
        kept < 3 * patterns,
        "kept {kept} for {patterns} pattern bytes"
    );
    assert!(
        stats.bytes_allocated < 16 * yaml.len(),
        "allocated {} for {} bytes of YAML",
        stats.bytes_allocated,
        yaml.len()
    );

    // Matching compiles the patterns it reaches and keeps at most a few
    // of them.
    let on = &config.workflows["ci"].on;
    let push = Event::Push {
        r#ref: "refs/heads/main".to_owned(),
        changed: vec!["src/module0/deep/path/to/a/b/c.rs".to_owned(); 20],
    };
    let region = Region::new(GLOBAL);
    assert!(triggered(on, &push));
    let stats = region.change();
    assert!(
        stats.bytes_allocated - stats.bytes_deallocated < 1024,
        "{stats:?}"
    );
    assert!(stats.bytes_allocated < 64 * yaml.len(), "{stats:?}");
}
