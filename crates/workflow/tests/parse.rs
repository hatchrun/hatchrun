// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Parsing `.hatchrun/config.yml`.

use hatchrun_workflow::{
    CheckoutStep, MAX_SOURCE, ParameterKind, Step, Submodules, WorkspaceStep, parse, to_yaml,
};

#[test]
fn parses_the_spec_example() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    assert_eq!(config.executors["rust"].runs_on, "ubuntu-26.04-4core");
    assert_eq!(config.jobs["test"].parallelism, Some(4));
    assert!(matches!(&config.jobs["test"].steps[0], Step::Command { name, .. } if name == "setup"));
    let ci = &config.workflows["ci"].jobs;
    assert!(ci[1].approval && ci[1].requires == ["test"]);
    assert_eq!(ci[2].requires, ["hold"]);
}

#[test]
fn reports_line_and_column() {
    let err = parse("jobs:\n  test:\n    parallelism: lots\n").unwrap_err();
    // 1-based position of `lots`
    assert_eq!((err[0].line, err[0].column), (Some(3), Some(18)));
}

#[test]
fn rejects_docker_phase_features() {
    for (yaml, word) in [
        ("executors: { d: { docker: [{ image: rust }] } }", "docker"),
        (
            "jobs: { t: { runs-on: ubuntu-latest, services: { pg: { image: postgres } }, steps: [] } }",
            "services",
        ),
        (
            "jobs: { t: { runs-on: ubuntu-latest, container: rust, steps: [] } }",
            "container",
        ),
    ] {
        let err = parse(yaml).unwrap_err();
        assert!(
            err[0].message.contains(word) && err[0].message.contains("later"),
            "{err:?}"
        );
    }
}

#[test]
fn round_trips_through_yaml() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let yaml = serde_saphyr::to_string(&config).unwrap();
    assert_eq!(parse(&yaml).unwrap(), config);
}

#[test]
fn parses_every_step_shape() {
    let config = parse(
        "jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - setup: { version: 2 }
      - uses: actions/setup-node@v5
        with: { node-version: 22 }
      - workspace: { persist: { root: ., paths: [dist] } }
      - workspace: { attach: { at: . } }
      - cache: { key: k, restore-keys: [a], paths: [p] }
      - run: make
        working-directory: app
        continue-on-error: true
",
    )
    .unwrap();
    let steps = &config.jobs["t"].steps;
    assert!(
        matches!(&steps[0], Step::Command { name, with } if name == "setup" && with["version"] == 2)
    );
    assert!(matches!(&steps[1], Step::Uses(u) if u.with["node-version"] == 22));
    assert!(
        matches!(&steps[2], Step::Workspace(WorkspaceStep::Persist { root, .. }) if root == ".")
    );
    assert!(matches!(&steps[3], Step::Workspace(WorkspaceStep::Attach { at }) if at == "."));
    assert!(matches!(&steps[4], Step::Cache(c) if c.restore_keys == ["a"]));
    assert!(
        matches!(&steps[5], Step::Run(r) if r.continue_on_error && r.working_directory.as_deref() == Some("app"))
    );
}

#[test]
fn parses_parameters_and_trigger_defaults() {
    let config = parse(
        "commands:
  c:
    parameters:
      level: { type: enum, enum: [low, high], default: low }
workflows:
  w:
    on: { pull_request: }
    jobs: [{ j: { matrix: { v: [1, 2] }, level: high } }]
",
    )
    .unwrap();
    let level = &config.commands["c"].parameters["level"];
    assert_eq!(
        level.kind,
        ParameterKind::Enum(vec!["low".into(), "high".into()])
    );
    let w = &config.workflows["w"];
    assert_eq!(
        w.on.pull_request.as_ref().unwrap().types,
        ["opened", "synchronize", "reopened"]
    );
    assert_eq!(w.jobs[0].matrix["v"].len(), 2);
    assert_eq!(w.jobs[0].with["level"], "high");
}

#[test]
fn rejects_a_step_with_run_and_uses() {
    let err = parse("jobs:\n  t:\n    steps:\n      - run: a\n        uses: b\n").unwrap_err();
    assert!(err[0].message.contains("both run and uses"), "{err:?}");
    assert_eq!(err[0].line, Some(4));
}

#[test]
fn reads_yaml_1_1_booleans_as_strings() {
    let config = parse(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    env: { A: yes, B: off }
    steps: [{ run: echo }]
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t:
          matrix:
            flag: [y, yes, on, off, n, no, true]
",
    )
    .unwrap();
    let wf = &config.workflows["ci"];
    assert!(wf.on.push.is_some());
    assert_eq!(
        wf.jobs[0].matrix["flag"],
        [
            "y".into(),
            "yes".into(),
            "on".into(),
            "off".into(),
            "n".into(),
            "no".into(),
            serde_json::Value::Bool(true)
        ]
    );
    assert_eq!(config.jobs["t"].env["A"], "yes");
}

