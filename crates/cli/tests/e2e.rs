// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The whole path a repository takes, end to end.
//!
//! Each scenario under `tests/fixtures/e2e/` is a set of GitHub workflow
//! files. The test copies them into a fresh repository, runs the real
//! `hatchrun convert` and `hatchrun validate` binaries on it, reads the
//! written `.hatchrun/config.yml` back and then plays every webhook in
//! `tests/fixtures/e2e/events/` against it the way the control plane
//! will: `triggered` picks the workflows, `compile` plans each one for
//! the event's ref, and every job and step `if:` is evaluated against a
//! context built from the payload.
//!
//! The run is simulated twice. In the "ok" run every job that runs
//! passes; in the "fail" run every job that runs fails. A job's line
//! says whether it runs in each. A step's `if:` is shown as `A/B`: A is
//! whether it runs while its job is passing, B whether it runs after an
//! earlier step of the job failed.
//!
//! The rendered transcript must match `expected.txt` in the scenario.
//! The latest transcript is also written under the target directory, in
//! `e2e-actual/`, for diffing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use hatchrun_workflow::{
    Arch, Config, Event, ExprError, Functions, JobKind, JobStatus, ParameterKind, Plan, PlannedJob,
    PlannedStep, Workflow, compile, evaluate, interpolate, parse, to_yaml, triggered, truthy,
};
use serde_json::{Map, Value, json};

/// The head of the default branch, which schedules and dispatches run on.
const DEFAULT_HEAD: &str = "7d2f9c1e4b6a8d0f3e5c7a9b1d3f5e7a9c1b3d5f";

/// Repository variables, as `vars` reads them.
fn vars() -> Value {
    json!({ "DEPLOY_ENV": "staging" })
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/e2e")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A fresh repository holding the scenario's workflows in
/// `.github/workflows/`.
fn repository(name: &str, workflows: &Path) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("e2e-{}", std::process::id()))
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    let dest = dir.join(".github/workflows");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::create_dir_all(dir.join(".hatchrun")).unwrap();
    for entry in std::fs::read_dir(workflows).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            std::fs::copy(&path, dest.join(path.file_name().unwrap())).unwrap();
        }
    }
    dir
}

