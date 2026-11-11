// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Compiling a workflow into a job plan.

use std::collections::BTreeMap;

use hatchrun_workflow::{
    CheckoutStep, Diagnostic, JobKind, Level, Outcome, Plan, PlannedJob, PlannedStep, Ready,
    Statuses, compile, config_errors, parse, ready, template, validate,
};

const CYCLE: &str = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: a, requires: [b] }
      - t: { name: b, requires: [a] }
";

fn plan(yaml: &str) -> Plan {
    compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap()
}

fn errors(yaml: &str) -> Vec<Diagnostic> {
    validate(&parse(yaml).unwrap())
}

fn assert_error(yaml: &str, words: &[&str]) {
    let errs = errors(yaml);
    for e in &errs {
        assert!(
            !e.message.contains("``"),
            "doubled backticks: {}",
            e.message
        );
    }
    assert!(
        errs.iter()
            .any(|e| words.iter().all(|w| e.message.contains(w))),
        "no error contains {words:?}: {errs:?}"
    );
}

fn runs(job: &PlannedJob) -> Vec<&str> {
    job.steps
        .iter()
        .filter_map(|s| match s {
            PlannedStep::Run(r) => Some(r.run.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn uses_steps_take_a_timeout() {
    let p = plan(
        r#"
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - { uses: "actions/cache@v4", timeout-minutes: 7 }
      - { uses: "actions/setup-node@v4" }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
"#,
    );
    let timeouts: Vec<Option<u32>> = p.jobs[0]
        .steps
        .iter()
        .filter_map(|s| match s {
            PlannedStep::Uses(u) => Some(u.timeout_minutes),
            _ => None,
        })
        .collect();
    assert_eq!(timeouts, [Some(7), None]);
}

#[test]
fn expands_the_spec_example() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let plan = compile(&config, "ci", "refs/heads/main").unwrap();
    let ids: Vec<_> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "test [1/4]",
            "test [2/4]",
            "test [3/4]",
            "test [4/4]",
            "hold",
            "deploy"
        ]
    );
    assert_eq!(plan.jobs[5].requires, ["hold"]);
    assert_eq!(
        plan.jobs[4].requires,
        ["test [1/4]", "test [2/4]", "test [3/4]", "test [4/4]"]
    );
    assert!(
        matches!(plan.jobs[0].steps[0], PlannedStep::Uses(ref u) if u.uses == "actions/checkout@v5")
    );
    assert_eq!(plan.jobs[0].node, Some((0, 4)));
}

#[test]
fn spec_example_details() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let plan = compile(&config, "ci", "refs/heads/main").unwrap();
    assert_eq!(plan.workflow, "ci");
    let test = &plan.jobs[3];
    assert_eq!(test.name, "test");
    assert_eq!(test.node, Some((3, 4)));
    assert_eq!(test.timeout_minutes, 360);
    assert_eq!(
        test.kind,
        JobKind::Run {
            template: template("ubuntu-26.04-4core").unwrap()
        }
    );
    assert_eq!(test.steps.len(), 5);
    assert!(
        matches!(test.steps[2], PlannedStep::Cache(ref c) if c.key == "deps-${{ hashFiles('Cargo.lock') }}")
    );
    assert!(
        matches!(test.steps[4], PlannedStep::TestResults { ref path } if path == "target/nextest")
    );
    assert_eq!(plan.jobs[4].kind, JobKind::Approval);
    assert_eq!(plan.jobs[4].steps, []);
    assert_eq!(runs(&plan.jobs[5]), ["./deploy.sh"]);
    assert_eq!(validate(&config), []);
}

#[test]
fn branch_filter_drops_jobs() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let plan = compile(&config, "ci", "refs/heads/feature").unwrap();
    assert!(plan.jobs.iter().all(|j| j.id != "deploy"));
    assert!(plan.jobs.iter().any(|j| j.id == "hold"));
}

#[test]
fn dropping_a_job_drops_what_requires_it() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: build, branches: [main] }
      - t: { name: ship, requires: [build] }
      - t: { name: release, tags: ['v*'] }
";
    let config = parse(yaml).unwrap();
    let ids = |r: &str| -> Vec<String> {
        compile(&config, "ci", r)
            .unwrap()
            .jobs
            .into_iter()
            .map(|j| j.id)
            .collect()
    };
    assert_eq!(ids("refs/heads/main"), ["build", "ship"]);
    assert_eq!(ids("refs/heads/dev"), Vec::<String>::new());
    assert_eq!(ids("refs/tags/v1"), ["release"]);
}

#[test]
fn unknown_workflow() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let err = compile(&config, "nightly", "refs/heads/main").unwrap_err();
    assert!(err[0].message.contains("nightly"));
}

#[test]
fn rejects_requires_cycle() {
    let err = compile(&parse(CYCLE).unwrap(), "ci", "refs/heads/main").unwrap_err();
    assert!(
        err[0].message.contains("`a`")
            && err[0].message.contains("`b`")
            && err[0].message.contains("cycle")
    );
}

#[test]
fn rejects_unknown_requires() {
    let err = compile(
        &parse("jobs: { t: { runs-on: ubuntu-latest, steps: [] } }\nworkflows: { ci: { on: { push: {} }, jobs: [{ t: { requires: [nope] } }] } }").unwrap(),
        "ci",
        "refs/heads/main",
    )
    .unwrap_err();
    assert!(err[0].message.contains("nope"));
}

#[test]
fn rejects_recursive_command() {
    let yaml = "
commands:
  a: { steps: [b] }
  b: { steps: [a] }
jobs: { t: { runs-on: ubuntu-latest, steps: [a] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
";
    let err = compile(&parse(yaml).unwrap(), "ci", "refs/heads/main").unwrap_err();
    assert!(err[0].message.contains("`a`") && err[0].message.contains("`b`"));
}

#[test]
fn unknown_template_lists_templates() {
    let err = validate(&parse("jobs: { t: { runs-on: ubuntu-24.04, steps: [] } }\nworkflows: { ci: { on: { push: {} }, jobs: [t] } }").unwrap());
    assert!(err[0].message.contains("ubuntu-24.04") && err[0].message.contains("ubuntu-26.04-arm"));
}

#[test]
fn unused_executor_with_a_bad_label_compiles() {
    let yaml = "
executors: { old: { runs-on: ubuntu-24.04 } }
jobs: { t: { runs-on: ubuntu-latest, steps: [] }, u: { executor: old, steps: [] } }
workflows:
  ci: { on: { push: {} }, jobs: [t] }
  other: { on: { push: {} }, jobs: [u] }
";
    let config = parse(yaml).unwrap();
    assert!(compile(&config, "ci", "refs/heads/main").is_ok());
    let err = compile(&config, "other", "refs/heads/main").unwrap_err();
    assert!(
        err[0].message.contains("executor `old`") && err[0].message.contains("ubuntu-24.04"),
        "{err:?}"
    );
    let all = validate(&config);
    assert!(
        all.iter().any(|d| d.message.contains("executor `old`")),
        "{all:?}"
    );
    let unused = parse(
        "executors: { old: { runs-on: ubuntu-24.04 } }\njobs: { t: { runs-on: ubuntu-latest, steps: [] } }\nworkflows: { ci: { on: { push: {} }, jobs: [t] } }",
    )
    .unwrap();
    assert!(compile(&unused, "ci", "refs/heads/main").is_ok());
    assert!(
        validate(&unused)
            .iter()
            .any(|d| d.message.contains("executor `old`"))
    );
    assert_eq!(config_errors(&unused), []);
}

#[test]
fn matrix_and_parameters() {
    let yaml = "
jobs:
  test:
    runs-on: ubuntu-latest
    parameters: { toolchain: { type: string } }
    steps:
      - run: cargo +${{ parameters.toolchain }} test
workflows:
  ci:
    on: { push: {} }
    jobs:
      - test: { matrix: { toolchain: [stable, nightly] } }
";
    let plan = plan(yaml);
    let ids: Vec<_> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["test (stable)", "test (nightly)"]);
    assert_eq!(runs(&plan.jobs[0]), ["cargo +stable test"]);
    assert_eq!(runs(&plan.jobs[1]), ["cargo +nightly test"]);
    assert_eq!(plan.jobs[1].matrix["toolchain"], "nightly");
}

#[test]
fn matrix_ids_follow_key_order_and_requires_all() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, steps: [{ run: 'echo ${{ matrix.os }}' }] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { matrix: { os: [a, b], n: [1, 2.0] } }
      - t: { name: after, requires: [t] }
";
    let plan = plan(yaml);
    let ids: Vec<_> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(
        ids,
        ["t (1, a)", "t (1, b)", "t (2, a)", "t (2, b)", "after"]
    );
    assert_eq!(plan.jobs[4].requires, &ids[..4]);
    // runtime expressions are left alone
    assert_eq!(runs(&plan.jobs[0]), ["echo ${{ matrix.os }}"]);
}

#[test]
fn parameters_substitute_with_defaults_and_kinds() {
    let yaml = "
commands:
  greet:
    parameters:
      who: { type: string, default: world }
      times: { type: integer }
      loud: { type: boolean, default: false }
    steps:
      - run: echo ${{parameters.who}} ${{ parameters.times }} ${{ parameters.loud }} ${{ github.sha }}
        env: { WHO: '${{ parameters.who }}' }
jobs:
  t:
    runs-on: ubuntu-latest
    parameters: { n: { type: integer, default: 3 } }
    steps:
      - greet: { times: '${{ parameters.n }}' }
      - greet: { who: you, times: 2, loud: true }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
";
    let plan = plan(yaml);
    assert_eq!(
        runs(&plan.jobs[0]),
        [
            "echo world 3 false ${{ github.sha }}",
            "echo you 2 true ${{ github.sha }}"
        ]
    );
    let PlannedStep::Run(ref step) = plan.jobs[0].steps[1] else {
        panic!("not a run step");
    };
    assert_eq!(step.env["WHO"], "you");
}

