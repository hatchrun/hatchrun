// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Parsing `.hatchrun/config.yml`.

use hatchrun_workflow::{ParameterKind, Step, WorkspaceStep, parse};

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