/// Runs `hatchrun` in `dir`: the exit code, stdout and stderr.
fn hatchrun(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_hatchrun"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// One webhook delivery.
struct Delivery {
    /// Shown in the transcript.
    label: String,
    /// The `X-GitHub-Event` header.
    event: &'static str,
    payload: Value,
}

/// The deliveries the scenarios are played against. A schedule is
/// delivered once per cron any workflow of the config lists.
fn deliveries(config: &Config) -> Vec<Delivery> {
    let dir = fixtures().join("events");
    let mut out = Vec::new();
    for (label, event) in [
        ("push-main", "push"),
        ("push-feature", "push"),
        ("push-tag", "push"),
        ("pull_request-opened", "pull_request"),
        ("pull_request-synchronize", "pull_request"),
        ("pull_request-closed", "pull_request"),
    ] {
        out.push(Delivery {
            label: label.to_owned(),
            event,
            payload: read_json(&dir.join(format!("{label}.json"))),
        });
    }
    let crons: BTreeSet<&str> = config
        .workflows
        .values()
        .flat_map(|wf| wf.on.schedule.iter().map(|s| s.cron.as_str()))
        .collect();
    for cron in crons {
        let mut payload = read_json(&dir.join("schedule.json"));
        payload["schedule"] = json!(cron);
        out.push(Delivery {
            label: format!("schedule '{cron}'"),
            event: "schedule",
            payload,
        });
    }
    out.push(Delivery {
        label: "workflow_dispatch".to_owned(),
        event: "workflow_dispatch",
        payload: read_json(&dir.join("workflow_dispatch.json")),
    });
    out
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// The paths a push changed, from its commits.
fn pushed_paths(payload: &Value) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for commit in payload["commits"].as_array().into_iter().flatten() {
        for key in ["added", "removed", "modified"] {
            for path in commit[key].as_array().into_iter().flatten() {
                paths.insert(text(path).to_owned());
            }
        }
    }
    paths.into_iter().collect()
}

/// What `triggered` sees. Pull request files come from the pull
/// request files API, as the payload does not list them.
fn trigger_event(d: &Delivery) -> Event {
    match d.event {
        "push" => Event::Push {
            r#ref: text(&d.payload["ref"]).to_owned(),
            changed: pushed_paths(&d.payload),
        },
        "pull_request" => Event::PullRequest {
            action: text(&d.payload["action"]).to_owned(),
            base_ref: text(&d.payload["pull_request"]["base"]["ref"]).to_owned(),
            changed: read_json(&fixtures().join("events/pull_request-files.json"))
                .as_array()
                .unwrap()
                .iter()
                .map(|f| text(&f["filename"]).to_owned())
                .collect(),
        },
        "schedule" => Event::Schedule {
            cron: text(&d.payload["schedule"]).to_owned(),
        },
        _ => Event::Dispatch,
    }
}

/// The ref `compile` gets, as its documentation asks.
fn compile_ref(d: &Delivery) -> String {
    match d.event {
        "push" | "workflow_dispatch" => text(&d.payload["ref"]).to_owned(),
        "pull_request" => format!(
            "refs/heads/{}",
            text(&d.payload["pull_request"]["base"]["ref"])
        ),
        _ => format!(
            "refs/heads/{}",
            text(&d.payload["repository"]["default_branch"])
        ),
    }
}

/// The `inputs` context: each declared input from the payload, read as
/// its kind, else its default.
fn inputs(d: &Delivery, wf: &Workflow) -> Value {
    let mut out = Map::new();
    if d.event != "workflow_dispatch" {
        return Value::Object(out);
    }
    let Some(dispatch) = &wf.on.workflow_dispatch else {
        return Value::Object(out);
    };
    for (name, param) in &dispatch.inputs {
        let given = d.payload["inputs"].get(name).and_then(Value::as_str);
        let value = match (given, &param.kind) {
            (Some(s), ParameterKind::Boolean) => json!(s == "true"),
            (Some(s), ParameterKind::Integer) => json!(s.parse::<i64>().unwrap()),
            (Some(s), _) => json!(s),
            (None, _) => param.default.clone().unwrap_or(Value::Null),
        };
        out.insert(name.clone(), value);
    }
    Value::Object(out)
}

/// The `github` context for one workflow of one delivery.
fn github(d: &Delivery, workflow: &str, inputs: &Value) -> Value {
    let mut event = d.payload.clone();
    let (git_ref, sha, head_ref, base_ref) = match d.event {
        "push" => (
            text(&d.payload["ref"]).to_owned(),
            text(&d.payload["after"]).to_owned(),
            String::new(),
            String::new(),
        ),
        "pull_request" => {
            let pr = &d.payload["pull_request"];
            (
                format!("refs/pull/{}/merge", d.payload["number"]),
                text(&pr["merge_commit_sha"]).to_owned(),
                text(&pr["head"]["ref"]).to_owned(),
                text(&pr["base"]["ref"]).to_owned(),
            )
        }
        _ => (
            compile_ref(d),
            DEFAULT_HEAD.to_owned(),
            String::new(),
            String::new(),
        ),
    };
    if d.event == "workflow_dispatch" {
        // GitHub sends only the declared inputs, as strings.
        let declared: Map<String, Value> = inputs
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let shown = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                (k.clone(), json!(shown))
            })
            .collect();
        event["inputs"] = Value::Object(declared);
        event["workflow"] = json!(format!(".github/workflows/{workflow}.yml"));
    }
    let (ref_name, ref_type) = if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
        (tag.to_owned(), "tag")
    } else if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
        (branch.to_owned(), "branch")
    } else {
        (
            git_ref.trim_start_matches("refs/pull/").to_owned(),
            "branch",
        )
    };
    json!({
        "event_name": d.event,
        "event": event,
        "ref": git_ref,
        "ref_name": ref_name,
        "ref_type": ref_type,
        "sha": sha,
        "head_ref": head_ref,
        "base_ref": base_ref,
        "repository": d.payload["repository"]["full_name"],
        "repository_owner": d.payload["repository"]["owner"]["login"],
        "actor": d.payload["sender"]["login"].as_str().unwrap_or("github-actions[bot]"),
        "server_url": "https://github.com",
        "workflow": workflow,
    })
}

