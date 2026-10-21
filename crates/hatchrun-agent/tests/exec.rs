// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs `hatchrun-agent exec` as a binary.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use hatchrun_workflow::{Conclusion, JobDocument, JobResult, PlannedStep};

/// A state directory the test owns. The path is canonical, since the
/// agent refuses links on the way to it.
fn state_dir(dir: &Path) -> std::path::PathBuf {
    dir.canonicalize().unwrap().join("state")
}

fn exec(dir: &Path, doc: &str) -> Output {
    exec_with(dir, doc, &[])
}

fn exec_with(dir: &Path, doc: &str, extra: &[&str]) -> Output {
    let job = dir.join("job.json");
    std::fs::write(&job, doc).unwrap();
    let env = dir.join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .arg("--state-dir")
        .arg(state_dir(dir))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.join("result.json"))
        .args(extra)
        .output()
        .unwrap()
}

fn json(doc: &JobDocument) -> String {
    serde_json::to_string(doc).unwrap()
}

#[test]
fn empty_job_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), &json(&make::document(Vec::new())));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let result = std::fs::read_to_string(dir.path().join("result.json")).unwrap();
    assert_eq!(
        result,
        r#"{"conclusion":"success","outputs":{},"steps":[],"annotations":[]}"#
    );
}

#[test]
fn layout_is_created() {
    let dir = tempfile::tempdir().unwrap();
    exec(dir.path(), &json(&make::document(Vec::new())));
    let work = dir.path().join("work");
    assert!(work.join("repo/repo").is_dir());
    assert!(work.join("_temp").is_dir());
    assert!(work.join("_actions").is_dir());
}

#[test]
fn bad_document_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), "{}");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing field"), "{stderr}");
}

#[test]
fn approval_job_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(dir.path(), &json(&make::approval()));
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("approval jobs do not run on a runner"),
        "{stderr}"
    );
}

fn result(dir: &Path) -> JobResult {
    let text = std::fs::read_to_string(dir.join("result.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_steps(dir: &Path, steps: Vec<hatchrun_workflow::PlannedStep>) -> Output {
    exec(dir, &json(&make::document(steps)))
}

#[test]
fn run_step_output_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo hi; exit 3")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "hi"), "{}", stdout(&out));
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Failure);
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
}

#[test]
fn env_layers() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(Vec::new());
    doc.job.env.insert("A".to_owned(), "job".to_owned());
    let mut step = make::run("echo $A $B");
    step.env.insert("A".to_owned(), "step".to_owned());
    step.env.insert("B".to_owned(), "${{ env.A }}".to_owned());
    doc.job.steps = vec![make::planned(step)];
    let out = exec(dir.path(), &json(&doc));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).lines().any(|l| l == "step job"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn github_env_and_path_reach_later_steps() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(
        dir.path(),
        vec![
            make::step("echo X=1 >> \"$GITHUB_ENV\"; echo /opt/x >> \"$GITHUB_PATH\""),
            make::step("echo \"x=$X\"; echo \"path=$PATH\""),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "x=1"), "{text}");
    assert!(
        text.lines().any(|l| l.starts_with("path=/opt/x:")),
        "{text}"
    );
}

#[test]
fn step_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = make::run("echo v=7 >> \"$GITHUB_OUTPUT\"");
    first.id = Some("a".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::planned(first),
            make::step("echo got ${{ steps.a.outputs.v }}"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).lines().any(|l| l == "got 7"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn continue_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("exit 1");
    step.continue_on_error = true;
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Success);
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
    assert_eq!(result.steps[0].conclusion, Conclusion::Success);
}

#[test]
fn if_always_runs_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut always = make::run("echo cleanup");
    always.if_ = Some("always()".to_owned());
    let mut skipped = make::run("echo skipme");
    skipped.name = Some("skipped".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::step("exit 1"),
            make::planned(skipped),
            make::planned(always),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "cleanup"), "{text}");
    assert!(!text.contains("skipme"), "{text}");
    let result = result(dir.path());
    assert_eq!(result.steps[1].outcome, Conclusion::Skipped);
    assert_eq!(result.steps[2].outcome, Conclusion::Success);
}

#[test]
fn timeout_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut slow = make::run("trap '' TERM; sleep 600");
    slow.timeout_minutes = Some(1);
    let mut always = make::run("echo after");
    always.if_ = Some("always()".to_owned());
    let doc = make::document(vec![make::planned(slow), make::planned(always)]);
    let began = Instant::now();
    let out = exec_with(dir.path(), &json(&doc), &["--minute", "2s"]);
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "after"));
    let result = result(dir.path());
    assert_eq!(result.steps[0].outcome, Conclusion::Failure);
}

#[test]
fn background_child_does_not_hold_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let began = Instant::now();
    let out = run_steps(
        dir.path(),
        vec![make::step(
            "sleep 600 & echo $! > \"$RUNNER_TEMP/bg.pid\"; echo done",
        )],
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(stdout(&out).lines().any(|l| l == "done"));
    let pid = std::fs::read_to_string(dir.path().join("work/_temp/bg.pid")).unwrap();
    let gone = (0..20).any(|_| {
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if alive {
            std::thread::sleep(Duration::from_millis(100));
        }
        !alive
    });
    assert!(gone, "background process {} is still running", pid.trim());
}

#[test]
fn working_directory_and_custom_shell() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("import os; print(os.getcwd())");
    step.working_directory = Some("sub".to_owned());
    step.shell = Some("python3 {0}".to_owned());
    let out = run_steps(
        dir.path(),
        vec![make::step("mkdir sub"), make::planned(step)],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let workspace = std::fs::canonicalize(dir.path().join("work/repo/repo")).unwrap();
    let want = workspace.join("sub").to_string_lossy().into_owned();
    assert!(stdout(&out).lines().any(|l| l == want), "{}", stdout(&out));
}

