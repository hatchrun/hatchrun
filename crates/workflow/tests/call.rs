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
    assert!(refs.names.contains("PROD"));
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
    let none = plan(
        "
  bare:
    on: { call: {} }
    jobs: [reads_s]
  release:
    on: { push: {} }
    jobs: [{ p: { call: bare } }]
",
    );
    assert_eq!(none[0].scope[0].secrets, SecretsPassed::None);
    // An ordinary secret of the caller is not passed on.
    assert_eq!(none[0].visible_secrets(&opened).len(), 0);
    assert!(!none[0].secret_refs().names.contains("A"));
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

#[test]
fn a_callee_that_sets_no_permissions_inherits_the_callers() {
    let jobs = plan(
        "
  w:
    on: { call: {} }
    jobs: [build]
  release:
    on: { push: {} }
    permissions: { contents: write }
    jobs: [{ p: { call: w } }]
",
    );
    assert_eq!(
        jobs[0].token_permissions.get("contents"),
        Some(&hatchrun_workflow::Level::Write)
    );
    // The entry's own permissions are what the callee inherits and may
    // lower.
    let jobs = plan(
        "
  w:
    on: { call: {} }
    jobs: [build]
  release:
    on: { push: {} }
    permissions: { contents: write }
    jobs: [{ p: { call: w, permissions: { contents: read } } }]
",
    );
    assert_eq!(
        jobs[0].token_permissions.get("contents"),
        Some(&hatchrun_workflow::Level::Read)
    );
}

#[test]
fn each_call_instance_has_its_own_strategy_groups() {
    let jobs = plan(
        "
  w:
    on: { call: {} }
    jobs: [{ build: { matrix: { n: [1, 2] } } }]
  release:
    on: { push: {} }
    jobs: [{ p: { call: w, matrix: { x: [a, b] } } }]
",
    );
    let groups: Vec<(String, u32)> = jobs
        .iter()
        .map(|j| {
            let s = j.strategy.as_ref().unwrap();
            (s.group.clone(), s.job_total)
        })
        .collect();
    assert_eq!(groups.len(), 4);
    assert_eq!(groups[0].0, "p (a) / build");
    assert_eq!(groups[2].0, "p (b) / build");
    assert!(groups.iter().all(|(_, total)| *total == 2));
}

