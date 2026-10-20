// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converting GitHub Actions workflows into a hatchrun config.

use hatchrun_workflow::{
    Conversion, Diagnostic, Filter, ParameterKind, Step, convert, parse, to_yaml, validate,
};
use serde_json::{Value, json};

fn one(name: &str, yaml: &str) -> Result<Conversion, Vec<Diagnostic>> {
    convert(&[(name.into(), yaml.into())])
}

fn ok(yaml: &str) -> Conversion {
    one("ci.yml", yaml).unwrap()
}

fn has(diags: &[Diagnostic], words: &[&str]) -> bool {
    diags
        .iter()
        .any(|d| words.iter().all(|w| d.message.contains(w)))
}

/// The YAML we write parses back to the same config and validates clean.
fn round_trips(out: &Conversion) {
    let yaml = to_yaml(&out.config);
    let reparsed = parse(&yaml).unwrap_or_else(|e| panic!("{e:?}\n{yaml}"));
    assert_eq!(reparsed, out.config, "{yaml}");
    assert_eq!(validate(&reparsed), Vec::new(), "{yaml}");
}

#[test]
fn converts_a_typical_ci_workflow() {
    let out = convert(&[(
        "ci.yml".into(),
        include_str!("fixtures/github/ci.yml").into(),
    )])
    .unwrap();
    let job = &out.config.jobs["ci-test"];
    assert_eq!(job.runs_on.as_deref(), Some("ubuntu-latest"));
    assert_eq!(
        out.config.workflows["ci"]
            .jobs
            .iter()
            .find(|j| j.job == "ci-lint")
            .unwrap()
            .requires,
        Vec::<String>::new()
    );
    assert!(out.warnings.iter().any(|w| w.message.contains("services")));
    // round trip: the YAML we write parses and validates clean
    let reparsed = parse(&to_yaml(&out.config)).unwrap();
    assert_eq!(validate(&reparsed), Vec::<Diagnostic>::new());
}

#[test]
fn the_ci_fixture_maps_every_part() {
    let out = convert(&[(
        ".github/workflows/ci.yml".into(),
        include_str!("fixtures/github/ci.yml").into(),
    )])
    .unwrap();
    round_trips(&out);

    let wf = &out.config.workflows["ci"];
    assert!(wf.on.push.is_some());
    assert!(wf.on.pull_request.is_some());
    let test = wf.jobs.iter().find(|j| j.job == "ci-test").unwrap();
    assert_eq!(test.requires, ["lint"]);
    assert_eq!(test.name.as_deref(), Some("test"));
    assert_eq!(test.matrix["toolchain"], [json!("stable"), json!("beta")]);
    assert!(test.with.is_empty());

    let job = &out.config.jobs["ci-test"];
    assert_eq!(job.env["CARGO_TERM_COLOR"], "always");
    assert_eq!(job.env["RUST_BACKTRACE"], "1");
    assert_eq!(job.steps.len(), 4);
    let Step::Uses(cache) = &job.steps[1] else {
        panic!("{:?}", job.steps[1])
    };
    assert_eq!(cache.uses, "actions/cache@v4");
    assert!(
        cache.with["key"]
            .as_str()
            .unwrap()
            .contains("${{ matrix.toolchain }}")
    );
    let Step::Run(run) = &job.steps[3] else {
        panic!("{:?}", job.steps[3])
    };
    assert_eq!(run.id.as_deref(), Some("test"));
    assert!(!run.continue_on_error);
    assert!(has(
        &out.warnings,
        &["ci.yml", "test", "continue-on-error", "expression"]
    ));
    assert!(has(&out.warnings, &["ci.yml", "test", "services"]));
    assert_eq!(out.config.jobs["ci-lint"].timeout_minutes, Some(10));
}

#[test]
fn old_ubuntu_becomes_latest_with_warning() {
    for old in ["ubuntu-24.04", "ubuntu-22.04"] {
        let out = ok(&format!(
            "on: push\njobs:\n  build:\n    runs-on: {old}\n    steps:\n      - run: make\n"
        ));
        assert_eq!(
            out.config.jobs["ci-build"].runs_on.as_deref(),
            Some("ubuntu-latest")
        );
        assert!(has(
            &out.warnings,
            &["ci.yml", "build", old, "ubuntu-latest"]
        ));
        round_trips(&out);
    }
}