#[test]
fn add_mask_hides_later_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("echo \"::add-mask::v4lue\"; echo v4lue");
    step.name = Some("mask".to_owned());
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    let text = stdout(&out);
    assert_eq!(text.lines().filter(|l| *l == "***").count(), 1, "{text}");
    assert!(
        !text.contains("add-mask") && !text.contains("v4lue"),
        "{text}"
    );
}

#[test]
fn bad_expression_fails_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo ${{ nope( }}")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).contains("at byte"), "{}", stdout(&out));
    assert_eq!(result(dir.path()).steps[0].outcome, Conclusion::Failure);
}

#[test]
fn secrets_are_masked_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(Vec::new());
    doc.secrets.insert("K".to_owned(), "s3cr3t".to_owned());
    let mut step = make::run("echo ${{ secrets.K }}; echo \"::error title=s3cr3t::s3cr3t\"");
    step.name = Some("show".to_owned());
    let mut bad = make::run("echo ${{ secrets.K.nope( }}");
    bad.name = Some("bad".to_owned());
    doc.job.steps = vec![make::planned(step), make::planned(bad)];
    let out = exec(dir.path(), &json(&doc));
    let text = stdout(&out);
    assert!(!text.contains("s3cr3t"), "{text}");
    assert!(text.lines().any(|l| l == "***"), "{text}");
    assert!(text.lines().any(|l| l == "Error: ***"), "{text}");
    let result = result(dir.path());
    assert!(!format!("{result:?}").contains("s3cr3t"));
}

#[test]
fn sigterm_cancels_the_running_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut always = make::run("echo after");
    always.if_ = Some("always()".to_owned());
    let doc = make::document(vec![make::step("sleep 600"), make::planned(always)]);
    let job = dir.path().join("job.json");
    std::fs::write(&job, json(&doc)).unwrap();
    let env = dir.path().join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.path().join("work"))
        .arg("--state-dir")
        .arg(state_dir(dir.path()))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.path().join("result.json"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let began = Instant::now();
    let out = child.wait_with_output().unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).lines().any(|l| l == "after"));
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Cancelled);
    assert_eq!(result.steps[0].outcome, Conclusion::Cancelled);
    assert_eq!(result.steps[1].outcome, Conclusion::Success);
}

#[test]
fn set_output_works_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = make::run("echo \"::set-output name=o::5\"");
    first.id = Some("a".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            make::planned(first),
            make::step("echo got ${{ steps.a.outputs.o }}"),
        ],
    );
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "got 5"), "{text}");
    assert!(
        text.contains("`set-output` command is deprecated"),
        "{text}"
    );
}

#[test]
fn script_is_private_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run(
        "import os, stat, sys\nprint('mode', oct(stat.S_IMODE(os.stat(sys.argv[0]).st_mode)))\nprint('file', sys.argv[0])",
    );
    step.shell = Some("python3 {0}".to_owned());
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "mode 0o600"), "{text}");
    let file = text.lines().find_map(|l| l.strip_prefix("file ")).unwrap();
    assert!(!Path::new(file).exists(), "{file} was not removed");
}

#[test]
fn script_is_removed_when_the_step_fails() {
    let dir = tempfile::tempdir().unwrap();
    run_steps(dir.path(), vec![make::step("exit 1")]);
    let left: Vec<_> = std::fs::read_dir(dir.path().join("work/_temp"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| Path::new(n).extension().is_some_and(|e| e == "sh"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn bad_step_name_warns() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = make::run("echo ok");
    step.name = Some("${{ nope( }}".to_owned());
    let out = run_steps(dir.path(), vec![make::planned(step)]);
    assert!(stdout(&out).contains("Warning: name:"), "{}", stdout(&out));
}

#[test]
fn no_process_survives_an_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(
        dir.path(),
        vec![make::step(
            "(trap '' HUP; sleep 600) & echo $! > \"$RUNNER_TEMP/p.pid\"",
        )],
    );
    assert_eq!(out.status.code(), Some(0));
    let pid = std::fs::read_to_string(dir.path().join("work/_temp/p.pid")).unwrap();
    let gone = (0..20).any(|_| {
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if alive {
            std::thread::sleep(Duration::from_millis(100));
        }
        !alive
    });
    assert!(gone);
}

/// Whether `node` can be found. Prints why a test is skipped.
fn has_node() -> bool {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("node").is_file()));
    if !found {
        eprintln!("skipping: no node on PATH");
    }
    found
}

/// Writes a local action `name` under the workspace of `dir`.
fn local_action(dir: &Path, name: &str, yml: &str, files: &[(&str, &str)]) {
    let act = dir.join("work/repo/repo").join(name);
    std::fs::create_dir_all(&act).unwrap();
    std::fs::write(act.join("action.yml"), yml).unwrap();
    for (file, text) in files {
        std::fs::write(act.join(file), text).unwrap();
    }
}

fn lines(out: &Output) -> Vec<String> {
    stdout(out).lines().map(str::to_owned).collect()
}

#[test]
fn js_action_inputs_and_outputs() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: greet\ninputs:\n  who is:\n    description: x\nruns:\n  using: node20\n  main: index.js\n",
        &[(
            "index.js",
            "console.log('who=' + process.env.INPUT_WHO_IS);\n\
             console.log('path=' + process.env.GITHUB_ACTION_PATH);\n\
             require('fs').appendFileSync(process.env.GITHUB_OUTPUT, 'greeting=hi\\n');\n",
        )],
    );
    let mut step = make::uses("g", "./act");
    step.with.insert("who is".to_owned(), "me".into());
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(step),
            make::step("echo got ${{ steps.g.outputs.greeting }}"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    let text = lines(&out);
    assert!(text.contains(&"who=me".to_owned()), "{text:?}");
    assert!(text.contains(&"got hi".to_owned()), "{text:?}");
    assert!(
        text.iter()
            .any(|l| l.starts_with("path=") && l.ends_with("/repo/repo/act")),
        "{text:?}"
    );
}

