// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs `hatchrun-agent split` as a binary.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Runs `split` with `args`, `stdin` as input and the given variables.
/// Every `HATCHRUN_*` variable of the test process is cleared first.
fn split(args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("split")
        .args(args)
        .env_remove("HATCHRUN_TIMINGS")
        .env_remove("HATCHRUN_NODE_INDEX")
        .env_remove("HATCHRUN_NODE_TOTAL")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn timings(dir: &Path, json: &str) -> String {
    let path = dir.join("timings.json");
    std::fs::write(&path, json).unwrap();
    path.to_string_lossy().into_owned()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn splits_by_timing() {
    let dir = tempfile::tempdir().unwrap();
    let file = timings(dir.path(), r#"{"a":10,"b":1,"c":1}"#);
    let items = ["a", "b", "c"];
    let run = |index: &str| {
        let mut args = vec!["--by", "timings"];
        args.extend(items);
        split(
            &args,
            "",
            &[
                ("HATCHRUN_TIMINGS", &file),
                ("HATCHRUN_NODE_INDEX", index),
                ("HATCHRUN_NODE_TOTAL", "2"),
            ],
        )
    };
    let first = run("0");
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    assert_eq!(text(&first), "a\n");
    let second = run("1");
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    assert_eq!(text(&second), "b\nc\n");
}

#[test]
fn reads_items_from_stdin() {
    let out = split(&["--by", "timings"], "x\n\ny\r\n  \nz", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out), "x\ny\nz\n");
}

#[test]
fn outside_a_parallel_job_prints_everything() {
    let out = split(&["--by", "timings", "b", "a", "c"], "", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out), "b\na\nc\n");
}

fn env(timings: Option<&str>) -> Vec<(&str, &str)> {
    let mut env = vec![("HATCHRUN_NODE_INDEX", "1"), ("HATCHRUN_NODE_TOTAL", "2")];
    if let Some(path) = timings {
        env.push(("HATCHRUN_TIMINGS", path));
    }
    env
}

#[test]
fn missing_timings_file_splits_evenly() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.json");
    let args = ["--by", "timings", "a", "b", "c", "d"];
    let unset = split(&args, "", &env(None));
    let absent = split(&args, "", &env(Some(missing.to_str().unwrap())));
    let empty = timings(dir.path(), "{}");
    let empty = split(&args, "", &env(Some(&empty)));
    assert_eq!(unset.status.code(), Some(0), "{unset:?}");
    assert_eq!(text(&unset), text(&absent));
    assert_eq!(text(&unset), text(&empty));
    assert_eq!(text(&unset).lines().count(), 2);
}

#[test]
fn only_timings_is_supported() {
    let out = split(&["--by", "name", "a"], "", &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("timings"));
    assert_eq!(text(&out), "");
}

#[test]
fn bad_node_variables_exit_2() {
    for (index, total) in [
        ("x", "2"),
        ("0", "y"),
        ("2", "2"),
        ("0", "0"),
        ("-1", "2"),
        ("", "2"),
    ] {
        let out = split(
            &["--by", "timings", "a"],
            "",
            &[
                ("HATCHRUN_NODE_INDEX", index),
                ("HATCHRUN_NODE_TOTAL", total),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{index}/{total}: {out:?}");
        assert_eq!(text(&out), "");
        assert_ne!(text_err(&out), "");
    }
    let out = split(
        &["--by", "timings", "a"],
        "",
        &[("HATCHRUN_NODE_INDEX", "0")],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn unparsable_timings_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let file = timings(dir.path(), "not json");
    let out = split(
        &["--by", "timings", "a"],
        "",
        &[
            ("HATCHRUN_TIMINGS", &file),
            ("HATCHRUN_NODE_INDEX", "0"),
            ("HATCHRUN_NODE_TOTAL", "2"),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(text(&out), "");
}

#[test]
fn too_many_items_exit_2() {
    let items = "a\n".repeat(1_000_001);
    let out = split(&["--by", "timings"], &items, &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(text(&out), "");
}

fn text_err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