#[test]
fn an_output_may_spell_a_job_name_in_a_string_and_must_parse() {
    let jobs = plan(
        "
  w:
    on: { call: { outputs: { u: \"${{ jobs.build.outputs.url || 'jobs.ghost.outputs.x' }}\" } } }
    jobs: [build]
  release:
    on: { push: {} }
    jobs: [{ p: { call: w } }]
",
    );
    assert_eq!(jobs.len(), 1);
    let msg = errors(
        "
  w:
    on: { call: { outputs: { u: '${{ jobs.build.outputs.x + }}' } } }
    jobs: [build]
  release:
    on: { push: {} }
    jobs: [{ p: { call: w } }]
",
    );
    assert!(
        msg.contains("`on.call.outputs.u` of `w` has a bad expression"),
        "{msg}"
    );
}

#[test]
fn what_a_call_waits_for_does_not_count_in_what_needs_the_call() {
    use hatchrun_workflow::{Outcome, Ready, ready};
    let plan = compile(
        &config(
            "
  w:
    on: { call: {} }
    jobs: [reads_s]
  release:
    on: { push: {} }
    jobs:
      - build
      - c: { call: w, requires: [{ build: failed }] }
      - { build: { name: after, requires: [{ c: success }] } }
",
        ),
        "release",
        "refs/heads/main",
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let after = plan.jobs.iter().find(|j| j.id == "after").unwrap();
    let outcome = |id: &str| match id {
        "build" => Some(Outcome::Failed),
        "c / reads_s" => Some(Outcome::Success),
        _ => None,
    };
    assert_eq!(ready(&plan, after, outcome), Ready::Start);
}

#[test]
fn if_and_permissions_and_secrets_belong_to_a_call() {
    for (key, value) in [
        ("if", "'false'"),
        ("permissions", "{ contents: read }"),
        ("secrets", "inherit"),
    ] {
        let text = format!(
            "{BASE}\nworkflows:\n  release:\n    on: {{ push: {{}} }}\n    jobs: [{{ build: {{ {key}: {value} }} }}]\n"
        );
        let err = parse(&text).unwrap_err();
        assert!(
            err[0].message.contains(&format!("`{key}`")),
            "{key}: {err:?}"
        );
    }
}

#[test]
fn a_matrix_expression_cannot_have_entries_beside_it() {
    use hatchrun_workflow::{Matrix, WorkflowJob};
    let mut cfg = config(
        "
  release:
    on: { push: {} }
    jobs: [build]
",
    );
    let entry = WorkflowJob {
        job: "build".into(),
        matrix: Matrix {
            from: Some("${{ fromJSON('{}') }}".into()),
            exclude: vec![BTreeMap::from([("a".to_owned(), serde_json::json!(1))])],
            ..Matrix::default()
        },
        ..WorkflowJob::default()
    };
    cfg.workflows.get_mut("release").unwrap().jobs = vec![entry];
    let msg = compile(&cfg, "release", "refs/heads/main")
        .unwrap_err()
        .iter()
        .map(|d| d.message.clone())
        .collect::<String>();
    assert!(
        msg.contains("one expression and keys, `include` or `exclude` beside it"),
        "{msg}"
    );
}

#[test]
fn an_exclude_matches_the_text_a_value_shows_with_or_without_an_include() {
    let ids = |include: &str| -> Vec<String> {
        plan(&format!(
            "
  release:
    on: {{ push: {{}} }}
    jobs:
      - build:
          matrix:
            node: [18, 20]
            exclude: [{{ node: '20' }}]
{include}
"
        ))
        .into_iter()
        .map(|j| j.id)
        .collect()
    };
    assert_eq!(ids(""), ["build (18)"]);
    assert_eq!(
        ids("            include: [{ node: 22 }]"),
        ["build (18)", "build (22)"]
    );
}

#[test]
fn an_include_leaves_the_axis_value_it_matched() {
    let jobs = plan(
        "
  release:
    on: { push: {} }
    jobs:
      - build:
          matrix:
            flag: [false]
            include: [{ flag: 'false', extra: x }]
",
    );
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].matrix["flag"], serde_json::json!(false));
    assert_eq!(jobs[0].matrix["extra"], "x");
}

#[test]
fn a_number_input_rejects_what_json_cannot_hold() {
    use hatchrun_workflow::InputType;
    for text in ["NaN", "inf", "-Infinity", "x"] {
        assert!(
            InputType::Number.coerce(&serde_json::json!(text)).is_err(),
            "{text}"
        );
    }
    assert_eq!(
        InputType::Number.coerce(&serde_json::json!("2.5")).unwrap(),
        serde_json::json!(2.5)
    );
}

#[test]
fn the_secrets_context_is_read_in_any_case() {
    use hatchrun_workflow::call::passed_secret;
    assert_eq!(
        passed_secret("${{ SECRETS.token }}").as_deref(),
        Some("TOKEN")
    );
    assert_eq!(
        passed_secret("${{ Secrets.Token }}").as_deref(),
        Some("TOKEN")
    );
    assert_eq!(passed_secret("${{ secretsX.token }}"), None);
}

#[test]
fn a_matrix_value_a_call_keeps_is_charged_for_every_job_of_the_call() {
    // 100 jobs in the callee, each keeping a frame with a 100 KB value.
    let entries: Vec<String> = (0..100)
        .map(|i| format!("{{ build: {{ name: j{i} }} }}"))
        .collect();
    let big = "x".repeat(100_000);
    let text = format!(
        "
  w:
    on: {{ call: {{}} }}
    jobs: [{}]
  release:
    on: {{ push: {{}} }}
    jobs:
      - p:
          call: w
          matrix:
            n: [1]
            include: [{{ n: 1, big: '{big}' }}]
",
        entries.join(", ")
    );
    let msg = errors(&text);
    assert!(msg.contains("bytes of steps and values"), "{msg}");
}
