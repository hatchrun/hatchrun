// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converting GitHub Actions workflows into a hatchrun config.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hatchrun_workflow::{
    Conversion, Diagnostic, Filter, JobKind, ParameterKind, Step, convert, parse, to_yaml, validate,
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
        "fail-fast",
    ] {
        assert!(
            has(&out.warnings, &["ci.yml", "build", key]),
            "{key}: {:?}",
            out.warnings
        );
    }
    for key in ["permissions", "concurrency"] {
        assert!(
            out.warnings.iter().any(|w| w.message.contains("ci.yml")
                && !w.message.contains("build")
                && w.message.contains(key)),
            "{key}: {:?}",
            out.warnings
        );
    }
    // `exclude` passes through; the `include` entry matches no
    // combination, so it runs as an entry of its own.
    let jobs = &out.config.workflows["ci"].jobs;
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].matrix["node"], [json!(20), json!(22)]);
    assert_eq!(jobs[0].matrix.exclude, [matrix_map(json!({"node": 20}))]);
    assert_eq!(jobs[1].name.as_deref(), Some("build-2"));
    assert_eq!(jobs[1].matrix["node"], [json!(23)]);
    round_trips(&out);
}

/// A JSON object as an `exclude` entry.
fn matrix_map(value: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(value).unwrap()
}

#[test]
fn reusable_workflow_calls_are_dropped_with_their_dependents() {
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
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: x }]
");
    assert!(has(&out.warnings, &["ci.yml", "call", "reusable"]));
    assert!(!out.config.jobs.contains_key("ci-call"));
    assert!(!out.config.jobs.contains_key("ci-after"));
    let wf = &out.config.workflows["ci"];
    assert_eq!(wf.jobs.len(), 1);
    assert_eq!(wf.jobs[0].job, "ci-lint");
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
    let out = convert(&[
        ("ci.yml".into(), "on: push\njobs: {}\n".into()),
        ("ci.yaml".into(), "on: push\njobs: {}\n".into()),
    ])
    .unwrap();
    assert!(has(&out.errors, &["ci.yml", "ci.yaml"]), "{:?}", out.errors);
    assert!(out.config.workflows.contains_key("ci"));
}

#[test]
fn bad_yaml_names_the_file_and_line() {
    let errs = one("ci.yml", "on: push\njobs:\n  t: [\n").unwrap_err();
    assert_eq!(errs[0].file.as_deref(), Some("ci.yml"), "{errs:?}");
    assert!(errs[0].line.is_some());
    assert!(!errs[0].message.contains("ci.yml"), "{errs:?}");
    assert!(errs[0].to_string().starts_with("ci.yml:3:"), "{}", errs[0]);
}

#[test]
fn a_job_that_fails_is_dropped_and_the_rest_converts() {
    let out = convert(&[
        (
            "release.yml".into(),
            "
on: push
jobs:
  build:
    strategy: { matrix: { runner: [macos-latest] } }
    runs-on: ${{ matrix.runner }}
    steps: [{ run: make }]
  publish:
    needs: build
    runs-on: ubuntu-latest
    steps: [{ run: publish }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: lint }]
"
            .into(),
        ),
        ("broken.yml".into(), "on: [push\n".into()),
    ])
    .unwrap();
    assert!(
        has(&out.errors, &["release.yml", "`build`", "runs-on"]),
        "{:?}",
        out.errors
    );
    assert!(
        out.errors
            .iter()
            .any(|e| e.file.as_deref() == Some("broken.yml"))
    );
    assert!(!out.config.jobs.contains_key("release-build"));
    assert!(!out.config.jobs.contains_key("release-publish"));
    let wf = &out.config.workflows["release"];
    assert_eq!(wf.jobs.len(), 1);
    assert_eq!(wf.jobs[0].job, "release-lint");
    assert!(
        has(
            &out.warnings,
            &["`publish` was dropped", "`build`", "did not convert"]
        ),
        "{:?}",
        out.warnings
    );
    assert!(!out.config.workflows.contains_key("broken"));
    round_trips(&out);
}

