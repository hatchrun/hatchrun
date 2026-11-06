// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reusable workflows in the planner: inputs, secrets, outputs, matrix,
//! nesting and the limits.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hatchrun_workflow::{
    Callee, Library, PlannedJob, RemoteCall, SecretsPassed, compile, compile_with, parse,
    remote_calls, validate,
};

const BASE: &str = "
jobs:
  build: { runs-on: ubuntu-latest, steps: [{ run: make }] }
  reads_s: { runs-on: ubuntu-latest, steps: [{ run: 'echo ${{ secrets.S }}' }] }
  ship:
    runs-on: ubuntu-latest
    outputs: { url: '${{ steps.x.outputs.url }}' }
    steps:
      - run: echo ${{ inputs.env }} ${{ secrets.TOKEN }} ${{ secrets.OTHER }}
";

fn config(workflows: &str) -> hatchrun_workflow::Config {
    parse(&format!("{BASE}\nworkflows:\n{workflows}")).unwrap_or_else(|e| panic!("{e:?}"))
}

fn plan(workflows: &str) -> Vec<PlannedJob> {
    compile(&config(workflows), "release", "refs/heads/main")
        .unwrap_or_else(|e| panic!("{e:?}"))
        .jobs
}

fn errors(workflows: &str) -> String {
    let err = compile(&config(workflows), "release", "refs/heads/main").unwrap_err();
    err.iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

const CALLEE: &str = "
  deploy:
    on:
      call:
        inputs:
          env: { type: string, required: true }
          fast: { type: boolean, default: false }
        secrets:
          TOKEN: { required: true }
        outputs:
          url: ${{ jobs.ship.outputs.url }}
    jobs: [ship]
";

#[test]
fn a_call_plans_the_callee_jobs_under_the_call_id() {
    let jobs = plan(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs:
      - build
      - prod:
          call: deploy
          requires: [build]
          with: {{ env: prod }}
          secrets: {{ TOKEN: '${{{{ secrets.PROD }}}}' }}
      - build: {{ name: after, requires: [prod] }}
"
    ));
    let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["build", "prod / ship", "after"]);
    let ship = &jobs[1];
    assert_eq!(ship.name, "ship");
    assert_eq!(ship.requires, ["build"]);
    assert_eq!(ship.scope.len(), 1);
    assert_eq!(ship.scope[0].name, "prod");
    assert_eq!(ship.scope[0].with["env"], "prod");
    assert_eq!(ship.scope[0].with["fast"], false);
    assert_eq!(jobs[2].requires, ["prod / ship"]);
    assert_eq!(jobs[2].entry_of(ship), "prod");
}

#[test]
fn a_call_matrix_plans_the_callee_once_per_entry() {
    let jobs = plan(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs:
      - prod:
          call: deploy
          secrets: inherit
          matrix: {{ env: [eu, us] }}
          with: {{ env: '${{{{ matrix.env }}}}' }}
"
    ));
    let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["prod (eu) / ship", "prod (us) / ship"]);
    assert_eq!(jobs[0].scope[0].matrix["env"], "eu");
}

#[test]
fn callee_jobs_wait_for_what_the_call_requires() {
    let jobs = plan(
        "
  two:
    on: { call: {} }
    jobs: [build, { ship: { requires: [build] } }]
  release:
    on: { push: {} }
    jobs:
      - build
      - c: { call: two, requires: [build] }
",
    );
    let by: BTreeMap<&str, &PlannedJob> = jobs.iter().map(|j| (j.id.as_str(), j)).collect();
    assert_eq!(by["c / build"].requires, ["build"]);
    assert_eq!(by["c / ship"].requires, ["c / build"]);
}

#[test]
fn secrets_reach_only_what_the_call_passes() {
    let jobs = plan(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs:
      - prod:
          call: deploy
          with: {{ env: prod }}
          secrets: {{ TOKEN: '${{{{ secrets.PROD }}}}' }}
"
    ));
    let ship = &jobs[0];
    let refs = ship.secret_refs();
    assert!(refs.names.contains("PROD"), "{refs:?}");
    // TOKEN is the callee's name; OTHER is read but not passed.
    assert!(!refs.names.contains("TOKEN"));
    assert!(!refs.names.contains("OTHER"));
    let opened: BTreeMap<String, String> = [("PROD", "p"), ("OTHER", "o"), ("TOKEN", "t")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let seen = ship.visible_secrets(&opened);
    assert_eq!(seen.get("TOKEN").map(String::as_str), Some("p"));
    assert!(!seen.contains_key("OTHER"));
    assert!(!seen.contains_key("PROD"));
}