#[test]
fn rejects_bad_parameter_values() {
    let base = |with: &str| {
        format!(
            "
commands:
  c:
    parameters:
      n: {{ type: integer }}
      mode: {{ type: enum, enum: [fast, slow], default: fast }}
    steps: [{{ run: 'echo ${{{{ parameters.n }}}}' }}]
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [{{ c: {with} }}] }} }}
workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [t] }} }}
"
        )
    };
    assert_error(&base("{}"), &["`c`", "`n`", "required"]);
    assert_error(&base("{ n: x }"), &["`n`", "integer"]);
    assert_error(&base("{ n: 1, mode: medium }"), &["`mode`", "fast"]);
    assert_error(&base("{ n: 1, extra: 2 }"), &["`extra`", "`c`"]);
}

#[test]
fn rejects_unknown_parameter_reference() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [{ run: 'echo ${{ parameters.nope }}' }] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`nope`", "`t`"],
    );
}

#[test]
fn rejects_unknown_workflow_with_keys() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, parameters: { x: { type: string, default: a } }, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [{ t: { requirez: [b] } }] } }
";
    assert_error(yaml, &["`requirez`", "`t`"]);
}

#[test]
fn approvals_name_no_job_and_take_no_parameters() {
    let ok = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t, { hold: { type: approval, requires: [t] } }] } }
",
    );
    assert_eq!(ok.jobs[1].id, "hold");
    assert_error(
        "
workflows: { ci: { on: { push: {} }, jobs: [{ hold: { type: approval, x: 1 } }] } }
",
        &["`hold`", "`x`"],
    );
}

#[test]
fn rejects_executor_problems() {
    assert_error(
        "
executors: { e: { runs-on: ubuntu-latest } }
jobs: { t: { executor: e, runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`t`", "executor", "runs-on"],
    );
    assert_error(
        "
jobs: { t: { steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`t`", "executor", "runs-on"],
    );
    assert_error(
        "
jobs: { t: { executor: e, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`t`", "`e`"],
    );
    assert_error(
        "
executors: { e: { runs-on: ubuntu-99 } }
jobs: { t: { executor: e, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["ubuntu-99", "ubuntu-latest"],
    );
}

#[test]
fn rejects_unknown_job_and_duplicate_ids() {
    assert_error(
        "workflows: { ci: { on: { push: {} }, jobs: [nope] } }",
        &["`nope`"],
    );
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t, t] } }
",
        &["`t`", "duplicate"],
    );
}

#[test]
fn rejects_unknown_command_and_step_key_names() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [missing] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`missing`", "`t`"],
    );
    assert_error(
        "
commands: { workspace: { steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [] } }
",
        &["`workspace`", "step key"],
    );
}

#[test]
fn docker_uses_plans_and_needs_an_image() {
    let p = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [{ uses: 'docker://alpine' }] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
    );
    let PlannedStep::Uses(step) = &p.jobs[0].steps[0] else {
        panic!("{:?}", p.jobs[0].steps)
    };
    assert_eq!(step.uses, "docker://alpine");
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [{ uses: 'docker://' }] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["job `t`", "no image"],
    );
}

#[test]
fn containers_plan_with_parameters_substituted() {
    let p = plan(
        "
jobs:
  t:
    parameters: { tag: { type: string, default: '17' } }
    runs-on: ubuntu-latest
    container: { image: 'rust:${{ parameters.tag }}', env: { A: '${{ secrets.X }}' } }
    services:
      db:
        image: 'postgres:${{ parameters.tag }}'
        ports: [5432]
        options: --health-cmd pg_isready
    steps: []
workflows: { ci: { on: { push: {} }, jobs: [{ t: { tag: '18' } }] } }
",
    );
    let job = &p.jobs[0];
    assert_eq!(job.container.as_ref().unwrap().image, "rust:18");
    assert_eq!(job.container.as_ref().unwrap().env["A"], "${{ secrets.X }}");
    assert_eq!(job.services["db"].image, "postgres:18");
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), p);
}

#[test]
fn rejects_bad_containers() {
    for (body, words) in [
        (
            "container: { image: 'a b' }",
            &["job `t`", "`container`", "whitespace"][..],
        ),
        (
            "container: { image: a, options: '--network host' }",
            &["`container`", "--network"],
        ),
        (
            "services: { db: { image: a, options: '--name x' } }",
            &["service `db`", "--name"],
        ),
        (
            "services: { 'a.b': { image: a } }",
            &["service `a.b`", "hostname"],
        ),
        (
            "container: { image: a, options: \"--x 'y\" }",
            &["`options`", "quote"],
        ),
        (
            "container: { image: a, env: { A: '${{ nosuch.x }}' } }",
            &["`container`", "unknown context"],
        ),
    ] {
        assert_error(
            &format!(
                "
jobs: {{ t: {{ runs-on: ubuntu-latest, {body}, steps: [] }} }}
workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [t] }} }}
"
            ),
            words,
        );
    }
}

#[test]
fn rejects_out_of_range_expansion() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, parallelism: 65, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`t`", "parallelism", "64"],
    );
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, parallelism: 0, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`t`", "parallelism"],
    );
    let values: Vec<String> = (0..17).map(|i| i.to_string()).collect();
    let list = values.join(", ");
    assert_error(
        &format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{list}], b: [{list}] }} }} }}] }} }}
"
        ),
        &["`t`", "matrix", "256"],
    );
}

#[test]
fn rejects_include_and_ignore_together() {
    for (trigger, key) in [
        (
            "push: { branches: [a], branches-ignore: [b] }",
            "branches-ignore",
        ),
        ("push: { tags: [a], tags-ignore: [b] }", "tags-ignore"),
        (
            "pull_request: { paths: [a], paths-ignore: [b] }",
            "paths-ignore",
        ),
    ] {
        assert_error(
            &format!("workflows: {{ ci: {{ on: {{ {trigger} }}, jobs: [] }} }}"),
            &["`ci`", key],
        );
    }
}

#[test]
fn timeout_env_and_if_carry_over() {
    let plan = plan(
        "
jobs:
  t:
    runs-on: ubuntu-26.04-arm
    timeout-minutes: 30
    if: github.ref == 'refs/heads/main'
    env: { A: '1' }
    outputs: { v: '${{ steps.x.outputs.v }}' }
    steps: []
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
    );
    let t = &plan.jobs[0];
    assert_eq!(t.timeout_minutes, 30);
    assert_eq!(t.if_.as_deref(), Some("github.ref == 'refs/heads/main'"));
    assert_eq!(t.env["A"], "1");
    assert_eq!(t.outputs["v"], "${{ steps.x.outputs.v }}");
    assert_eq!(t.node, None);
}

#[test]
fn cycle_message_leaves_out_jobs_after_the_cycle() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: a, requires: [b] }
      - t: { name: b, requires: [a] }
      - t: { name: c, requires: [a] }
";
    let err = errors(yaml);
    assert_eq!(err.len(), 1);
    assert!(err[0].message.contains("`a`, `b`") && !err[0].message.contains("`c`"));
}

/// Commands `c0` to `c<depth-1>` each list the next one `fan` times; the
/// last one is `leaf`.
fn fan_out(depth: usize, fan: usize, leaf: &str) -> String {
    use std::fmt::Write as _;
    let mut yaml = String::from("commands:\n");
    for i in 0..depth {
        let list = vec![format!("c{}", i + 1); fan].join(", ");
        writeln!(yaml, "  c{i}: {{ steps: [{list}] }}").unwrap();
    }
    writeln!(yaml, "  c{depth}: {{ steps: [{leaf}] }}").unwrap();
    yaml += "jobs: { t: { runs-on: ubuntu-latest, steps: [c0] } }\n";
    yaml += "workflows: { ci: { on: { push: {} }, jobs: [t] } }\n";
    yaml
}

#[test]
fn command_expansion_has_a_work_budget() {
    let config = parse(&fan_out(9, 10, "")).unwrap();
    let started = std::time::Instant::now();
    let err = validate(&config);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(err[0].message.contains("workflow `ci`") && err[0].message.contains("50000"));

    // Leaves that fail add no error per visit.
    let err = validate(&parse(&fan_out(9, 10, "{ run: '${{ parameters.x }}' }")).unwrap());
    assert!(err.len() <= 2, "{err:?}");
    assert!(err.iter().any(|e| e.message.contains("50000")));

    // A wide but small expansion still compiles.
    let plan = plan(&fan_out(2, 10, "{ run: echo }"));
    assert_eq!(plan.jobs[0].steps.len(), 100);
}

#[test]
fn approval_id_is_its_key() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t, { hold: { type: approval, name: wait, requires: [t] } }] } }
",
        &["`hold`", "`name`"],
    );
}

#[test]
fn rejects_repeated_matrix_values() {
    for values in ["[1, '1']", "[a, a]"] {
        assert_error(
            &format!(
                "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ n: {values} }} }} }}] }} }}
"
            ),
            &["`t`", "`n`", "twice"],
        );
    }
}

#[test]
fn rejects_expanded_id_collisions() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [{ t: { matrix: { n: [a] } } }, { t: { name: 't (a)' } }] } }
",
        &["`t (a)`", "`t`", "id"],
    );
}

#[test]
fn rejects_parameter_reads_at_workflow_level() {
    let job =
        "jobs: { t: { runs-on: ubuntu-latest, parameters: { x: { type: string } }, steps: [] } }\n";
    assert_error(
        &format!(
            "{job}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ x: '${{{{parameters.y}}}}' }} }}] }} }}"
        ),
        &["`t`", "`x`", "parameters"],
    );
    assert_error(
        &format!(
            "{job}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ x: ['a ${{{{ parameters.y }}}}'] }} }} }}] }} }}"
        ),
        &["`t`", "`x`", "parameters"],
    );
}

/// The reviewer's probe: `c0` calls `c1` nine times, down to `c3`, which
/// has nine run steps (6561 steps), on a 16x16 matrix job.
fn wide_matrix(parallelism: u32, entries: usize) -> String {
    use std::fmt::Write as _;
    let mut yaml = String::from("commands:\n");
    for i in 0..3 {
        let list = vec![format!("c{}", i + 1); 9].join(", ");
        writeln!(yaml, "  c{i}: {{ steps: [{list}] }}").unwrap();
    }
    yaml += "  c3: { steps: [{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a}] }\n";
    writeln!(
        yaml,
        "jobs: {{ t: {{ runs-on: ubuntu-latest, parallelism: {parallelism}, parameters: {{ a: {{type: string}}, b: {{type: string}} }}, steps: [c0] }} }}"
    )
    .unwrap();
    let values: Vec<String> = (0..16).map(|i| format!("v{i}")).collect();
    let m = values.join(", ");
    let jobs: Vec<String> = (0..entries)
        .map(|e| format!("{{ t: {{ name: e{e}, matrix: {{ a: [{m}], b: [{m}] }} }} }}"))
        .collect();
    writeln!(
        yaml,
        "workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{}] }} }}",
        jobs.join(", ")
    )
    .unwrap();
    yaml
}