#[test]
fn arm_labels_become_the_26_04_arm_template() {
    for old in ["ubuntu-24.04-arm", "ubuntu-22.04-arm"] {
        let out = ok(&format!(
            "on: push\njobs:\n  b:\n    runs-on: {old}\n    steps: [{{ run: make }}]\n"
        ));
        assert_eq!(
            out.config.jobs["ci-b"].runs_on.as_deref(),
            Some("ubuntu-26.04-arm")
        );
        assert!(has(&out.warnings, &["`b`", old, "ubuntu-26.04-arm"]));
        round_trips(&out);
    }
}

#[test]
fn third_party_labels_become_the_template_of_their_size() {
    for (label, want, words) in [
        (
            "ubuntu-slim",
            "ubuntu-latest",
            "asks for 1 vCPUs, rounded up to 2",
        ),
        ("blacksmith-4vcpu-ubuntu-2404", "ubuntu-26.04-4core", ""),
        ("blacksmith-2vcpu-ubuntu-2404-arm", "ubuntu-26.04-arm", ""),
        ("buildjet-8vcpu-ubuntu-2204", "ubuntu-26.04-8core", ""),
        (
            "buildjet-4vcpu-ubuntu-2204-arm",
            "ubuntu-26.04-4core-arm",
            "",
        ),
        ("depot-ubuntu-24.04", "ubuntu-latest", ""),
        ("depot-ubuntu-24.04-arm-8", "ubuntu-26.04-8core-arm", ""),
        (
            "namespace-profile-default-6x12",
            "ubuntu-26.04-8core",
            "asks for 6 vCPUs, rounded up to 8",
        ),
        ("namespace-profile-ci", "ubuntu-latest", ""),
        ("ubicloud-standard-4", "ubuntu-26.04-4core", ""),
        (
            "ubicloud-standard-30-arm",
            "ubuntu-26.04-16core-arm",
            "asks for 30 vCPUs and the largest template has 16",
        ),
        ("warp-ubuntu-latest-x64-16x", "ubuntu-26.04-16core", ""),
        ("warp-ubuntu-2404-arm64-2x", "ubuntu-26.04-arm", ""),
    ] {
        let out = ok(&format!(
            "on: push\njobs:\n  b:\n    runs-on: {label}\n    steps: [{{ run: make }}]\n"
        ));
        assert_eq!(out.config.jobs["ci-b"].runs_on.as_deref(), Some(want));
        assert!(
            has(
                &out.warnings,
                &["`b`", &format!("`runs-on: {label}` became `{want}`"), words]
            ),
            "{label}: {:?}",
            out.warnings
        );
        round_trips(&out);
    }
}

#[test]
fn runs_on_labels_maps_read_their_label_and_groups_are_errors() {
    for runs_on in [
        "{ labels: depot-ubuntu-24.04-4 }",
        "{ labels: [depot-ubuntu-24.04-4] }",
    ] {
        let out = ok(&format!(
            "on: push\njobs:\n  b:\n    runs-on: {runs_on}\n    steps: [{{ run: make }}]\n"
        ));
        assert_eq!(
            out.config.jobs["ci-b"].runs_on.as_deref(),
            Some("ubuntu-26.04-4core")
        );
        round_trips(&out);
    }
    for runs_on in [
        "{ group: large }",
        "{ group: large, labels: ubuntu-latest }",
    ] {
        let errs = one(
            "ci.yml",
            &format!(
                "on: push\njobs:\n  b:\n    runs-on: {runs_on}\n    steps: [{{ run: make }}]\n"
            ),
        )
        .unwrap_err();
        assert!(
            has(
                &errs,
                &["`b`", "group: large", "runner groups are not supported"]
            ),
            "{errs:?}"
        );
    }
    let errs = one(
        "ci.yml",
        "on: push\njobs:\n  b:\n    runs-on: { labels: [a, b] }\n    steps: [{ run: make }]\n",
    )
    .unwrap_err();
    assert!(has(&errs, &["`b`", "names no runner template"]), "{errs:?}");
}