#[test]
fn inherit_passes_every_secret_and_none_passes_nothing() {
    let opened: BTreeMap<String, String> = [("A".to_owned(), "a".to_owned())].into();
    let inherit = plan(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs: [{{ p: {{ call: deploy, with: {{ env: x }}, secrets: inherit }} }}]
"
    ));
    assert_eq!(inherit[0].scope[0].secrets, SecretsPassed::Inherit);
    assert_eq!(
        inherit[0]
            .visible_secrets(&opened)
            .get("A")
            .map(String::as_str),
        Some("a")
    );
    let nested = plan(
        "
  inner:
    on: { call: { secrets: { S: {} } } }
    jobs: [reads_s]
  outer:
    on: { call: { secrets: { T: {} } } }
    jobs: [{ i: { call: inner, secrets: { S: '${{ secrets.T }}' } } }]
  release:
    on: { push: {} }
    jobs: [{ o: { call: outer, secrets: { T: '${{ secrets.TOP }}' } } }]
",
    );
    let job = &nested[0];
    assert_eq!(job.id, "o / i / reads_s");
    assert_eq!(job.scope.len(), 2);
    assert_eq!(job.scope[1].id, "o / i");
    let opened: BTreeMap<String, String> = [("TOP", "t"), ("A", "a")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let seen = job.visible_secrets(&opened);
    assert_eq!(seen.keys().collect::<Vec<_>>(), ["S"]);
    assert_eq!(seen["S"], "t");
    assert!(job.secret_refs().names.contains("TOP"));
    assert!(!job.secret_refs().names.contains("A"));
}

#[test]
fn a_required_input_that_is_not_passed_is_named() {
    let msg = errors(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs: [{{ p: {{ call: deploy, secrets: inherit }} }}]
"
    ));
    assert!(
        msg.contains("does not pass the input `env` of `deploy`"),
        "{msg}"
    );
}

#[test]
fn input_and_secret_mistakes_are_named() {
    let msg = errors(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs:
      - p:
          call: deploy
          with: {{ env: prod, fast: 'yes', nope: 1 }}
          secrets: {{ EXTRA: '${{{{ secrets.A }}}}' }}
"
    ));
    assert!(
        msg.contains("`nope`, which is not an input of `deploy`"),
        "{msg}"
    );
    assert!(
        msg.contains("input `fast` of `deploy` must be a boolean"),
        "{msg}"
    );
    assert!(
        msg.contains("secret `EXTRA`, which `deploy` does not declare"),
        "{msg}"
    );
    assert!(
        msg.contains("secret `TOKEN` of `deploy`, which is required"),
        "{msg}"
    );
}

#[test]
fn a_secret_must_be_passed_as_a_whole_reference() {
    let msg = errors(&format!(
        "{CALLEE}
  release:
    on: {{ push: {{}} }}
    jobs:
      - p:
          call: deploy
          with: {{ env: prod }}
          secrets: {{ TOKEN: 'x-${{{{ secrets.A }}}}' }}
"
    ));
    assert!(msg.contains("pass one whole"), "{msg}");
}

#[test]
fn a_workflow_without_on_call_cannot_be_called() {
    let msg = errors(
        "
  other:
    on: { push: {} }
    jobs: [build]
  release:
    on: { push: {} }
    jobs: [{ p: { call: other } }]
",
    );
    assert!(msg.contains("has no `on: call`"), "{msg}");
}

#[test]
fn an_output_must_read_a_job_of_the_callee() {
    let msg = errors(
        "
  w:
    on: { call: { outputs: { x: '${{ jobs.ghost.outputs.y }}' } } }
    jobs: [build]
  release:
    on: { push: {} }
    jobs: [{ p: { call: w } }]
",
    );
    assert!(
        msg.contains("`jobs.ghost`, which is not a job of `w`"),
        "{msg}"
    );
}