#[test]
fn non_string_inputs_are_json_text() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: n\nruns:\n  using: node20\n  main: index.js\n",
        &[(
            "index.js",
            "console.log('n=' + process.env.INPUT_N + ' b=' + process.env.INPUT_B + \
             ' l=' + process.env.INPUT_L);\n",
        )],
    );
    let mut step = make::uses("g", "./act");
    step.with.insert("n".to_owned(), 3.into());
    step.with.insert("b".to_owned(), true.into());
    step.with
        .insert("l".to_owned(), serde_json::json!([1, "x"]));
    let out = run_steps(dir.path(), vec![PlannedStep::Uses(step)]);
    assert!(
        lines(&out).contains(&r#"n=3 b=true l=[1,"x"]"#.to_owned()),
        "{}",
        stdout(&out)
    );
}

#[test]
fn input_defaults_are_evaluated() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: d\ninputs:\n  repo:\n    default: ${{ github.repository }}\n  secret:\n    default: ${{ secrets.S }}\nruns:\n  using: node20\n  main: index.js\n",
        &[(
            "index.js",
            "console.log('repo=' + process.env.INPUT_REPO);\n\
             console.log('secret=[' + process.env.INPUT_SECRET + ']');\n",
        )],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("g", "./act"))],
    );
    let text = lines(&out);
    assert!(text.contains(&"repo=acme/repo".to_owned()), "{text:?}");
    assert!(text.contains(&"secret=[]".to_owned()), "{text:?}");
}

#[test]
fn caller_expressions_see_caller_contexts() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: c\nruns:\n  using: node20\n  main: index.js\n",
        &[(
            "index.js",
            "console.log('v=' + process.env.INPUT_V + ' e=' + process.env.E);\n",
        )],
    );
    let mut doc = make::document(Vec::new());
    doc.secrets.insert("S".to_owned(), "hunter2".to_owned());
    let mut first = make::run("echo val=1 >> \"$GITHUB_OUTPUT\"");
    first.id = Some("a".to_owned());
    let mut step = make::uses("g", "./act");
    step.with
        .insert("v".to_owned(), "${{ steps.a.outputs.val }}".into());
    step.env
        .insert("E".to_owned(), "${{ github.actor }}".to_owned());
    doc.job.steps = vec![make::planned(first), PlannedStep::Uses(step)];
    let out = exec(dir.path(), &json(&doc));
    assert!(
        lines(&out).contains(&"v=1 e=octocat".to_owned()),
        "{}",
        stdout(&out)
    );
}

fn hooked(dir: &Path, name: &str) {
    local_action(
        dir,
        name,
        &format!(
            "name: {name}\nruns:\n  using: node20\n  pre: pre.js\n  main: main.js\n  post: post.js\n"
        ),
        &[
            ("pre.js", &format!("console.log('pre{name}');\n")),
            (
                "main.js",
                &format!(
                    "console.log('main{name}');\n\
                     require('fs').appendFileSync(process.env.GITHUB_STATE, 'x=saved{name}\\n');\n"
                ),
            ),
            (
                "post.js",
                &format!(
                    "console.log('post{name}');\nconsole.log('state' + process.env.STATE_x);\n"
                ),
            ),
        ],
    );
}

#[test]
fn pre_and_post_order() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    hooked(dir.path(), "1");
    hooked(dir.path(), "2");
    let out = run_steps(
        dir.path(),
        vec![
            make::step("echo first"),
            PlannedStep::Uses(make::uses("a", "./1")),
            PlannedStep::Uses(make::uses("b", "./2")),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    let text: Vec<String> = lines(&out)
        .into_iter()
        .filter(|l| !l.starts_with("=="))
        .collect();
    assert_eq!(
        text,
        [
            "pre1",
            "pre2",
            "first",
            "main1",
            "main2",
            "post2",
            "statesaved2",
            "post1",
            "statesaved1"
        ]
    );
    assert_eq!(result(dir.path()).steps.len(), 3);
}

#[test]
fn post_runs_after_failure() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    hooked(dir.path(), "1");
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(make::uses("a", "./1")),
            make::step("exit 1"),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        lines(&out).contains(&"post1".to_owned()),
        "{}",
        stdout(&out)
    );
    assert_eq!(result(dir.path()).conclusion, Conclusion::Failure);
}

#[test]
fn post_if_decides_and_skipped_main_queues_no_post() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "ok",
        "name: ok\nruns:\n  using: node20\n  main: main.js\n  post: post.js\n  post-if: success()\n",
        &[
            ("main.js", "console.log('main-ok');\n"),
            ("post.js", "console.log('post-ok');\n"),
        ],
    );
    hooked(dir.path(), "1");
    let mut skipped = make::uses("s", "./1");
    skipped.if_ = Some("false".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(make::uses("a", "./ok")),
            PlannedStep::Uses(skipped),
            make::step("exit 1"),
        ],
    );
    let text = lines(&out);
    assert!(text.contains(&"main-ok".to_owned()), "{text:?}");
    assert!(!text.contains(&"post-ok".to_owned()), "{text:?}");
    assert!(!text.contains(&"post1".to_owned()), "{text:?}");
}

#[test]
fn failing_node_action_fails_the_step() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: f\nruns:\n  using: node20\n  main: index.js\n",
        &[("index.js", "console.log('boom'); process.exit(3);\n")],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("a", "./act"))],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("Process completed with exit code 3."),
        "{}",
        stdout(&out)
    );
}

#[test]
fn docker_action_fails_the_step() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "act",
        "name: d\nruns:\n  using: docker\n  image: Dockerfile\n",
        &[],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("a", "./act"))],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains(
            "`./act` is a Docker container action; Docker services and containers come in a later release"
        ),
        "{}",
        stdout(&out)
    );
}

