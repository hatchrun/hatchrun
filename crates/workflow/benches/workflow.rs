// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Benchmarks for the calls the control plane makes on every webhook
//! (`parse`, `validate`, `compile`, `triggered`, `evaluate`,
//! `interpolate`) and for `convert`, which the CLI runs.
//!
//! Inputs are the CLI's end-to-end fixtures, a large generated workflow
//! shaped like the slowest real file seen (52 jobs, about 400 KB), and
//! hostile inputs that push each budget and limit.
//!
//! Run with `cargo bench -p hatchrun-workflow --bench workflow`.

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use hatchrun_workflow::{
    Config, Event, ExprError, Filter, Functions, JobStatus, compile, convert, evaluate,
    interpolate, parse, to_yaml, triggered, validate,
};
use serde_json::{Value, json};

struct Funcs;

impl Functions for Funcs {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Ok("0123456789abcdef".to_owned())
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

/// The e2e scenarios: each is the list of GitHub workflow files in one
/// fixture repository.
fn fixtures() -> Vec<(String, Vec<(String, String)>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/tests/fixtures/e2e");
    let mut scenarios = Vec::new();
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("workflows").is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let mut files: Vec<_> = std::fs::read_dir(dir.join("workflows"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        let files = files
            .into_iter()
            .map(|p| {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read_to_string(&p).unwrap())
            })
            .collect();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        scenarios.push((name, files));
    }
    scenarios
}

/// A GitHub workflow shaped like deno's `ci.generated.yml`: one gate job
/// and `jobs` jobs that need it, each with a two-value matrix and 20
/// steps whose `if:`, `run:` and `with:` read contexts.
fn large_workflow(jobs: usize) -> String {
    let mut y = String::from(
        "name: ci\non:\n  push:\n    branches: [main]\n    tags: ['*']\n  pull_request:\n    \
         types: [opened, reopened, synchronize, ready_for_review]\njobs:\n  pre_build:\n    \
         runs-on: ubuntu-latest\n    outputs:\n      skip_build: '${{ steps.check.outputs.skip_build }}'\n    \
         steps:\n      - id: check\n        if: github.event_name == 'pull_request'\n        \
         run: echo skip_build=false >> $GITHUB_OUTPUT\n",
    );
    for j in 0..jobs {
        write!(
            y,
            "  test_{j}:\n    name: test {j} ${{{{ matrix.profile }}}}\n    needs: [pre_build]\n    \
             if: needs.pre_build.outputs.skip_build != 'true'\n    runs-on: ubuntu-24.04\n    \
             timeout-minutes: 240\n    env:\n      CARGO_TERM_COLOR: always\n      \
             RUST_BACKTRACE: full\n      SHARD: '{j}'\n    strategy:\n      matrix:\n        \
             profile: [debug, release]\n    steps:\n"
        )
        .unwrap();
        for s in 0..20 {
            if s % 5 == 0 {
                write!(
                    y,
                    "      - name: Cache {s}\n        uses: actions/cache@v4\n        \
                     if: '!startsWith(github.ref, ''refs/tags/'')'\n        with:\n          \
                     path: target\n          key: '${{{{ runner.os }}}}-{j}-{s}-${{{{ matrix.profile }}}}-${{{{ hashFiles(''Cargo.lock'') }}}}'\n"
                )
                .unwrap();
            } else {
                write!(
                    y,
                    "      - name: Step {s} of job {j}\n        \
                     if: '(contains(github.event.pull_request.labels.*.name, ''ci-full'') || github.ref == ''refs/heads/main'') && !startsWith(github.ref, ''refs/tags/'') && matrix.profile == ''release'''\n        \
                     run: |-\n          echo base ${{{{ github.event.pull_request.base.sha }}}} step {s}\n          \
                     cargo test --profile ${{{{ matrix.profile }}}} -p crate_{j}_{s} -- --test-threads 4\n          \
                     echo done ${{{{ github.sha }}}} >> $GITHUB_STEP_SUMMARY\n"
                )
                .unwrap();
            }
        }
    }
    y
}

/// A `github` context shaped like a real pull request webhook, about
/// 25 KB of JSON.
fn github_context(event_padding: usize) -> Value {
    let repo: serde_json::Map<String, Value> = (0..80)
        .map(|i| {
            (
                format!("field_{i}"),
                json!(format!("value {i} {}", "x".repeat(40))),
            )
        })
        .collect();
    let labels: Vec<Value> = ["bug", "ci-full", "ci-bench", "needs-review", "size/L"]
        .iter()
        .map(|n| json!({"id": 1, "name": n, "color": "ededed", "description": "a label"}))
        .collect();
    let commits: Vec<Value> = (0..60)
        .map(|i| json!({"sha": format!("{i:040x}"), "message": format!("commit {i} {}", "y".repeat(80))}))
        .collect();
    json!({
        "event_name": "pull_request",
        "ref": "refs/pull/42/merge",
        "sha": "9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d",
        "repository": "acme/widgets",
        "event": {
            "action": "synchronize",
            "n": 0,
            "number": 42,
            "padding": "p".repeat(event_padding),
            "pull_request": {
                "number": 42,
                "draft": false,
                "title": "Add a login form",
                "labels": labels,
                "base": {"ref": "main", "sha": "7d2f9c1e4b6a8d0f3e5c7a9b1d3f5e7a9c1b3d5f", "repo": repo.clone()},
                "head": {"ref": "feature/login", "sha": "9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d", "repo": repo.clone()},
            },
            "repository": repo,
            "commits": commits,
        }
    })
}

fn context(github: &Value) -> Value {
    json!({
        "github": github,
        "matrix": {"profile": "release", "os": "ubuntu-latest"},
        "needs": {"pre_build": {"result": "success", "outputs": {"skip_build": "false"}}},
        "env": {"CARGO_TERM_COLOR": "always"},
        "steps": {}, "inputs": {}, "vars": {}, "secrets": {},
        "runner": {"os": "Linux", "arch": "X64"},
    })
}

fn bench_convert(c: &mut Criterion) {
    let mut g = c.benchmark_group("convert");
    for (name, files) in fixtures() {
        g.bench_function(format!("e2e/{name}"), |b| {
            b.iter(|| convert(black_box(&files)).unwrap());
        });
    }
    let large = vec![("ci.yml".to_owned(), large_workflow(52))];
    g.bench_function("large/52-jobs", |b| {
        b.iter(|| convert(black_box(&large)).unwrap());
    });
    g.finish();
}

/// The converted configs, as YAML, with the e2e scenarios first and the
/// large workflow last.
fn configs() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = fixtures()
        .into_iter()
        .map(|(name, files)| {
            (
                format!("e2e/{name}"),
                to_yaml(&convert(&files).unwrap().config),
            )
        })
        .collect();
    let large = vec![("ci.yml".to_owned(), large_workflow(52))];
    out.push((
        "large/52-jobs".to_owned(),
        to_yaml(&convert(&large).unwrap().config),
    ));
    out
}

