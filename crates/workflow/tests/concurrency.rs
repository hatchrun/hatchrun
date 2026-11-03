// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `concurrency` key: parsing, validation, planning and conversion.

use hatchrun_workflow::{
    Cancel, Conversion, Diagnostic, Plan, compile, convert, parse, to_yaml, validate,
};

fn plan(yaml: &str) -> Plan {
    compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap()
}

fn errors(yaml: &str) -> Vec<Diagnostic> {
    validate(&parse(yaml).unwrap())
}

fn assert_error(yaml: &str, words: &[&str]) {
    let errs = errors(yaml);
    assert!(
        errs.iter()
            .any(|e| words.iter().all(|w| e.message.contains(w))),
        "no error contains {words:?}: {errs:?}"
    );
}

fn converted(yaml: &str) -> Conversion {
    convert(&[("ci.yml".into(), yaml.into())]).unwrap()
}

#[test]
fn concurrency_reaches_the_plan() {
    let plan = plan(
        "
jobs:
  build:
    runs-on: ubuntu-latest
    concurrency: { group: 'b-${{ matrix.os }}', cancel-in-progress: true }
    steps: []
  test: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    concurrency: ci-${{ github.ref }}
    jobs: [build, test]
",
    );
    assert_eq!(
        plan.concurrency.as_ref().unwrap().group,
        "ci-${{ github.ref }}"
    );
    assert_eq!(
        plan.jobs[0].concurrency.as_ref().unwrap().group,
        "b-${{ matrix.os }}"
    );
    assert_eq!(plan.jobs[1].concurrency, None);
}

#[test]
fn job_concurrency_takes_parameters() {
    let plan = plan(
        "
jobs:
  deploy:
    parameters: { target: { type: string } }
    runs-on: ubuntu-latest
    concurrency: deploy-${{ parameters.target }}
    steps: []
workflows:
  ci:
    on: { push: {} }
    jobs: [{ deploy: { target: prod } }]
",
    );
    assert_eq!(
        plan.jobs[0].concurrency.as_ref().unwrap().group,
        "deploy-prod"
    );
}

#[test]
fn workflow_concurrency_reads_only_event_contexts() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    concurrency: g-${{ matrix.os }}
    jobs: [t]
",
        &["workflow `ci`", "concurrency.group", "matrix"],
    );
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    concurrency: { group: g, cancel-in-progress: \"${{ secrets.X }}\" }
    jobs: [t]
",
        &["concurrency.cancel-in-progress", "secrets"],
    );
}

#[test]
fn job_concurrency_reads_matrix_and_needs_but_not_steps() {
    assert!(
        errors(
            "
jobs:
  t:
    runs-on: ubuntu-latest
    concurrency: ${{ matrix.os }}-${{ needs.a.outputs.x }}-${{ vars.V }}-${{ inputs.i }}
    steps: []
workflows:
  ci: { on: { push: {} }, jobs: [t] }
"
        )
        .is_empty()
    );
    assert_error(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    concurrency: ${{ steps.a.outputs.x }}
    steps: []
workflows:
  ci: { on: { push: {} }, jobs: [t] }
",
        &["job `t`", "steps"],
    );
    assert_error(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    concurrency: ${{ github. }}
    steps: []
workflows:
  ci: { on: { push: {} }, jobs: [t] }
",
        &["concurrency.group", "bad expression"],
    );
}

#[test]
fn workflow_concurrency_cannot_read_parameters() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    concurrency: ${{ parameters.x }}
    jobs: [t]
",
        &["concurrency.group", "parameters"],
    );
}

#[test]
fn convert_carries_both_forms() {
    let out = converted(
        "
on: push
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
jobs:
  deploy:
    runs-on: ubuntu-latest
    concurrency:
      group: deploy
      cancel-in-progress: true
    steps:
      - run: echo
  lint:
    runs-on: ubuntu-latest
    concurrency: lint
    steps:
      - run: echo
",
    );
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.message.contains("concurrency")),
        "{:?}",
        out.warnings
    );
    let wf = out.config.workflows["ci"].concurrency.as_ref().unwrap();
    assert_eq!(wf.group, "${{ github.workflow }}-${{ github.ref }}");
    assert!(matches!(wf.cancel_in_progress, Cancel::When(_)));
    let deploy = out.config.jobs["ci-deploy"].concurrency.as_ref().unwrap();
    assert_eq!(deploy.cancel_in_progress, Cancel::Yes);
    let lint = out.config.jobs["ci-lint"].concurrency.as_ref().unwrap();
    assert_eq!(lint.cancel_in_progress, Cancel::No);
    let yaml = to_yaml(&out.config).unwrap();
    let reparsed = parse(&yaml).unwrap_or_else(|e| panic!("{e:?}\n{yaml}"));
    assert_eq!(reparsed, out.config, "{yaml}");
    assert_eq!(validate(&reparsed), Vec::new(), "{yaml}");
}

#[test]
fn convert_refuses_a_concurrency_without_a_group() {
    let out = converted(
        "
on: push
concurrency: { cancel-in-progress: true }
jobs:
  a: { runs-on: ubuntu-latest, steps: [{ run: x }] }
",
    );
    assert!(
        out.errors
            .iter()
            .any(|d| d.message.contains("concurrency") && d.message.contains("group")),
        "{:?}",
        out.errors
    );
}
