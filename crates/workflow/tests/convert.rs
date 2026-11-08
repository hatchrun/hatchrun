// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Converting GitHub Actions workflows into a hatchrun config.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hatchrun_workflow::{
    ContinueOnError, Conversion, Diagnostic, Filter, JobKind, Level, ParameterKind, Permissions,
    Require, Step, convert, parse, to_yaml, validate,
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
    let yaml = to_yaml(&out.config).unwrap();
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
        Vec::<Require>::new()
    );
    assert!(job.services.contains_key("postgres"));
    // round trip: the YAML we write parses and validates clean
    let reparsed = parse(&to_yaml(&out.config).unwrap()).unwrap();
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
    assert_eq!(test.requires, [Require::new("lint")]);
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
    let pg = &out.config.jobs["ci-test"].services["postgres"];
    assert_eq!(pg.image, "postgres:17");
    assert_eq!(pg.env["POSTGRES_PASSWORD"], "postgres");
    assert_eq!(pg.ports, ["5432:5432"]);
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
fn docker_steps_convert_to_uses_steps() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - name: Alpine
        uses: docker://alpine:3.20
        with:
          entrypoint: /bin/echo
          args: hello
");
    let Step::Uses(step) = &out.config.jobs["ci-build"].steps[0] else {
        panic!("{:?}", out.config.jobs["ci-build"].steps)
    };
    assert_eq!(step.uses, "docker://alpine:3.20");
    assert_eq!(step.with["entrypoint"], "/bin/echo");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let errs = one(
        "ci.yml",
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: docker://\n",
    )
    .unwrap_err();
    assert!(has(&errs, &["ci.yml", "build", "no image"]), "{errs:?}");
}

#[test]
fn containers_and_services_convert() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/acme/build:${{ matrix.v }}
      credentials:
        username: bot
        password: ${{ secrets.GHCR_TOKEN }}
      env:
        MODE: 1
      ports: [8080, '9090:90']
      volumes: ['/data:/data']
      options: --cpus 1
      bogus: x
    services:
      redis: redis:7
      db:
        image: postgres:17
        options: >-
          --health-cmd pg_isready
          --health-interval 10s
    strategy:
      matrix:
        v: [1, 2]
    steps:
      - run: true
");
    let job = &out.config.jobs["ci-build"];
    let container = job.container.as_ref().unwrap();
    assert_eq!(container.image, "ghcr.io/acme/build:${{ matrix.v }}");
    assert_eq!(container.credentials.as_ref().unwrap().username, "bot");
    assert_eq!(container.env["MODE"], "1");
    assert_eq!(container.ports, ["8080", "9090:90"]);
    assert_eq!(job.services["redis"].image, "redis:7");
    assert!(
        job.services["db"]
            .options
            .as_deref()
            .unwrap()
            .contains("--health-cmd pg_isready")
    );
    assert!(has(&out.warnings, &["ci.yml", "build", "container.bogus"]));
    // The converted config validates, round trips and plans.
    let yaml = to_yaml(&out.config).unwrap();
    let reparsed = parse(&yaml).unwrap();
    assert_eq!(reparsed, out.config);
    assert_eq!(validate(&reparsed), Vec::<Diagnostic>::new());
}