#[test]
fn a_runs_on_matrix_maps_third_party_labels() {
    let out = ok("on: push
jobs:
  t:
    runs-on: ${{ matrix.os }}
    strategy: { matrix: { os: [blacksmith-4vcpu-ubuntu-2404, windows-latest] } }
    steps: [{ run: make }]
");
    assert_eq!(out.errors, Vec::new());
    assert_eq!(
        out.config.jobs["ci-t"].runs_on.as_deref(),
        Some("ubuntu-26.04-4core")
    );
    assert!(has(
        &out.warnings,
        &["`runs-on: blacksmith-4vcpu-ubuntu-2404` became `ubuntu-26.04-4core`"]
    ));
    round_trips(&out);
}

#[test]
fn a_job_that_reads_strategy_or_jobs_is_dropped() {
    // hatchrun provides neither context, so validate would reject the
    // job; convert leaves it out instead and its output validates.
    let out = ok("
on: push
jobs:
  b:
    runs-on: ubuntu-latest
    if: strategy.fail-fast
    steps: [{ run: \"echo ${{ jobs.x.result }}\" }]
  after:
    needs: b
    runs-on: ubuntu-latest
    steps: [{ run: x }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: lint }]
");
    assert!(
        has(&out.errors, &["`b`", "`strategy` context"]),
        "{:?}",
        out.errors
    );
    assert!(
        has(&out.errors, &["`b`", "`jobs` context"]),
        "{:?}",
        out.errors
    );
    assert!(has(&out.warnings, &["`after` was dropped", "needs `b`"]));
    assert_eq!(out.config.jobs.keys().collect::<Vec<_>>(), ["ci-lint"]);
    round_trips(&out);
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
fn dependents_of_a_dropped_reusable_job_are_dropped() {
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
        has(
            &out.warnings,
            &[
                "ci.yml",
                "`after` was dropped",
                "needs `call`, which calls a reusable workflow"
            ]
        ),
        "{:?}",
        out.warnings
    );
    assert_eq!(out.config.jobs.len(), 0);
    assert_eq!(out.config.workflows["ci"].jobs, Vec::new());
    round_trips(&out);
}

#[test]
fn a_dropped_job_takes_every_job_that_needs_it() {
    // `c` comes first so the chain is followed whatever the file order.
    let out = convert(&[(
        "ci.yml".into(),
        "
on: push
jobs:
  c:
    needs: [b, d]
    runs-on: ubuntu-latest
    steps: [{ run: c }]
  a:
    runs-on: macos-latest
    steps: [{ run: a }]
  b:
    needs: a
    runs-on: ubuntu-latest
    steps: [{ run: b }]
  d:
    runs-on: ubuntu-latest
    steps: [{ run: d }]
  e:
    needs: d
    runs-on: ubuntu-latest
    steps: [{ run: e }]
"
        .into(),
    )])
    .unwrap();
    assert!(has(&out.errors, &["`a`", "runs-on"]), "{:?}", out.errors);
    assert!(
        has(
            &out.warnings,
            &["`b` was dropped: it needs `a`, which did not convert"]
        ),
        "{:?}",
        out.warnings
    );
    assert!(
        has(
            &out.warnings,
            &["`c` was dropped: it needs `b`, which needs `a`, which did not convert"]
        ),
        "{:?}",
        out.warnings
    );
    let jobs: Vec<&str> = out.config.workflows["ci"]
        .jobs
        .iter()
        .map(|j| j.job.as_str())
        .collect();
    assert_eq!(jobs, ["ci-d", "ci-e"]);
    assert_eq!(
        out.config
            .jobs
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["ci-d", "ci-e"]
    );
    round_trips(&out);
}

#[test]
fn converts_a_file_with_a_long_run_of_comments() {
    let comments = "  # old:\n  #   runs-on: ubuntu-latest\n\n".repeat(20);
    let out = ok(&format!(
        "on: push\njobs:\n{comments}  t:\n    runs-on: ubuntu-latest\n    steps: [{{ run: a }}]\n"
    ));
    assert_eq!(out.config.jobs["ci-t"].steps.len(), 1);
    round_trips(&out);
}

#[test]
fn a_runs_on_matrix_keeps_its_linux_legs() {
    let out = ok("on: push
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
        node: [20, 22]
    steps: [{ run: npm test }]
  report:
    needs: test
    runs-on: ubuntu-latest
    steps: [{ run: echo done }]
");
    assert_eq!(out.errors, Vec::new());
    assert_eq!(
        out.config.jobs["ci-test"].runs_on.as_deref(),
        Some("ubuntu-latest")
    );
    let entry = &out.config.workflows["ci"].jobs[0];
    assert_eq!(entry.matrix["os"], [json!("ubuntu-latest")]);
    assert_eq!(entry.matrix["node"], [json!(20), json!(22)]);
    assert!(
        has(
            &out.warnings,
            &["job `test`", "`macos-latest`, `windows-latest`", "dropped"]
        ),
        "{:?}",
        out.warnings
    );
    assert_eq!(out.config.workflows["ci"].jobs.len(), 2);
    round_trips(&out);
}

#[test]
fn a_runs_on_matrix_keeps_one_leg_per_template() {
    let out = ok("on: push
jobs:
  test:
    runs-on: ${{matrix.os}}
    strategy: { matrix: { os: [ubuntu-22.04, windows-2022, ubuntu-26.04] } }
    steps: [{ run: make }]
");
    assert_eq!(out.errors, Vec::new());
    assert_eq!(
        out.config.jobs["ci-test"].runs_on.as_deref(),
        Some("ubuntu-latest")
    );
    // `ubuntu-26.04` runs on the template `ubuntu-22.04` became, so its
    // leg would repeat the first one; the first value is kept as written.
    assert_eq!(
        out.config.workflows["ci"].jobs[0].matrix["os"],
        [json!("ubuntu-22.04")]
    );
    assert!(
        has(
            &out.warnings,
            &["`runs-on: ubuntu-22.04` became `ubuntu-latest`"]
        ),
        "{:?}",
        out.warnings
    );
    assert!(
        has(
            &out.warnings,
            &[
                "matrix key `os` values `ubuntu-26.04` (same runner as `ubuntu-22.04`) were dropped",
                "same runner template",
            ]
        ),
        "{:?}",
        out.warnings
    );
    round_trips(&out);
    let plan = hatchrun_workflow::compile(
        &parse(&to_yaml(&out.config)).unwrap(),
        "ci",
        "refs/heads/main",
    )
    .unwrap();
    assert_eq!(plan.jobs.len(), 1);
}

#[test]
fn a_runs_on_matrix_with_latest_and_a_version_plans_one_leg() {
    let out = ok("on: push
jobs:
  a:
    runs-on: ${{ matrix.os }}
    strategy: { matrix: { os: [ubuntu-latest, ubuntu-24.04, macos-latest], node: [20, 22] } }
    steps:
      - run: echo ${{ matrix.os }}
");
    assert_eq!(out.errors, Vec::new());
    assert_eq!(
        out.config.workflows["ci"].jobs[0].matrix["os"],
        [json!("ubuntu-latest")]
    );
    assert!(has(
        &out.warnings,
        &["`ubuntu-24.04` (same runner as `ubuntu-latest`)"]
    ));
    assert!(has(
        &out.warnings,
        &["`macos-latest`", "no runner template"]
    ));
    round_trips(&out);
    let plan = hatchrun_workflow::compile(
        &parse(&to_yaml(&out.config)).unwrap(),
        "ci",
        "refs/heads/main",
    )
    .unwrap();
    assert_eq!(plan.jobs.len(), 2);
}

#[test]
fn a_runs_on_matrix_without_a_template_is_an_error() {
    let out = ok("on: push
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy: { matrix: { os: [macos-latest, windows-latest] } }
    steps: [{ run: make }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: lint }]
");
    assert!(
        has(&out.errors, &["names no runner template"]),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-test"));
}

/// Compiles the converted config, read back from YAML, for a push to
/// main.
fn compiled(out: &Conversion) -> hatchrun_workflow::Plan {
    let config = parse(&to_yaml(&out.config)).unwrap();
    hatchrun_workflow::compile(&config, "ci", "refs/heads/main").unwrap()
}

/// Each planned job's id and the first label of its template.
fn legs(plan: &hatchrun_workflow::Plan) -> Vec<(String, &'static str)> {
    plan.jobs
        .iter()
        .map(|j| match j.kind {
            JobKind::Run { template } => (j.id.clone(), template.labels[0]),
            JobKind::Approval => (j.id.clone(), "approval"),
        })
        .collect()
}

#[test]
fn a_runs_on_matrix_over_two_templates_runs_each_leg_on_its_own() {
    let out = ok("on: push
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-22.04, ubuntu-24.04-arm, macos-latest]
        node: [20, 22]
        exclude:
          - os: ubuntu-24.04-arm
            node: 20
          - os: macos-latest
    steps: [{ run: 'echo ${{ matrix.os }}' }]
");
    assert_eq!(out.errors, Vec::new());
    let job = &out.config.jobs["ci-test"];
    assert_eq!(job.runs_on.as_deref(), Some("${{ parameters.os }}"));
    assert_eq!(job.parameters["os"].kind, ParameterKind::String);
    let entry = &out.config.workflows["ci"].jobs[0];
    assert_eq!(
        entry.matrix["os"],
        [json!("ubuntu-latest"), json!("ubuntu-26.04-arm")]
    );
    // The exclude of a dropped leg is gone; the other names the new label.
    assert_eq!(
        entry.matrix.exclude,
        [matrix_map(json!({"os": "ubuntu-26.04-arm", "node": 20}))]
    );
    for (old, new) in [
        ("ubuntu-22.04", "ubuntu-latest"),
        ("ubuntu-24.04-arm", "ubuntu-26.04-arm"),
    ] {
        assert!(
            has(
                &out.warnings,
                &[&format!("`runs-on: {old}` became `{new}`")]
            ),
            "{:?}",
            out.warnings
        );
    }
    round_trips(&out);
    assert_eq!(
        legs(&compiled(&out)),
        [
            ("test (20, ubuntu-latest)".into(), "ubuntu-latest"),
            ("test (22, ubuntu-latest)".into(), "ubuntu-latest"),
            ("test (22, ubuntu-26.04-arm)".into(), "ubuntu-26.04-arm"),
        ]
    );
}

#[test]
fn include_extends_matching_combinations_and_adds_the_rest() {
    // GitHub's documented example: six jobs.
    let out = ok("on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        fruit: [apple, pear]
        animal: [cat, dog]
        include:
          - color: green
          - color: pink
            animal: cat
          - fruit: apple
            shape: circle
          - fruit: banana
          - fruit: banana
            animal: cat
    steps: [{ run: 'echo ${{ matrix.fruit }}' }]
  after:
    needs: t
    runs-on: ubuntu-latest
    steps: [{ run: done }]
");
    assert_eq!(out.errors, Vec::new());
    round_trips(&out);
    let plan = compiled(&out);
    let mut got: Vec<Value> = plan.jobs[..6].iter().map(|j| json!(j.matrix)).collect();
    got.sort_by_key(Value::to_string);
    let mut want = vec![
        json!({"fruit": "apple", "animal": "cat", "color": "pink", "shape": "circle"}),
        json!({"fruit": "apple", "animal": "dog", "color": "green", "shape": "circle"}),
        json!({"fruit": "pear", "animal": "cat", "color": "pink"}),
        json!({"fruit": "pear", "animal": "dog", "color": "green"}),
        json!({"fruit": "banana"}),
        json!({"fruit": "banana", "animal": "cat"}),
    ];
    want.sort_by_key(Value::to_string);
    assert_eq!(got, want);
    // `needs: t` waits for every entry `t` became.
    let entries = &out.config.workflows["ci"].jobs;
    let names: Vec<&str> = entries[..6]
        .iter()
        .map(|e| e.name.as_deref().unwrap())
        .collect();
    assert_eq!(names, ["t", "t-2", "t-3", "t-4", "t-5", "t-6"]);
    assert_eq!(entries[6].requires, names);
    let ids: Vec<&str> = plan.jobs[..6].iter().map(|j| j.id.as_str()).collect();
    assert_eq!(plan.jobs[6].requires, ids);
}

#[test]
fn include_that_extends_every_combination_alike_keeps_one_entry() {
    let out = ok("on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        node: [20, 22]
        include: [{ npm: 10 }]
    steps: [{ run: x }]
");
    let entries = &out.config.workflows["ci"].jobs;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].matrix["npm"], [json!(10)]);
    round_trips(&out);
    assert_eq!(compiled(&out).jobs.len(), 2);
}

#[test]
fn an_include_only_matrix_runs_each_entry_on_its_runner() {
    // The shape of this repository's release build.
    let out = ok("on: push
jobs:
  build:
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        include:
          - { runner: ubuntu-24.04, target: x86_64 }
          - { runner: ubuntu-24.04-arm, target: aarch64 }
          - { runner: macos-latest, target: darwin }
          - { target: none }
    steps: [{ run: 'cargo build --target ${{ matrix.target }}' }]
  build-2:
    runs-on: ubuntu-latest
    steps: [{ run: x }]
  publish:
    needs: [build, build-2]
    runs-on: ubuntu-latest
    steps: [{ run: publish }]
");
    assert_eq!(out.errors, Vec::new());
    assert!(has(&out.warnings, &["`macos-latest`", "dropped"]));
    assert!(has(
        &out.warnings,
        &["without `runner`", "`runs-on` reads", "dropped"]
    ));
    let entries = &out.config.workflows["ci"].jobs;
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_deref().unwrap()).collect();
    // `build-2` is a job of its own, so the second entry skips it.
    assert_eq!(names, ["build", "build-3", "build-2", "publish"]);
    assert_eq!(entries[3].requires, ["build", "build-3", "build-2"]);
    round_trips(&out);
    assert_eq!(
        legs(&compiled(&out))[..2],
        [
            ("build (ubuntu-latest, x86_64)".into(), "ubuntu-latest"),
            (
                "build-3 (ubuntu-26.04-arm, aarch64)".into(),
                "ubuntu-26.04-arm"
            ),
        ]
    );
}

#[test]
fn bad_include_and_exclude_entries_are_dropped_with_a_warning() {
    let out = ok("on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        node: [20, 22]
        os: ${{ fromJSON(needs.setup.outputs.os) }}
        include: ${{ fromJSON(needs.setup.outputs.include) }}
        exclude:
          - os: linux
          - 7
    steps: [{ run: x }]
");
    for words in [
        &["matrix key `os`", "not a list"][..],
        &["`strategy.matrix.include`", "not a list of maps"],
        &["`strategy.matrix.exclude` entry `7`", "not a map"],
        &[
            "`strategy.matrix.exclude` entry names `os`",
            "not a matrix key",
        ],
    ] {
        assert!(has(&out.warnings, words), "{words:?}: {:?}", out.warnings);
    }
    let entry = &out.config.workflows["ci"].jobs[0];
    assert_eq!(entry.matrix.exclude, Vec::<BTreeMap<String, Value>>::new());
    round_trips(&out);
}

#[test]
fn defaults_run_applies_to_run_steps() {
    let out = ok("
on: push
defaults:
  run:
    shell: bash
    working-directory: app
jobs:
  build:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: web
    steps:
      - run: npm test
      - run: make
        shell: sh
        working-directory: .
      - uses: actions/checkout@v5
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: lint
");
    assert_eq!(out.warnings, Vec::new());
    let shape = |job: &str, i: usize| {
        let Step::Run(run) = &out.config.jobs[job].steps[i] else {
            panic!("{job} step {i} is not a run step");
        };
        (run.shell.clone(), run.working_directory.clone())
    };
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(shape("ci-build", 0), (some("bash"), some("web")));
    assert_eq!(shape("ci-build", 1), (some("sh"), some(".")));
    assert_eq!(shape("ci-lint", 0), (some("bash"), some("app")));
    round_trips(&out);
}

#[test]
fn other_defaults_are_dropped_with_a_warning() {
    let out = ok("
on: push
defaults:
  run:
    shell: bash
    locale: C
  other: 1
jobs:
  build:
    runs-on: ubuntu-latest
    defaults: [bash]
    steps:
      - run: make
");
    assert!(has(
        &out.warnings,
        &["ci.yml", "`defaults.run.locale`", "dropped"]
    ));
    assert!(has(
        &out.warnings,
        &["ci.yml", "`defaults.other`", "dropped"]
    ));
    assert!(has(
        &out.warnings,
        &["job `build`", "`defaults`", "dropped"]
    ));
    let Step::Run(run) = &out.config.jobs["ci-build"].steps[0] else {
        panic!("not a run step");
    };
    assert_eq!(run.shell.as_deref(), Some("bash"));
    round_trips(&out);
}

#[test]
fn a_file_over_one_mebibyte_is_skipped() {
    let big = format!(
        "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: x\n{}",
        "# padding\n".repeat(110_000)
    );
    let errors = one("big.yml", &big).unwrap_err();
    assert!(
        has(&errors, &["`big.yml` is", "bytes; the limit is 1048576"]),
        "{errors:?}"
    );
    assert_eq!(errors[0].file.as_deref(), Some("big.yml"));
    let out = convert(&[
        ("big.yml".into(), big),
        (
            "ci.yml".into(),
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: x\n"
                .into(),
        ),
    ])
    .unwrap();
    assert!(out.config.workflows.contains_key("ci"));
    assert!(!out.config.workflows.contains_key("big"));
}

#[test]
fn a_job_with_a_bad_expression_is_dropped() {
    let out = ok(r"
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ foo.bar }}
  b:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ 1 + }}
  c:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-node@v4
        with:
          node-version: ${{ parameters.node }}
  d:
    runs-on: ubuntu-latest
    if: github.event_name == 'push'
    steps:
      - run: echo ${{ github.sha
  ok:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ github.sha }}
");
    assert!(
        has(
            &out.errors,
            &["job `a`", "unknown context 'foo'", "dropped"]
        ),
        "{:?}",
        out.errors
    );
    assert!(has(&out.errors, &["job `b`", "bad expression", "dropped"]));
    assert!(has(&out.errors, &["job `c`", "`parameters`", "dropped"]));
    assert!(has(&out.errors, &["job `d`", "no closing", "dropped"]));
    assert_eq!(out.config.jobs.keys().collect::<Vec<_>>(), ["ci-ok"]);
    round_trips(&out);
}

/// A GitHub workflow with `jobs` jobs, each with a run step of `pad`
/// bytes.
fn workflow_with(jobs: usize, pad: usize) -> String {
    let mut y = String::from("on: push\njobs:\n");
    for j in 0..jobs {
        write!(
            y,
            "  j{j}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo {}\n",
            "x".repeat(pad)
        )
        .unwrap();
    }
    y
}

#[test]
fn a_workflow_over_the_job_limit_is_an_error() {
    let out = ok(&workflow_with(257, 1));
    assert!(
        has(
            &out.errors,
            &["workflow `ci` plans more than 256 jobs; the limit is 256"]
        ),
        "{:?}",
        out.errors
    );
    let at_limit = ok(&workflow_with(256, 1));
    assert_eq!(at_limit.errors, Vec::new());
    round_trips(&at_limit);
}

#[test]
fn a_converted_config_over_one_mebibyte_is_an_error() {
    // Six files of about 192 KB: each is under the limit, together the
    // written config is not.
    let files: Vec<(String, String)> = (0..6)
        .map(|i| (format!("w{i}.yml"), workflow_with(48, 4000)))
        .collect();
    assert!(files.iter().all(|(_, f)| f.len() < 200_000));
    let out = convert(&files).unwrap();
    let written = to_yaml(&out.config).len();
    assert!(written > 1 << 20);
    assert!(
        has(
            &out.errors,
            &[&format!(
                "the converted config is {written} bytes; the limit is 1048576"
            )]
        ),
        "{:?}",
        out.errors
    );
}

#[test]
fn workflows_with_no_events_left_are_dropped() {
    let out = convert(&[
        (
            "triage.yml".into(),
            "
on:
  issues:
    types: [opened]
  issue_comment:
jobs:
  label: { runs-on: windows-latest, steps: [{ run: x }] }
"
            .into(),
        ),
        (
            "ci.yml".into(),
            "on: push\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n".into(),
        ),
    ])
    .unwrap();
    assert!(has(
        &out.warnings,
        &[
            "triage.yml",
            "dropped",
            "`issues`",
            "`issue_comment`",
            "GitHub Actions"
        ]
    ));
    // One warning for the file, none per event, and its jobs are not
    // converted, so the Windows runner is not an error.
    assert_eq!(
        out.warnings
            .iter()
            .filter(|w| w.message.contains("triage.yml"))
            .count(),
        1,
        "{:?}",
        out.warnings
    );
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(!out.config.workflows.contains_key("triage"));
    assert!(!out.config.jobs.contains_key("triage-label"));
    assert!(out.config.workflows.contains_key("ci"));
    round_trips(&out);
}

#[test]
fn reusable_workflow_files_are_dropped() {
    let out = ok("
on:
  workflow_call:
    inputs:
      target: { type: string }
jobs:
  build: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert!(has(
        &out.warnings,
        &["ci.yml", "reusable workflow", "workflow_call"]
    ));
    assert!(out.config.workflows.is_empty());
    assert!(out.config.jobs.is_empty());
}

#[test]
fn a_workflow_whose_events_all_fail_is_dropped() {
    let out = one(
        "ci.yml",
        "on:\n  push:\n    branches: 3\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    )
    .unwrap_err();
    assert!(has(&out, &["ci.yml", "on.push"]), "{out:?}");
}