fn timed_compile(yaml: &str) -> Vec<Diagnostic> {
    let config = parse(yaml).unwrap();
    let started = std::time::Instant::now();
    let err = compile(&config, "ci", "refs/heads/main").unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    err
}

#[test]
fn one_budget_covers_matrix_and_parallel_copies() {
    // 256 combinations of 6561 steps each: the shared budget runs out.
    let err = timed_compile(&wide_matrix(1, 1));
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(err[0].message.contains("workflow `ci`") && err[0].message.contains("50000"));

    // With 64 nodes the plan would hold 16384 jobs; refused before any
    // expansion.
    let err = timed_compile(&wide_matrix(64, 1));
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(err[0].message.contains("workflow `ci`") && err[0].message.contains("256 jobs"));
}

#[test]
fn a_plan_holds_at_most_256_jobs() {
    let job = "jobs: { t: { runs-on: ubuntu-latest, parallelism: 2, steps: [] } }\n";
    let values: Vec<String> = (0..16).map(|i| i.to_string()).collect();
    let m = values.join(", ");
    // 16x16 matrix, two nodes each: 512 jobs.
    assert_error(
        &format!(
            "{job}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}] }} }} }}] }} }}"
        ),
        &["workflow `ci`", "256 jobs"],
    );
    // 16x17 matrix: 272 entries.
    let job = "jobs: { t: { runs-on: ubuntu-latest, steps: [] } }\n";
    assert_error(
        &format!(
            "{job}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}, 16] }} }} }}] }} }}"
        ),
        &["`t`", "272", "256"],
    );
    // 256 jobs over two entries is fine.
    let plan = plan(&format!(
        "{job}workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{}] }} }} }}, {{ t: {{ name: u, matrix: {{ a: [{m}], b: [{}] }} }} }}] }} }}",
        values[..8].join(", "),
        values[..8].join(", ")
    ));
    assert_eq!(plan.jobs.len(), 256);
}

#[test]
fn validate_shares_the_budget_across_workflows() {
    use std::fmt::Write as _;
    // One workflow costs about 27000 units (7380 visits plus 3 nodes of
    // 6561 steps), so each fits alone and the two together do not.
    let mut yaml = String::from("commands:\n");
    for i in 0..3 {
        let list = vec![format!("c{}", i + 1); 9].join(", ");
        writeln!(yaml, "  c{i}: {{ steps: [{list}] }}").unwrap();
    }
    yaml += "  c3: { steps: [{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a}] }\n";
    yaml += "jobs: { t: { runs-on: ubuntu-latest, parallelism: 3, steps: [c0] } }\n";
    yaml += "workflows:\n";
    for w in ["one", "two"] {
        writeln!(yaml, "  {w}: {{ on: {{ push: {{}} }}, jobs: [t] }}").unwrap();
    }
    let config = parse(&yaml).unwrap();
    assert!(compile(&config, "one", "refs/heads/main").is_ok());
    assert!(compile(&config, "two", "refs/heads/main").is_ok());
    let err = validate(&config);
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(err[0].message.contains("50000"));
}

#[test]
fn spec_example_fits_the_budget() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    assert!(compile(&config, "ci", "refs/heads/main").is_ok());
    assert_eq!(validate(&config), []);
}

/// Each command passes `${{ parameters.x }}${{ parameters.x }}` to the
/// next, doubling the value at every level.
fn doubling(levels: usize) -> String {
    use std::fmt::Write as _;
    let mut yaml = String::from("commands:\n");
    for i in 0..levels {
        writeln!(
            yaml,
            "  c{i}: {{ parameters: {{ x: {{ type: string }} }}, steps: [{{ c{}: {{ x: '${{{{ parameters.x }}}}${{{{ parameters.x }}}}' }} }}] }}",
            i + 1
        )
        .unwrap();
    }
    writeln!(
        yaml,
        "  c{levels}: {{ parameters: {{ x: {{ type: string }} }}, steps: [{{ run: '${{{{ parameters.x }}}}' }}] }}"
    )
    .unwrap();
    yaml += "jobs: { t: { runs-on: ubuntu-latest, steps: [{ c0: { x: ab } }] } }\n";
    yaml += "workflows: { ci: { on: { push: {} }, jobs: [t] } }\n";
    yaml
}

/// One `run` of `size` bytes, called 190 times, on a 16x16 matrix.
fn big_step(size: usize) -> String {
    let m: Vec<String> = (0..16).map(|i| format!("v{i}")).collect();
    let m = m.join(", ");
    format!(
        "commands:\n  b: {{ steps: [{{ run: {} }}] }}\n  f: {{ steps: [{}] }}\njobs: {{ t: {{ runs-on: ubuntu-latest, steps: [f] }} }}\nworkflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}] }} }} }}] }} }}\n",
        "x".repeat(size),
        vec!["b"; 190].join(", ")
    )
}

#[test]
fn substituted_values_are_capped() {
    let err = timed_compile(&doubling(26));
    assert!(err.iter().any(|e| e.message.contains("65536")), "{err:?}");
    // A few doublings stay well under the cap.
    let plan = plan(&doubling(4));
    assert_eq!(runs(&plan.jobs[0]), ["ab".repeat(16)]);
}

#[test]
fn the_budget_counts_bytes() {
    let err = timed_compile(&big_step(10_240));
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(err[0].message.contains("workflow `ci`") && err[0].message.contains("8388608"));
}

/// A 16x16 matrix of job `t`, whose job or step holds `items`.
fn many_items(job: &str, step: &str) -> String {
    let m: Vec<String> = (0..16).map(|i| format!("v{i}")).collect();
    let m = m.join(", ");
    format!(
        "jobs: {{ t: {{ runs-on: ubuntu-latest, {job} steps: [{step}] }} }}\nworkflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}] }} }} }}] }} }}\n"
    )
}

