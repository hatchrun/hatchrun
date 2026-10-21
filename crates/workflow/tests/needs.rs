// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Finding the secrets and variables a job or workflow reads.

use std::collections::BTreeSet;

use hatchrun_workflow::{Needed, Needs, job_needs, parse, workflow_needs};

fn read(text: &str) -> Needs {
    let mut needs = Needs::default();
    needs.read(text);
    needs
}

fn names(list: &[&str]) -> Needed {
    Needed::Names(list.iter().map(|s| (*s).to_owned()).collect())
}

#[test]
fn property_and_literal_index() {
    let needs = read("${{ secrets.NPM_TOKEN }} ${{ secrets['docker-hub'] }} x");
    assert_eq!(needs.secrets, names(&["DOCKER-HUB", "NPM_TOKEN"]));
    assert_eq!(needs.vars, Needed::default());
    assert!(!needs.github_token);
}

#[test]
fn whole_or_dynamic_reads_need_all() {
    for text in [
        "${{ toJSON(secrets) }}",
        "${{ secrets[matrix.name] }}",
        "${{ secrets.* }}",
        "${{ secrets[0] }}",
        "${{ secrets }}",
    ] {
        assert_eq!(read(text).secrets, Needed::All, "{text}");
    }
    // A key read from another context inside the index still counts.
    let needs = read("${{ secrets[vars.WHICH] }}");
    assert_eq!(needs.secrets, Needed::All);
    assert_eq!(needs.vars, names(&["WHICH"]));
}

#[test]
fn nested_calls_and_vars() {
    let needs = read("${{ format('{0}:{1}', secrets.user, vars.Registry) && vars['region'] }}");
    assert_eq!(needs.secrets, names(&["USER"]));
    assert_eq!(needs.vars, names(&["REGION", "REGISTRY"]));
    assert_eq!(needs.to_string(), "secrets USER; vars REGION, REGISTRY");
}

#[test]
fn names_and_contexts_are_case_insensitive() {
    let needs = read("${{ Secrets.Token }} ${{ SECRETS.TOKEN }} ${{ secrets['token'] }}");
    assert_eq!(needs.secrets, names(&["TOKEN"]));
}

#[test]
fn github_token_is_flagged_not_listed() {
    let needs = read("${{ secrets.github_token }}");
    assert!(needs.github_token);
    assert!(needs.secrets.is_empty());
    assert!(needs.is_empty());
    assert_eq!(needs.to_string(), "");
}

#[test]
fn bad_expressions_and_plain_text_need_nothing() {
    assert!(read("secrets.X ${{ secrets.( }}").is_empty());
    assert!(read("${{ secrets.X").is_empty());
}

#[test]
fn all_absorbs_names() {
    let mut needs = read("${{ secrets.A }}");
    needs.extend(read("${{ toJSON(vars) }}"));
    needs.extend(read("${{ secrets[x] }}"));
    needs.extend(read("${{ secrets.B }}"));
    assert_eq!(needs.secrets, Needed::All);
    assert_eq!(needs.vars, Needed::All);
    assert_eq!(needs.to_string(), "secrets all; vars all");
}

#[test]
fn walks_every_field_of_a_job_and_its_commands() {
    let config = parse(
        "
commands:
  login:
    parameters: { user: { type: string, default: '${{ vars.DEFAULT_USER }}' } }
    steps:
      - run: echo ${{ secrets.LOGIN }}
      - login
jobs:
  t:
    runs-on: ubuntu-latest
    env: { A: '${{ secrets.ENV_SECRET }}' }
    if: vars.ENABLED == 'yes'
    outputs: { o: '${{ secrets.OUT }}' }
    steps:
      - run: make
        if: secrets.STEP_IF != ''
        env: { B: '${{ secrets.STEP_ENV }}' }
      - uses: actions/setup@v1
        with: { token: '${{ secrets.WITH }}', list: ['${{ vars.IN_LIST }}'] }
      - cache: { key: '${{ vars.CACHE }}', paths: [x] }
      - login: { user: '${{ secrets.PASSED }}' }
  other:
    runs-on: ubuntu-latest
    steps: [{ run: 'echo ${{ secrets.OTHER }}' }]
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { branches: [release] }
      - other: { matrix: { x: ['${{ vars.MATRIX }}'] } }
      - hold: { type: approval }
",
    )
    .unwrap();
    let job = job_needs(&config, &config.jobs["t"]);
    let want: BTreeSet<String> = [
        "ENV_SECRET",
        "LOGIN",
        "OUT",
        "PASSED",
        "STEP_ENV",
        "STEP_IF",
        "WITH",
    ]
    .map(String::from)
    .into();
    assert_eq!(job.secrets, Needed::Names(want));
    assert_eq!(
        job.vars,
        names(&["CACHE", "DEFAULT_USER", "ENABLED", "IN_LIST"])
    );
    let wf = workflow_needs(&config, &config.workflows["ci"]);
    let Needed::Names(secrets) = &wf.secrets else {
        panic!("{wf:?}")
    };
    assert!(secrets.contains("OTHER") && secrets.contains("LOGIN"));
    let Needed::Names(vars) = &wf.vars else {
        panic!("{wf:?}")
    };
    assert!(vars.contains("MATRIX") && vars.contains("ENABLED"));
}