#[test]
fn runs_on_must_name_a_template() {
    for label in ["macos-latest", "[self-hosted, linux]", "${{ matrix.os }}"] {
        let errs = one(
            "ci.yml",
            &format!(
                "on: push\njobs:\n  build:\n    runs-on: {label}\n    steps:\n      - run: make\n"
            ),
        )
        .unwrap_err();
        assert!(
            has(&errs, &["ci.yml", "build", "ubuntu-latest"]),
            "{errs:?}"
        );
    }
    let out =
        ok("on: push\njobs:\n  b:\n    runs-on: [ubuntu-26.04-arm]\n    steps: [{ run: make }]\n");
    assert_eq!(
        out.config.jobs["ci-b"].runs_on.as_deref(),
        Some("ubuntu-26.04-arm")
    );
}

#[test]
fn docker_steps_are_errors() {
    let errs = one(
        "ci.yml",
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - name: Alpine\n        uses: docker://alpine:3.20\n",
    )
    .unwrap_err();
    assert!(
        has(&errs, &["ci.yml", "build", "docker://alpine:3.20"]),
        "{errs:?}"
    );
}

#[test]
fn reads_on_and_yaml_1_2_scalars() {
    let out = ok("
on:
  push:
    branches: [main, y]
    tags: ['v*']
  pull_request:
    types: [opened]
    paths-ignore: [docs/**]
  schedule:
    - cron: '0 3 * * 1'
  workflow_dispatch:
    inputs:
      level:
        type: choice
        options: [low, high]
        default: low
      dry:
        type: boolean
        default: false
      note:
        description: Why
jobs:
  t:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        flag: [y, n, on, off, yes]
    steps:
      - run: echo ${{ matrix.flag }}
        env:
          ANSWER: no
");
    round_trips(&out);
    let on = &out.config.workflows["ci"].on;
    let push = on.push.as_ref().unwrap();
    assert_eq!(
        push.branches,
        Some(Filter::new(vec!["main".into(), "y".into()]))
    );
    assert_eq!(push.tags, Some(Filter::new(vec!["v*".into()])));
    let pr = on.pull_request.as_ref().unwrap();
    assert_eq!(pr.types, ["opened"]);
    assert_eq!(pr.paths_ignore, Some(Filter::new(vec!["docs/**".into()])));
    assert_eq!(on.schedule[0].cron, "0 3 * * 1");
    let inputs = &on.workflow_dispatch.as_ref().unwrap().inputs;
    assert_eq!(
        inputs["level"].kind,
        ParameterKind::Enum(vec!["low".into(), "high".into()])
    );
    assert_eq!(inputs["dry"].kind, ParameterKind::Boolean);
    assert_eq!(inputs["dry"].default, Some(Value::Bool(false)));
    assert_eq!(inputs["note"].kind, ParameterKind::String);
    assert_eq!(inputs["note"].description.as_deref(), Some("Why"));

    let entry = &out.config.workflows["ci"].jobs[0];
    assert_eq!(
        entry.matrix["flag"],
        ["y", "n", "on", "off", "yes"].map(|s| json!(s))
    );
    let Step::Run(run) = &out.config.jobs["ci-t"].steps[0] else {
        panic!()
    };
    assert_eq!(run.env["ANSWER"], "no");
}

#[test]
fn on_as_a_single_event_name() {
    let out =
        ok("on: workflow_dispatch\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n");
    let on = &out.config.workflows["ci"].on;
    assert!(on.workflow_dispatch.is_some());
    assert!(on.push.is_none());
    round_trips(&out);
}

#[test]
fn unsupported_events_are_dropped_with_a_warning() {
    let out = ok("
on:
  push:
  release:
    types: [published]
  pull_request_target:
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert!(out.config.workflows["ci"].on.push.is_some());
    assert!(has(&out.warnings, &["ci.yml", "release"]));
    assert!(has(&out.warnings, &["ci.yml", "pull_request_target"]));
    round_trips(&out);
}

#[test]
fn dropped_job_keys_warn_and_name_the_file_and_job() {
    let out = ok("
on: push
permissions: read-all
concurrency: ci
defaults:
  run:
    shell: bash
jobs:
  build:
    runs-on: ubuntu-latest
    container: node:22
    concurrency: build
    permissions:
      contents: read
    environment: prod
    defaults:
      run:
        working-directory: app
    strategy:
      fail-fast: false
      matrix:
        node: [20, 22]
        include:
          - node: 23
        exclude:
          - node: 20
    steps:
      - run: npm test
");
    for key in [
        "container",
        "concurrency",
        "permissions",
        "environment",
        "defaults",
        "include",
        "exclude",
        "fail-fast",
    ] {
        assert!(
            has(&out.warnings, &["ci.yml", "build", key]),
            "{key}: {:?}",
            out.warnings
        );
    }
    for key in ["permissions", "concurrency", "defaults"] {
        assert!(
            out.warnings.iter().any(|w| w.message.contains("ci.yml")
                && !w.message.contains("build")
                && w.message.contains(key)),
            "{key}: {:?}",
            out.warnings
        );
    }
    let entry = &out.config.workflows["ci"].jobs[0];
    assert_eq!(entry.matrix["node"], [json!(20), json!(22)]);
    round_trips(&out);
}

#[test]
fn reusable_workflow_calls_are_dropped_with_their_requires() {
    let out = ok("
on: push
jobs:
  call:
    uses: org/repo/.github/workflows/build.yml@main
    with:
      target: x
    secrets: inherit
  after:
    needs: [call]
    runs-on: ubuntu-latest
    steps: [{ run: x }]
");
    assert!(has(&out.warnings, &["ci.yml", "call", "reusable"]));
    assert!(!out.config.jobs.contains_key("ci-call"));
    let wf = &out.config.workflows["ci"];
    assert_eq!(wf.jobs.len(), 1);
    assert_eq!(wf.jobs[0].requires, Vec::<String>::new());
    round_trips(&out);
}

#[test]
fn steps_copy_across() {
    let out = ok(r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main'
    timeout-minutes: 30
    outputs:
      version: ${{ steps.v.outputs.version }}
    steps:
      - uses: actions/setup-node@v4
        id: node
        name: Node
        if: always()
        continue-on-error: true
        with:
          node-version: 22
          cache: npm
        env:
          A: "1"
      - id: v
        name: Version
        run: echo "version=1" >> "$GITHUB_OUTPUT"
        shell: bash
        working-directory: app
        timeout-minutes: 5
        env:
          B: true
"#);
    round_trips(&out);
    let job = &out.config.jobs["ci-build"];
    assert_eq!(job.if_.as_deref(), Some("github.ref == 'refs/heads/main'"));
    assert_eq!(job.timeout_minutes, Some(30));
    assert_eq!(job.outputs["version"], "${{ steps.v.outputs.version }}");
    let Step::Uses(uses) = &job.steps[0] else {
        panic!()
    };
    assert_eq!(uses.id.as_deref(), Some("node"));
    assert_eq!(uses.name.as_deref(), Some("Node"));
    assert_eq!(uses.if_.as_deref(), Some("always()"));
    assert!(uses.continue_on_error);
    assert_eq!(uses.with["node-version"], json!(22));
    assert_eq!(uses.with["cache"], json!("npm"));
    assert_eq!(uses.env["A"], "1");
    let Step::Run(run) = &job.steps[1] else {
        panic!()
    };
    assert_eq!(run.shell.as_deref(), Some("bash"));
    assert_eq!(run.working_directory.as_deref(), Some("app"));
    assert_eq!(run.timeout_minutes, Some(5));
    assert_eq!(run.env["B"], "true");
}

#[test]
fn several_files_become_several_workflows() {
    let out = convert(&[
        (
            ".github/workflows/ci.yml".into(),
            "on: push\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n".into(),
        ),
        (
            "release.yaml".into(),
            "on: { push: { tags: ['v*'] } }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: y }] }\n"
                .into(),
        ),
    ])
    .unwrap();
    assert!(out.config.jobs.contains_key("ci-t"));
    assert!(out.config.jobs.contains_key("release-t"));
    assert_eq!(out.config.workflows["release"].jobs[0].job, "release-t");
    round_trips(&out);
}

#[test]
fn two_files_with_one_stem_are_an_error() {
    let errs = convert(&[
        ("ci.yml".into(), "on: push\njobs: {}\n".into()),
        ("ci.yaml".into(), "on: push\njobs: {}\n".into()),
    ])
    .unwrap_err();
    assert!(has(&errs, &["ci.yml", "ci.yaml"]), "{errs:?}");
}

#[test]
fn bad_yaml_names_the_file_and_line() {
    let errs = one("ci.yml", "on: push\njobs:\n  t: [\n").unwrap_err();
    assert!(errs[0].message.contains("ci.yml"), "{errs:?}");
    assert!(errs[0].line.is_some());
}

#[test]
fn matrix_expressions_are_dropped_with_a_warning() {
    let out = ok("
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy:
      matrix: ${{ fromJSON(needs.setup.outputs.m) }}
    steps: [{ run: x }]
");
    assert!(has(&out.warnings, &["ci.yml", "t", "matrix"]));
    assert!(out.config.workflows["ci"].jobs[0].matrix.is_empty());
    round_trips(&out);
}

fn dispatch_with(inputs: &str) -> Conversion {
    ok(&format!(
        "on:\n  workflow_dispatch:\n    inputs:\n{inputs}\njobs:\n  t: {{ runs-on: ubuntu-latest, steps: [{{ run: x }}] }}\n"
    ))
}

#[test]
fn dispatch_defaults_are_coerced_or_dropped() {
    let out = dispatch_with(
        "      a: { type: number, default: '5' }
      b: { type: number, default: 1.5 }
      c: { type: boolean, default: 'yes' }
      d: { type: boolean, default: 'true' }
      e: { type: choice, options: [x, y], default: z }
      f: { type: choice, options: [x, y], default: y }
      g: { type: string, default: 3 }",
    );
    round_trips(&out);
    let inputs = &out.config.workflows["ci"]
        .on
        .workflow_dispatch
        .as_ref()
        .unwrap()
        .inputs;
    assert_eq!(inputs["a"].default, Some(json!(5)));
    assert_eq!(inputs["b"].default, None);
    assert_eq!(inputs["c"].default, None);
    assert_eq!(inputs["d"].default, Some(json!(true)));
    assert_eq!(inputs["e"].default, None);
    assert_eq!(inputs["f"].default, Some(json!("y")));
    assert_eq!(inputs["g"].default, Some(json!("3")));
    for name in ["`b`", "`c`", "`e`"] {
        assert!(
            has(&out.warnings, &["ci.yml", name, "default"]),
            "{name}: {:?}",
            out.warnings
        );
    }
    assert!(!has(&out.warnings, &["`a`", "default"]));
}

#[test]
fn unknown_keys_warn_and_are_dropped() {
    let out = ok("
on: push
mystery: 1
jobs:
  build:
    runs-on: ubuntu-latest
    snapshot: image
    steps:
      - name: Go
        run: make
        sparkle: true
");
    assert!(has(&out.warnings, &["ci.yml", "mystery"]));
    assert!(has(&out.warnings, &["ci.yml", "build", "snapshot"]));
    assert!(has(&out.warnings, &["ci.yml", "build", "Go", "sparkle"]));
    round_trips(&out);
}

#[test]
fn scalar_step_fields_read_as_text() {
    let out = ok(
        "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      - name: 1\n        run: 123\n",
    );
    let Step::Run(run) = &out.config.jobs["ci-t"].steps[0] else {
        panic!()
    };
    assert_eq!(run.name.as_deref(), Some("1"));
    assert_eq!(run.run, "123");
}

#[test]
fn needs_must_name_a_job_and_repeats_collapse() {
    let errs = one(
        "ci.yml",
        "on: push\njobs:\n  t:\n    needs: [ghost]\n    runs-on: ubuntu-latest\n    steps: [{ run: x }]\n",
    )
    .unwrap_err();
    assert!(has(&errs, &["ci.yml", "`t`", "ghost"]), "{errs:?}");

    let out = ok(
        "on: push\njobs:\n  a: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n  b: { needs: [a, a], runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    );
    assert_eq!(out.config.workflows["ci"].jobs[1].requires, ["a"]);
}

#[test]
fn file_stems_ignore_extension_case_and_must_not_be_empty() {
    let out = one("CI.YML", "on: push\njobs: {}\n").unwrap();
    assert!(out.config.workflows.contains_key("CI"));
    let out = one("deploy.Yaml", "on: push\njobs: {}\n").unwrap();
    assert!(out.config.workflows.contains_key("deploy"));
    let errs = one(".github/workflows/.yml", "on: push\njobs: {}\n").unwrap_err();
    assert!(has(&errs, &[".yml", "name"]), "{errs:?}");
}

#[test]
fn dependents_of_a_dropped_reusable_job_are_warned() {
    let out = ok("
on: push
jobs:
  call:
    uses: org/repo/.github/workflows/build.yml@main
  after:
    needs: call
    runs-on: ubuntu-latest
    steps: [{ run: x }]
");
    assert!(
        has(&out.warnings, &["ci.yml", "`after`", "`call`", "dropped"]),
        "{:?}",
        out.warnings
    );
}