fn short_entries(n: usize) -> String {
    (0..n)
        .map(|i| format!("{i:x}: ''"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn short_strings(n: usize) -> String {
    (0..n)
        .map(|i| format!("'{i:x}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn the_budget_counts_each_stored_entry() {
    // Each entry is a few bytes of text but tens of bytes in memory.
    let cases = [
        many_items(&format!("env: {{ {} }},", short_entries(5_000)), ""),
        many_items(&format!("env: {{ {} }},", short_entries(20_000)), ""),
        many_items(
            "",
            &format!("{{ run: x, env: {{ {} }} }}", short_entries(5_000)),
        ),
        many_items(
            "",
            &format!(
                "{{ cache: {{ key: k, restore-keys: [{}] }} }}",
                short_strings(5_000)
            ),
        ),
        many_items(
            "",
            &format!(
                "{{ cache: {{ key: k, paths: [{}] }} }}",
                short_strings(5_000)
            ),
        ),
    ];
    for yaml in &cases {
        let err = timed_compile(yaml);
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(err[0].message.contains("8388608"), "{err:?}");
    }
    // A job env of a few hundred entries still compiles on 16x16.
    let config = parse(&many_items(
        &format!("env: {{ {} }},", short_entries(200)),
        "",
    ))
    .unwrap();
    let plan = compile(&config, "ci", "refs/heads/main").unwrap();
    assert_eq!(plan.jobs.len(), 256);
    assert_eq!(plan.jobs[0].env.len(), 200);
}

/// Command `b` holds `step` once; `f` calls `b` `calls` times and `g`
/// calls `f` ten times, so one config step is planned `10 * calls` times.
fn fan_out_step(step: &str, calls: usize) -> String {
    format!(
        "commands:\n  b: {{ steps: [{step}] }}\n  f: {{ steps: [{}] }}\n  g: {{ steps: [f, f, f, f, f, f, f, f, f, f] }}\njobs: {{ t: {{ runs-on: ubuntu-latest, steps: [g] }} }}\nworkflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [t] }} }}\n",
        vec!["b"; calls].join(", ")
    )
}

#[test]
fn the_budget_is_charged_before_each_entry_is_made() {
    let ones = vec!["1"; 5_000].join(", ");
    let params: Vec<String> = (0..1_000)
        .map(|i| format!("p{i}: {{ type: string }}"))
        .collect();
    let cases = [
        fan_out_step(
            &format!("{{ run: x, env: {{ {} }} }}", short_entries(5_000)),
            1_000,
        ),
        fan_out_step(
            &format!("{{ run: x, env: {{ {} }} }}", short_entries(20_000)),
            1_000,
        ),
        fan_out_step(
            &format!("{{ uses: a/b@v1, with: {{ {} }} }}", short_entries(5_000)),
            1_000,
        ),
        fan_out_step(
            &format!("{{ uses: a/b@v1, env: {{ {} }} }}", short_entries(5_000)),
            1_000,
        ),
        fan_out_step(
            &format!("{{ uses: a/b@v1, with: {{ a: [{ones}] }} }}"),
            1_000,
        ),
        fan_out_step(
            &format!(
                "{{ workspace: {{ persist: {{ root: r, paths: [{}] }} }} }}",
                short_strings(5_000)
            ),
            1_000,
        ),
    ];
    for yaml in &cases {
        let err = timed_compile(yaml);
        assert!(err.iter().any(|e| e.message.contains("8388608")), "{err:?}");
    }
    // A command missing many required parameters, called many times:
    // the repeated messages are charged too.
    let yaml = fan_out_step("x", 1_000).replace(
        "  b: { steps: [x] }",
        &format!(
            "  b: {{ parameters: {{ {} }}, steps: [] }}",
            params.join(", ")
        ),
    );
    let err = timed_compile(&yaml);
    assert!(err.iter().any(|e| e.message.contains("8388608")), "{err:?}");
}

#[test]
fn dispatch_input_defaults_must_fit_their_kind() {
    for (input, words) in [
        (
            "{ type: integer, default: '5' }",
            &["`ci`", "`n`", "an integer"][..],
        ),
        (
            "{ type: integer, default: 1.5 }",
            &["`ci`", "`n`", "an integer"],
        ),
        (
            "{ type: boolean, default: 'yes' }",
            &["`ci`", "`n`", "a boolean"],
        ),
        (
            "{ type: enum, enum: [a, b], default: z }",
            &["`ci`", "`n`", "one of a, b"],
        ),
        (
            "{ type: string, default: [x] }",
            &["`ci`", "`n`", "a string"],
        ),
    ] {
        assert_error(
            &format!(
                "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ workflow_dispatch: {{ inputs: {{ n: {input} }} }} }}
    jobs: [t]
"
            ),
            words,
        );
    }
    assert_eq!(
        errors(
            "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { workflow_dispatch: { inputs: { n: { type: integer, default: 5 }, e: { type: enum, enum: [a], default: a } } } }
    jobs: [t]
"
        ),
        Vec::new()
    );
}

fn one_job(job: &str) -> String {
    format!(
        "
jobs:
  t:
{job}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [t]
"
    )
}

#[test]
fn validate_checks_every_expression() {
    for (job, words) in [
        (
            "    runs-on: ubuntu-latest\n    steps: [{ run: \"echo ${{ github.ref == }}\" }]",
            &[
                "job `t` step 1 `run` has a bad expression",
                "after",
                "at byte",
            ][..],
        ),
        (
            "    runs-on: ubuntu-latest\n    steps: [{ run: x, if: nosuch.x }]",
            &["step 1 `if`", "unknown context 'nosuch'"],
        ),
        (
            "    runs-on: ubuntu-latest\n    env: { A: \"${{ nosuch(1) }}\" }\n    steps: []",
            &["job `t` `env.A` has a bad expression", "unknown function"],
        ),
        (
            "    runs-on: ubuntu-latest\n    steps: [{ run: x, env: { B: \"${{ nosuch(1) }}\" } }]",
            &[
                "job `t` step 1 `env.B` has a bad expression",
                "unknown function",
            ],
        ),
        (
            "    runs-on: ubuntu-latest\n    if: \"${{ nosuch.job-index }}\"\n    steps: []",
            &["`if`", "unknown context 'nosuch'"],
        ),
        (
            "    runs-on: ubuntu-latest\n    steps: [{ uses: a/b@v1, with: { x: [\"${{ 1 +\" ] } }]",
            &["step 1 `with.x`", "no closing"],
        ),
        (
            "    runs-on: ubuntu-latest\n    steps: [{ cache: { key: \"${{ jobs }}\", paths: [a] } }]",
            &["`cache.key`", "unknown context 'jobs'"],
        ),
    ] {
        assert_error(&one_job(job), words);
    }
    // vars and inputs are known; a whole substitution and run-time
    // contexts pass.
    let ok = one_job(
        "    runs-on: ubuntu-latest\n    parameters: { go: { type: boolean, default: true } }\n    if: ${{ parameters.go }}\n    steps: [{ run: \"echo ${{ vars.A }} ${{ inputs.b }}\", if: \"github.ref == 'refs/heads/main' && success()\" }]",
    );
    assert_eq!(errors(&ok), []);
}

#[test]
fn validate_rejects_parameters_read_at_run_time() {
    for (cond, field) in [
        ("if: parameters.go", "`if`"),
        ("if: \"${{ parameters.go && 'a' }}\"", "`if`"),
        (
            "env: { A: \"${{ format('{0}', parameters.go) }}\" }",
            "`env.A`",
        ),
    ] {
        let yaml = one_job(&format!(
            "    runs-on: ubuntu-latest\n    parameters: {{ go: {{ type: boolean, default: true }} }}\n    {cond}\n    steps: []"
        ));
        assert_error(&yaml, &["job `t`", field, "reads `parameters`"]);
    }
    let yaml = "
commands:
  c:
    parameters: { go: { type: boolean, default: true } }
    steps: [{ run: x, if: parameters.go }]
jobs: { t: { runs-on: ubuntu-latest, steps: [c] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
";
    assert_error(yaml, &["command `c` step 1 `if` reads `parameters`"]);
    let yaml = "
commands:
  c:
    parameters: { v: { type: string, default: x } }
    steps: [{ run: x }]
jobs: { t: { runs-on: ubuntu-latest, steps: [{ c: { v: \"${{ 1 + }}\" } }] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
";
    assert_error(yaml, &["job `t` step 1 `c` `v` has a bad expression"]);
    let yaml = "
jobs:
  t: { runs-on: ubuntu-latest, parameters: { v: { type: string } }, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs: [ t: { v: \"${{ github.sha && parameters.v }}\" } ]
";
    assert_error(yaml, &["`t` sets `v`", "reads `parameters`"]);
}

#[test]
fn rejects_job_parameters_named_like_entry_keys() {
    for key in ["tags", "branches", "name", "requires", "type", "matrix"] {
        let yaml = one_job(&format!(
            "    runs-on: ubuntu-latest\n    parameters: {{ {key}: {{ type: string, default: x }} }}\n    steps: []"
        ));
        assert_error(&yaml, &["job `t`", &format!("parameter `{key}`"), "rename"]);
    }
}

#[test]
fn validate_checks_jobs_and_commands_no_workflow_runs() {
    for (yaml, words) in [
        (
            "jobs: { a: { runs-on: nope, steps: [] } }",
            &["job `a`", "runs-on: nope"][..],
        ),
        (
            "jobs: { a: { executor: e, steps: [] } }",
            &["job `a`", "executor `e`"],
        ),
        (
            "jobs: { a: { steps: [] } }",
            &["job `a`", "needs `executor` or `runs-on`"],
        ),
        (
            "jobs: { a: { runs-on: ubuntu-latest, parameters: { n: { type: integer, default: x } }, steps: [] } }",
            &["job `a` parameter `n`", "an integer"],
        ),
        (
            "commands: { c: { parameters: { b: { type: boolean, default: 1 } }, steps: [] } }",
            &["command `c` parameter `b`", "a boolean"],
        ),
        (
            "commands: { c: { steps: [missing] } }",
            &["command `c`", "command `missing`"],
        ),
        (
            "commands: { c: { steps: [{ run: \"${{ nosuch }}\" }] } }",
            &["command `c`", "unknown context"],
        ),
        (
            "commands: { c: { steps: [{ uses: 'docker://' }] } }",
            &["command `c`", "docker://"],
        ),
        (
            "commands:\n  c: { parameters: { v: { type: string } }, steps: [] }\njobs: { a: { runs-on: ubuntu-latest, steps: [ c: { w: 1 } ] } }",
            &["command `c`", "no parameter `w`"],
        ),
        (
            "commands:\n  c: { parameters: { v: { type: string } }, steps: [] }\njobs: { a: { runs-on: ubuntu-latest, steps: [c] } }",
            &["command `c`", "parameter `v`", "required"],
        ),
    ] {
        assert_error(yaml, words);
    }
}

/// A config with `workflows` workflows, each filtering pushes on
/// `patterns` patterns made by `pattern`.
fn many_patterns(workflows: usize, patterns: usize, pattern: fn(usize) -> String) -> String {
    use std::fmt::Write as _;
    let list: Vec<String> = (0..patterns).map(pattern).collect();
    let mut y = String::from("jobs: { t: { runs-on: ubuntu-latest, steps: [] } }\nworkflows:\n");
    for w in 0..workflows {
        writeln!(
            y,
            "  w{w}: {{ on: {{ push: {{ paths: [{}] }} }}, jobs: [t] }}",
            list.join(", ")
        )
        .unwrap();
    }
    y
}

#[test]
fn filters_are_capped() {
    let glob = |i| format!("'src/m{i}/**/*.rs'");
    assert_eq!(errors(&many_patterns(1, 256, glob)), Vec::new());
    assert_error(
        &many_patterns(1, 257, glob),
        &["workflow `w0`: `on.push.paths` has 257 patterns; the limit is 256"],
    );
    assert_eq!(errors(&many_patterns(16, 256, glob)), Vec::new());
    assert_error(
        &many_patterns(17, 256, glob),
        &["the config's filters have 4352 glob patterns; the limit is 4096"],
    );
    // Plain text, `text**` and `**text` match as text and do not count.
    let plain = many_patterns(20, 250, |i| format!("'docs/{i}/**'"));
    assert_eq!(errors(&plain), Vec::new());
}

fn token(yaml: &str) -> std::collections::BTreeMap<String, Level> {
    plan(yaml).jobs[0].token_permissions.clone()
}

fn levels(entries: &[(&str, Level)]) -> std::collections::BTreeMap<String, Level> {
    entries.iter().map(|(k, l)| ((*k).to_owned(), *l)).collect()
}

#[test]
fn no_permissions_gives_the_read_default() {
    let got = token(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci: { on: { push: {} }, jobs: [t] }
",
    );
    assert_eq!(
        got,
        levels(&[
            ("checks", Level::Read),
            ("contents", Level::Read),
            ("metadata", Level::Read),
            ("pull_requests", Level::Read),
        ])
    );
}

#[test]
fn job_permissions_replace_the_workflow_ones() {
    let got = token(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    permissions: { pull-requests: write }
    steps: []
workflows:
  ci:
    on: { push: {} }
    permissions: { contents: write, issues: write }
    jobs: [t]
",
    );
    assert_eq!(
        got,
        levels(&[("metadata", Level::Read), ("pull_requests", Level::Write)])
    );
}

#[test]
fn workflow_permissions_apply_to_jobs_without_their_own() {
    let got = token(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    permissions: { contents: write, issues: write }
    jobs: [t]
",
    );
    assert_eq!(
        got,
        levels(&[
            ("contents", Level::Write),
            ("issues", Level::Write),
            ("metadata", Level::Read),
        ])
    );
}

#[test]
fn id_token_write_reaches_the_planned_job() {
    let got = token(
        "
jobs: { t: { runs-on: ubuntu-latest, permissions: { id-token: write }, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs: [t]
",
    );
    assert_eq!(
        got,
        levels(&[("id_token", Level::Write), ("metadata", Level::Read)])
    );
}

#[test]
fn bad_permissions_fail_the_parse() {
    let err = parse(
        "
jobs: { t: { runs-on: ubuntu-latest, permissions: { id-token: read }, steps: [] } }
workflows: {}
",
    )
    .unwrap_err();
    assert!(
        err[0]
            .message
            .contains("`permissions.id-token` is `read`; use write or none"),
        "{err:?}"
    );
}

#[test]
fn environment_reaches_the_planned_job() {
    let plan = plan(
        "
jobs:
  deploy: { runs-on: ubuntu-latest, environment: production, steps: [] }
  test: { runs-on: ubuntu-latest, steps: [] }
workflows: { ci: { on: { push: {} }, jobs: [deploy, test] } }
",
    );
    assert_eq!(plan.jobs[0].environment.as_deref(), Some("production"));
    assert_eq!(plan.jobs[1].environment, None);
}

#[test]
fn environment_takes_parameters() {
    let plan = plan(
        r#"
jobs:
  deploy:
    runs-on: ubuntu-latest
    parameters: { target: { type: string } }
    environment: "${{ parameters.target }}"
    steps: []
workflows:
  ci:
    on: { push: {} }
    jobs:
      - deploy: { matrix: { target: [staging, production] } }
"#,
    );
    assert_eq!(plan.jobs[0].environment.as_deref(), Some("staging"));
    assert_eq!(plan.jobs[1].environment.as_deref(), Some("production"));
}

#[test]
fn environment_with_a_run_time_expression_is_kept_for_the_host() {
    let plan = plan(
        r#"
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: "${{ matrix.stage }}-${{ needs.pick.outputs.zone }}"
    environment-url: "https://${{ steps.up.outputs.host }}"
    steps: []
workflows:
  ci:
    on: { push: {} }
    jobs:
      - deploy: { matrix: { stage: [a, b] } }
"#,
    );
    assert_eq!(
        plan.jobs[0].environment.as_deref(),
        Some("${{ matrix.stage }}-${{ needs.pick.outputs.zone }}")
    );
    assert_eq!(
        plan.jobs[1].environment_url.as_deref(),
        Some("https://${{ steps.up.outputs.host }}")
    );
}

#[test]
fn environment_names_read_only_what_the_host_has_when_the_job_starts() {
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment: "${{ env.STAGE }}", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["`environment` reads `env`", "it can read only"],
    );
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment: "${{ steps.x.outputs.y }}", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["`environment` reads `steps`"],
    );
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment: "${{ github.ref_name", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["`environment` has a bad expression"],
    );
}

#[test]
fn environment_url_needs_an_environment_and_a_good_expression() {
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment-url: "https://x", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["`environment-url` but no `environment`"],
    );
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment: prod, environment-url: "${{ nope.x }}", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["`environment-url` has a bad expression"],
    );
}

#[test]
fn run_name_is_checked_and_reaches_the_plan() {
    let p = plan(
        r#"
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    run-name: "Deploy ${{ inputs.target }} by ${{ github.actor }}"
    on: { push: {} }
    jobs: [t]
"#,
    );
    assert_eq!(
        p.run_name.as_deref(),
        Some("Deploy ${{ inputs.target }} by ${{ github.actor }}")
    );
    let none = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
    );
    assert_eq!(none.run_name, None);
    assert_error(
        r#"
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { run-name: "${{ matrix.x }}", on: { push: {} }, jobs: [t] } }
"#,
        &["`run-name` reads `matrix`"],
    );
    assert_error(
        r#"
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { run-name: "${{ github.", on: { push: {} }, jobs: [t] } }
"#,
        &["`run-name` has a bad expression"],
    );
}

#[test]
fn run_titles_are_evaluated_against_github_and_inputs() {
    let ctx = serde_json::json!({
        "github": { "actor": "octocat", "ref_name": "main" },
        "inputs": { "target": "staging" },
    });
    assert_eq!(
        hatchrun_workflow::run_title("Deploy ${{ inputs.target }} by ${{ github.actor }}", &ctx),
        Ok("Deploy staging by octocat".to_owned())
    );
    assert_eq!(
        hatchrun_workflow::run_title("  ${{ github.ref_name }}\nsecond", &ctx),
        Ok("main".to_owned())
    );
    assert!(hatchrun_workflow::run_title("${{ nope.x }}", &ctx).is_err());
}

#[test]
fn environment_with_a_bad_name_is_an_error() {
    assert_error(
        r#"
jobs: { deploy: { runs-on: ubuntu-latest, environment: "prod eu", steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [deploy] } }
"#,
        &["Use letters, digits, -, _ and . in environment names"],
    );
}

#[test]
fn approval_jobs_have_no_environment() {
    let plan = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, environment: production, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t, { hold: { type: approval, requires: [t] } }] } }
",
    );
    assert_eq!(plan.jobs[1].environment, None);
}

/// A, B after A, C after B, with C's statuses on B as given.
fn chain(statuses: &str) -> Plan {
    plan(&format!(
        "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - t: {{ name: a }}
      - t: {{ name: b, requires: [a] }}
      - t: {{ name: c, requires: [{{ b: {statuses} }}] }}
"
    ))
}

/// What `ready` says for the plan's last job, given how A and B ended.
fn decide(plan: &Plan, a: Option<Outcome>, b: Option<Outcome>) -> Ready {
    let ended: BTreeMap<&str, Outcome> = [("a", a), ("b", b)]
        .into_iter()
        .filter_map(|(id, o)| Some((id, o?)))
        .collect();
    ready(plan, plan.jobs.last().unwrap(), |id| ended.get(id).copied())
}

#[test]
fn compile_carries_requires_statuses() {
    let plan = chain("[failed, not_run]");
    assert_eq!(
        plan.jobs[2].statuses,
        BTreeMap::from([("b".to_owned(), Statuses::FAILED.union(Statuses::NOT_RUN))])
    );
    assert_eq!(
        plan.jobs[1].statuses,
        BTreeMap::from([("a".to_owned(), Statuses::SUCCESS)])
    );
}

#[test]
fn ready_checks_statuses_over_every_ancestor() {
    use Outcome::{Canceled, Failed, NotRun, Success};
    // (statuses of C on B, how A ended, how B ended, what C does)
    let table = [
        ("success", Success, Success, Ready::Start),
        ("success", Success, Failed, Ready::NotRun),
        ("success", Failed, NotRun, Ready::NotRun),
        // A failed and B did not run: B's ancestry failed.
        ("failed", Failed, NotRun, Ready::Start),
        ("failed", Success, Failed, Ready::Start),
        ("failed", Success, Success, Ready::NotRun),
        ("failed", Success, NotRun, Ready::NotRun),
        ("failed", Canceled, NotRun, Ready::NotRun),
        ("not_run", Success, NotRun, Ready::Start),
        ("not_run", Failed, NotRun, Ready::NotRun),
        ("canceled", Canceled, NotRun, Ready::Start),
        ("canceled", Failed, Canceled, Ready::Start),
        ("[success, failed, not_run]", Failed, NotRun, Ready::Start),
        ("[success, failed, not_run]", Success, Success, Ready::Start),
        (
            "[success, failed, not_run]",
            Success,
            Canceled,
            Ready::NotRun,
        ),
        (
            "[success, failed, not_run]",
            Canceled,
            NotRun,
            Ready::NotRun,
        ),
        ("terminal", Canceled, NotRun, Ready::Start),
        ("terminal", Success, Failed, Ready::Start),
    ];
    for (statuses, a, b, want) in table {
        let plan = chain(statuses);
        assert_eq!(
            decide(&plan, Some(a), Some(b)),
            want,
            "{statuses} after a={a:?} b={b:?}"
        );
    }
}

#[test]
fn ready_waits_for_every_ancestor() {
    let plan = chain("terminal");
    assert_eq!(decide(&plan, None, None), Ready::Wait);
    assert_eq!(decide(&plan, Some(Outcome::Success), None), Ready::Wait);
    // B cannot end before A; if it did, C would still wait for A.
    assert_eq!(decide(&plan, None, Some(Outcome::Failed)), Ready::Wait);
    // A job that requires nothing starts at once.
    assert_eq!(ready(&plan, &plan.jobs[0], |_| None), Ready::Start);
}

#[test]
fn ready_reads_every_matrix_leg_and_each_entry_on_its_own() {
    use Outcome::{Failed, Success};

    let plan = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, parameters: { v: { type: integer } }, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: m, matrix: { v: [1, 2] } }
      - t: { name: other, v: 0 }
      - t: { name: last, v: 0, requires: [{ m: failed }, other] }
",
    );
    let last = plan.jobs.last().unwrap();
    assert_eq!(last.requires, ["m (1)", "m (2)", "other"]);
    let check = |legs: [Outcome; 3]| {
        let ended: BTreeMap<&str, Outcome> =
            ["m (1)", "m (2)", "other"].into_iter().zip(legs).collect();
        ready(&plan, last, |id| ended.get(id).copied())
    };
    // One failed leg makes the entry failed.
    assert_eq!(check([Success, Failed, Success]), Ready::Start);
    assert_eq!(check([Success, Success, Success]), Ready::NotRun);
    // `other` asks for success on its own.
    assert_eq!(check([Failed, Failed, Failed]), Ready::NotRun);
}

#[test]
fn an_entry_required_twice_meets_both_lists() {
    let plan = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: a }
      - t: { name: b, requires: [{ a: [failed, success] }, { a: [failed, canceled] }] }
",
    );
    assert_eq!(plan.jobs[1].requires, ["a"]);
    assert_eq!(plan.jobs[1].statuses["a"], Statuses::FAILED);
}

#[test]
fn exclude_leaves_out_matching_combinations() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t:
          matrix:
            os: [a, b]
            n: [1, 2, 3]
            exclude:
              - { os: a, n: 2 }
              - { n: 3 }
      - t: { name: after, requires: [t] }
";
    let plan = plan(yaml);
    let ids: Vec<_> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["t (1, a)", "t (1, b)", "t (2, b)", "after"]);
    assert_eq!(plan.jobs[3].requires, &ids[..3]);
}