#[test]
fn cycles_are_reported_with_the_chain() {
    let msg = errors(
        "
  a:
    on: { call: {} }
    jobs: [{ x: { call: b } }]
  b:
    on: { call: {} }
    jobs: [{ y: { call: a } }]
  release:
    on: { push: {} }
    jobs: [{ p: { call: a } }]
",
    );
    assert!(
        msg.contains("makes a cycle (`release` calls `a` calls `b` calls `a`)"),
        "{msg}"
    );
}

fn chain(depth: usize) -> String {
    let mut out = String::new();
    for i in 0..depth {
        let next = if i + 1 == depth {
            "build".to_owned()
        } else {
            format!("{{ n: {{ call: w{} }} }}", i + 1)
        };
        let _ = write!(
            out,
            "  w{i}:\n    on: {{ call: {{}} }}\n    jobs: [{next}]\n"
        );
    }
    out.push_str("  release:\n    on: { push: {} }\n    jobs: [{ p: { call: w0 } }]\n");
    out
}

#[test]
fn calls_nest_four_levels_deep() {
    let jobs = plan(&chain(4));
    assert_eq!(jobs[0].id, "p / n / n / n / build");
    assert_eq!(jobs[0].scope.len(), 4);
}

#[test]
fn a_fifth_level_is_refused() {
    let msg = errors(&chain(5));
    assert!(msg.contains("levels deep"), "{msg}");
}

#[test]
fn twenty_one_different_workflows_are_too_many() {
    let mut text = String::new();
    let mut jobs = Vec::new();
    for i in 0..21 {
        let _ = write!(
            text,
            "  w{i}:\n    on: {{ call: {{}} }}\n    jobs: [build]\n"
        );
        jobs.push(format!("{{ c{i}: {{ call: w{i} }} }}"));
    }
    let _ = write!(
        text,
        "  release:\n    on: {{ push: {{}} }}\n    jobs: [{}]\n",
        jobs.join(", ")
    );
    let msg = errors(&text);
    assert!(msg.contains("the limit is 20"), "{msg}");
}

#[test]
fn a_callee_cannot_raise_the_callers_permissions() {
    let jobs = plan(
        "
  w:
    on: { call: {} }
    permissions: { contents: write, issues: write }
    jobs: [build]
  release:
    on: { push: {} }
    permissions: { contents: read }
    jobs: [{ p: { call: w } }]
",
    );
    let perms = &jobs[0].token_permissions;
    assert_eq!(perms.get("contents"), Some(&hatchrun_workflow::Level::Read));
    assert!(!perms.contains_key("issues"));
}

#[test]
fn validate_checks_calls_and_skips_other_repositories() {
    let cfg = config(
        "
  w:
    on: { call: {} }
    jobs: [build]
  release:
    on: { push: {} }
    jobs: [{ p: { call: w } }, { r: { call: 'acme/x/.github/workflows/y.yml@v1' } }]
",
    );
    assert!(validate(&cfg).is_empty(), "{:?}", validate(&cfg));
}

#[test]
fn a_remote_call_needs_the_file_in_the_library() {
    let cfg = config(
        "
  release:
    on: { push: {} }
    jobs: [{ r: { call: 'acme/x/.github/workflows/y.yml@v1' } }]
",
    );
    let wanted = remote_calls(&cfg, &Library::default());
    assert_eq!(wanted.len(), 1);
    assert_eq!(wanted[0].repository(), "acme/x");
    let err = compile(&cfg, "release", "refs/heads/main").unwrap_err();
    assert!(err[0].message.contains("was not read"), "{err:?}");

    let callee = parse(&format!(
        "{BASE}\nworkflows:\n  y:\n    on: {{ call: {{}} }}\n    jobs: [ship]\n"
    ))
    .unwrap();
    let mut library = Library::default();
    library.insert(
        wanted[0].clone(),
        Callee {
            config: callee,
            workflow: "y".into(),
            sha: "abc123".into(),
        },
    );
    assert_eq!(remote_calls(&cfg, &library).len(), 0);
    let plan = compile_with(&cfg, "release", "refs/heads/main", &library).unwrap();
    assert_eq!(plan.jobs[0].id, "r / ship");
    assert_eq!(
        plan.jobs[0].scope[0].source.as_deref(),
        Some("acme/x/.github/workflows/y.yml@abc123")
    );
    let _: RemoteCall = wanted[0].clone();
}