/// The status functions. `hashFiles` hashes nothing: the control plane
/// has no checkout, and no fixture condition reads it.
struct Status(JobStatus);

impl Functions for Status {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Err(ExprError {
            message: "hashFiles needs a checkout".into(),
            offset: 0,
        })
    }

    fn status(&self) -> JobStatus {
        self.0
    }
}

/// How the jobs a job needs ended, as its status functions see them.
#[derive(Clone, Copy)]
enum Upstream {
    /// Every ancestor passed.
    Passed,
    /// An ancestor failed.
    Failed,
    /// None failed but one was skipped: `success()` and `failure()` are
    /// both false. `JobStatus` has no such state, so `Cancelled` stands
    /// in; no fixture calls `cancelled()`.
    Skipped,
}

impl Upstream {
    fn status(self) -> JobStatus {
        match self {
            Self::Passed => JobStatus::Success,
            Self::Failed => JobStatus::Failure,
            Self::Skipped => JobStatus::Cancelled,
        }
    }
}

/// Evaluates an `if:` as GitHub does: a condition that calls no status
/// function runs only on success, and a whole `${{ }}` is unwrapped.
fn condition(cond: &str, ctx: &Value, status: JobStatus) -> Result<bool, ExprError> {
    let trimmed = cond.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .filter(|inner| !inner.contains("${{"));
    let funcs = Status(status);
    let value = match inner {
        Some(inner) => evaluate(inner, ctx, &funcs)?,
        None if trimmed.contains("${{") => Value::String(interpolate(trimmed, ctx, &funcs)?),
        None => evaluate(trimmed, ctx, &funcs)?,
    };
    let lower = trimmed.to_ascii_lowercase();
    let checks_status = ["success(", "failure(", "cancelled(", "always("]
        .iter()
        .any(|f| lower.contains(f));
    Ok(truthy(&value) && (checks_status || status == JobStatus::Success))
}

fn shown(result: Result<bool, ExprError>) -> String {
    match result {
        Ok(true) => "run".to_owned(),
        Ok(false) => "skip".to_owned(),
        Err(e) => format!("error: {e}"),
    }
}

fn step_parts(step: &PlannedStep) -> (String, Option<&str>, Option<&str>) {
    match step {
        PlannedStep::Run(run) => (
            {
                let mut label = run
                    .name
                    .clone()
                    .or_else(|| run.id.clone())
                    .unwrap_or_else(|| format!("run: {}", run.run.lines().next().unwrap_or("")));
                if let Some(dir) = &run.working_directory {
                    write!(label, " (in {dir})").unwrap();
                }
                if let Some(shell) = &run.shell {
                    write!(label, " (with {shell})").unwrap();
                }
                label
            },
            run.id.as_deref(),
            run.if_.as_deref(),
        ),
        PlannedStep::Uses(uses) => (
            uses.name.clone().unwrap_or_else(|| uses.uses.clone()),
            uses.id.as_deref(),
            uses.if_.as_deref(),
        ),
        other => (format!("{other:?}"), None, None),
    }
}

/// One simulated run of a plan: the result of every job by name.
struct Run {
    results: BTreeMap<String, &'static str>,
    lines: BTreeMap<String, Result<bool, ExprError>>,
}