#[test]
fn bad_containers_drop_the_job() {
    for (body, word) in [
        ("container: { env: { A: b } }", "needs an `image`"),
        ("container: [a]", "must be an image"),
        (
            "container: { image: a, credentials: { username: u } }",
            "credentials",
        ),
        (
            "container: { image: a, options: --network host }",
            "--network",
        ),
        ("services: { 'a.b': x }", "not a hostname"),
        ("services: [x]", "map of service ids"),
        ("services: { db: { image: x, ports: 5 } }", "must be a list"),
    ] {
        let yaml = format!(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    {body}\n    steps:\n      - run: true\n"
        );
        let errs = one("ci.yml", &yaml).unwrap_err();
        assert!(has(&errs, &["ci.yml", "build", word]), "{body}: {errs:?}");
    }
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
    let on = &out.config.workflows["ci"].on;
    assert!(on.push.is_some());
    assert_eq!(on.release.as_ref().unwrap().types, ["published"]);
    assert!(!has(&out.warnings, &["`on.release`"]));
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
    assert!(
        !out.warnings.iter().any(|w| w.message.contains("container")),
        "container: {:?}",
        out.warnings
    );
    assert_eq!(
        out.config.jobs["ci-build"]
            .container
            .as_ref()
            .unwrap()
            .image,
        "node:22"
    );
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.message.contains("concurrency")),
        "{:?}",
        out.warnings
    );
    assert_eq!(
        out.config.workflows["ci"]
            .concurrency
            .as_ref()
            .unwrap()
            .group,
        "ci"
    );
    assert_eq!(
        out.config.jobs["ci-build"]
            .concurrency
            .as_ref()
            .unwrap()
            .group,
        "build"
    );
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
fn permissions_carry_over() {
    let out = ok("
on: push
permissions: read-all
jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - run: x
  other:
    runs-on: ubuntu-latest
    permissions: write-all
    steps:
      - run: x
");
    assert_eq!(out.warnings, Vec::new());
    assert_eq!(
        out.config.workflows["ci"].permissions,
        Some(Permissions::ReadAll)
    );
    assert_eq!(
        out.config.jobs["ci-build"].permissions,
        Some(Permissions::Scopes(
            [
                ("contents".to_owned(), Level::Read),
                ("pull-requests".to_owned(), Level::Write)
            ]
            .into()
        ))
    );
    assert_eq!(
        out.config.jobs["ci-other"].permissions,
        Some(Permissions::WriteAll)
    );
    round_trips(&out);
}

#[test]
fn an_invalid_job_permissions_value_gives_the_narrowest_set() {
    let out = ok("
on: push
permissions: write-all
jobs:
  build:
    runs-on: ubuntu-latest
    permissions: read
    steps:
      - run: x
");
    assert_eq!(
        out.config.jobs["ci-build"].permissions,
        Some(Permissions::Scopes(BTreeMap::new()))
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.message.contains("permissions: read")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn id_token_write_is_kept() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    permissions: { id-token: write, contents: read }
    steps:
      - run: x
");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        out.config.jobs["ci-build"].permissions,
        Some(Permissions::Scopes(
            [
                ("contents".to_owned(), Level::Read),
                ("id-token".to_owned(), Level::Write),
            ]
            .into()
        ))
    );
    round_trips(&out);
}

#[test]
fn id_token_read_is_dropped_with_a_warning() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    permissions: { id-token: read, contents: read }
    steps:
      - run: x
");
    assert!(
        has(
            &out.warnings,
            &[
                "build",
                "`permissions.id-token` is `read`, not write or none"
            ]
        ),
        "{:?}",
        out.warnings
    );
    assert_eq!(
        out.config.jobs["ci-build"].permissions,
        Some(Permissions::Scopes(
            [("contents".to_owned(), Level::Read)].into()
        ))
    );
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
fn uses_steps_keep_their_timeout() {
    let out = ok("
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/cache@v4
        timeout-minutes: 4
      - uses: actions/setup-node@v4
        timeout-minutes: soon
");
    let steps = &out.config.jobs["ci-t"].steps;
    let Step::Uses(first) = &steps[0] else {
        panic!("{:?}", steps[0])
    };
    let Step::Uses(second) = &steps[1] else {
        panic!("{:?}", steps[1])
    };
    assert_eq!(first.timeout_minutes, Some(4));
    assert_eq!(second.timeout_minutes, None);
    assert!(has(&out.warnings, &["timeout-minutes: soon", "dropped"]));
    assert!(!has(&out.warnings, &["only on run steps"]));
    round_trips(&out);
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
fn a_job_that_reads_the_jobs_context_is_dropped() {
    // hatchrun provides no `jobs` context, so validate would reject the
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
        has(&out.errors, &["`b`", "`jobs` context"]),
        "{:?}",
        out.errors
    );
    assert!(has(&out.warnings, &["`after` was dropped", "needs `b`"]));
    assert_eq!(out.config.jobs.keys().collect::<Vec<_>>(), ["ci-lint"]);
    round_trips(&out);
}

#[test]
fn a_matrix_expression_is_kept_for_run_time() {
    let out = ok("
on: push
jobs:
  setup:
    runs-on: ubuntu-latest
    outputs: { m: '${{ steps.m.outputs.m }}' }
    steps: [{ id: m, run: 'echo m={} >> $GITHUB_OUTPUT' }]
  t:
    needs: setup
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix: ${{ fromJSON(needs.setup.outputs.m) }}
    steps: [{ run: 'echo ${{ matrix.os }}' }]
");
    assert_eq!(out.errors, Vec::new());
    let entry = &out.config.workflows["ci"].jobs[1];
    assert_eq!(
        entry.matrix.from.as_deref(),
        Some("${{ fromJSON(needs.setup.outputs.m) }}")
    );
    assert!(!entry.strategy.as_ref().unwrap().fail_fast);
    round_trips(&out);
    let plan = compiled(&out);
    assert_eq!(plan.jobs.len(), 2);
    assert!(plan.jobs[1].deferred.is_some());
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
    assert_eq!(
        out.config.workflows["ci"].jobs[1].requires,
        [Require::new("a")]
    );
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
        &parse(&to_yaml(&out.config).unwrap()).unwrap(),
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
        &parse(&to_yaml(&out.config).unwrap()).unwrap(),
        "ci",
        "refs/heads/main",
    )
    .unwrap();
    assert_eq!(plan.jobs.len(), 2);
}

#[test]
fn github_larger_runner_labels_map_to_templates() {
    let cases = [
        ("ubuntu-latest-4-cores", "ubuntu-26.04-4core"),
        ("ubuntu-22.04-8core", "ubuntu-26.04-8core"),
        ("ubuntu-24.04-16core", "ubuntu-26.04-16core"),
        ("ubuntu-latest-xl", "ubuntu-26.04-8core"),
        ("ubuntu-24.04-arm-8-cores", "ubuntu-26.04-8core-arm"),
    ];
    for (label, want) in cases {
        let out = ok(&format!(
            "on: push\njobs:\n  t: {{ runs-on: {label}, steps: [{{ run: x }}] }}\n"
        ));
        assert_eq!(out.errors, [], "{label}");
        assert_eq!(
            out.config.jobs["ci-t"].runs_on.as_deref(),
            Some(want),
            "{label}"
        );
        assert!(has(&out.warnings, &[label, "became"]), "{label}");
        round_trips(&out);
    }
    // Sizes with no template run on the largest, and the warning says so.
    let out =
        ok("on: push\njobs:\n  t: { runs-on: ubuntu-latest-32-cores, steps: [{ run: x }] }\n");
    assert_eq!(
        out.config.jobs["ci-t"].runs_on.as_deref(),
        Some("ubuntu-26.04-16core")
    );
    assert!(has(
        &out.warnings,
        &["asks for 32 vCPUs", "the largest template has 16"]
    ));
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
    let config = parse(&to_yaml(&out.config).unwrap()).unwrap();
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
    let needed: Vec<&str> = entries[6].requires.iter().map(|r| r.job.as_str()).collect();
    assert_eq!(needed, names);
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
    let needed: Vec<&str> = entries[3].requires.iter().map(|r| r.job.as_str()).collect();
    assert_eq!(needed, ["build", "build-3", "build-2"]);
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
    assert!(
        has(
            &out.warnings,
            &["`strategy.matrix.exclude` entry `7`", "not a map"]
        ),
        "{:?}",
        out.warnings
    );
    let entry = &out.config.workflows["ci"].jobs[0];
    // Keys the expressions make are unknown, so `exclude` keeps its entry.
    assert_eq!(entry.matrix.exclude, [matrix_map(json!({"os": "linux"}))]);
    assert!(entry.matrix.lists.contains_key("os"));
    assert!(entry.matrix.include_from.is_some());
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
fn a_double_quoted_string_is_reported_with_its_location() {
    let out = ok(r#"
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ secrets["X"] }}
  ok:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ secrets['it''s'] }}
"#);
    assert!(
        has(
            &out.errors,
            &["job `a`", "use single quotes", "at byte", "dropped"]
        ),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-a"), "{:?}", out.errors);
    assert!(out.config.jobs.contains_key("ci-ok"), "{:?}", out.errors);
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
    let written = to_yaml(&out.config).unwrap().len();
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
fn environment_carries_over() {
    let out = ok("
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    environment: production
    steps: [{ run: a }]
  b:
    runs-on: ubuntu-latest
    environment: { name: staging, url: 'https://x' }
    steps: [{ run: b }]
");
    assert_eq!(
        out.config.jobs["ci-a"].environment.as_deref(),
        Some("production")
    );
    assert_eq!(
        out.config.jobs["ci-b"].environment.as_deref(),
        Some("staging")
    );
    assert_eq!(
        out.config.jobs["ci-b"].environment_url.as_deref(),
        Some("https://x")
    );
    assert_eq!(out.config.jobs["ci-a"].environment_url, None);
    assert_eq!(out.warnings, []);
    round_trips(&out);
}

#[test]
fn run_name_converts() {
    let out = ok("
run-name: Deploy ${{ inputs.target }} by ${{ github.actor }}
on: push
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert_eq!(
        out.config.workflows["ci"].run_name.as_deref(),
        Some("Deploy ${{ inputs.target }} by ${{ github.actor }}")
    );
    assert_eq!(out.warnings, []);
    round_trips(&out);

    let out = ok("
run-name: ${{ env.X }}
on: push
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert_eq!(out.config.workflows["ci"].run_name, None);
    assert!(has(&out.warnings, &["`run-name` reads `env`", "dropped"]));

    let out = ok("
run-name: ${{ github.
on: push
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert_eq!(out.config.workflows["ci"].run_name, None);
    assert!(has(&out.warnings, &["`run-name`", "bad expression"]));
}

#[test]
fn environment_url_is_checked_with_the_job() {
    let out = ok("
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: { name: prod, url: '${{ secrets.X }}/${{ nothing.here }}' }
    steps: [{ run: d }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: l }]
");
    assert!(
        has(&out.errors, &["unknown context 'nothing'"]),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-deploy"));
    assert!(out.config.jobs.contains_key("ci-lint"));
}

#[test]
fn environment_names_and_urls_can_be_expressions() {
    let out = ok("
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    needs: pick
    strategy: { matrix: { region: [eu, us] } }
    environment:
      name: ${{ needs.pick.outputs.stage }}-${{ matrix.region }}
      url: ${{ steps.up.outputs.url }}
    steps: [{ id: up, run: d }]
  pick:
    runs-on: ubuntu-latest
    outputs: { stage: '${{ steps.s.outputs.v }}' }
    steps: [{ id: s, run: p }]
");
    assert_eq!(out.errors, []);
    assert_eq!(out.warnings, []);
    let job = &out.config.jobs["ci-deploy"];
    assert_eq!(
        job.environment.as_deref(),
        Some("${{ needs.pick.outputs.stage }}-${{ matrix.region }}")
    );
    assert_eq!(
        job.environment_url.as_deref(),
        Some("${{ steps.up.outputs.url }}")
    );
    round_trips(&out);
}

#[test]
fn environment_names_that_read_other_contexts_drop_the_job() {
    let out = ok("
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: ${{ env.STAGE }}
    steps: [{ run: d }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: l }]
");
    assert!(
        has(&out.errors, &["`environment` reads `env`"]),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-deploy"));
}

#[test]
fn an_environment_expression_that_does_not_parse_drops_the_job() {
    let out = convert(&[(
        "ci.yml".into(),
        "
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: ${{ matrix.env
    steps: [{ run: d }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: l }]
"
        .into(),
    )])
    .unwrap();
    assert!(
        has(&out.errors, &["`environment`", "bad expression"]),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-deploy"));
}

#[test]
fn environment_of_another_shape_drops_the_job() {
    for value in ["[a]", "{ url: 'https://x' }", "{ name: [a] }"] {
        let yaml = format!(
            "
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: {value}
    steps: [{{ run: d }}]
  lint:
    runs-on: ubuntu-latest
    steps: [{{ run: l }}]
"
        );
        let out = convert(&[("ci.yml".into(), yaml)]).unwrap();
        assert!(
            has(&out.errors, &["must be a name or a map with `name`"]),
            "{value}: {:?}",
            out.errors
        );
        assert!(!out.config.jobs.contains_key("ci-deploy"), "{value}");
    }
}

#[test]
fn environment_with_a_bad_name_drops_the_job() {
    let out = convert(&[(
        "ci.yml".into(),
        "
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: prod eu
    steps: [{ run: d }]
  lint:
    runs-on: ubuntu-latest
    steps: [{ run: l }]
"
        .into(),
    )])
    .unwrap();
    assert!(
        has(
            &out.errors,
            &["`environment` `prod eu`", "Use letters, digits"]
        ),
        "{:?}",
        out.errors
    );
    assert!(!out.config.jobs.contains_key("ci-deploy"));
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
fn a_workflow_whose_events_all_fail_is_dropped() {
    let out = one(
        "ci.yml",
        "on:\n  push:\n    branches: 3\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    )
    .unwrap_err();
    assert!(has(&out, &["ci.yml", "on.push"]), "{out:?}");
}

#[test]
fn github_only_events_say_to_stay_on_github_actions() {
    for event in ["pull_request_target", "issues", "issue_comment"] {
        let out = ok(&format!(
            "on: [push, {event}]\njobs:\n  t: {{ runs-on: ubuntu-latest, steps: [{{ run: x }}] }}\n"
        ));
        assert!(
            has(
                &out.warnings,
                &[
                    &format!("`on.{event}` is not supported"),
                    "keep",
                    "GitHub Actions"
                ]
            ),
            "{event}: {:?}",
            out.warnings
        );
    }
    // Others do not.
    let out =
        ok("on: [push, watch]\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n");
    assert!(has(&out.warnings, &["`on.watch` is not supported"]));
    assert!(!has(&out.warnings, &["GitHub Actions"]));
}

/// Converts several files at once.
fn many(files: &[(&str, &str)]) -> Conversion {
    let files: Vec<(String, String)> = files
        .iter()
        .map(|(n, s)| ((*n).to_owned(), (*s).to_owned()))
        .collect();
    convert(&files).unwrap()
}

const CI: &str =
    "name: CI\non: push\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n";

#[test]
fn workflow_run_names_workflows_by_their_stem() {
    let out = many(&[
        ("ci.yml", CI),
        (
            "unnamed.yml",
            "on: push\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
        (
            "deploy.yml",
            "
on:
  workflow_run:
    workflows: [CI, unnamed.yml, Missing, Deploy]
    types: [completed, bogus]
    branches: [main]
name: Deploy
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
",
        ),
    ]);
    let run = out.config.workflows["deploy"]
        .on
        .workflow_run
        .as_ref()
        .unwrap();
    assert_eq!(run.workflows, ["ci", "unnamed"]);
    assert_eq!(run.types, ["completed"]);
    assert_eq!(run.branches, Some(Filter::new(vec!["main".into()])));
    assert!(has(&out.warnings, &["deploy.yml", "`Missing`", "dropped"]));
    assert!(has(&out.warnings, &["deploy.yml", "`Deploy`", "this file"]));
    assert!(has(&out.warnings, &["deploy.yml", "`bogus`", "dropped"]));
    assert_eq!(out.errors, []);
    round_trips(&out);
}

#[test]
fn workflow_run_with_no_known_workflow_is_dropped() {
    let out = many(&[
        ("ci.yml", CI),
        (
            "deploy.yml",
            "on:\n  push:\n  workflow_run: { workflows: [Nope] }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
        (
            "only.yml",
            "on:\n  workflow_run: { workflows: [CI], types: [bogus] }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
    ]);
    let deploy = &out.config.workflows["deploy"].on;
    assert!(deploy.workflow_run.is_none() && deploy.push.is_some());
    assert!(has(
        &out.warnings,
        &["deploy.yml", "`on.workflow_run` was dropped"]
    ));
    // A workflow left with no event is dropped with its jobs.
    assert!(!out.config.workflows.contains_key("only"));
    assert!(!out.config.jobs.contains_key("only-t"));
    assert!(has(&out.warnings, &["only.yml", "`bogus`"]));
    round_trips(&out);
}

#[test]
fn workflow_run_after_a_workflow_that_did_not_convert() {
    let out = many(&[
        (
            "ci.yml",
            "name: CI\non: issues\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
        (
            "deploy.yml",
            "name: Deploy\non:\n  workflow_run: { workflows: [CI] }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
        (
            "notify.yml",
            "on:\n  workflow_run: { workflows: [Deploy] }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
    ]);
    // ci is dropped, so deploy is, and so notify is.
    assert!(
        out.config.workflows.is_empty(),
        "{:?}",
        out.config.workflows
    );
    assert!(out.config.jobs.is_empty());
    assert!(has(
        &out.warnings,
        &["deploy.yml", "`ci`", "did not convert"]
    ));
    assert!(has(&out.warnings, &["notify.yml` was dropped"]));
}

#[test]
fn release_types_convert() {
    let out = ok(
        "on:\n  release:\n    types: [published, created, bogus]\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    );
    let release = out.config.workflows["ci"].on.release.as_ref().unwrap();
    assert_eq!(release.types, ["published", "created"]);
    assert!(has(&out.warnings, &["`on.release`", "`bogus`", "dropped"]));
    round_trips(&out);

    let out = ok("on: release\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n");
    let release = out.config.workflows["ci"].on.release.as_ref().unwrap();
    assert_eq!(release.types, ["published"]);
    assert!(has(
        &out.warnings,
        &["`on.release` has no `types`", "`published`"]
    ));
    round_trips(&out);

    let out = ok(
        "on:\n  push:\n  release: { types: bogus }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    );
    assert!(out.config.workflows["ci"].on.release.is_none());
    assert!(has(&out.warnings, &["`on.release` was dropped", "`bogus`"]));
}

/// The steps of the one job of a converted one-step workflow.
fn converted_steps(steps: &str) -> Conversion {
    ok(&format!(
        "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{steps}"
    ))
}

#[test]
fn checkout_stays_a_uses_step_at_its_ref() {
    let out = converted_steps(
        "
      - uses: actions/checkout@v4
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683
        with:
          ref: ${{ github.head_ref }}
          fetch-depth: 0
      - uses: https://code.forgejo.org/actions/checkout@v4
",
    );
    let uses: Vec<_> = out.config.jobs["ci-t"]
        .steps
        .iter()
        .map(|s| match s {
            Step::Uses(u) => u.uses.as_str(),
            other => panic!("not a uses step: {other:?}"),
        })
        .collect();
    assert_eq!(
        uses,
        [
            "actions/checkout@v4",
            "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
            "https://code.forgejo.org/actions/checkout@v4",
        ]
    );
    assert!(!has(&out.warnings, &["built-in checkout"]));
    round_trips(&out);
}

#[test]
fn fail_fast_and_max_parallel_become_the_entry_strategy() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      max-parallel: 2
      matrix: { node: [18, 20, 22] }
    steps: [{ run: \"echo ${{ strategy.job-index }} of ${{ strategy.job-total }}\" }]
  lint:
    runs-on: ubuntu-latest
    strategy: { matrix: { node: [20] } }
    steps: [{ run: lint }]
");
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.message.contains("fail-fast") || w.message.contains("max-parallel")),
        "{:?}",
        out.warnings
    );
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let jobs = &out.config.workflows["ci"].jobs;
    let strategy = jobs[0].strategy.as_ref().unwrap();
    assert!(!strategy.fail_fast);
    assert_eq!(strategy.max_parallel, Some(2));
    assert_eq!(strategy.group, None);
    assert_eq!(jobs[1].strategy, None);
    round_trips(&out);
}

#[test]
fn include_entries_stay_one_matrix_through_a_group() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      max-parallel: 3
      matrix:
        node: [18, 20]
        include:
          - node: 22
            experimental: true
    steps: [{ run: x }]
");
    let jobs = &out.config.workflows["ci"].jobs;
    assert!(jobs.len() > 1, "{jobs:?}");
    for entry in jobs {
        let strategy = entry.strategy.as_ref().unwrap();
        assert_eq!(strategy.group.as_deref(), Some("ci-build"));
        assert!(!strategy.fail_fast);
        assert_eq!(strategy.max_parallel, Some(3));
    }
    round_trips(&out);
}

#[test]
fn bad_strategy_values_are_dropped_with_a_warning() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: ${{ github.ref == 'x' }}
      max-parallel: 0
      matrix: { node: [18] }
    steps: [{ run: x }]
");
    assert!(has(&out.warnings, &["build", "fail-fast", "expression"]));
    assert!(has(&out.warnings, &["build", "max-parallel", "1 or more"]));
    let strategy = &out.config.workflows["ci"].jobs[0].strategy;
    assert_eq!(strategy, &None);
    round_trips(&out);
}

#[test]
fn job_continue_on_error_converts() {
    let out = ok("
on: push
jobs:
  soft:
    runs-on: ubuntu-latest
    continue-on-error: true
    steps: [{ run: x }]
  matrixed:
    runs-on: ubuntu-latest
    continue-on-error: ${{ matrix.experimental }}
    strategy:
      matrix:
        node: [18, 20]
        experimental: [false]
    steps: [{ run: x }]
  odd:
    runs-on: ubuntu-latest
    continue-on-error: maybe
    steps: [{ run: x }]
");
    let jobs = &out.config.jobs;
    assert_eq!(
        jobs["ci-soft"].continue_on_error,
        Some(ContinueOnError::Always(true))
    );
    assert_eq!(
        jobs["ci-matrixed"].continue_on_error,
        Some(ContinueOnError::When("${{ matrix.experimental }}".into()))
    );
    assert_eq!(jobs["ci-odd"].continue_on_error, None);
    assert!(has(&out.warnings, &["odd", "continue-on-error", "maybe"]));
    assert!(
        !has(&out.warnings, &["soft", "continue-on-error"])
            && !has(&out.warnings, &["matrixed", "continue-on-error"])
    );
    round_trips(&out);
}

#[test]
fn a_job_that_reads_the_strategy_context_converts() {
    let out = ok("
on: push
jobs:
  b:
    runs-on: ubuntu-latest
    strategy: { matrix: { n: [1, 2] } }
    if: strategy.job-index == 0
    steps: [{ run: \"echo ${{ strategy.job-total }} ${{ strategy['max-parallel'] }}\" }]
");
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.config.jobs.contains_key("ci-b"));
    round_trips(&out);
}

#[test]
fn create_and_delete_convert() {
    let out = ok(
        "on:\n  create:\n  delete:\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
    );
    let on = &out.config.workflows["ci"].on;
    assert!(on.create.is_some() && on.delete.is_some());
    assert_eq!(out.errors, []);
    assert_eq!(out.warnings, []);
    round_trips(&out);

    let out =
        ok("on: [create, delete]\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n");
    let on = &out.config.workflows["ci"].on;
    assert!(on.create.is_some() && on.delete.is_some());
}

#[test]
fn workflow_run_and_release_keep_their_activity_types() {
    let out = many(&[
        ("ci.yml", CI),
        (
            "after.yml",
            "on:\n  workflow_run: { workflows: [CI], types: [requested, in_progress, completed] }\n  release: { types: [unpublished, edited, deleted] }\njobs:\n  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        ),
    ]);
    let on = &out.config.workflows["after"].on;
    assert_eq!(
        on.workflow_run.as_ref().unwrap().types,
        ["requested", "in_progress", "completed"]
    );
    assert_eq!(
        on.release.as_ref().unwrap().types,
        ["unpublished", "edited", "deleted"]
    );
    assert_eq!(out.warnings, []);
    round_trips(&out);
}

#[test]
fn runs_on_expressions_are_kept_for_run_time() {
    let out = ok("on: push
jobs:
  a:
    runs-on: ${{ inputs.runner }}
    steps: [{ run: x }]
  b:
    runs-on: ${{ github.event_name == 'push' && 'ubuntu-24.04' || 'ubuntu-latest' }}
    steps: [{ run: x }]
  c:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: ${{ fromJSON(needs.a.outputs.os) }}
        include: ${{ fromJSON(needs.a.outputs.extra) }}
    needs: a
    steps: [{ run: x }]
");
    assert_eq!(out.errors, Vec::new());
    let jobs = &out.config.jobs;
    assert_eq!(
        jobs["ci-a"].runs_on.as_deref(),
        Some("${{ inputs.runner }}")
    );
    // The text stays as written: the label it reads is brought to a
    // template when the job is ready, so a comparison reads it unchanged.
    assert_eq!(
        jobs["ci-b"].runs_on.as_deref(),
        Some("${{ github.event_name == 'push' && 'ubuntu-24.04' || 'ubuntu-latest' }}")
    );
    assert_eq!(jobs["ci-c"].runs_on.as_deref(), Some("${{ matrix.os }}"));
    assert!(has(&out.warnings, &["`runs-on: ubuntu-24.04` became"]));
    round_trips(&out);
}

#[test]
fn a_runs_on_that_reads_an_unknown_context_drops_the_job() {
    let out = ok("on: push
jobs:
  a:
    runs-on: ${{ secrets.RUNNER }}
    steps: [{ run: x }]
  b:
    runs-on: ubuntu-latest
    steps: [{ run: x }]
");
    assert!(
        has(&out.errors, &["`runs-on` reads `secrets`"]),
        "{:?}",
        out.errors
    );
    assert_eq!(out.config.jobs.keys().collect::<Vec<_>>(), ["ci-b"]);
}

const DOCKER_BUILD: &str = include_str!("fixtures/github/docker-build.yml");
const RELEASE: &str = include_str!("fixtures/github/release.yml");

const REUSABLE_JOB: &str =
    "on: workflow_call\njobs:\n  j: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n";

fn release() -> Conversion {
    many(&[("release.yml", RELEASE), ("docker-build.yml", DOCKER_BUILD)])
}

fn entry<'a>(out: &'a Conversion, workflow: &str, id: &str) -> &'a hatchrun_workflow::WorkflowJob {
    out.config.workflows[workflow]
        .jobs
        .iter()
        .find(|j| j.name.as_deref().unwrap_or(&j.job) == id)
        .unwrap_or_else(|| panic!("no entry {id}"))
}

#[test]
fn a_reusable_workflow_file_becomes_a_workflow_with_on_call() {
    let out = release();
    assert_eq!(out.errors, Vec::new());
    let build = &out.config.workflows["docker-build"];
    let call = build.on.call.as_ref().expect("on: call");
    assert_eq!(call.inputs.len(), 5);
    assert!(call.inputs["image"].required);
    assert_eq!(call.inputs["push"].default, Some(json!(false)));
    assert_eq!(call.inputs["replicas"].default, Some(json!(2)));
    assert!(call.secrets.contains_key("REGISTRY_TOKEN"));
    assert_eq!(call.outputs["digest"], "${{ jobs.build.outputs.digest }}");
    // The jobs of the file convert like any other, with its runtime
    // `runs-on` and matrix kept.
    let job = &out.config.jobs["docker-build-build"];
    assert_eq!(job.runs_on.as_deref(), Some("${{ inputs.runner }}"));
    let build = entry(&out, "docker-build", "build");
    assert!(build.matrix.dynamic());
    round_trips(&out);
}

#[test]
fn a_local_call_becomes_a_call_entry_with_its_matrix_inputs_and_secrets() {
    let out = release();
    let images = entry(&out, "release", "images");
    assert_eq!(images.call.as_deref(), Some("docker-build"));
    assert_eq!(images.requires, [Require::new("test")]);
    assert_eq!(
        images.matrix.values["image"],
        [json!("api"), json!("worker")]
    );
    assert_eq!(images.with["image"], "${{ matrix.image }}");
    assert_eq!(images.with["push"], true);
    assert_eq!(images.with["replicas"], 3);
    assert_eq!(
        images.secrets,
        hatchrun_workflow::SecretsPassed::Named(BTreeMap::from([(
            "REGISTRY_TOKEN".to_owned(),
            "${{ secrets.GHCR_TOKEN }}".to_owned()
        )]))
    );
    let announce = entry(&out, "release", "announce");
    assert_eq!(
        announce.requires,
        [Require::new("images"), Require::new("shared")]
    );
}

#[test]
fn a_remote_call_keeps_its_ref_and_what_the_job_passes() {
    let out = release();
    let shared = entry(&out, "release", "shared");
    assert_eq!(
        shared.call.as_deref(),
        Some("acme/shared-workflows/.github/workflows/scan.yml@v2")
    );
    assert_eq!(shared.secrets, hatchrun_workflow::SecretsPassed::Inherit);
    let Some(Permissions::Scopes(scopes)) = &shared.permissions else {
        panic!("no permissions");
    };
    assert_eq!(scopes["security-events"], Level::Write);
}

#[test]
fn a_converted_call_plans_with_the_callee_jobs_under_the_call() {
    let out = release();
    assert!(
        out.warnings.iter().all(|w| !w.message.contains("dropped")),
        "{:?}",
        out.warnings
    );
    let mut library = hatchrun_workflow::Library::default();
    let wanted = hatchrun_workflow::remote_calls(&out.config, &library);
    assert_eq!(wanted.len(), 1);
    assert_eq!(wanted[0].repository(), "acme/shared-workflows");
    // The remote file is another converted workflow.
    let scan = many(&[("scan.yml", REUSABLE_JOB)]);
    library.insert(
        wanted[0].clone(),
        hatchrun_workflow::Callee {
            config: scan.config,
            workflow: "scan".into(),
            sha: "0123abc".into(),
        },
    );
    let plan = hatchrun_workflow::compile_with(&out.config, "release", "refs/tags/v1", &library)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let ids: Vec<&str> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert!(ids.contains(&"images (api) / build"), "{ids:?}");
    assert!(ids.contains(&"images (worker) / sign"), "{ids:?}");
    assert!(ids.contains(&"shared / j"), "{ids:?}");
    assert!(ids.contains(&"announce"), "{ids:?}");
    let build = plan
        .jobs
        .iter()
        .find(|j| j.id == "images (api) / build")
        .unwrap();
    // The runtime matrix and `runs-on` inside the call wait to be read.
    assert!(build.deferred.is_some());
}

#[test]
fn a_call_of_a_file_that_was_not_given_is_dropped_with_its_dependents() {
    let out = many(&[(
        "ci.yml",
        "
on: push
jobs:
  call:
    uses: ./.github/workflows/missing.yml
  after:
    needs: call
    runs-on: ubuntu-latest
    steps: [{ run: x }]
  other:
    runs-on: ubuntu-latest
    steps: [{ run: y }]
",
    )]);
    let ids: Vec<&str> = out.config.workflows["ci"]
        .jobs
        .iter()
        .map(|j| j.name.as_deref().unwrap_or(&j.job))
        .collect();
    assert_eq!(ids, ["other"]);
    assert!(has(
        &out.warnings,
        &["missing.yml", "not among the converted files"]
    ));
    assert!(has(
        &out.warnings,
        &["`after` was dropped", "calls a reusable workflow"]
    ));
}

#[test]
fn a_matrix_read_at_run_time_on_a_call_is_dropped() {
    let out = many(&[
        (
            "ci.yml",
            "
on: push
jobs:
  setup: { runs-on: ubuntu-latest, steps: [{ run: x }] }
  c:
    needs: setup
    strategy:
      fail-fast: false
      matrix: ${{ fromJSON(needs.setup.outputs.m) }}
    uses: ./.github/workflows/w.yml
",
        ),
        ("w.yml", REUSABLE_JOB),
    ]);
    assert!(has(&out.warnings, &["matrix that is read at run time"]));
    assert!(
        out.config.workflows["ci"]
            .jobs
            .iter()
            .all(|j| j.call.is_none())
    );
}

#[test]
fn strategy_options_and_concurrency_on_a_call_are_warned_about() {
    let out = many(&[
        (
            "ci.yml",
            "
on: push
jobs:
  c:
    strategy: { fail-fast: false, matrix: { n: [1, 2] } }
    concurrency: { group: g }
    uses: ./.github/workflows/w.yml
",
        ),
        ("w.yml", REUSABLE_JOB),
    ]);
    assert!(has(&out.warnings, &["`strategy.fail-fast`"]));
    assert!(has(&out.warnings, &["`concurrency`"]));
    assert_eq!(entry(&out, "ci", "c").matrix.values["n"].len(), 2);
    round_trips(&out);
}

#[test]
fn a_secret_that_is_not_a_whole_reference_is_dropped_from_the_call() {
    let out = many(&[
        (
            "ci.yml",
            "
on: push
jobs:
  c:
    uses: ./.github/workflows/w.yml
    secrets:
      A: ${{ secrets.A }}
      B: literal
      C: ${{ secrets.X || secrets.Y }}
",
        ),
        ("w.yml", REUSABLE_JOB),
    ]);
    assert_eq!(
        entry(&out, "ci", "c").secrets,
        hatchrun_workflow::SecretsPassed::Named(BTreeMap::from([(
            "A".to_owned(),
            "${{ secrets.A }}".to_owned()
        )]))
    );
    assert!(has(&out.warnings, &["`secrets.B`", "not one whole"]));
    assert!(has(&out.warnings, &["`secrets.C`"]));
}

#[test]
fn a_call_that_reads_a_context_a_call_does_not_have_is_an_error() {
    let out = many(&[
        (
            "ci.yml",
            "
on: push
jobs:
  c:
    if: ${{ steps.x.outputs.y }}
    uses: ./.github/workflows/w.yml
    with: { a: '${{ secrets.TOKEN }}' }
  ok:
    runs-on: ubuntu-latest
    steps: [{ run: x }]
",
        ),
        ("w.yml", REUSABLE_JOB),
    ]);
    assert!(has(&out.errors, &["`if`", "steps"]), "{:?}", out.errors);
    assert!(
        has(&out.errors, &["`with.a`", "secrets"]),
        "{:?}",
        out.errors
    );
}

#[test]
fn an_output_that_reads_more_than_jobs_is_dropped_with_a_warning() {
    let out = many(&[
        (
            "ci.yml",
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/w.yml\n",
        ),
        (
            "w.yml",
            "
on:
  workflow_call:
    outputs:
      good: { value: '${{ jobs.j.outputs.x }}' }
      bad: { value: '${{ github.sha }}' }
jobs:
  j:
    runs-on: ubuntu-latest
    outputs: { x: '1' }
    steps: [{ run: x }]
",
        ),
    ]);
    let call = out.config.workflows["w"].on.call.as_ref().unwrap();
    assert!(call.outputs.contains_key("good"));
    assert!(!call.outputs.contains_key("bad"));
    assert!(has(&out.warnings, &["output `bad`", "reads `github`"]));
}

#[test]
fn merge_group_and_repository_dispatch_convert() {
    let out = ok("
on:
  merge_group:
  repository_dispatch: { types: [deploy, rollback] }
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    let on = &out.config.workflows["ci"].on;
    assert_eq!(on.merge_group.as_ref().unwrap().types, ["checks_requested"]);
    assert_eq!(
        on.repository_dispatch.as_ref().unwrap().types,
        ["deploy", "rollback"]
    );
    assert_eq!(out.errors, []);
    assert_eq!(out.warnings, []);
    round_trips(&out);

    let out = ok("on: [merge_group, repository_dispatch]
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    let on = &out.config.workflows["ci"].on;
    assert!(on.merge_group.is_some());
    assert_eq!(
        on.repository_dispatch.as_ref().unwrap().types,
        Vec::<String>::new()
    );
    assert_eq!(out.warnings, []);
    round_trips(&out);

    let out = ok("
on:
  merge_group: { types: [checks_requested], branches: [main] }
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert!(out.config.workflows["ci"].on.merge_group.is_some());
    assert!(has(
        &out.warnings,
        &["`on.merge_group`", "branch filters", "dropped"]
    ));

    let out = ok("
on:
  push:
  merge_group: { types: [destroyed] }
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: x }] }
");
    assert!(out.config.workflows["ci"].on.merge_group.is_none());
    assert!(has(&out.warnings, &["`on.merge_group`", "`destroyed`"]));
}

#[test]
fn the_deploy_fixture_converts_without_losing_a_capability() {
    let out = convert(&[(
        ".github/workflows/deploy.yml".into(),
        include_str!("fixtures/github/deploy.yml").into(),
    )])
    .unwrap();
    assert_eq!(out.errors, []);
    // Only the larger runner label is reported, as it is renamed.
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(has(
        &out.warnings,
        &["ubuntu-latest-8-cores", "ubuntu-26.04-8core"]
    ));
    round_trips(&out);

    let wf = &out.config.workflows["deploy"];
    assert_eq!(
        wf.run_name.as_deref(),
        Some("Deploy ${{ inputs.target || 'staging' }} (${{ github.actor }})")
    );
    assert!(wf.on.merge_group.is_some());
    assert_eq!(
        wf.on.repository_dispatch.as_ref().unwrap().types,
        ["deploy-request"]
    );
    let job = &out.config.jobs["deploy-deploy"];
    assert_eq!(job.runs_on.as_deref(), Some("ubuntu-26.04-8core"));
    assert!(
        job.environment
            .as_deref()
            .unwrap()
            .contains("matrix.region")
    );
    assert!(
        job.environment_url
            .as_deref()
            .unwrap()
            .contains("steps.up.outputs.id")
    );
    let Step::Uses(checkout) = &job.steps[0] else {
        panic!("{:?}", job.steps[0])
    };
    assert_eq!(checkout.timeout_minutes, Some(5));

    // The plan names the environment of each matrix leg when it starts.
    let plan = hatchrun_workflow::compile(&out.config, "deploy", "refs/heads/main").unwrap();
    let deploys: Vec<_> = plan.jobs.iter().filter(|j| j.name == "deploy").collect();
    assert_eq!(deploys.len(), 2);
    assert_eq!(
        deploys[1].matrix["region"],
        json!("us"),
        "{:?}",
        deploys[1].matrix
    );
    assert_eq!(plan.run_name, wf.run_name);
}

#[test]
fn a_comparison_in_a_runs_on_expression_keeps_its_operand() {
    let out = ok("on: push
jobs:
  a:
    runs-on: ${{ inputs.runner == 'ubuntu-24.04' && 'ubuntu-26.04-arm' || 'ubuntu-latest' }}
    steps: [{ run: x }]
");
    assert_eq!(
        out.config.jobs["ci-a"].runs_on.as_deref(),
        Some("${{ inputs.runner == 'ubuntu-24.04' && 'ubuntu-26.04-arm' || 'ubuntu-latest' }}")
    );
}

#[test]
fn a_calls_job_split_by_include_reads_its_outputs_from_every_entry() {
    let out = many(&[
        (
            "ci.yml",
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/w.yml\n  after:\n    needs: c\n    runs-on: ubuntu-latest\n    steps: [{ run: 'echo ${{ needs.c.outputs.tag }}' }]\n",
        ),
        (
            "w.yml",
            "
on:
  workflow_call:
    outputs:
      tag: { value: '${{ jobs.build.outputs.tag }}' }
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-latest]
        include:
          - { os: ubuntu-latest, extra: 1 }
          - { os: ubuntu-26.04-arm, extra: 2 }
    outputs: { tag: x }
    steps: [{ run: x }]
",
        ),
    ]);
    let tag = &out.config.workflows["w"].on.call.as_ref().unwrap().outputs["tag"];
    assert!(
        tag.contains("(jobs.build-2.outputs.tag || jobs.build.outputs.tag)"),
        "{tag}"
    );
    // The caller reads the last entry that has a value, as the outputs
    // of a matrix merge.
    let read = |first: &str, second: &str| {
        let ctx = json!({ "jobs": {
            "build": { "outputs": { "tag": first } },
            "build-2": { "outputs": { "tag": second } },
        } });
        hatchrun_workflow::interpolate(tag, &ctx, &Offline).unwrap()
    };
    assert_eq!(read("", "v2"), "v2");
    assert_eq!(read("v1", ""), "v1");
    assert_eq!(read("v1", "v2"), "v2");
}

struct Offline;

impl hatchrun_workflow::Functions for Offline {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, hatchrun_workflow::ExprError> {
        Err(hatchrun_workflow::ExprError {
            message: "no files".into(),
            offset: 0,
        })
    }

    fn status(&self) -> hatchrun_workflow::JobStatus {
        hatchrun_workflow::JobStatus::Success
    }
}

#[test]
fn split_outputs_are_read_from_the_parsed_expression() {
    let splits = BTreeMap::from([(
        "build".to_owned(),
        vec!["build".to_owned(), "build-2".to_owned()],
    )]);
    let split = |text: &str| hatchrun_workflow::split_job_outputs(text, &splits);
    // A bracket read is a read.
    assert_eq!(
        split("${{ jobs['build'].outputs.tag }}"),
        "${{ (jobs.build-2.outputs.tag || jobs.build.outputs.tag) }}"
    );
    // Text in a string is not.
    let literal = "${{ jobs.other.outputs.t || 'fallback jobs.build.outputs.tag' }}";
    assert_eq!(split(literal), literal);
    assert_eq!(
        split("${{ jobs.build.outputs.tag || 'jobs.build.outputs.tag' }}"),
        "${{ (jobs.build-2.outputs.tag || jobs.build.outputs.tag) || 'jobs.build.outputs.tag' }}"
    );
    // A property of something else is not a job.
    let other = "${{ github.jobs.build.outputs.tag }}";
    assert_eq!(split(other), other);
}

#[test]
fn an_output_of_a_job_the_conversion_dropped_is_dropped_with_a_warning() {
    let out = many(&[
        (
            "ci.yml",
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/w.yml\n",
        ),
        (
            "w.yml",
            "
on:
  workflow_call:
    outputs:
      kept: { value: '${{ jobs.ok.outputs.x }}' }
      lost: { value: '${{ jobs.bad.outputs.x }}' }
jobs:
  ok:
    runs-on: ubuntu-latest
    outputs: { x: '1' }
    steps: [{ run: x }]
  bad:
    runs-on: macos-latest
    outputs: { x: '1' }
    steps: [{ run: x }]
",
        ),
    ]);
    let call = out.config.workflows["w"].on.call.as_ref().unwrap();
    assert!(call.outputs.contains_key("kept"));
    assert!(!call.outputs.contains_key("lost"));
    assert!(has(
        &out.warnings,
        &["output `lost`", "job `bad`", "dropped"]
    ));
    assert_eq!(validate(&out.config), Vec::new());
    assert!(
        hatchrun_workflow::compile(&out.config, "ci", "refs/heads/main").is_ok(),
        "the caller plans"
    );
}

#[test]
fn a_call_of_a_workflow_that_did_not_convert_is_dropped_with_what_needs_it() {
    let out = many(&[
        (
            "ci.yml",
            "
on: push
jobs:
  c:
    uses: ./.github/workflows/w.yml
  after:
    needs: c
    runs-on: ubuntu-latest
    steps: [{ run: x }]
  fine:
    runs-on: ubuntu-latest
    steps: [{ run: y }]
",
        ),
        (
            "w.yml",
            "on: workflow_call\njobs:\n  bad:\n    runs-on: macos-latest\n    steps: [{ run: x }]\n",
        ),
    ]);
    assert!(!out.config.workflows.contains_key("w"));
    let ids: Vec<&str> = out.config.workflows["ci"]
        .jobs
        .iter()
        .map(|j| j.name.as_deref().unwrap_or(&j.job))
        .collect();
    assert_eq!(ids, ["fine"]);
    assert!(has(&out.warnings, &["`c` was dropped", "did not convert"]));
    assert!(has(&out.warnings, &["`after` was dropped", "needs `c`"]));
    assert_eq!(validate(&out.config), Vec::new());
}

#[test]
fn pruning_a_workflow_run_chain_keeps_a_job_a_call_shares_a_key_with() {
    // `ci-c` is a job key of workflow `ci`; the pruned workflow `deploy`
    // has a call whose entry is also named `ci-c`.
    let out = many(&[
        (
            "ci.yml",
            "on: push\njobs:\n  c:\n    runs-on: ubuntu-latest\n    steps: [{ run: x }]\n",
        ),
        (
            "w.yml",
            "on: workflow_call\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps: [{ run: x }]\n",
        ),
        (
            "ghost.yml",
            "on: push\njobs:\n  g:\n    runs-on: macos-latest\n    steps: [{ run: x }]\n",
        ),
        (
            "deploy.yml",
            "on:\n  workflow_run: { workflows: [ghost.yml] }\njobs:\n  ci-c:\n    uses: ./.github/workflows/w.yml\n",
        ),
    ]);
    assert!(!out.config.workflows.contains_key("ghost"));
    assert!(!out.config.workflows.contains_key("deploy"));
    assert!(has(
        &out.warnings,
        &[
            "`deploy.yml` was dropped",
            "runs only after workflows that did not convert"
        ]
    ));
    assert!(out.config.jobs.contains_key("ci-c"));
    assert_eq!(validate(&out.config), Vec::new());
}

#[test]
fn a_dynamic_matrix_reads_a_static_runner_label_at_run_time() {
    let out = ok("on: push
jobs:
  setup: { runs-on: ubuntu-latest, steps: [{ run: x }], outputs: { extra: x } }
  b:
    needs: setup
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-24.04]
        include: ${{ fromJSON(needs.setup.outputs.extra) }}
    steps: [{ run: x }]
");
    assert_eq!(out.errors, Vec::new());
    let entry = entry(&out, "ci", "b");
    assert_eq!(entry.matrix.values["os"], [json!("ubuntu-24.04")]);
    let plan = hatchrun_workflow::compile(&out.config, "ci", "refs/heads/main").unwrap();
    let b = plan.jobs.iter().find(|j| j.name == "b").unwrap();
    let ctx = json!({
        "needs": { "setup": { "result": "success", "outputs": { "extra": "[]" } } },
        "github": {}, "inputs": {}, "vars": {}
    });
    let legs = hatchrun_workflow::expand(b, &ctx, hatchrun_workflow::Siblings::default()).unwrap();
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].error, None);
    // The matrix keeps the label as written.
    assert_eq!(legs[0].job.matrix["os"], "ubuntu-24.04");
}

#[test]
fn container_fields_that_cannot_be_read_are_errors_not_dropped() {
    for (body, word) in [
        ("container: 'node 20'", "whitespace"),
        ("container: { image: a, options: { x: 1 } }", "options"),
        ("container: { image: a, options: [--cpus] }", "options"),
        ("container: { image: a, ports: [{ a: 1 }] }", "ports"),
        ("services: { db: { image: a, volumes: [[x]] } }", "volumes"),
        (
            "container: { image: a, options: '--network-alias db' }\n    services: { db: b }",
            "id of a service",
        ),
    ] {
        let yaml = format!(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    {body}\n    steps:\n      - run: true\n"
        );
        let errs = one("ci.yml", &yaml).unwrap_err();
        assert!(has(&errs, &["ci.yml", "build", word]), "{body}: {errs:?}");
    }
}

#[test]
fn a_null_options_stays_none() {
    let out = ok("
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container: { image: a, options: null }
    steps:
      - run: true
");
    let c = out.config.jobs["ci-build"].container.as_ref().unwrap();
    assert_eq!(c.options, None);
}