#[tokio::test]
async fn remote_action_is_fetched_and_readable() {
    use std::os::unix::fs::PermissionsExt;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    if !has_node() {
        return;
    }
    let mut tar = tar::Builder::new(Vec::new());
    let mut add = |name: &str, kind: tar::EntryType, mode: u32, body: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(body.len() as u64);
        tar.append_data(&mut h, name, body).unwrap();
    };
    add("acme-act-abc/", tar::EntryType::Directory, 0o700, b"");
    add(
        "acme-act-abc/action.yml",
        tar::EntryType::Regular,
        0o600,
        b"name: r\nruns:\n  using: node24\n  main: index.js\n",
    );
    add(
        "acme-act-abc/index.js",
        tar::EntryType::Regular,
        0o600,
        b"console.log('remote ' + process.env.GITHUB_ACTION_REPOSITORY + '@' + process.env.GITHUB_ACTION_REF);\n",
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
    let body = gz.finish().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/act/tarball/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![PlannedStep::Uses(make::uses("r", "acme/act@v1"))]);
    doc.github.api_url = server.uri();
    let job = json(&doc);
    let root = dir.path().to_owned();
    let out = tokio::task::spawn_blocking(move || exec(&root, &job))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    assert!(
        lines(&out).contains(&"remote acme/act@v1".to_owned()),
        "{}",
        stdout(&out)
    );

    let tree = dir.path().join("work/_actions/acme/act/v1/tree");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&tree) & 0o055, 0o055);
    assert_eq!(mode(&tree.join("index.js")) & 0o044, 0o044);
    assert_eq!(
        mode(&dir.path().join("work/_actions/acme/act")) & 0o055,
        0o055
    );
}

/// Starts the agent on `doc` with minutes of one second.
fn start_agent(dir: &Path, doc: &JobDocument) -> std::process::Child {
    let job = dir.join("job.json");
    std::fs::write(&job, json(doc)).unwrap();
    let env = dir.join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .arg("--state-dir")
        .arg(state_dir(dir))
        .args(["--runner-user", "", "--minute", "1s"])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.join("result.json"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

fn term(child: &std::process::Child) {
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
}

/// A job that sleeps in a step, then in the `post` of an earlier action.
fn sleeping_post(dir: &Path) -> JobDocument {
    local_action(
        dir,
        "act",
        "name: s\nruns:\n  using: node20\n  main: main.js\n  post: post.js\n",
        &[
            ("main.js", "console.log('main');\n"),
            (
                "post.js",
                "console.log('post-started');\nsetTimeout(() => {}, 600000);\n",
            ),
        ],
    );
    make::document(vec![
        PlannedStep::Uses(make::uses("a", "./act")),
        make::step("sleep 600"),
    ])
}

#[test]
fn a_post_after_a_cancel_ends_within_the_grace_period() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let doc = sleeping_post(dir.path());
    let child = start_agent(dir.path(), &doc);
    let began = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    term(&child);
    let out = child.wait_with_output().unwrap();
    let took = began.elapsed();
    assert!(took < Duration::from_secs(12), "{took:?}");
    assert!(took > Duration::from_secs(4), "{took:?}");
    assert!(lines(&out).contains(&"post-started".to_owned()));
    assert_eq!(result(dir.path()).conclusion, Conclusion::Cancelled);
}

#[test]
fn a_second_sigterm_stops_the_job_at_once() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut doc = sleeping_post(dir.path());
    doc.job.steps.push(make::step("echo never"));
    let child = start_agent(dir.path(), &doc);
    std::thread::sleep(Duration::from_secs(1));
    term(&child);
    // Wait until the post is running, then ask again.
    std::thread::sleep(Duration::from_secs(1));
    let second = Instant::now();
    term(&child);
    let out = child.wait_with_output().unwrap();
    assert!(
        second.elapsed() < Duration::from_secs(3),
        "{:?}",
        second.elapsed()
    );
    assert!(!lines(&out).contains(&"never".to_owned()));
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Cancelled);
    assert_eq!(result.steps[2].outcome, Conclusion::Skipped);
}

// Composite actions.

/// A `with:` value for a step.
fn with(step: &mut hatchrun_workflow::UsesStep, key: &str, value: &str) {
    step.with.insert(key.to_owned(), value.into());
}

#[test]
fn composite_inputs_steps_outputs() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\n\
         inputs:\n  n:\n    description: x\n\
         outputs:\n  out:\n    description: x\n    value: ${{ steps.s.outputs.v }}\n\
         runs:\n  using: composite\n  steps:\n\
         \x20   - id: s\n      shell: bash\n      run: echo \"v=${{ inputs.n }}0\" >> \"$GITHUB_OUTPUT\"\n",
        &[],
    );
    let mut step = make::uses("c", "./comp");
    with(&mut step, "n", "4");
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(step),
            make::step("echo got=${{ steps.c.outputs.out }}"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    assert!(
        lines(&out).contains(&"got=40".to_owned()),
        "{}",
        stdout(&out)
    );
    assert_eq!(result(dir.path()).steps.len(), 2);
}

#[test]
fn composite_steps_context_is_scoped() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n\
         \x20   - id: inner\n      shell: bash\n      run: echo \"v=2\" >> \"$GITHUB_OUTPUT\"\n\
         \x20   - shell: bash\n      run: |\n        echo \"sees-caller=[${{ steps.before.outputs.v }}]\"\n        echo \"sees-own=[${{ steps.inner.outputs.v }}]\"\n        echo \"secrets=${{ toJSON(secrets) }}\"\n",
        &[],
    );
    let mut before = make::run("echo v=1 >> \"$GITHUB_OUTPUT\"");
    before.id = Some("before".to_owned());
    let mut doc = make::document(vec![
        make::planned(before),
        PlannedStep::Uses(make::uses("c", "./comp")),
        make::step("echo \"sees-inner=[${{ steps.inner.outputs.v }}]\""),
    ]);
    doc.secrets.insert("S".to_owned(), "s3cr3t".to_owned());
    let out = exec(dir.path(), &json(&doc));
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    let text = lines(&out);
    for want in [
        "sees-caller=[]",
        "sees-own=[2]",
        "secrets={}",
        "sees-inner=[]",
    ] {
        assert!(text.contains(&want.to_owned()), "{want}: {text:?}");
    }
    assert!(!stdout(&out).contains("s3cr3t"), "{}", stdout(&out));
}