/// A config whose one workflow runs `t` with `matrix`.
fn with_matrix(matrix: &str) -> String {
    format!(
        "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}\nworkflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {matrix} }} }}] }} }}\n"
    )
}

#[test]
fn exclude_must_name_matrix_keys_and_leave_a_combination() {
    assert_error(
        &with_matrix("{ os: [a], exclude: [{ arch: x }] }"),
        &["`t`", "`exclude` key `arch`", "not a key of the matrix"],
    );
    assert_error(
        &with_matrix("{ os: [a], exclude: [{}] }"),
        &["`t`", "empty matrix `exclude` entry"],
    );
    assert_error(
        &with_matrix("{ os: [a, b], exclude: [{ os: a }, { os: b }] }"),
        &["`t`", "leaves no combination"],
    );
    assert_error(
        &with_matrix("{ os: [a], exclude: [{ os: '${{ parameters.x }}' }] }"),
        &["`t`", "reads `parameters`"],
    );
    assert_eq!(
        errors(&with_matrix("{ os: [a], exclude: [{ os: b }] }")),
        Vec::new()
    );
    // An approval has no matrix, `exclude` included.
    assert_error(
        "workflows: { ci: { on: { push: {} }, jobs: [{ ok: { type: approval, matrix: { exclude: [{ a: 1 }] } } }] } }",
        &["approval `ok`", "cannot have a matrix"],
    );
}