fn bench_parse_validate_compile(c: &mut Criterion) {
    let configs = configs();
    let mut g = c.benchmark_group("parse");
    for (name, yaml) in &configs {
        g.bench_function(name, |b| b.iter(|| parse(black_box(yaml)).unwrap()));
    }
    g.finish();

    let parsed: Vec<(String, Config)> = configs
        .iter()
        .map(|(n, y)| (n.clone(), parse(y).unwrap()))
        .collect();
    let mut g = c.benchmark_group("validate");
    for (name, config) in &parsed {
        g.bench_function(name, |b| b.iter(|| validate(black_box(config))));
    }
    g.finish();

    let mut g = c.benchmark_group("compile");
    for (name, config) in &parsed {
        g.bench_function(name, |b| {
            b.iter(|| {
                for wf in config.workflows.keys() {
                    let _ = black_box(compile(config, wf, "refs/heads/main"));
                }
            });
        });
    }
    g.finish();
}

fn bench_triggers(c: &mut Criterion) {
    let mut g = c.benchmark_group("filter");
    let branches = Filter::new(
        [
            "main",
            "release/**",
            "!release/old-*",
            "v[0-9]+.[0-9]+",
            "feature/*",
        ]
        .map(String::from)
        .to_vec(),
    );
    let refs = [
        "main",
        "release/2026/q4",
        "release/old-1",
        "v12.3",
        "feature/login",
        "dependabot/npm_and_yarn/lodash-4.17.21",
    ];
    g.bench_function("branches/6-refs", |b| {
        b.iter(|| {
            refs.iter()
                .filter(|r| branches.matches(black_box(r)))
                .count()
        });
    });

    let paths = [
        "**.md",
        "docs/**",
        "!docs/api/**",
        "src/**/*.rs",
        "*.toml",
        "**/package.json",
    ];
    let on: hatchrun_workflow::Triggers = {
        let mut y = String::from("push:\n  branches: [main, 'release/**']\n  paths-ignore:\n");
        for p in paths {
            writeln!(y, "    - '{p}'").unwrap();
        }
        y.push_str("pull_request:\n  branches: [main]\n  paths:\n");
        for p in paths {
            writeln!(y, "    - '{p}'").unwrap();
        }
        let mut yaml = String::from("workflows:\n  ci:\n    on:\n");
        for l in y.lines() {
            writeln!(yaml, "      {l}").unwrap();
        }
        yaml.push_str("    jobs: []\n");
        parse(&yaml).unwrap().workflows["ci"].on.clone()
    };
    let changed: Vec<String> = (0..200)
        .map(|i| format!("packages/pkg{}/src/components/widget_{i}.tsx", i % 17))
        .collect();
    let push = Event::Push {
        r#ref: "refs/heads/release/2026".to_owned(),
        changed: changed.clone(),
    };
    let pr = Event::PullRequest {
        action: "synchronize".to_owned(),
        base_ref: "main".to_owned(),
        changed,
    };
    g.bench_function("triggered/push-200-paths", |b| {
        b.iter(|| triggered(black_box(&on), black_box(&push)));
    });
    g.bench_function("triggered/pr-200-paths", |b| {
        b.iter(|| triggered(black_box(&on), black_box(&pr)));
    });
    g.finish();
}