#[test]
fn library_messages_read_plainly() {
    let errs = parse("jobs: {}\njobs: {}\n").unwrap_err();
    assert_eq!(errs[0].message, "duplicate key `jobs`");
    assert_eq!(errs[0].line, Some(2));

    let big = format!(
        "commands: {{ c: {{ steps: [{}] }} }}\n",
        vec!["a"; 250_001].join(",")
    );
    let errs = parse(&big).unwrap_err();
    assert_eq!(
        errs[0].message,
        "the file is too large or nests too deeply to read"
    );
}

#[test]
fn reads_a_long_run_of_comments() {
    // A job commented out: more comment lines in a row than the YAML
    // library buffers when it keeps comments.
    let comments = "  # old:\n  #   runs-on: ubuntu-latest\n\n".repeat(20);
    let source =
        format!("jobs:\n{comments}  t:\n    runs-on: ubuntu-latest\n    steps: [{{ run: a }}]\n");
    let config = parse(&source).unwrap();
    assert_eq!(config.jobs["t"].steps.len(), 1);
}

#[test]
fn a_config_over_one_mebibyte_is_an_error() {
    let mut yaml = String::from("jobs: {}\n");
    yaml.push_str(&"# padding\n".repeat(MAX_SOURCE / 10 + 1));
    let errors = parse(&yaml).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].message,
        format!("the config is {} bytes; the limit is 1048576", yaml.len())
    );
    // At the limit it parses.
    yaml.truncate(MAX_SOURCE);
    parse(&yaml).unwrap();
}

#[test]
fn files_past_the_parser_limits_get_a_plain_message() {
    let many = format!(
        "jobs: {{t: {{runs-on: ubuntu-latest, steps: [{{uses: a/b@v1, with: {{x: [{}]}}}}]}}}}\n",
        vec!["1"; 260_000].join(",")
    );
    let deep = format!(
        "jobs: {{t: {{runs-on: ubuntu-latest, steps: [{{run: {}{}}}]}}}}\n",
        "[".repeat(2000),
        "]".repeat(2000)
    );
    for source in [many, deep] {
        let err = parse(&source).unwrap_err();
        assert_eq!(
            err[0].message, "the file is too large or nests too deeply to read",
            "{err:?}"
        );
    }
}

fn steps(yaml_steps: &str) -> Vec<Step> {
    let config = parse(&format!(
        "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: {yaml_steps} }} }}"
    ))
    .unwrap();
    config.jobs["t"].steps.clone()
}

#[test]
fn parses_checkout_steps() {
    let parsed = steps("[checkout, { checkout: }, { checkout: {} }]");
    for step in &parsed {
        assert_eq!(*step, Step::Checkout(CheckoutStep::default()));
    }
    assert_eq!(CheckoutStep::default().fetch_depth, 1);
    let parsed = steps(
        "[{ checkout: { ref: \"${{ github.head_ref }}\", fetch-depth: 0, submodules: recursive, lfs: true, path: src } }]",
    );
    assert_eq!(
        parsed[0],
        Step::Checkout(CheckoutStep {
            r#ref: Some("${{ github.head_ref }}".into()),
            fetch_depth: 0,
            submodules: Submodules::Recursive,
            lfs: true,
            path: Some("src".into()),
        })
    );
    for (yaml, want) in [
        ("true", Submodules::Top),
        ("false", Submodules::None),
        ("recursive", Submodules::Recursive),
    ] {
        let parsed = steps(&format!("[{{ checkout: {{ submodules: {yaml} }} }}]"));
        let Step::Checkout(c) = &parsed[0] else {
            panic!("{parsed:?}")
        };
        assert_eq!(c.submodules, want);
    }
}

#[test]
fn rejects_bad_checkout_steps() {
    for (yaml, word) in [
        ("[{ checkout: { submodules: deep } }]", "recursive"),
        ("[{ checkout: { token: x } }]", "token"),
        ("[{ checkout: {}, name: Checkout }]", "no other keys"),
        ("[{ checkout: { fetch-depth: -1 } }]", ""),
    ] {
        let err = parse(&format!(
            "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: {yaml} }} }}"
        ))
        .unwrap_err();
        assert!(err[0].message.contains(word), "{yaml}: {err:?}");
    }
}

#[test]
fn checkout_steps_round_trip() {
    let source = "jobs: { t: { runs-on: ubuntu-latest, steps: [checkout, { checkout: { fetch-depth: 0, submodules: true, lfs: true, path: a, ref: main } }] } }";
    let config = parse(source).unwrap();
    let written = to_yaml(&config);
    assert_eq!(parse(&written).unwrap(), config, "{written}");
    // A checkout with no options is written as the bare name.
    assert!(written.contains("- checkout\n"), "{written}");
}