#[test]
fn the_matrix_limit_counts_after_exclude() {
    let list = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let m = list(17);
    // 17x17 is 289; leaving out `a: 0` and `a: 1` leaves 255.
    assert_eq!(
        plan(&with_matrix(&format!(
            "{{ a: [{m}], b: [{m}], exclude: [{{ a: 0 }}, {{ a: 1 }}] }}"
        )))
        .jobs
        .len(),
        255
    );
    assert_error(
        &with_matrix(&format!("{{ a: [{m}], b: [{m}], exclude: [{{ a: 0 }}] }}")),
        &["`t`", "272 entries", "256"],
    );
    // Too many combinations to check, whatever `exclude` leaves.
    let big = list(65);
    assert_error(
        &with_matrix(&format!(
            "{{ a: [{big}], b: [{big}], exclude: [{{ a: 0 }}] }}"
        )),
        &["`t`", "4225 combinations before `exclude`", "4096"],
    );
    let many: Vec<String> = (0..257).map(|i| format!("{{ a: {i} }}")).collect();
    assert_error(
        &with_matrix(&format!("{{ a: [{m}], exclude: [{}] }}", many.join(", "))),
        &["`t`", "257 matrix `exclude` entries", "256"],
    );
}

#[test]
fn exclude_round_trips() {
    let config = parse(&with_matrix("{ os: [a, b], exclude: [{ os: a }] }")).unwrap();
    let entry = &config.workflows["ci"].jobs[0];
    assert_eq!(entry.matrix["os"].len(), 2);
    assert_eq!(entry.matrix.exclude.len(), 1);
    let yaml = hatchrun_workflow::to_yaml(&config).unwrap();
    assert!(yaml.contains("exclude"), "{yaml}");
    assert_eq!(parse(&yaml).unwrap(), config);
}

const PER_LEG: &str = "
jobs:
  t:
    runs-on: ${{ parameters.os }}
    parameters: { os: { type: string } }
    steps: [{ run: 'echo ${{ parameters.os }}' }]
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { matrix: { os: [ubuntu-latest, ubuntu-26.04-16core-arm] } }
";

#[test]
fn runs_on_a_parameter_picks_each_legs_template() {
    let plan = plan(PER_LEG);
    let picked: Vec<_> = plan
        .jobs
        .iter()
        .map(|j| match j.kind {
            JobKind::Run { template } => (j.id.as_str(), template),
            JobKind::Approval => panic!("{j:?}"),
        })
        .collect();
    assert_eq!(
        picked,
        [
            ("t (ubuntu-latest)", template("ubuntu-latest").unwrap()),
            (
                "t (ubuntu-26.04-16core-arm)",
                template("ubuntu-26.04-16core-arm").unwrap()
            ),
        ]
    );
    assert_eq!(runs(&plan.jobs[1]), ["echo ubuntu-26.04-16core-arm"]);
}

#[test]
fn runs_on_a_parameter_must_name_templates() {
    assert_error(
        &PER_LEG.replace("ubuntu-26.04-16core-arm]", "macos-latest]"),
        &[
            "job `t`",
            "`runs-on: ${{ parameters.os }}`",
            "`macos-latest` for `t (macos-latest)`",
            "names no runner template",
        ],
    );
    assert_error(
        &PER_LEG.replace("parameters: { os: { type: string } }", "parameters: {}"),
        &["job `t`", "declares no parameter `os`"],
    );
    assert_error(
        &PER_LEG.replace("{ type: string }", "{ type: integer }"),
        &["job `t`", "parameter `os`", "string or an enum"],
    );
    assert_error(
        &PER_LEG.replace("{ type: string }", "{ type: string, default: macos-14 }"),
        &["job `t` parameter `os`", "`runs-on: macos-14`"],
    );
    assert_error(
        &PER_LEG.replace(
            "{ type: string }",
            "{ type: enum, enum: [ubuntu-latest, windows-latest] }",
        ),
        &["job `t` parameter `os`", "`runs-on: windows-latest`"],
    );
    // With a default, an entry without a matrix runs on it.
    let yaml = PER_LEG
        .replace(
            "{ type: string }",
            "{ type: string, default: ubuntu-26.04-arm }",
        )
        .replace(
            "- t: { matrix: { os: [ubuntu-latest, ubuntu-26.04-16core-arm] } }",
            "- t",
        );
    assert_eq!(
        plan(&yaml).jobs[0].kind,
        JobKind::Run {
            template: template("ubuntu-26.04-arm").unwrap()
        }
    );
}

/// A config with workflow `ci` and workflow `deploy`, whose `on:` is
/// `on`.
fn chained(on: &str) -> String {
    format!(
        "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [t]
  deploy:
    on: {on}
    jobs: [t]
"
    )
}

#[test]
fn workflow_run_names_another_workflow_of_the_config() {
    assert_eq!(
        errors(&chained("{ workflow_run: { workflows: [ci] } }")),
        []
    );
    assert_error(
        &chained("{ workflow_run: { workflows: [ci, nosuch] } }"),
        &[
            "workflow `deploy`",
            "`on.workflow_run` names workflow `nosuch`",
            "not defined",
        ],
    );
    assert_error(
        &chained("{ workflow_run: { workflows: [deploy] } }"),
        &["workflow `deploy`", "names the workflow itself"],
    );
    assert_error(
        &chained("{ workflow_run: { workflows: [] } }"),
        &["`on.workflow_run` names no workflow"],
    );
    assert_error(
        &chained("{ workflow_run: { workflows: [ci], types: [bogus] } }"),
        &[
            "`on.workflow_run` type `bogus` is not supported",
            "completed",
        ],
    );
    assert_error(
        &chained("{ workflow_run: { workflows: [ci], branches: [a], branches-ignore: [b] } }"),
        &["`on.workflow_run` has both `branches` and `branches-ignore`"],
    );
}

#[test]
fn release_types_and_tags_are_checked() {
    assert_eq!(
        errors(&chained(
            "{ release: { types: [published, unpublished, created, edited, deleted, released, prereleased] } }"
        )),
        []
    );
    assert_error(
        &chained("{ release: { types: [published, bogus] } }"),
        &["`on.release` type `bogus` is not supported"],
    );
    assert_error(
        &chained("{ release: { types: [] } }"),
        &["`on.release.types` is empty"],
    );
    assert_error(
        &chained("{ release: { tags: [a], tags-ignore: [b] } }"),
        &["`on.release` has both `tags` and `tags-ignore`"],
    );
}

