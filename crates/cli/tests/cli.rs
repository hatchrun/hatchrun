// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs the `hatchrun` binary end to end.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn hatchrun() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hatchrun"));
    for var in [
        "HATCHRUN_NODE_INDEX",
        "HATCHRUN_NODE_TOTAL",
        "HATCHRUN_TIMINGS_FILE",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap()
}

#[test]
fn validate_ok_and_error() {
    let out = run(hatchrun().arg("validate").arg(fixture("spec_example.yml")));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");

    let bad = scratch("bad.yml");
    std::fs::write(&bad, "jobs:\n  t:\n    parallelism: lots\n").unwrap();
    let out = run(hatchrun().arg("validate").arg(&bad));
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(stdout.contains(":3:18:"), "{stdout}");
    assert!(stdout.starts_with(&bad.display().to_string()), "{stdout}");
}

#[test]
fn validate_missing_file_fails() {
    let out = run(hatchrun().arg("validate").arg(scratch("nope.yml")));
    assert_eq!(out.status.code(), Some(1));
    assert_ne!(text(&out.stderr), "");
}

#[test]
fn convert_writes_yaml() {
    let dest = scratch("converted.yml");
    let out = run(hatchrun()
        .arg("convert")
        .arg(fixture("github/ci.yml"))
        .arg("-o")
        .arg(&dest));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("services"));
    let out = run(hatchrun().arg("validate").arg(&dest));
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

#[test]
fn convert_prints_to_stdout_and_fails_on_bad_input() {
    let out = run(hatchrun().arg("convert").arg(fixture("github/ci.yml")));
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("workflows"));

    let bad = scratch("broken.yml");
    std::fs::write(&bad, "on: [push\n").unwrap();
    let out = run(hatchrun().arg("convert").arg(&bad));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    assert_ne!(text(&out.stderr), "");
}

fn timings_file() -> PathBuf {
    let path = scratch("timings.json");
    std::fs::write(&path, r#"{"a":10,"b":1,"c":1,"d":8}"#).unwrap();
    path
}

#[test]
fn split_reads_env() {
    let out = run(hatchrun()
        .env("HATCHRUN_NODE_INDEX", "1")
        .env("HATCHRUN_NODE_TOTAL", "2")
        .env("HATCHRUN_TIMINGS_FILE", timings_file())
        .args(["tests", "split", "a", "b", "c", "d", "e"]));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "d e\n");
}

#[test]
fn split_defaults_to_one_node_and_reads_stdin() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = hatchrun()
        .args(["tests", "split"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a\n\nb\nc\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(text(&out.stdout), "a b c\n");
}

#[test]
fn split_by_name_ignores_timings() {
    let out = run(hatchrun()
        .env("HATCHRUN_TIMINGS_FILE", scratch("absent.json"))
        .args([
            "tests", "split", "--by", "name", "--total", "2", "a", "b", "c",
        ]));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "a c\n");
}

#[test]
fn split_bad_timings_fail() {
    let out = run(hatchrun()
        .env("HATCHRUN_TIMINGS_FILE", scratch("absent.json"))
        .args(["tests", "split", "a"]));
    assert_eq!(out.status.code(), Some(1));
    assert_ne!(text(&out.stderr), "");

    let bad = scratch("bad.json");
    std::fs::write(&bad, "{nope").unwrap();
    let out = run(hatchrun()
        .args(["tests", "split", "--timings"])
        .arg(&bad)
        .arg("a"));
    assert_eq!(out.status.code(), Some(1));
    assert_ne!(text(&out.stderr), "");
}

#[test]
fn split_huge_total_is_cheap() {
    let out = run(hatchrun().args([
        "tests",
        "split",
        "--by",
        "name",
        "--index",
        "1",
        "--total",
        "4000000000",
        "a",
        "b",
        "c",
    ]));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "b\n");

    let out = run(hatchrun().args([
        "tests",
        "split",
        "--by",
        "name",
        "--index",
        "3999999999",
        "--total",
        "4000000000",
        "a",
    ]));
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "\n");
}

#[test]
fn version() {
    let out = run(hatchrun().arg("--version"));
    let expected = format!(
        "hatchrun {}\n",
        option_env!("HATCHRUN_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(text(&out.stdout), expected);
    assert!(expected.starts_with("hatchrun "));
}
