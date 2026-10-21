// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs the `hatchrun` binary end to end.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn hatchrun() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hatchrun"))
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

#[test]
fn convert_rejects_two_files_with_one_name() {
    let a = scratch("dup-a/ci.yml");
    let b = scratch("dup-b/ci.yml");
    for path in [&a, &b] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(fixture("github/ci.yml"), path).unwrap();
    }
    let out = run(hatchrun().arg("convert").arg(&a).arg(&b));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    let err = text(&out.stderr);
    assert!(err.contains(&a.display().to_string()), "{err}");
    assert!(err.contains(&b.display().to_string()), "{err}");
    assert!(err.contains("have the same file name"), "{err}");
}

#[test]
fn convert_writes_what_converts_and_reports_the_rest() {
    let good = scratch("partial.yml");
    std::fs::write(
        &good,
        "on: push\njobs:\n  build:\n    runs-on: ${{ matrix.runner }}\n    steps: [{ run: make }]\n  lint:\n    runs-on: ubuntu-latest\n    steps: [{ run: lint }]\n",
    )
    .unwrap();
    let bad = scratch("torn.yml");
    std::fs::write(&bad, "on: push\njobs:\n  t: [\n").unwrap();
    let dest = scratch("partial-out.yml");
    let out = run(hatchrun()
        .arg("convert")
        .arg(&good)
        .arg(&bad)
        .arg("-o")
        .arg(&dest));
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("job `build`"), "{stderr}");
    assert!(
        stderr.contains(&format!("{}:3:", bad.display())),
        "{stderr}"
    );
    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(written.contains("partial-lint"), "{written}");
    assert!(!written.contains("partial-build"), "{written}");
}

#[test]
fn tests_split_is_not_a_subcommand() {
    let out = run(hatchrun().args(["tests", "split", "a"]));
    assert!(!out.status.success());
    assert_ne!(text(&out.stderr), "");
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

#[test]
fn convert_creates_the_output_directory() {
    let dir = scratch("fresh-repo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::copy(
        fixture("github/ci.yml"),
        dir.join(".github/workflows/ci.yml"),
    )
    .unwrap();
    let out = run(hatchrun()
        .args(["convert", "-o", ".hatchrun/config.yml"])
        .current_dir(&dir));
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(dir.join(".hatchrun/config.yml").is_file());
}

#[test]
fn convert_fails_on_what_validate_would_refuse() {
    let mut many = String::from("on: push\njobs:\n");
    for j in 0..257 {
        write!(
            many,
            "  j{j}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: x\n"
        )
        .unwrap();
    }
    let src = scratch("many.yml");
    std::fs::write(&src, many).unwrap();
    let dest = scratch("many-out.yml");
    let out = run(hatchrun().arg("convert").arg(&src).arg("-o").arg(&dest));
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("workflow `many` plans more than 256 jobs; the limit is 256"),
        "{stderr}"
    );
    let out = run(hatchrun().arg("validate").arg(&dest));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn oversized_files_are_refused_before_reading_them_whole() {
    let big = scratch("big.yml");
    std::fs::write(&big, format!("# {}\n", "x".repeat(2 << 20))).unwrap();
    for command in ["validate", "convert"] {
        let out = run(hatchrun().arg(command).arg(&big));
        assert_eq!(out.status.code(), Some(1), "{command}");
        let err = text(&out.stderr);
        assert!(
            err.contains("bytes; the limit is 1048576"),
            "{command}: {err}"
        );
    }
}

#[test]
fn convert_skips_an_oversized_file_and_converts_the_rest() {
    let big = scratch("huge.yml");
    std::fs::write(&big, format!("# {}\n", "x".repeat(2 << 20))).unwrap();
    let out_file = scratch("oversized-out.yml");
    let out = run(hatchrun()
        .arg("convert")
        .arg(&big)
        .arg(fixture("github/ci.yml"))
        .arg("-o")
        .arg(&out_file));
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("bytes; the limit is 1048576"), "{err}");
    let written = std::fs::read_to_string(&out_file).unwrap();
    assert!(written.contains("workflows"), "{written}");
    let out = run(hatchrun().arg("validate").arg(&out_file));
    assert!(out.status.success(), "{}", text(&out.stdout));
}