#[test]
fn checkout_steps_are_planned_and_checked() {
    let config = "
jobs:
  t:
    runs-on: ubuntu-latest
    parameters: { at: { type: string, default: v1 } }
    steps:
      - checkout
      - checkout: { ref: \"${{ parameters.at }}\", fetch-depth: 0, path: \"src/${{ parameters.at }}\" }
workflows:
  ci:
    on: { push: {} }
    jobs: [t]
";
    assert_eq!(errors(config), []);
    let plan = plan(config);
    let steps = &plan.jobs[0].steps;
    assert_eq!(steps[0], PlannedStep::Checkout(CheckoutStep::default()));
    assert_eq!(
        steps[1],
        PlannedStep::Checkout(CheckoutStep {
            r#ref: Some("v1".into()),
            fetch_depth: 0,
            path: Some("src/v1".into()),
            ..CheckoutStep::default()
        })
    );
    for (step, words) in [
        (
            "{ checkout: { ref: \"${{ nosuch.x }}\" } }",
            &["step 1 `checkout.ref`", "unknown context 'nosuch'"][..],
        ),
        (
            "{ checkout: { path: /etc } }",
            &[
                "step 1 `checkout.path` is `/etc`",
                "inside the working directory",
            ],
        ),
        (
            "{ checkout: { path: a/../../b } }",
            &["`checkout.path`", "inside the working directory"],
        ),
    ] {
        assert_error(
            &one_job(&format!("    runs-on: ubuntu-latest\n    steps: [{step}]")),
            words,
        );
    }
}

#[test]
fn a_command_cannot_be_named_checkout() {
    assert_error(
        "commands: { checkout: { steps: [] } }",
        &["command `checkout` has the name of a step key"],
    );
}

#[test]
fn the_hatchrun_context_is_known() {
    let ok = one_job(
        "    runs-on: ubuntu-latest\n    if: hatchrun.event_name == 'push'\n    steps: [{ run: \"echo ${{ hatchrun.ref_name }} ${{ hatchrun.sha }}\" }]",
    );
    assert_eq!(errors(&ok), []);
}

#[test]
fn planned_jobs_stored_without_statuses_still_load() {
    let yaml = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { name: a }
      - t: { name: b, requires: [a] }
";
    let mut job = plan(yaml).jobs.pop().unwrap();
    let mut stored = serde_json::to_value(&job).unwrap();
    // A graph saved before `statuses` existed has no such key; an empty
    // map asks every required entry for success, as before.
    stored.as_object_mut().unwrap().remove("statuses");
    let loaded: PlannedJob = serde_json::from_value(stored).unwrap();
    assert!(loaded.statuses.is_empty());
    job.statuses.clear();
    assert_eq!(loaded, job);
}

const STRATEGY_JOBS: &str = "
jobs:
  t: { runs-on: ubuntu-latest, continue-on-error: '${{ matrix.experimental }}', steps: [] }
  u: { runs-on: ubuntu-latest, continue-on-error: true, steps: [] }
  v: { runs-on: ubuntu-latest, continue-on-error: false, steps: [] }
";

#[test]
fn matrix_jobs_get_a_strategy_numbered_in_plan_order() {
    let p = plan(&format!(
        "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - t:
          matrix: {{ os: [a, b, c] }}
          strategy: {{ fail-fast: false, max-parallel: 2 }}
      - u
"
    ));
    assert_eq!(p.jobs.len(), 4);
    for (i, job) in p.jobs[..3].iter().enumerate() {
        let s = job.strategy.as_ref().unwrap();
        assert_eq!(s.group, "t");
        assert!(!s.fail_fast);
        assert_eq!(s.max_parallel, Some(2));
        assert_eq!((s.job_index, s.job_total), (u32::try_from(i).unwrap(), 3));
    }
    assert_eq!(p.jobs[3].strategy, None);
    let ctx = p.jobs[1].strategy_context();
    assert_eq!(ctx["job-index"], 1);
    assert_eq!(ctx["job-total"], 3);
    assert_eq!(ctx["max-parallel"], 2);
    assert_eq!(ctx["fail-fast"], false);
    let alone = p.jobs[3].strategy_context();
    assert_eq!(alone["fail-fast"], true);
    assert_eq!(alone["job-index"], 0);
    assert_eq!(alone["job-total"], 1);
}

#[test]
fn a_matrix_defaults_to_fail_fast_with_no_limit() {
    let p = plan(&format!(
        "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - t: {{ matrix: {{ experimental: [true, false] }} }}
"
    ));
    let s = p.jobs[0].strategy.as_ref().unwrap();
    assert!(s.fail_fast);
    assert_eq!(s.max_parallel, None);
    assert_eq!(p.jobs[0].strategy_context()["max-parallel"], 2);
}

#[test]
fn entries_of_one_group_share_numbers() {
    let p = plan(&format!(
        "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - t: {{ name: a, matrix: {{ x: [1, 2] }}, strategy: {{ group: g }} }}
      - t: {{ name: b, matrix: {{ x: [3] }}, strategy: {{ group: g }} }}
"
    ));
    let seen: Vec<(&str, u32, u32)> = p
        .jobs
        .iter()
        .map(|j| {
            let s = j.strategy.as_ref().unwrap();
            (s.group.as_str(), s.job_index, s.job_total)
        })
        .collect();
    assert_eq!(seen, [("g", 0, 3), ("g", 1, 3), ("g", 2, 3)]);
}

#[test]
fn a_job_without_matrix_or_strategy_has_none() {
    let p = plan(&format!(
        "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [u]
"
    ));
    assert_eq!(p.jobs[0].strategy, None);
}

#[test]
fn continue_on_error_reaches_the_plan() {
    let p = plan(&format!(
        "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [t, u, v]
"
    ));
    assert_eq!(
        p.jobs[0].continue_on_error.as_deref(),
        Some("${{ matrix.experimental }}")
    );
    assert_eq!(p.jobs[1].continue_on_error.as_deref(), Some("true"));
    assert_eq!(p.jobs[2].continue_on_error, None);
}

#[test]
fn continue_on_error_substitutes_parameters() {
    let p = plan(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    parameters: { soft: { type: boolean, default: true } }
    continue-on-error: ${{ parameters.soft }}
    steps: []
workflows:
  ci: { on: { push: {} }, jobs: [t] }
",
    );
    assert_eq!(p.jobs[0].continue_on_error.as_deref(), Some("true"));
}

#[test]
fn rejects_a_bad_strategy() {
    let wf = |entries: &str| {
        format!(
            "{STRATEGY_JOBS}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: {entries}
"
        )
    };
    assert_error(
        &wf("[{ t: { matrix: { x: [1] }, strategy: { max-parallel: 0 } } }]"),
        &["max-parallel: 0", "1 or more"],
    );
    assert_error(
        &wf("[{ t: { matrix: { x: [1] }, strategy: { max-parallel: 2147483648 } } }]"),
        &["max-parallel", "2147483647"],
    );
    assert_error(
        &wf("[{ t: { matrix: { x: [1] }, strategy: { group: '' } } }]"),
        &["empty", "strategy.group"],
    );
    assert_error(
        &wf(
            "[{ t: { name: a, strategy: { group: g } } }, { t: { name: b, strategy: { group: g, fail-fast: false } } }]",
        ),
        &["`a` and `b`", "group `g`", "differ"],
    );
    assert_error(
        &wf("[{ hold: { type: approval, strategy: { fail-fast: false } } }]"),
        &["approval `hold`", "strategy"],
    );
}

#[test]
fn rejects_a_continue_on_error_that_reads_an_unknown_context() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, continue-on-error: '${{ nosuch.x }}', steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["continue-on-error", "unknown context 'nosuch'"],
    );
}

#[test]
fn a_job_parameter_cannot_be_named_strategy() {
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, parameters: { strategy: { type: string } }, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["parameter `strategy`"],
    );
}

fn expand(
    job: &PlannedJob,
    ctx: &serde_json::Value,
) -> Result<Vec<hatchrun_workflow::Leg>, String> {
    hatchrun_workflow::expand(job, ctx, hatchrun_workflow::Siblings::default())
}

const DYNAMIC: &str = "
jobs:
  setup: { runs-on: ubuntu-latest, outputs: { m: x }, steps: [] }
  test: { runs-on: '${{ matrix.os }}', steps: [] }
  after: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - setup
      - test:
          requires: [setup]
          strategy: { fail-fast: false, max-parallel: 2 }
          matrix:
            node: [18, 20]
            os: ${{ fromJSON(needs.setup.outputs.os) }}
            include: ${{ fromJSON(needs.setup.outputs.extra) }}
            exclude: [{ node: 18, os: ubuntu-latest }]
      - after: { requires: [test] }
";

fn ctx(os: &str, extra: &str) -> serde_json::Value {
    serde_json::json!({
        "needs": { "setup": { "result": "success", "outputs": { "os": os, "extra": extra } } },
        "github": {}, "inputs": {}, "vars": {}, "hatchrun": {},
    })
}

fn deferred_test() -> PlannedJob {
    let p = plan(DYNAMIC);
    assert_eq!(p.jobs.len(), 3, "one stand-in job for the matrix");
    let job = p.jobs[1].clone();
    assert!(job.deferred.is_some());
    assert_eq!(p.jobs[2].requires, std::slice::from_ref(&job.id));
    job
}

#[test]
fn a_dynamic_matrix_plans_one_stand_in_job() {
    let job = deferred_test();
    assert_eq!(job.id, "test");
    let strategy = job.strategy.unwrap();
    assert!(!strategy.fail_fast);
    assert_eq!(strategy.max_parallel, Some(2));
}

#[test]
fn a_dynamic_matrix_expands_as_on_github() {
    let job = deferred_test();
    let legs = expand(
        &job,
        &ctx(
            r#"["ubuntu-latest","ubuntu-26.04-arm"]"#,
            r#"[{"os":"ubuntu-latest","experimental":true},{"os":"windows","node":20}]"#,
        ),
    )
    .unwrap();
    let ids: Vec<&str> = legs.iter().map(|l| l.job.id.as_str()).collect();
    // `exclude` takes out node 18 on ubuntu-latest. The first `include`
    // extends the combination it matches; the second matches none.
    assert_eq!(
        ids,
        [
            "test (18, ubuntu-26.04-arm)",
            "test (20, ubuntu-latest)",
            "test (20, ubuntu-26.04-arm)",
            "test (20, windows)",
        ]
    );
    assert_eq!(legs[1].job.matrix["experimental"], true);
    assert_eq!(legs[0].job.strategy.as_ref().unwrap().job_total, 4);
    for leg in &legs[..3] {
        assert_eq!(leg.error, None);
        assert!(leg.job.deferred.is_none());
    }
    let JobKind::Run { template } = legs[0].job.kind else {
        panic!("not a run job");
    };
    assert_eq!(template.labels[0], "ubuntu-26.04-arm");
}

#[test]
fn an_unknown_label_fails_that_job_alone() {
    let job = deferred_test();
    let legs = expand(
        &job,
        &ctx(r#"["ubuntu-latest"]"#, r#"[{"os":"windows","node":20}]"#),
    )
    .unwrap();
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0].error, None);
    let message = legs[1].error.as_deref().unwrap();
    assert!(message.contains("`test (20, windows)`"), "{message}");
    assert!(message.contains("evaluated to a label that names no runner template"));
    assert!(!message.contains("`windows`"), "the value is not echoed");
    // A rerun reads the label again.
    assert!(legs[1].job.deferred.is_some());
}