fn bench_expr(c: &mut Criterion) {
    let ctx = context(&github_context(0));
    let mut g = c.benchmark_group("evaluate");
    let exprs = [
        ("if/event-name", "github.event_name == 'pull_request'"),
        (
            "if/labels",
            "(contains(github.event.pull_request.labels.*.name, 'ci-full') || github.ref == 'refs/heads/main') && !startsWith(github.ref, 'refs/tags/') && matrix.profile == 'release'",
        ),
        (
            "if/needs",
            "needs.pre_build.outputs.skip_build != 'true' && success()",
        ),
        (
            "if/fork",
            "github.event.pull_request.head.repo.field_3 != github.event.repository.field_3",
        ),
    ];
    for (name, e) in exprs {
        g.bench_function(name, |b| {
            b.iter(|| evaluate(black_box(e), &ctx, &Funcs).unwrap());
        });
    }
    g.finish();

    let mut g = c.benchmark_group("interpolate");
    let run = "echo base ${{ github.event.pull_request.base.sha }} step 3\n\
               cargo test --profile ${{ matrix.profile }} -p crate_3 -- --test-threads 4\n\
               echo done ${{ github.sha }} >> $GITHUB_STEP_SUMMARY\n";
    g.bench_function("run/3-exprs", |b| {
        b.iter(|| interpolate(black_box(run), &ctx, &Funcs).unwrap());
    });
    g.finish();
}

/// `c0` calls `c1` nine times, down to `c3`, which has nine run steps,
/// under a 16x16 matrix: the compile budget stops it.
fn command_fanout() -> String {
    let mut y = String::from("commands:\n");
    for i in 0..3 {
        let l = vec![format!("c{}", i + 1); 9].join(", ");
        writeln!(y, "  c{i}: {{ steps: [{l}] }}").unwrap();
    }
    y += "  c3: { steps: [{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a},{run: a}] }\n";
    y += "jobs: { t: { runs-on: ubuntu-latest, parameters: { a: {type: string}, b: {type: string} }, steps: [c0] } }\n";
    let m = (0..16)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        y,
        "workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}] }} }} }}] }} }}"
    )
    .unwrap();
    y
}

/// A string parameter doubled through 20 nested commands: the 64 KiB
/// value cap stops it.
fn doubling() -> String {
    let n = 20;
    let mut y = String::from("commands:\n");
    for i in 0..n {
        writeln!(
            y,
            "  c{i}: {{ parameters: {{ x: {{ type: string }} }}, steps: [{{ c{}: {{ x: '${{{{ parameters.x }}}}${{{{ parameters.x }}}}' }} }}] }}",
            i + 1
        )
        .unwrap();
    }
    writeln!(
        y,
        "  c{n}: {{ parameters: {{ x: {{ type: string }} }}, steps: [{{ run: '${{{{ parameters.x }}}}' }}] }}"
    )
    .unwrap();
    y += "jobs: { t: { runs-on: ubuntu-latest, steps: [{ c0: { x: ab } }] } }\nworkflows: { ci: { on: { push: {} }, jobs: [t] } }\n";
    y
}