/// Plays a plan through once. `fail` makes every job that runs fail.
fn simulate(plan: &Plan, base: &Value, outputs: &Value, fail: bool) -> Run {
    let by_id: BTreeMap<&str, &PlannedJob> = plan.jobs.iter().map(|j| (j.id.as_str(), j)).collect();
    let mut ran: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    let mut lines = BTreeMap::new();
    for job in &plan.jobs {
        let upstream = upstream(job, &by_id, &ran);
        let ctx = job_context(job, base, &by_id, &ran, outputs);
        let runs = match &job.if_ {
            Some(cond) => condition(cond, &ctx, upstream.status()),
            None => Ok(matches!(upstream, Upstream::Passed)),
        };
        let result = match runs {
            Ok(true) if fail => "failure",
            Ok(true) => "success",
            _ => "skipped",
        };
        ran.entry(job.name.clone()).or_default().push(result);
        lines.insert(job.id.clone(), runs);
    }
    let results = ran
        .into_iter()
        .map(|(name, legs)| (name, combined(&legs)))
        .collect();
    Run { results, lines }
}

/// One result for a job and all its matrix legs and nodes.
fn combined(legs: &[&'static str]) -> &'static str {
    if legs.contains(&"failure") {
        "failure"
    } else if legs.iter().all(|r| *r == "skipped") {
        "skipped"
    } else {
        "success"
    }
}

/// The names a job requires, in order, once each.
fn needs_names<'a>(job: &PlannedJob, by_id: &BTreeMap<&str, &'a PlannedJob>) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for id in &job.requires {
        let name = by_id[id.as_str()].name.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn upstream(
    job: &PlannedJob,
    by_id: &BTreeMap<&str, &PlannedJob>,
    ran: &BTreeMap<String, Vec<&'static str>>,
) -> Upstream {
    // Every ancestor, not only the direct needs.
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&str> = job.requires.iter().map(String::as_str).collect();
    let mut skipped = false;
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        let up = by_id[id];
        match combined(&ran[&up.name]) {
            "failure" => return Upstream::Failed,
            "skipped" => skipped = true,
            _ => {}
        }
        todo.extend(up.requires.iter().map(String::as_str));
    }
    if skipped {
        Upstream::Skipped
    } else {
        Upstream::Passed
    }
}

/// The step outputs a step id produced, from the scenario's
/// `step-outputs.json`.
fn step_outputs(outputs: &Value, id: &str) -> Value {
    outputs.get(id).cloned().unwrap_or_else(|| json!({}))
}

/// The `steps` context after every step of `job` passed.
fn steps_context(job: &PlannedJob, outputs: &Value, upto: usize) -> Value {
    let mut steps = Map::new();
    for step in &job.steps[..upto] {
        if let (_, Some(id), _) = step_parts(step) {
            steps.insert(
                id.to_owned(),
                json!({
                    "outcome": "success",
                    "conclusion": "success",
                    "outputs": step_outputs(outputs, id),
                }),
            );
        }
    }
    Value::Object(steps)
}

/// The values of a job's `outputs:` once all its steps passed.
fn job_outputs(job: &PlannedJob, base: &Value, outputs: &Value) -> Value {
    let mut ctx = base.clone();
    ctx["steps"] = steps_context(job, outputs, job.steps.len());
    ctx["matrix"] = json!(job.matrix);
    let mut out = Map::new();
    for (key, template) in &job.outputs {
        let value = interpolate(template, &ctx, &Status(JobStatus::Success))
            .unwrap_or_else(|e| format!("error: {e}"));
        out.insert(key.clone(), json!(value));
    }
    Value::Object(out)
}

/// The context a job's `if:` sees: `github`, `needs`, `vars` and
/// `inputs`.
fn job_context(
    job: &PlannedJob,
    base: &Value,
    by_id: &BTreeMap<&str, &PlannedJob>,
    ran: &BTreeMap<String, Vec<&'static str>>,
    outputs: &Value,
) -> Value {
    let mut needs = Map::new();
    for name in needs_names(job, by_id) {
        let first = by_id.values().find(|j| j.name == name).unwrap();
        let result = combined(&ran[name]);
        let outs = if result == "success" {
            job_outputs(first, base, outputs)
        } else {
            json!({})
        };
        needs.insert(
            name.to_owned(),
            json!({ "result": result, "outputs": outs }),
        );
    }
    let mut ctx = base.clone();
    ctx["needs"] = Value::Object(needs);
    ctx
}

/// Renders what one delivery does to one config.
fn play(config: &Config, d: &Delivery, outputs: &Value, out: &mut String) {
    let git_ref = compile_ref(d);
    writeln!(out, "event {} (compile {git_ref})", d.label).unwrap();
    let event = trigger_event(d);
    let outputs = outputs.get(&d.label).cloned().unwrap_or_else(|| json!({}));
    for (name, wf) in &config.workflows {
        if !triggered(&wf.on, &event) {
            writeln!(out, "  {name}: not triggered").unwrap();
            continue;
        }
        let plan = match compile(config, name, &git_ref) {
            Ok(plan) => plan,
            Err(errors) => {
                writeln!(out, "  {name}: compile failed").unwrap();
                for e in errors {
                    writeln!(out, "    {e}").unwrap();
                }
                continue;
            }
        };
        assert_eq!(plan.workflow, *name);
        writeln!(out, "  {name}: triggered").unwrap();
        let inputs = inputs(d, wf);
        let base = json!({
            "github": github(d, name, &inputs),
            "vars": vars(),
            "inputs": inputs,
        });
        let ok = simulate(&plan, &base, &outputs, false);
        let fail = simulate(&plan, &base, &outputs, true);
        let by_id: BTreeMap<&str, &PlannedJob> =
            plan.jobs.iter().map(|j| (j.id.as_str(), j)).collect();
        for job in &plan.jobs {
            render_job(job, &base, &by_id, &outputs, (&ok, &fail), out);
        }
    }
}

fn render_job(
    job: &PlannedJob,
    base: &Value,
    by_id: &BTreeMap<&str, &PlannedJob>,
    outputs: &Value,
    (ok, fail): (&Run, &Run),
    out: &mut String,
) {
    let JobKind::Run { template } = job.kind else {
        writeln!(out, "    job {} (approval)", job.id).unwrap();
        return;
    };
    write!(out, "    job {} on {}", job.id, template.labels[0]).unwrap();
    if !job.requires.is_empty() {
        write!(out, " after {}", job.requires.join(" + ")).unwrap();
    }
    writeln!(
        out,
        ": ok={} fail={}",
        shown(ok.lines[&job.id].clone()),
        shown(fail.lines[&job.id].clone())
    )
    .unwrap();
    if job.name != job.id.split(" (").next().unwrap_or_default() {
        writeln!(out, "      name {}", job.name).unwrap();
    }
    if let Some(cond) = &job.if_ {
        writeln!(out, "      if {cond}").unwrap();
    }
    if !job.matrix.is_empty() {
        let shown: Vec<String> = job.matrix.iter().map(|(k, v)| format!("{k}={v}")).collect();
        writeln!(out, "      matrix {}", shown.join(" ")).unwrap();
    }
    if job.timeout_minutes != 360 {
        writeln!(out, "      timeout {}m", job.timeout_minutes).unwrap();
    }
    if let Some((index, total)) = job.node {
        writeln!(out, "      node {}/{total}", index + 1).unwrap();
    }
    let ran_ok = matches!(ok.lines[&job.id], Ok(true));
    if ran_ok && !job.outputs.is_empty() {
        let values = job_outputs(job, base, outputs);
        let shown: Vec<String> = values
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k}={}", text(v)))
            .collect();
        writeln!(out, "      outputs {}", shown.join(" ")).unwrap();
    }
    // Steps see the job's env and matrix and its needs as the run left
    // them.
    let worlds: Vec<(&str, &Run)> = [("ok", ok), ("fail", fail)]
        .into_iter()
        .filter(|(_, run)| matches!(run.lines[&job.id], Ok(true)))
        .collect();
    for (i, step) in job.steps.iter().enumerate() {
        let (label, _, cond) = step_parts(step);
        let Some(cond) = cond else {
            writeln!(out, "      step {label}").unwrap();
            continue;
        };
        write!(out, "      step {label} if {cond}:").unwrap();
        if worlds.is_empty() {
            write!(out, " never reached").unwrap();
        }
        for (world, run) in &worlds {
            let mut ctx = job_context(job, base, by_id, &as_legs(&run.results), outputs);
            ctx["env"] = json!(job.env);
            ctx["matrix"] = json!(job.matrix);
            ctx["steps"] = steps_context(job, outputs, i);
            ctx["runner"] = json!({ "os": "Linux", "arch": arch(job) });
            let passing = shown(condition(cond, &ctx, JobStatus::Success));
            let failed = shown(condition(cond, &ctx, JobStatus::Failure));
            write!(out, " {world}={passing}/{failed}").unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn as_legs(results: &BTreeMap<String, &'static str>) -> BTreeMap<String, Vec<&'static str>> {
    results
        .iter()
        .map(|(name, result)| (name.clone(), vec![*result]))
        .collect()
}

fn arch(job: &PlannedJob) -> &'static str {
    match job.kind {
        JobKind::Run { template } if template.arch == Arch::Arm64 => "ARM64",
        _ => "X64",
    }
}

/// Converts and validates a repository with the binaries, then plays
/// every delivery. Returns the transcript and the config.
fn transcript(name: &str, dir: &Path) -> (String, Config) {
    let mut out = String::new();
    let (code, stdout, stderr) = hatchrun(dir, &["convert", "-o", ".hatchrun/config.yml"]);
    assert_eq!(stdout, "", "convert writes the config to the file");
    writeln!(out, "convert: exit {code}").unwrap();
    for line in stderr.lines() {
        writeln!(out, "  {line}").unwrap();
    }
    let (code, stdout, stderr) = hatchrun(dir, &["validate"]);
    assert_eq!(stderr, "", "{name}: validate");
    writeln!(out, "validate: exit {code}").unwrap();
    for line in stdout.lines() {
        writeln!(out, "  {line}").unwrap();
    }
    let source = std::fs::read_to_string(dir.join(".hatchrun/config.yml")).unwrap();
    let config = parse(&source).unwrap_or_else(|e| panic!("{name}: {e:?}\n{source}"));
    assert_eq!(
        parse(&to_yaml(&config)).unwrap(),
        config,
        "{name}: round trip"
    );
    let outputs = fixtures().join(name).join("step-outputs.json");
    let outputs = if outputs.exists() {
        read_json(&outputs)
    } else {
        json!({})
    };
    for d in deliveries(&config) {
        play(&config, &d, &outputs, &mut out);
    }
    (out, config)
}

/// The expected transcript, without its `#` comment lines.
fn expected(name: &str) -> String {
    let path = fixtures().join(name).join("expected.txt");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = String::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        out.push_str(line);
        out.push('\n');
    }
    out.trim_start_matches('\n').to_owned()
}

/// Runs one scenario and compares its transcript.
fn scenario(name: &str) -> Config {
    let dir = repository(name, &fixtures().join(name).join("workflows"));
    let (actual, config) = transcript(name, &dir);
    let saved = Path::new(env!("CARGO_TARGET_TMPDIR")).join("e2e-actual");
    std::fs::create_dir_all(&saved).unwrap();
    std::fs::write(saved.join(format!("{name}.txt")), &actual).unwrap();
    let want = expected(name);
    if actual != want {
        let line = actual
            .lines()
            .zip(want.lines())
            .position(|(a, w)| a != w)
            .unwrap_or_else(|| actual.lines().count().min(want.lines().count()));
        panic!(
            "{name}: transcript differs from expected.txt at line {}\nexpected: {:?}\nactual:   {:?}\n\nfull transcript:\n{actual}",
            line + 1,
            want.lines().nth(line),
            actual.lines().nth(line),
        );
    }
    config
}

#[test]
fn rust_ci_with_a_matrix_and_caches() {
    scenario("rust");
}

#[test]
fn node_ci_reports_the_non_linux_matrix_legs() {
    scenario("node");
}

#[test]
fn python_tests_drop_services_with_a_warning() {
    scenario("python");
}

#[test]
fn monorepo_workflows_run_on_their_own_paths() {
    scenario("monorepo");
}

#[test]
fn release_on_a_tag_or_by_hand() {
    scenario("release");
}

#[test]
fn nightly_schedule() {
    scenario("nightly");
}

#[test]
fn reusable_workflow_calls_drop_their_dependents() {
    scenario("reusable");
}

/// A developer adds `parallelism` to a converted job: each matrix leg
/// becomes one job per node, and the jobs after it wait for all of them.
#[test]
fn parallelism_added_after_converting() {
    let dir = repository("rust-parallel", &fixtures().join("rust/workflows"));
    let (code, _, stderr) = hatchrun(&dir, &["convert", "-o", ".hatchrun/config.yml"]);
    assert_eq!(code, 0, "{stderr}");
    let path = dir.join(".hatchrun/config.yml");
    let mut config = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config.jobs.get_mut("ci-test").unwrap().parallelism = Some(2);
    std::fs::write(&path, to_yaml(&config)).unwrap();
    let (code, stdout, _) = hatchrun(&dir, &["validate"]);
    assert_eq!((code, stdout.as_str()), (0, ""));

    let plan = compile(&config, "ci", "refs/heads/main").unwrap();
    let ids: Vec<&str> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    let mut legs = Vec::new();
    for features in ["default", "all"] {
        for toolchain in ["stable", "beta"] {
            for node in 1..=2 {
                legs.push(format!("test ({features}, {toolchain}) [{node}/2]"));
            }
        }
    }
    let mut want = vec!["fmt", "clippy"];
    want.extend(legs.iter().map(String::as_str));
    want.push("coverage");
    assert_eq!(ids, want);
    let coverage = plan.jobs.last().unwrap();
    assert_eq!(coverage.requires, legs);
    let node = &plan.jobs[2];
    assert_eq!(node.node, Some((0, 2)));
    assert_eq!(node.matrix["toolchain"], json!("stable"));
    assert_eq!(node.matrix["features"], json!("default"));
}

/// This repository's own workflows: everything converts but the release
/// build matrix, which names its runner through `matrix.runner`, so the
/// release workflow keeps only `version` and `sbom`.
#[test]
fn this_repository() {
    let workflows = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows");
    let dir = repository("self", &workflows);
    let (code, stdout, stderr) = hatchrun(&dir, &["convert", "-o", ".hatchrun/config.yml"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("`release.yml` job `build` has `runs-on: ${{ matrix.runner }}`"),
        "{stderr}"
    );
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("names no runner template"))
        .collect();
    assert_eq!(errors.len(), 1, "{stderr}");
    let (code, stdout, stderr) = hatchrun(&dir, &["validate"]);
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (0, "", ""));

    let config =
        parse(&std::fs::read_to_string(dir.join(".hatchrun/config.yml")).unwrap()).unwrap();
    let release: Vec<&str> = config.workflows["release"]
        .jobs
        .iter()
        .map(|j| j.name.as_deref().unwrap_or(&j.job))
        .collect();
    assert_eq!(release, ["version", "sbom"]);
    let mut planned = 0;
    for d in deliveries(&config) {
        let event = trigger_event(&d);
        for (name, wf) in &config.workflows {
            if triggered(&wf.on, &event) {
                let plan = compile(&config, name, &compile_ref(&d))
                    .unwrap_or_else(|e| panic!("{name} on {}: {e:?}", d.label));
                planned += plan.jobs.len();
            }
        }
    }
    assert!(planned > 0);
    // Release runs on every push to main.
    let push = deliveries(&config)
        .into_iter()
        .find(|d| d.label == "push-main")
        .unwrap();
    assert!(triggered(
        &config.workflows["release"].on,
        &trigger_event(&push)
    ));
    let plan = compile(&config, "release", &compile_ref(&push)).unwrap();
    let ids: Vec<&str> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["version", "sbom"]);
}