#[test]
fn a_matrix_that_cannot_be_read_fails_the_job() {
    let job = deferred_test();
    for (os, extra, words) in [
        ("not json", "[]", "fromJSON"),
        (r#"{"a":1}"#, "[]", "must be a list"),
        ("[]", "[]", "no values"),
        (r#"["ubuntu-latest"]"#, "[1]", "must be maps"),
    ] {
        let err = expand(&job, &ctx(os, extra)).unwrap_err();
        assert!(err.contains(words), "{err}");
    }
    let many: Vec<u32> = (0..300).collect();
    let os = serde_json::to_string(&many).unwrap();
    let err = expand(&job, &ctx(&os, "[]")).unwrap_err();
    assert!(err.contains("limit is 256"), "{err}");
}

#[test]
fn a_whole_matrix_expression_and_a_runs_on_expression() {
    let p = plan(
        "
jobs:
  t: { runs-on: \"${{ inputs.runner || 'ubuntu-latest' }}\", parallelism: 2, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { matrix: '${{ fromJSON(needs.s.outputs.m) }}' }
",
    );
    let c = serde_json::json!({
        "needs": { "s": { "outputs": { "m": r#"{"k":["a","b"],"include":[{"k":"c"}]}"# } } },
        "github": {}, "inputs": { "runner": "ubuntu-26.04-arm" }, "vars": {}, "hatchrun": {},
    });
    let legs = expand(&p.jobs[0], &c).unwrap();
    let ids: Vec<&str> = legs.iter().map(|l| l.job.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "t (a) [1/2]",
            "t (a) [2/2]",
            "t (b) [1/2]",
            "t (b) [2/2]",
            "t (c) [1/2]",
            "t (c) [2/2]"
        ]
    );
    assert_eq!(legs[0].job.node, Some((0, 2)));
}

#[test]
fn a_runs_on_expression_alone_resolves_each_planned_job() {
    let p = plan(
        "
jobs:
  t: { runs-on: '${{ matrix.os }}', steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { matrix: { os: [ubuntu-latest, ubuntu-26.04-arm] } }
",
    );
    assert_eq!(p.jobs.len(), 2);
    let legs = expand(&p.jobs[1], &serde_json::json!({})).unwrap();
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].job.id, "t (ubuntu-26.04-arm)");
    let JobKind::Run { template } = legs[0].job.kind else {
        panic!();
    };
    assert_eq!(template.labels[0], "ubuntu-26.04-arm");
}

#[test]
fn dynamic_matrix_and_runs_on_are_validated() {
    assert_error(
        "
jobs: { t: { runs-on: '${{ secrets.X }}', steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`runs-on` reads `secrets`"],
    );
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { matrix: '${{ matrix.x }}' } }]
",
        &["`matrix` reads `matrix`"],
    );
}

#[test]
fn a_static_matrix_takes_include_and_exclude_as_on_github() {
    let p = plan(
        "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t:
          matrix:
            node: [18, 20]
            os: [a, b]
            exclude: [{ node: 18, os: a }]
            include:
              - { os: b, extra: yes }
              - { node: 22, os: c }
",
    );
    let ids: Vec<&str> = p.jobs.iter().map(|j| j.id.as_str()).collect();
    // `extra` joins the entries of os b, and shows in no name.
    assert_eq!(ids, ["t (18, b)", "t (20, a)", "t (20, b)", "t (22, c)"]);
    assert_eq!(p.jobs[0].matrix["extra"], "yes");
    assert_eq!(p.jobs[2].matrix["extra"], "yes");
    assert!(!p.jobs[1].matrix.contains_key("extra"));
    assert_eq!(p.jobs[3].strategy.as_ref().unwrap().job_total, 4);
}

#[test]
fn a_matrix_of_include_only_runs_each_entry() {
    let p = plan(
        "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { matrix: { include: [{ os: a }, { os: b }] } } }]
",
    );
    assert_eq!(p.jobs.len(), 2);
}

#[test]
fn include_matches_with_the_equality_exclude_uses() {
    let job = deferred_test();
    // `20` and `"20"` are the same text, as in `exclude`.
    let legs = expand(
        &job,
        &ctx(
            r#"["ubuntu-latest"]"#,
            r#"[{"os":"ubuntu-latest","node":"20","tag":"x"}]"#,
        ),
    )
    .unwrap();
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].job.matrix["tag"], "x");
}

#[test]
fn an_empty_matrix_makes_no_job_and_a_big_run_is_refused() {
    let job = deferred_test();
    let c = |m: &str| {
        serde_json::json!({
            "needs": { "setup": { "outputs": { "m": m } } },
            "github": {}, "inputs": {}, "vars": {}, "hatchrun": {},
        })
    };
    let p = plan(
        "
jobs:
  t: { runs-on: ubuntu-latest, parallelism: 64, steps: [{ run: x }] }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { matrix: '${{ fromJSON(needs.setup.outputs.m) }}' } }]
",
    );
    for empty in ["[]", "{}"] {
        assert!(expand(&p.jobs[0], &c(empty)).unwrap().is_empty(), "{empty}");
    }
    let err = expand(&p.jobs[0], &c(r#"{"k":[1,2,3,4,5]}"#)).unwrap_err();
    assert!(err.contains("limit is 256"), "{err}");
    let _ = job;
}

#[test]
fn a_matrix_is_numbered_and_limited_with_the_jobs_around_it() {
    let job = deferred_test();
    let c = ctx(r#"["ubuntu-latest","ubuntu-26.04-arm"]"#, "[]");
    let siblings = hatchrun_workflow::Siblings {
        jobs: 5,
        group_before: 2,
        group_others: 3,
    };
    let legs = hatchrun_workflow::expand(&job, &c, siblings).unwrap();
    // The exclude leaves three of four entries.
    let numbers: Vec<(u32, u32)> = legs
        .iter()
        .map(|l| {
            let s = l.job.strategy.as_ref().unwrap();
            (s.job_index, s.job_total)
        })
        .collect();
    assert_eq!(numbers, [(2, 6), (3, 6), (4, 6)]);
    let crowded = hatchrun_workflow::Siblings {
        jobs: 254,
        ..Default::default()
    };
    let err = hatchrun_workflow::expand(&job, &c, crowded).unwrap_err();
    assert!(err.contains("limit is 256"), "{err}");
}

#[test]
fn matrix_and_runs_on_read_only_the_vars_the_host_gives() {
    let p = plan(
        "
jobs:
  t: { runs-on: \"${{ vars.RUNNER }}\", steps: [] }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { matrix: { k: '${{ fromJSON(vars.LIST) }}' } } }]
",
    );
    // An environment-only var is not in what the host passes.
    let c = serde_json::json!({
        "needs": {}, "github": {}, "inputs": {}, "hatchrun": {},
        "vars": { "LIST": "[1]" },
    });
    let legs = expand(&p.jobs[0], &c).unwrap();
    let message = legs[0].error.as_deref().unwrap();
    assert!(message.contains("evaluated to a label"), "{message}");
}

#[test]
fn container_env_names_are_substituted_and_checked() {
    let p = plan(
        "
jobs:
  t:
    parameters: { n: { type: string, default: A } }
    runs-on: ubuntu-latest
    container: { image: a, env: { 'K_${{ parameters.n }}': v } }
    steps: []
workflows: { ci: { on: { push: {} }, jobs: [{ t: { n: B } }] } }
",
    );
    let env = &p.jobs[0].container.as_ref().unwrap().env;
    assert_eq!(env.keys().collect::<Vec<_>>(), ["K_B"]);
    assert_error(
        "
jobs: { t: { runs-on: ubuntu-latest, container: { image: a, env: { '${{ nosuch.x }}': v } }, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`container`", "unknown context"],
    );
}

#[test]
fn the_job_container_cannot_take_a_services_alias() {
    assert_error(
        "
jobs:
  t:
    runs-on: ubuntu-latest
    container: { image: a, options: '--network-alias db' }
    services: { db: { image: b } }
    steps: []
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
        &["`container`", "--network-alias db", "id of a service"],
    );
}

#[test]
fn a_stored_plan_without_services_loads() {
    let p = plan(
        "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows: { ci: { on: { push: {} }, jobs: [t] } }
",
    );
    let mut json = serde_json::to_value(&p).unwrap();
    json["jobs"][0].as_object_mut().unwrap().remove("services");
    let back: Plan = serde_json::from_value(json).unwrap();
    assert!(back.jobs[0].services.is_empty());
}

/// A config with one workflow whose `on` is `on`.
fn on_yaml(on: &str) -> String {
    format!(
        "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}\nworkflows:\n  ci:\n    on: {on}\n    jobs: [t]\n"
    )
}

#[test]
fn issues_and_issue_comment_types_are_checked() {
    assert_eq!(errors(&on_yaml("{ issues: {}, issue_comment: {} }")), []);
    assert_eq!(
        errors(&on_yaml(
            "{ issues: { types: [opened, field_added] }, issue_comment: { types: created } }"
        )),
        []
    );
    assert_error(
        &on_yaml("{ issues: { types: [reopen] } }"),
        &["on.issues", "`reopen`", "not supported"],
    );
    assert_error(
        &on_yaml("{ issue_comment: { types: [] } }"),
        &["on.issue_comment.types", "empty"],
    );
    assert_error(
        &on_yaml("{ issue_comment: { types: [created, pinned] } }"),
        &["on.issue_comment", "`pinned`"],
    );
}

#[test]
fn command_names_options_and_shorthands_validate() {
    for on in [
        "{ command: retest }",
        "{ command: [retest, deploy] }",
        "{ command: { retest: {}, deploy: { permission: maintain, in: [pull_request, issue], sha: optional, args: [environment], reaction: rocket } } }",
        "{ command: { retest: } }",
        "{ command: { a_b-9: {} } }",
        "{ command: { issue-only: { in: [issue], sha: auto } } }",
        "{ command: { both: { in: [pull_request, issue], sha: required } } }",
    ] {
        assert_eq!(errors(&on_yaml(on)), [], "{on}");
    }
}

#[test]
fn command_errors_name_the_command_and_the_fix() {
    assert_error(
        &on_yaml("{ command: approve-run }"),
        &["on.command.approve-run", "reserved"],
    );
    assert_error(
        &on_yaml("{ command: { approve-run: {} } }"),
        &["approve-run", "reserved"],
    );
    for bad in [
        "Retest",
        "re test",
        "re/test",
        "r\u{e9}test",
        &"x".repeat(33),
        "\"\"",
    ] {
        assert_error(
            &on_yaml(&format!("{{ command: {{ {bad}: {{}} }} }}")),
            &["on.command", "lowercase letters, digits"],
        );
    }
    assert_eq!(
        errors(&on_yaml(&format!("{{ command: {} }}", "x".repeat(32)))),
        []
    );
    assert_error(
        &on_yaml("{ command: { look: { sha: required, in: [issue] } } }"),
        &["on.command.look", "sha: required", "pull_request"],
    );
    assert_error(
        &on_yaml("{ command: { look: { in: [] } } }"),
        &["on.command.look", "`in` is empty"],
    );
    assert_error(
        &on_yaml("{ command: [] }"),
        &["on.command", "names no command"],
    );
    assert_error(
        &on_yaml("{ command: { a: { args: [x, x] } } }"),
        &["argument name `x`", "twice"],
    );
    assert_error(
        &on_yaml("{ command: { a: { args: ['x y'] } } }"),
        &["argument name `x y`"],
    );
}

#[test]
fn command_options_that_do_not_exist_do_not_parse() {
    for on in [
        "{ command: { a: { permission: owner } } }",
        "{ command: { a: { in: [discussion] } } }",
        "{ command: { a: { sha: maybe } } }",
        "{ command: { a: { reaction: tada } } }",
        "{ command: { a: { color: red } } }",
        "{ command: 3 }",
        "{ command: [a, a] }",
    ] {
        assert!(parse(&on_yaml(on)).is_err(), "{on}");
    }
}