/// One 40 KB run step called 190 times under a 16x16 matrix: the plan
/// byte budget stops it.
fn big_step() -> String {
    let big = "x".repeat(40_000);
    let mut y = format!("commands:\n  b: {{ steps: [{{ run: {big} }}] }}\n");
    let l = vec!["b"; 190].join(", ");
    writeln!(y, "  f: {{ steps: [{l}] }}").unwrap();
    let m = (0..16)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        y,
        "jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [f] }} }}\nworkflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [{{ t: {{ matrix: {{ a: [{m}], b: [{m}] }} }} }}] }} }}"
    )
    .unwrap();
    y
}

/// A step with 1000 env entries, called 19 times by `f` and `f` ten
/// times by the job.
fn wide_env() -> String {
    let env = (0..1000)
        .map(|i| format!("{i:x}: ''"))
        .collect::<Vec<_>>()
        .join(", ");
    let l = vec!["b"; 19].join(", ");
    format!(
        "commands:\n  b: {{ steps: [{{ run: x, env: {{ {env} }} }}] }}\n  f: {{ steps: [{l}] }}\n  \
         g: {{ steps: [f, f, f, f, f, f, f, f, f, f] }}\njobs: {{ t: {{ runs-on: ubuntu-latest, steps: [g] }} }}\n\
         workflows: {{ ci: {{ on: {{ push: {{}} }}, jobs: [t] }} }}\n"
    )
}

/// `fromJSON` of 14 strings of 512 KiB built by nested `format`: the
/// expression memory budget stops it.
fn from_json_flood() -> String {
    let mut x = "'0,'".to_owned();
    for _ in 0..9 {
        x = format!("format('{{0}}{{0}}{{0}}{{0}}', {x})");
    }
    let x = format!("format('[{{0}}0]', {x})");
    let args = vec![format!("fromJSON({x})"); 14].join(", ");
    format!("format('a', {args})")
}

fn bench_hostile(c: &mut Criterion) {
    let mut g = c.benchmark_group("hostile");
    g.sample_size(10);
    for (name, yaml) in [
        ("compile/command-fanout", command_fanout()),
        ("compile/doubling", doubling()),
        ("compile/big-step", big_step()),
        ("compile/wide-env", wide_env()),
    ] {
        let config = parse(&yaml).unwrap();
        g.bench_function(name, |b| {
            b.iter(|| compile(black_box(&config), "ci", "refs/heads/main"));
        });
    }

    let flood = from_json_flood();
    g.bench_function("evaluate/fromjson-flood", |b| {
        b.iter(|| evaluate(black_box(&flood), &json!({}), &Funcs));
    });
    let deep = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    g.bench_function("evaluate/deep-parens", |b| {
        b.iter(|| evaluate(black_box(&deep), &json!({}), &Funcs).unwrap());
    });

    // A 1 MiB `github.event` read 300 times in one 3.9 KB expression.
    let ctx = context(&github_context(1 << 20));
    let reads = vec!["github.event"; 300].join(",");
    let e = format!("format('{{0}}',{reads})");
    g.bench_function("evaluate/1mib-event-300-reads", |b| {
        b.iter(|| evaluate(black_box(&e), &ctx, &Funcs).unwrap());
    });
    let reads = vec!["github.event.n"; 250].join(",");
    let e = format!("format('{{0}}',{reads})");
    g.bench_function("evaluate/1mib-event-250-leaf-reads", |b| {
        b.iter(|| evaluate(black_box(&e), &ctx, &Funcs).unwrap());
    });
    let labels = "contains(github.event.pull_request.labels.*.name, 'ci-full')";
    g.bench_function("evaluate/1mib-event-labels", |b| {
        b.iter(|| evaluate(black_box(labels), &ctx, &Funcs).unwrap());
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_convert,
    bench_parse_validate_compile,
    bench_triggers,
    bench_expr,
    bench_hostile
);
criterion_main!(benches);