#[test]
fn composite_run_needs_shell() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n",
        &[],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("c", "./comp"))],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stdout(&out).contains("Required property is missing: shell"),
        "{}",
        stdout(&out)
    );
    assert!(!lines(&out).contains(&"hi".to_owned()));
}

#[test]
fn composite_calls_a_js_action() {
    if !has_node() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    hooked(dir.path(), "1");
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n    - uses: ./1\n    - shell: bash\n      run: echo after-js\n",
        &[],
    );
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(make::uses("c", "./comp")),
            make::step("echo last"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    let text: Vec<String> = lines(&out)
        .into_iter()
        .filter(|l| !l.starts_with("=="))
        .collect();
    // The nested action has no `pre`; its `post` runs at the job's end.
    assert_eq!(text, ["main1", "after-js", "last", "post1", "statesaved1"]);
    assert_eq!(result(dir.path()).steps.len(), 2);
}

#[test]
fn nesting_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: loop\nruns:\n  using: composite\n  steps:\n    - uses: ./comp\n",
        &[],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("c", "./comp"))],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stdout(&out).contains("`./comp` is nested more than 10 composite actions deep"),
        "{}",
        stdout(&out)
    );
    assert_eq!(result(dir.path()).conclusion, Conclusion::Failure);
}

#[test]
fn ten_levels_of_nesting_run() {
    let dir = tempfile::tempdir().unwrap();
    for level in 1..=10 {
        let body = if level < 10 {
            format!("    - uses: ./c{}\n", level + 1)
        } else {
            "    - shell: bash\n      run: echo deepest\n".to_owned()
        };
        local_action(
            dir.path(),
            &format!("c{level}"),
            &format!("name: c\nruns:\n  using: composite\n  steps:\n{body}"),
            &[],
        );
    }
    let out = run_steps(dir.path(), vec![PlannedStep::Uses(make::uses("c", "./c1"))]);
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    assert!(lines(&out).contains(&"deepest".to_owned()));
}

#[test]
fn composite_fan_out_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: fan\nruns:\n  using: composite\n  steps:\n\
         \x20   - uses: ./comp\n      if: always()\n\
         \x20   - uses: ./comp\n      if: always()\n",
        &[],
    );
    let out = run_steps(
        dir.path(),
        vec![PlannedStep::Uses(make::uses("c", "./comp"))],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stdout(&out).contains("`./comp` would run more than 1000 composite actions in one job"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn composite_env_github_env_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n\
         \x20   - shell: bash\n      run: echo \"a=$A b=$B path=$GITHUB_ACTION_PATH\"\n      env:\n        B: inner\n\
         \x20   - shell: bash\n      run: echo X=set >> \"$GITHUB_ENV\"\n\
         \x20   - shell: bash\n      run: echo \"x=$X\"\n\
         \x20   - shell: bash\n      run: exit 1\n      continue-on-error: true\n\
         \x20   - shell: bash\n      run: echo after-tolerated-failure\n\
         \x20   - shell: bash\n      run: exit 3\n\
         \x20   - shell: bash\n      run: echo not-run\n\
         \x20   - if: always()\n      shell: bash\n      run: echo cleanup\n",
        &[],
    );
    let mut step = make::uses("c", "./comp");
    step.env.insert("A".to_owned(), "outer".to_owned());
    step.env.insert("B".to_owned(), "outer".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(step),
            make::step("echo caller-x=$X caller-a=[$A]"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}\n{}", stdout(&out));
    let text = lines(&out);
    assert!(
        text.iter()
            .any(|l| l.starts_with("a=outer b=inner path=") && l.ends_with("/repo/repo/comp")),
        "{text:?}"
    );
    assert!(text.contains(&"x=set".to_owned()), "{text:?}");
    assert!(
        text.contains(&"after-tolerated-failure".to_owned()),
        "{text:?}"
    );
    assert!(!text.contains(&"not-run".to_owned()), "{text:?}");
    assert!(text.contains(&"cleanup".to_owned()), "{text:?}");
    let result = result(dir.path());
    assert_eq!(result.steps[0].conclusion, Conclusion::Failure);
}

#[test]
fn composite_github_env_persists_for_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo X=set >> \"$GITHUB_ENV\"\n",
        &[],
    );
    let mut step = make::uses("c", "./comp");
    step.env.insert("A".to_owned(), "outer".to_owned());
    let out = run_steps(
        dir.path(),
        vec![
            PlannedStep::Uses(step),
            make::step("echo caller-x=$X caller-a=[$A]"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}\n{}", stdout(&out));
    assert!(
        lines(&out).contains(&"caller-x=set caller-a=[]".to_owned()),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_cancel_stops_a_composite_step() {
    let dir = tempfile::tempdir().unwrap();
    local_action(
        dir.path(),
        "comp",
        "name: c\nruns:\n  using: composite\n  steps:\n\
         \x20   - shell: bash\n      run: echo inner-started; sleep 600\n\
         \x20   - shell: bash\n      run: echo never\n",
        &[],
    );
    let doc = make::document(vec![
        PlannedStep::Uses(make::uses("c", "./comp")),
        make::step("echo also-never"),
    ]);
    let child = start_agent(dir.path(), &doc);
    std::thread::sleep(Duration::from_secs(2));
    term(&child);
    let out = child.wait_with_output().unwrap();
    let text = lines(&out);
    assert!(text.contains(&"inner-started".to_owned()), "{text:?}");
    assert!(!text.contains(&"never".to_owned()), "{text:?}");
    assert!(!text.contains(&"also-never".to_owned()), "{text:?}");
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Cancelled);
    assert_eq!(result.steps[0].outcome, Conclusion::Cancelled);
}

// Built-in steps.

use hatchrun_workflow::{CacheStep, Need, WorkspaceStep};

fn cache(key: &str, restore_keys: &[&str], paths: &[&str]) -> PlannedStep {
    PlannedStep::Cache(CacheStep {
        key: key.to_owned(),
        restore_keys: restore_keys.iter().map(|k| (*k).to_owned()).collect(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    })
}

fn persist(root: &str, paths: &[&str]) -> PlannedStep {
    PlannedStep::Workspace(WorkspaceStep::Persist {
        root: root.to_owned(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    })
}

fn need() -> Need {
    Need {
        result: Conclusion::Success,
        outputs: std::collections::BTreeMap::new(),
    }
}

fn attach(at: &str) -> PlannedStep {
    PlannedStep::Workspace(WorkspaceStep::Attach { at: at.to_owned() })
}

fn stored(dir: &Path, steps: Vec<PlannedStep>) -> Output {
    let store = dir.join("store");
    exec_with(
        dir,
        &json(&make::document(steps)),
        &["--store", store.to_str().unwrap()],
    )
}

/// Runs `steps` in a fresh work directory that shares `store`.
fn shared(store: &Path, steps: Vec<PlannedStep>) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(&make::document(steps)),
        &["--store", store.to_str().unwrap()],
    );
    (dir, out)
}

fn cache_file(store: &Path, key: &str) -> std::path::PathBuf {
    store
        .join("cache/acme%2Frepo")
        .join(format!("{key}.tar.gz"))
}

#[test]
fn cache_restore_then_save() {
    let store = tempfile::tempdir().unwrap();
    let (_first, out) = shared(
        store.path(),
        vec![
            cache("k1", &[], &["target"]),
            make::step("mkdir target && echo built > target/a"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("Cache not found for key: k1"));
    assert!(stdout(&out).contains("Cache saved with key: k1"));
    assert!(cache_file(store.path(), "k1").is_file());

    let (_second, out) = shared(
        store.path(),
        vec![cache("k1", &[], &["target"]), make::step("cat target/a")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("Cache restored from key: k1"));
    assert!(
        stdout(&out).lines().any(|l| l == "built"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn cache_restore_keys_take_the_newest_prefix_match() {
    let store = tempfile::tempdir().unwrap();
    for (key, text) in [("deps-a", "old"), ("deps-b", "new")] {
        let script = format!("echo {text} > file");
        let (_dir, out) = shared(
            store.path(),
            vec![cache(key, &[], &["file"]), make::step(&script)],
        );
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    }
    let older = std::fs::File::options()
        .write(true)
        .open(cache_file(store.path(), "deps-a"))
        .unwrap();
    older
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(5000))
        .unwrap();

    let (_dir, out) = shared(
        store.path(),
        vec![
            cache("deps-c", &["deps-"], &["file"]),
            make::step("cat file"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("Cache restored from key: deps-b"));
    assert!(stdout(&out).lines().any(|l| l == "new"), "{}", stdout(&out));
    assert!(cache_file(store.path(), "deps-c").is_file());
}

#[test]
fn cache_not_saved_on_failure_or_exact_hit() {
    let store = tempfile::tempdir().unwrap();
    let (_dir, out) = shared(
        store.path(),
        vec![
            cache("k", &[], &["t"]),
            make::step("mkdir t && echo x > t/a; exit 1"),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(!cache_file(store.path(), "k").exists());

    let (_dir, out) = shared(
        store.path(),
        vec![
            cache("k", &[], &["t"]),
            make::step("mkdir t && echo x > t/a"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let saved = cache_file(store.path(), "k");
    assert!(saved.is_file());
    let long_ago = std::time::SystemTime::now() - Duration::from_secs(5000);
    std::fs::File::options()
        .write(true)
        .open(&saved)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    let (_dir, out) = shared(
        store.path(),
        vec![cache("k", &[], &["t"]), make::step("echo y > t/a || true")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("Cache restored from key: k"));
    assert_eq!(
        std::fs::metadata(&saved).unwrap().modified().unwrap(),
        long_ago,
        "an exact hit saves nothing"
    );
}

#[test]
fn workspace_persist_attach() {
    let store = tempfile::tempdir().unwrap();
    let (_dir, out) = shared(
        store.path(),
        vec![
            make::step("mkdir dist && echo built > dist/x"),
            persist(".", &["dist"]),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(store.path().join("workspace/1/build.1.tar.gz").is_file());

    let mut doc = make::document(vec![attach("ws"), make::step("cat ws/dist/x")]);
    doc.job.id = "test".to_owned();
    doc.job.name = "test".to_owned();
    doc.job.requires = vec!["build".to_owned()];
    doc.needs.insert("build".to_owned(), need());
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(&doc),
        &["--store", store.path().to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stdout(&out).lines().any(|l| l == "built"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn workspace_attach_applies_required_jobs_in_order() {
    let store = tempfile::tempdir().unwrap();
    for (name, text) in [("one", "from one"), ("two", "from two")] {
        let mut doc = make::document(vec![
            make::step(&format!("echo '{text}' > f")),
            persist(".", &["f"]),
        ]);
        doc.job.id = format!("{name} (x)");
        name.clone_into(&mut doc.job.name);
        let dir = tempfile::tempdir().unwrap();
        let out = exec_with(
            dir.path(),
            &json(&doc),
            &["--store", store.path().to_str().unwrap()],
        );
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    }
    let mut doc = make::document(vec![attach("."), make::step("cat f")]);
    doc.job.id = "last".to_owned();
    doc.job.name = "last".to_owned();
    doc.job.requires = vec!["two (x)".to_owned(), "one (x)".to_owned()];
    for name in ["one", "two"] {
        doc.needs.insert(name.to_owned(), need());
    }
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(&doc),
        &["--store", store.path().to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stdout(&out).lines().any(|l| l == "from one"),
        "the later file wins: {}",
        stdout(&out)
    );
}

#[test]
fn workspace_attach_with_nothing_persisted_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let out = stored(dir.path(), vec![attach("ws")]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("nothing to attach"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn test_results_are_stored() {
    let dir = tempfile::tempdir().unwrap();
    let out = stored(
        dir.path(),
        vec![
            make::step(
                "mkdir -p r/sub && echo '<a/>' > r/a.xml && echo '<b/>' > r/sub/b.XML && echo no > r/c.txt",
            ),
            PlannedStep::TestResults {
                path: "r".to_owned(),
            },
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let archive = dir.path().join("store/test-results/1/build.1.tar.gz");
    let list = Command::new("tar")
        .arg("tzf")
        .arg(&archive)
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&list.stdout).into_owned();
    assert!(
        names.contains("a.xml") && names.contains("sub/b.XML"),
        "{names}"
    );
    assert!(!names.contains("c.txt"), "{names}");

    let out = stored(
        dir.path(),
        vec![PlannedStep::TestResults {
            path: "nothing".to_owned(),
        }],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("no test results found at nothing"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn no_store_fails_the_builtin_step() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_steps(dir.path(), vec![cache("k", &[], &["t"])]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("this agent has no store for cache steps"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn bad_keys_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for key in ["a/b", "a\\b", "caf\u{e9}"] {
        let out = stored(dir.path(), vec![cache(key, &[], &["t"])]);
        assert_eq!(out.status.code(), Some(1), "{key}");
        assert!(stdout(&out).contains("Error: key"), "{}", stdout(&out));
    }
    assert!(!dir.path().join("store").join("cache").exists());
}

#[test]
fn paths_cannot_leave_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("work/repo")).unwrap();
    std::fs::write(dir.path().join("work/repo/secret"), "s").unwrap();
    for step in [
        cache("k", &[], &["../secret"]),
        cache("k2", &[], &["link/secret"]),
        persist("..", &["secret"]),
        persist("link", &["secret"]),
        attach("../x"),
        attach("/etc"),
    ] {
        let out = stored(
            dir.path(),
            vec![make::step("ln -sfn ../.. link"), step.clone()],
        );
        let text = stdout(&out);
        let saved = dir.path().join("store").exists()
            && std::fs::read_dir(dir.path().join("store/cache/acme%2Frepo"))
                .map_or(0, Iterator::count)
                > 0;
        assert!(!saved, "{step:?}: {text}");
        if matches!(step, PlannedStep::Cache(_)) {
            // A failed save is a warning, and the job stays green.
            assert!(text.contains("Failed to save cache"), "{step:?}: {text}");
        } else {
            assert_eq!(out.status.code(), Some(1), "{step:?}: {text}");
        }
    }
}

#[test]
fn restore_refuses_a_planted_symlink_component() {
    let store = tempfile::tempdir().unwrap();
    let (_dir, out) = shared(
        store.path(),
        vec![
            make::step("mkdir -p d/sub && echo x > d/sub/f"),
            persist(".", &["d"]),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let outside = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![
        make::step(&format!("ln -s {} d", outside.path().display())),
        attach("."),
    ]);
    doc.job.id = "t".to_owned();
    doc.job.name = "t".to_owned();
    doc.job.requires = vec!["build".to_owned()];
    doc.needs.insert("build".to_owned(), need());
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(&doc),
        &["--store", store.path().to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("d is a symlink"), "{}", stdout(&out));
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

    // The same planted link on the attach target.
    let mut doc = make::document(vec![
        make::step(&format!("ln -s {} at", outside.path().display())),
        attach("at/deeper"),
    ]);
    doc.job.requires = vec!["build".to_owned()];
    doc.job.id = "t".to_owned();
    doc.job.name = "t".to_owned();
    doc.needs.insert("build".to_owned(), need());
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(&doc),
        &["--store", store.path().to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("at is a symlink"), "{}", stdout(&out));
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn restore_replaces_a_planted_file_symlink_without_following_it() {
    let store = tempfile::tempdir().unwrap();
    let (_dir, out) = shared(
        store.path(),
        vec![
            cache("k", &[], &["d"]),
            make::step("mkdir d && echo real > d/f"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("victim");
    std::fs::write(&target, "keep").unwrap();
    let script = format!("mkdir d && ln -s {} d/f", target.display());
    let dir = tempfile::tempdir().unwrap();
    let doc = make::document(vec![make::step(&script), cache("k", &[], &["d"])]);
    let out = exec_with(
        dir.path(),
        &json(&doc),
        &["--store", store.path().to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    let placed = dir.path().join("work/repo/repo/d/f");
    assert!(!std::fs::symlink_metadata(&placed).unwrap().is_symlink());
    assert_eq!(std::fs::read_to_string(placed).unwrap(), "real\n");
}

#[test]
fn symlinks_are_stored_as_links() {
    let store = tempfile::tempdir().unwrap();
    let (_dir, out) = shared(
        store.path(),
        vec![
            cache("k", &[], &["d"]),
            make::step("mkdir d && ln -s /etc/passwd d/p && ln -s f d/q"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let list = Command::new("tar")
        .arg("tvzf")
        .arg(cache_file(store.path(), "k"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&list.stdout).into_owned();
    assert!(text.contains("d/p -> /etc/passwd"), "{text}");
}

fn job_named(id: &str, name: &str, steps: Vec<PlannedStep>) -> JobDocument {
    let mut doc = make::document(steps);
    id.clone_into(&mut doc.job.id);
    name.clone_into(&mut doc.job.name);
    doc
}

fn run_in_new_dir(store: &Path, doc: &JobDocument) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let out = exec_with(
        dir.path(),
        &json(doc),
        &["--store", store.to_str().unwrap()],
    );
    (dir, out)
}

fn requiring(ids: &[&str], steps: Vec<PlannedStep>) -> JobDocument {
    let mut doc = job_named("last", "last", steps);
    doc.job.requires = ids.iter().map(|id| (*id).to_owned()).collect();
    doc.needs.insert("m".to_owned(), need());
    doc
}

#[test]
fn matrix_jobs_and_repeated_steps_keep_their_own_workspaces() {
    let store = tempfile::tempdir().unwrap();
    for (id, file) in [("m (a)", "fa"), ("m (b)", "fb")] {
        let doc = job_named(
            id,
            "m",
            vec![
                make::step(&format!("echo {file} > {file}")),
                persist(".", &[file]),
            ],
        );
        let (_dir, out) = run_in_new_dir(store.path(), &doc);
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    }
    let doc = job_named(
        "two",
        "two",
        vec![
            make::step("echo one > f1 && echo two > f2"),
            persist(".", &["f1"]),
            persist(".", &["f2"]),
        ],
    );
    let (_dir, out) = run_in_new_dir(store.path(), &doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let doc = requiring(
        &["m (a)", "m (b)", "two"],
        vec![attach("."), make::step("cat fa fb f1 f2")],
    );
    let (_dir, out) = run_in_new_dir(store.path(), &doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let text = stdout(&out);
    for line in ["fa", "fb", "one", "two"] {
        assert!(text.lines().any(|l| l == line), "{line}: {text}");
    }
}

#[test]
fn the_stage_is_private_and_left_empty() {
    use std::os::unix::fs::PermissionsExt;

    let store = tempfile::tempdir().unwrap();
    let (dir, out) = shared(
        store.path(),
        vec![
            cache("k", &[], &["t"]),
            make::step("mkdir t && echo x > t/a"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let stage = state_dir(dir.path()).join("stage");
    let mode = std::fs::metadata(&stage).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0);
}

#[test]
fn a_planted_stage_link_stops_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_dir(dir.path());
    std::fs::create_dir_all(&state).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, state.join("stage")).unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo ran")]);
    assert_ne!(out.status.code(), Some(0));
    assert!(!stdout(&out).contains("ran"));
}

#[test]
fn a_state_dir_others_can_write_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let state = state_dir(dir.path());
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = run_steps(dir.path(), vec![make::step("echo ran")]);
    assert_ne!(out.status.code(), Some(0));
    assert!(!stdout(&out).contains("ran"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not be writable by others"), "{stderr}");
}

#[test]
fn a_failed_restore_does_not_queue_a_save() {
    let store = tempfile::tempdir().unwrap();
    let dir = store.path().join("cache/acme%2Frepo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.tar.gz"), "not an archive").unwrap();
    let (_dir, out) = shared(store.path(), vec![cache("new", &["old"], &["t"])]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(!cache_file(store.path(), "new").exists());
    assert!(!stdout(&out).contains("Post Cache"), "{}", stdout(&out));
}

#[test]
fn a_cancel_ends_a_large_save_promptly() {
    let store = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A sparse file that takes minutes to compress.
    let doc = make::document(vec![
        make::step("dd if=/dev/null of=big bs=1048576 seek=200000 count=0 2>/dev/null"),
        persist(".", &["big"]),
    ]);
    let job = dir.path().join("job.json");
    std::fs::write(&job, json(&doc)).unwrap();
    let env = dir.path().join("environment");
    std::fs::write(&env, "PATH=/usr/bin:/bin\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.path().join("work"))
        .arg("--state-dir")
        .arg(state_dir(dir.path()))
        .args(["--runner-user", "", "--minute", "1s"])
        .arg("--env-file")
        .arg(&env)
        .arg("--store")
        .arg(store.path())
        .arg("--result")
        .arg(dir.path().join("result.json"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let asked = Instant::now();
    term(&child);
    let _ = child.wait_with_output().unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "{:?}",
        asked.elapsed()
    );
    let result = result(dir.path());
    assert_eq!(result.conclusion, Conclusion::Cancelled);
    assert_eq!(result.steps[1].outcome, Conclusion::Cancelled);
    assert!(!store.path().join("workspace/1/build.1.tar.gz").exists());
    assert_eq!(
        std::fs::read_dir(state_dir(dir.path()).join("stage"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn parallel_node_gets_timings_and_node_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = make::document(vec![make::step(
        "echo \"node=$HATCHRUN_NODE_INDEX/$HATCHRUN_NODE_TOTAL\"; cat \"$HATCHRUN_TIMINGS\"; echo; \
         stat -c 'mode=%a' \"$HATCHRUN_TIMINGS\" 2>/dev/null || stat -f 'mode=%Lp' \"$HATCHRUN_TIMINGS\"",
    )]);
    doc.job.node = Some((1, 3));
    doc.timings.insert("a".to_owned(), 2.5);
    let out = exec(dir.path(), &json(&doc));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "node=1/3"), "{text}");
    assert!(text.lines().any(|l| l == r#"{"a":2.5}"#), "{text}");
    assert!(text.lines().any(|l| l == "mode=644"), "{text}");
}

#[test]
fn job_without_a_node_gets_timings_only() {
    let dir = tempfile::tempdir().unwrap();
    let doc = make::document(vec![make::step(
        "echo \"node=[$HATCHRUN_NODE_INDEX][$HATCHRUN_NODE_TOTAL]\"; test -f \"$HATCHRUN_TIMINGS\" && echo file",
    )]);
    let out = exec(dir.path(), &json(&doc));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(text.lines().any(|l| l == "node=[][]"), "{text}");
    assert!(text.lines().any(|l| l == "file"), "{text}");
}
