# Orchestrator Plan 1: Workflow Format and `hatchrun` CLI

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `hatchrun-workflow` crate that parses `.hatchrun/config.yml`,
evaluates `${{ }}` expressions, matches triggers and compiles a workflow
into a job plan, plus a separate `hatchrun` binary that validates and
converts configs and splits tests.

**Architecture:** `crates/workflow` is pure Rust with no I/O and no
clock, so the control plane (wasm32 Worker), the agent and the CLI all
link it and read a config the same way. `crates/cli` is a thin `clap`
binary on top. Everything later plans need from the format comes out
of `compile()` as a `Plan`.

**Tech Stack:** Rust 1.99 (edition 2024), `serde`, `serde-saphyr`
2.0.0 (YAML with locations), `serde_json` (expression values),
`globset` 0.4.20 (filters), `thiserror`, `clap` 4.

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`

## The plans that follow

The design covers five subsystems, each with its own plan, in this
order. Each one ships and tests on its own.

1. **This plan.** The format crate and the `hatchrun` CLI.
2. **Agent executor.** `hatchrun-agent` runs one compiled job: `run:`
   steps, JavaScript and composite actions, the built-in steps, with
   the `github` context and `GITHUB_*` files. Testable on a laptop
   with a plan file.
3. **Control plane.** GitHub App events, runs and jobs in Postgres, one
   Durable Object per run, check runs, the launch queue into the
   hatchrun-owned AWS account, the agent job protocol, live and
   finished logs.
4. **Accounts.** Runner templates in the launcher, warm pool sized by
   demand, secrets (Postgres + Secrets Store key), billing caps.
5. **Extensions.** Timing data for `tests split`, cache, workspace and
   test result storage, approval jobs in the app.

## Global Constraints

<!-- REUSE-IgnoreStart -->
- Every source file starts with the SPDX header used across the repo:
  `SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>`
  and `SPDX-License-Identifier: AGPL-3.0-only`.
<!-- REUSE-IgnoreEnd -->
- Crates use `version = "0.1.0"`, `publish = false`, the workspace's
  `authors`, `edition`, `rust-version`, `license`, `repository`, and
  `[lints] workspace = true` (clippy `all` + `pedantic` as warnings,
  `missing_docs` warns, `unsafe_code` denied). CI runs
  `cargo clippy --locked --all-targets -- -D warnings`.
- `hatchrun-workflow` builds for `wasm32-unknown-unknown`; it takes no
  dependency that needs `std::fs`, threads or a clock.
- The file is `.hatchrun/config.yml`. Workflows live under
  `workflows:`; each has its own `on:`.
- Expressions are `${{ }}` only. Parameters are read as
  `${{ parameters.x }}`; CircleCI's `<< >>` is not supported.
- Runner templates, verbatim from the spec:

  | `runs-on` | Arch | vCPU | Memory | Price |
  | --- | --- | --- | --- | --- |
  | `ubuntu-latest`, `ubuntu-26.04` | amd64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-arm` | arm64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-4core` (and `-arm`) | either | 4 | 16 GB | $0.004/min |
  | `ubuntu-26.04-8core` (and `-arm`) | either | 8 | 32 GB | $0.008/min |
  | `ubuntu-26.04-16core` (and `-arm`) | either | 16 | 64 GB | $0.016/min |

- A label that names no template fails with a message listing the
  templates.
- Docker-phase features fail validation with a message that says they
  come later: a `docker:` executor, `services:`, `container:`, and
  `uses: docker://...`.
- Pull requests from forks never run (enforced in Plan 3, not here).
- Commits are Conventional Commits, GPG-signed and DCO signed off
  (`git commit -S -s`), with no AI attribution.

## Review Focus

1. A config with a YAML syntax error or a wrong type: the user expects
   the line and column of the problem, not a serde type dump. Pinned in
   Task 2 (`reports_line_and_column`).
2. `requires` that form a cycle or name a missing job: the user expects
   one clear error naming the jobs, never a stack overflow or a hang.
   Pinned in Task 5 (`rejects_requires_cycle`, `rejects_unknown_requires`).
3. Branch filters with `**`, `!` negation and tags: the user expects
   GitHub's semantics, where later patterns override earlier ones and
   `*` does not cross `/`. Pinned in Task 4 (`negation_order_matters`,
   `star_does_not_cross_slash`).
4. Expressions comparing mixed types (`'1' == 1`, `null == ''`,
   case of strings): the user expects GitHub's loose equality, since
   their actions' `if:` lines were written for it. Pinned in Task 3
   (`loose_equality_matches_github`).
5. A command that uses itself, directly or through another command:
   the user expects an error, not unbounded expansion. Pinned in Task 5
   (`rejects_recursive_command`).

---

### Task 1: The crate and the runner templates

**Files:**
- Create: `crates/workflow/Cargo.toml`
- Create: `crates/workflow/src/lib.rs`
- Create: `crates/workflow/src/templates.rs`

**Interfaces:**
- Produces:
  - `pub enum Arch { Amd64, Arm64 }`
  - `pub struct Template { pub labels: &'static [&'static str], pub arch: Arch, pub vcpu: u32, pub memory_gib: u32, pub price_micros_per_minute: u64 }`
    (price in millionths of a dollar: $0.002 = `2_000`)
  - `pub const TEMPLATES: &[Template]`
  - `pub fn template(label: &str) -> Option<&'static Template>`
  - `pub fn template_labels() -> Vec<&'static str>` (every label, in
    table order, for error messages)

- [ ] **Step 1: Write the failing tests** in `templates.rs`

```rust
#[test]
fn latest_is_two_vcpu_amd64() {
    let t = template("ubuntu-latest").unwrap();
    assert_eq!((t.arch, t.vcpu, t.memory_gib, t.price_micros_per_minute), (Arch::Amd64, 2, 8, 2_000));
    assert!(std::ptr::eq(t, template("ubuntu-26.04").unwrap()));
}

#[test]
fn sixteen_core_arm() {
    let t = template("ubuntu-26.04-16core-arm").unwrap();
    assert_eq!((t.arch, t.vcpu, t.memory_gib, t.price_micros_per_minute), (Arch::Arm64, 16, 64, 16_000));
}

#[test]
fn unknown_label() {
    assert!(template("ubuntu-24.04").is_none());
    assert!(template_labels().contains(&"ubuntu-26.04-8core"));
    assert_eq!(template_labels().len(), 10);
}
```

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow templates` and
  expect a compile failure (crate missing).
- [ ] **Step 3: Create the crate and fill `TEMPLATES`** from the table
  in Global Constraints: 8 templates, 10 labels. Memory is 4 GiB per
  vCPU. Dependencies for the whole plan go in now: `serde` (derive),
  `serde-saphyr = "2.0.0"`, `serde_json`, `globset = "0.4.20"`,
  `thiserror = "2.0.21"`.
- [ ] **Step 4: Run** `cargo test -p hatchrun-workflow templates`
  (PASS) and `cargo build -p hatchrun-workflow --target wasm32-unknown-unknown`
  (builds).
- [ ] **Step 5: Commit** `feat(workflow): add the crate and the runner templates`

### Task 2: Parsing `.hatchrun/config.yml`

**Files:**
- Create: `crates/workflow/src/config.rs`
- Create: `crates/workflow/src/diagnostic.rs`
- Create: `crates/workflow/src/trigger.rs` (types only)
- Test: `crates/workflow/tests/parse.rs`, fixtures in `crates/workflow/tests/fixtures/`

**Interfaces:**
- Produces:
  - `pub struct Diagnostic { pub message: String, pub line: Option<u64>, pub column: Option<u64> }`
    with `Display` as `line:column: message` (or just `message`).
  - `pub fn parse(source: &str) -> Result<Config, Vec<Diagnostic>>`
  - `pub struct Config { pub executors: BTreeMap<String, Executor>, pub commands: BTreeMap<String, Command>, pub jobs: BTreeMap<String, Job>, pub workflows: BTreeMap<String, Workflow> }`
  - `pub struct Executor { pub runs_on: String }` (YAML `runs-on`)
  - `pub struct Command { pub parameters: BTreeMap<String, Parameter>, pub steps: Vec<Step> }`
  - `pub struct Parameter { pub kind: ParameterKind, pub default: Option<serde_json::Value>, pub description: Option<String> }`,
    `pub enum ParameterKind { String, Boolean, Integer, Enum(Vec<String>) }`
    (YAML `type: string|boolean|integer|enum` with `enum: [..]`)
  - `pub struct Job { pub executor: Option<String>, pub runs_on: Option<String>, pub parameters: BTreeMap<String, Parameter>, pub parallelism: Option<u32>, pub env: BTreeMap<String, String>, pub if_: Option<String>, pub timeout_minutes: Option<u32>, pub outputs: BTreeMap<String, String>, pub steps: Vec<Step> }`
    (`runs-on` on a job is shorthand for an executor; exactly one of
    the two is required, checked in Task 5)
  - `pub enum Step { Run(RunStep), Uses(UsesStep), Command { name: String, with: BTreeMap<String, serde_json::Value> }, Cache(CacheStep), Workspace(WorkspaceStep), TestResults { path: String } }`
    - `RunStep { name, id, run, shell, working_directory, env, if_, continue_on_error, timeout_minutes }`
    - `UsesStep { name, id, uses, with: BTreeMap<String, serde_json::Value>, env, if_, continue_on_error }`
    - `CacheStep { key: String, restore_keys: Vec<String>, paths: Vec<String> }`
    - `WorkspaceStep { Persist { root: String, paths: Vec<String> }, Attach { at: String } }`
    - In YAML a bare string step (`- setup`) or a single-key map whose
      key names a command (`- setup: { version: 2 }`) is `Step::Command`;
      `cache:`, `workspace:` (`persist:` / `attach:`), `test-results:`,
      `run:` and `uses:` are the rest.
  - `pub struct Workflow { pub on: Triggers, pub jobs: Vec<WorkflowJob> }`
  - `pub struct WorkflowJob { pub job: String, pub name: Option<String>, pub requires: Vec<String>, pub approval: bool, pub branches: Option<Filter>, pub tags: Option<Filter>, pub matrix: BTreeMap<String, Vec<serde_json::Value>>, pub with: BTreeMap<String, serde_json::Value> }`
    (YAML: a bare string, or `- <job>: { requires, type: approval, branches, tags, matrix, name, ...parameter values }`;
    an approval entry names no job of `jobs:`)
  - Also create `crates/workflow/src/trigger.rs` with the `Triggers`,
    `Filter` and trigger structs exactly as Task 4's Interfaces list them,
    deriving `Deserialize` (and `Serialize`); Task 4 adds the matching.
  - `steps`, `parameters`, `env`, `outputs` and every list default to
    empty when absent.

- [ ] **Step 1: Write the failing tests** in `tests/parse.rs`

```rust
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
        ("jobs: { t: { runs-on: ubuntu-latest, services: { pg: { image: postgres } }, steps: [] } }", "services"),
        ("jobs: { t: { runs-on: ubuntu-latest, container: rust, steps: [] } }", "container"),
    ] {
        let err = parse(yaml).unwrap_err();
        assert!(err[0].message.contains(word) && err[0].message.contains("later"), "{err:?}");
    }
}
```

  `fixtures/spec_example.yml` is the YAML example from the spec's
  "Workflow format" section, copied exactly.

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test parse`
  and expect FAIL.
- [ ] **Step 3: Implement `parse`** with `serde_saphyr::from_str`. Map
  `Error::location()` to `Diagnostic.line/column`. Unknown keys are
  errors (`deny_unknown_fields`) except the Docker-phase keys, which
  get the message `"<key> is not supported yet; Docker services and containers come in a later release"`.
  Step and workflow-job shapes need hand-written `Deserialize` impls
  for the string/map forms above.
- [ ] **Step 4: Run** the tests (PASS).
- [ ] **Step 5: Commit** `feat(workflow): parse .hatchrun/config.yml with locations`

### Task 3: Expressions

**Files:**
- Create: `crates/workflow/src/expr/mod.rs`, `expr/lexer.rs`, `expr/parser.rs`, `expr/eval.rs`
- Test: `crates/workflow/tests/expr.rs`

**Interfaces:**
- Produces:
  - `pub fn evaluate(expr: &str, ctx: &serde_json::Value, funcs: &dyn Functions) -> Result<serde_json::Value, ExprError>`
    (`expr` is the inside of `${{ }}`; `ctx` is an object whose keys
    are context names: `github`, `env`, `matrix`, `needs`, `steps`,
    `secrets`, `parameters`, `runner`, `job`)
  - `pub fn interpolate(template: &str, ctx: &serde_json::Value, funcs: &dyn Functions) -> Result<String, ExprError>`
    (replaces every `${{ }}` in a string)
  - `pub fn truthy(v: &serde_json::Value) -> bool`
  - `pub trait Functions { fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError>; fn status(&self) -> JobStatus; }`
    with `pub enum JobStatus { Success, Failure, Cancelled }`. The
    control plane passes an impl whose `hash_files` errors ("hashFiles
    runs on the runner"); the agent (Plan 2) hashes real files.
  - `pub struct ExprError { pub message: String, pub offset: usize }`

GitHub's semantics, which the tests pin: literals `null`, `true`,
`false`, numbers (incl. hex `0xff`, exponent), single-quoted strings
with `''` escape; property access `a.b`, `a['b']`, `a.*.b` filters;
operators `!`, `<`, `<=`, `>`, `>=`, `==`, `!=`, `&&`, `||`, grouping;
`&&`/`||` return an operand, not a bool. Loose equality: compare as
numbers when types differ (`null`→0, `true`→1, `''`→0, non-numeric
string → NaN), strings compare case-insensitively. Functions:
`contains` (case-insensitive, string or array), `startsWith`,
`endsWith`, `format('{0}', ..)` with `{{`/`}}` escapes, `join` (default
`,`), `toJSON` (pretty), `fromJSON`, `hashFiles`, `success`, `always`,
`failure`, `cancelled`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn loose_equality_matches_github() {
    for (e, want) in [
        ("'1' == 1", true), ("null == ''", true), ("null == 0", true),
        ("'ABC' == 'abc'", true), ("true == 1", true), ("'a' == 0", false),
        ("1 < '2'", true), ("'x' != 'X'", false),
    ] {
        assert_eq!(evaluate(e, &json!({}), &NoFiles).unwrap(), json!(want), "{e}");
    }
}

#[test]
fn and_or_return_operands() {
    assert_eq!(evaluate("'' || 'fallback'", &json!({}), &NoFiles).unwrap(), json!("fallback"));
    assert_eq!(evaluate("github.ref == 'refs/heads/main' && 'prod'", &json!({"github": {"ref": "refs/heads/main"}}), &NoFiles).unwrap(), json!("prod"));
}

#[test]
fn contexts_and_filters() {
    let ctx = json!({"needs": {"a": {"outputs": {"v": "1"}}, "b": {"outputs": {"v": "2"}}}});
    assert_eq!(evaluate("needs.*.outputs.v", &ctx, &NoFiles).unwrap(), json!(["1", "2"]));
    assert_eq!(evaluate("needs['a'].outputs.v", &ctx, &NoFiles).unwrap(), json!("1"));
    assert_eq!(evaluate("needs.missing.outputs", &ctx, &NoFiles).unwrap(), json!(null));
}

#[test]
fn functions() {
    assert_eq!(evaluate("format('{0}-{{x}}-{1}', 'a', 2)", &json!({}), &NoFiles).unwrap(), json!("a-{x}-2"));
    assert_eq!(evaluate("contains(fromJSON('[\"A\"]'), 'a')", &json!({}), &NoFiles).unwrap(), json!(true));
    assert_eq!(evaluate("join(fromJSON('[1,2]'), '+')", &json!({}), &NoFiles).unwrap(), json!("1+2"));
}

#[test]
fn interpolates_and_reports_offsets() {
    assert_eq!(interpolate("v${{ 1 + }}", &json!({}), &NoFiles).unwrap_err().offset, 7);
    assert_eq!(interpolate("deps-${{ matrix.os }}", &json!({"matrix": {"os": "arm"}}), &NoFiles).unwrap(), "deps-arm");
}
```

  `NoFiles` is a test helper implementing `Functions` with
  `status() == Success` and `hash_files` returning an error.

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test expr` (FAIL).
- [ ] **Step 3: Implement** a hand-written lexer and a precedence-climbing
  parser into an AST, then a tree-walking evaluator. Precedence, low to
  high: `||`, `&&`, `==` `!=`, `<` `<=` `>` `>=`, `!`, access.
  act's `pkg/exprparser` is the behaviour reference for edge cases.
- [ ] **Step 4: Run** the tests (PASS).
- [ ] **Step 5: Commit** `feat(workflow): evaluate github-style expressions`

### Task 4: Triggers and filters

**Files:**
- Modify: `crates/workflow/src/trigger.rs` (types from Task 2)
- Test: `crates/workflow/tests/trigger.rs`

**Interfaces:**
- Produces:
  - `pub struct Filter(Vec<String>)`: ordered GitHub patterns, a
    leading `!` negates. `pub fn matches(&self, value: &str) -> bool`.
  - `pub struct Triggers { pub push: Option<PushTrigger>, pub pull_request: Option<PullRequestTrigger>, pub schedule: Vec<Schedule>, pub workflow_dispatch: Option<Dispatch> }`
    - `PushTrigger { branches, branches_ignore, tags, tags_ignore, paths, paths_ignore: Option<Filter> }`
    - `PullRequestTrigger { types: Vec<String>, branches, branches_ignore, paths, paths_ignore: Option<Filter> }`
      (`types` defaults to `opened`, `synchronize`, `reopened`)
    - `Schedule { cron: String }`, `Dispatch { inputs: BTreeMap<String, Parameter> }`
  - `pub enum Event { Push { r#ref: String, changed: Vec<String> }, PullRequest { action: String, base_ref: String, changed: Vec<String> }, Schedule { cron: String }, Dispatch }`
  - `pub fn triggered(on: &Triggers, event: &Event) -> bool`

GitHub filter rules: `*` matches any character but `/`; `**` matches
anything; `?`, `+`, `[]` as in GitHub's cheat sheet; patterns apply in
order and the last match wins, so `!` after a positive pattern
excludes. `branches` and `branches-ignore` together is an error (Task 5
reports it). A push of a tag matches only `tags`/`tags-ignore`; a push
of a branch only `branches`. A path filter passes when any changed path
passes.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn star_does_not_cross_slash() {
    let f = Filter(vec!["feature/*".into()]);
    assert!(f.matches("feature/a") && !f.matches("feature/a/b"));
    assert!(Filter(vec!["feature/**".into()]).matches("feature/a/b"));
}

#[test]
fn negation_order_matters() {
    let f = Filter(vec!["releases/**".into(), "!releases/**-alpha".into()]);
    assert!(f.matches("releases/1.0") && !f.matches("releases/1.0-alpha"));
    let g = Filter(vec!["!releases/**-alpha".into(), "releases/**".into()]);
    assert!(g.matches("releases/1.0-alpha"));
}

#[test]
fn push_branch_tag_and_paths() {
    let on: Triggers = serde_saphyr::from_str("push: { branches: [main], paths: ['src/**'] }").unwrap();
    assert!(triggered(&on, &Event::Push { r#ref: "refs/heads/main".into(), changed: vec!["src/a.rs".into()] }));
    assert!(!triggered(&on, &Event::Push { r#ref: "refs/heads/main".into(), changed: vec!["README.md".into()] }));
    assert!(!triggered(&on, &Event::Push { r#ref: "refs/tags/v1".into(), changed: vec!["src/a.rs".into()] }));
}

#[test]
fn pull_request_default_types() {
    let on: Triggers = serde_saphyr::from_str("pull_request: {}").unwrap();
    let pr = |action: &str| Event::PullRequest { action: action.into(), base_ref: "main".into(), changed: vec![] };
    assert!(triggered(&on, &pr("synchronize")) && !triggered(&on, &pr("closed")));
}
```

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test trigger` (FAIL).
- [ ] **Step 3: Implement** by translating each GitHub pattern to a
  `globset::GlobBuilder` with `literal_separator(true)`; evaluate the
  list in order, last match wins.
- [ ] **Step 4: Run** the tests (PASS).
- [ ] **Step 5: Commit** `feat(workflow): match triggers with github filter rules`

### Task 5: Compiling a workflow into a plan

**Files:**
- Create: `crates/workflow/src/plan.rs`
- Test: `crates/workflow/tests/plan.rs`

**Interfaces:**
- Consumes: `Config`, `Step`, `WorkflowJob` (Task 2), `template()` (Task 1), `interpolate` (Task 3).
- Produces:
  - `pub fn compile(config: &Config, workflow: &str, git_ref: &str) -> Result<Plan, Vec<Diagnostic>>`
    (`git_ref` decides `branches:`/`tags:` filters on workflow jobs)
  - `pub fn validate(config: &Config) -> Vec<Diagnostic>` (compiles
    every workflow against `refs/heads/main` and collects every error)
  - `pub struct Plan { pub workflow: String, pub jobs: Vec<PlannedJob> }`
    in an order where every job comes after the jobs it requires.
  - `pub struct PlannedJob { pub id: String, pub name: String, pub requires: Vec<String>, pub kind: JobKind, pub matrix: BTreeMap<String, serde_json::Value>, pub node: Option<(u32, u32)>, pub env: BTreeMap<String, String>, pub if_: Option<String>, pub timeout_minutes: u32, pub outputs: BTreeMap<String, String>, pub steps: Vec<PlannedStep> }`
    - `pub enum JobKind { Run { template: &'static Template }, Approval }`
    - `node` is `Some((index, total))` for `parallelism`, 0-based.
    - `timeout_minutes` defaults to 360.
  - `pub enum PlannedStep { Run(RunStep), Uses(UsesStep), Cache(CacheStep), Workspace(WorkspaceStep), TestResults { path: String } }`
    (no `Command`: commands are expanded)
  - Job ids: the workflow job's `name`, else the job key; matrix
    entries get `<id> (<v1>, <v2>)` in key order; parallel nodes get
    `<id> [<i+1>/<n>]`. `requires` on a matrix or parallel job means
    all its expansions.

Rules, each an error with the job or command named:
- exactly one of `executor` and `runs-on` on a job; the executor
  exists; its `runs-on` names a template (message lists
  `template_labels()`);
- every `requires` names a workflow job in the same workflow; no
  cycles; no duplicate ids;
- every command step names a command; no command reaches itself;
  `with:` keys are declared parameters, required ones (no default) are
  given, values match `ParameterKind`;
- parameter values are substituted where step strings contain
  `${{ parameters.x }}` (string fields of the step) at compile time;
  every other expression is left for run time;
- `parallelism` is 1 to 64; a matrix expands to at most 256 entries;
- a matrix key that names a declared parameter of the job is passed to
  it as that parameter's value for each entry, so
  `${{ parameters.x }}` substitutes at compile time; every matrix key is
  also available as `${{ matrix.x }}` at run time.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn expands_the_spec_example() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let plan = compile(&config, "ci", "refs/heads/main").unwrap();
    let ids: Vec<_> = plan.jobs.iter().map(|j| j.id.as_str()).collect();
    assert_eq!(ids, ["test [1/4]", "test [2/4]", "test [3/4]", "test [4/4]", "hold", "deploy"]);
    assert_eq!(plan.jobs[5].requires, ["hold"]);
    assert_eq!(plan.jobs[4].requires, ["test [1/4]", "test [2/4]", "test [3/4]", "test [4/4]"]);
    assert!(matches!(plan.jobs[0].steps[0], PlannedStep::Uses(ref u) if u.uses == "actions/checkout@v5"));
    assert_eq!(plan.jobs[0].node, Some((0, 4)));
}

#[test]
fn branch_filter_drops_jobs() {
    let config = parse(include_str!("fixtures/spec_example.yml")).unwrap();
    let plan = compile(&config, "ci", "refs/heads/feature").unwrap();
    assert!(plan.jobs.iter().all(|j| j.id != "deploy"));
}

#[test]
fn rejects_requires_cycle() {
    let err = compile(&parse(CYCLE).unwrap(), "ci", "refs/heads/main").unwrap_err();
    assert!(err[0].message.contains("a") && err[0].message.contains("b") && err[0].message.contains("cycle"));
}

#[test]
fn rejects_unknown_requires() { /* `requires: [nope]` -> message names "nope" */ }

#[test]
fn rejects_recursive_command() { /* a -> b -> a -> message names both commands */ }

#[test]
fn unknown_template_lists_templates() {
    let err = validate(&parse("jobs: { t: { runs-on: ubuntu-24.04, steps: [] } }\nworkflows: { ci: { on: { push: {} }, jobs: [t] } }").unwrap());
    assert!(err[0].message.contains("ubuntu-24.04") && err[0].message.contains("ubuntu-26.04-arm"));
}

#[test]
fn matrix_and_parameters() {
    // job with parameter `toolchain` used as `run: cargo +${{ parameters.toolchain }} test`,
    // workflow job with matrix { toolchain: [stable, nightly] } passed as `with`
    // -> ids "test (stable)", "test (nightly)", run strings "cargo +stable test" / "cargo +nightly test"
}
```

  `CYCLE` is a const config where workflow jobs `a` requires `b` and
  `b` requires `a`. Fill in the three sketched tests with the
  assertions their comments state.

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test plan` (FAIL).
- [ ] **Step 3: Implement `compile` and `validate`.** Topological sort
  with Kahn's algorithm; leftover nodes are the cycle. Command
  expansion is depth-first with a visiting set.
- [ ] **Step 4: Run** the tests (PASS), then
  `cargo build -p hatchrun-workflow --target wasm32-unknown-unknown`.
- [ ] **Step 5: Commit** `feat(workflow): compile a workflow into a job plan`

### Task 6: Converting GitHub workflows

**Files:**
- Create: `crates/workflow/src/convert.rs`
- Test: `crates/workflow/tests/convert.rs`, fixtures `tests/fixtures/github/*.yml`

**Interfaces:**
- Consumes: `Config` and friends (Task 2), `template()` (Task 1).
- Produces:
  - `pub struct Conversion { pub config: Config, pub warnings: Vec<Diagnostic> }`
  - `pub fn convert(files: &[(String, String)]) -> Result<Conversion, Vec<Diagnostic>>`
    (each pair is a file name and its contents; the workflow key is the
    file stem)
  - `pub fn to_yaml(config: &Config) -> String` (via
    `serde_saphyr::to_string`; `Config` and its parts derive
    `Serialize` with the same key names they parse)

Mapping: `on:` as is; each GitHub job becomes `jobs.<stem>-<job>` with
`runs-on` copied (a template label must result; `ubuntu-24.04`,
`ubuntu-22.04` become `ubuntu-latest` with a warning);
`needs` → `requires`; `strategy.matrix` (plain lists only) → workflow
job `matrix` and `${{ matrix.x }}` stays as is; `if`, `env`,
`timeout-minutes`, `outputs` and steps (`run`, `uses`, `with`, `env`,
`if`, `id`, `name`, `shell`, `working-directory`, `continue-on-error`)
copy across. Each of these becomes a warning naming the file and job
and is dropped: `services`, `container`, `concurrency`, `permissions`,
`environment`, `strategy.matrix.include/exclude`, `defaults`,
reusable workflow calls (`jobs.<id>.uses`). `docker://` steps are an
error.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn converts_a_typical_ci_workflow() {
    let out = convert(&[("ci.yml".into(), include_str!("fixtures/github/ci.yml").into())]).unwrap();
    let job = &out.config.jobs["ci-test"];
    assert_eq!(job.runs_on.as_deref(), Some("ubuntu-latest"));
    assert_eq!(out.config.workflows["ci"].jobs.iter().find(|j| j.job == "ci-lint").unwrap().requires, Vec::<String>::new());
    assert!(out.warnings.iter().any(|w| w.message.contains("services")));
    // round trip: the YAML we write parses and validates clean
    let reparsed = parse(&to_yaml(&out.config)).unwrap();
    assert!(validate(&reparsed).is_empty());
}

#[test]
fn old_ubuntu_becomes_latest_with_warning() { /* runs-on: ubuntu-24.04 -> "ubuntu-latest" + warning naming ubuntu-24.04 */ }

#[test]
fn docker_steps_are_errors() { /* uses: docker://alpine -> Err naming the step */ }
```

  `fixtures/github/ci.yml` is a realistic workflow: `on: [push, pull_request]`,
  a `lint` job, a `test` job with `needs: lint`, a 2-entry matrix,
  `actions/checkout@v5`, `actions/cache@v4`, and a `services: postgres`
  block.

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test convert` (FAIL).
- [ ] **Step 3: Implement** with a private `serde` model of the GitHub
  workflow subset (accept `on:` as string, list or map).
- [ ] **Step 4: Run** the tests (PASS).
- [ ] **Step 5: Commit** `feat(workflow): convert github workflows to hatchrun config`

### Task 7: Splitting tests across parallel nodes

**Files:**
- Create: `crates/workflow/src/split.rs`
- Test: in-file `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub fn split<'a>(items: &[&'a str], timings: &BTreeMap<String, f64>, index: u32, total: u32) -> Vec<&'a str>`
  - Deterministic for the same inputs on every node, so the nodes'
    shares never overlap and together cover every item.

Algorithm: items with a timing keep it; items without get the median
of the known timings (or 1.0 when none are known). Sort by time
descending, ties by name ascending, then assign each to the node with
the smallest total so far, ties to the lowest node index. Return node
`index`'s items in their original input order.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn nodes_partition_the_items() {
    let items = ["a", "b", "c", "d", "e"];
    let timings = BTreeMap::from([("a".into(), 10.0), ("b".into(), 1.0), ("c".into(), 1.0), ("d".into(), 8.0)]);
    let all: Vec<Vec<&str>> = (0..2).map(|i| split(&items, &timings, i, 2)).collect();
    // e gets the median 4.5; order a10 d8 e4.5 b1 c1 -> node0 a,b,c (12), node1 d,e (12.5)
    assert_eq!(all[0], ["a", "b", "c"]);
    assert_eq!(all[1], ["d", "e"]);
}

#[test]
fn no_timings_balances_by_count() {
    let items = ["a", "b", "c", "d"];
    let lens: Vec<usize> = (0..3).map(|i| split(&items, &BTreeMap::new(), i, 3).len()).collect();
    assert_eq!(lens, [2, 1, 1]);
}
```

- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow split` (FAIL).
- [ ] **Step 3: Implement `split`.**
- [ ] **Step 4: Run** the tests (PASS).
- [ ] **Step 5: Commit** `feat(workflow): split tests across nodes by timing`

### Task 8: The `hatchrun` binary

**Files:**
- Create: `crates/cli/Cargo.toml` (package `hatchrun-cli`, `[[bin]] name = "hatchrun"`), `crates/cli/build.rs`, `crates/cli/src/main.rs`
- Test: `crates/cli/tests/cli.rs`

**Interfaces:**
- Consumes: `parse`, `validate`, `convert`, `to_yaml`, `split`.
- Produces the commands later plans and the runner image rely on:
  - `hatchrun validate [PATH]`: default `.hatchrun/config.yml`; prints
    `PATH:line:column: message` per diagnostic; exit 0 clean, 1 with
    errors.
  - `hatchrun convert [FILES]... [-o PATH]`: default files
    `.github/workflows/*.yml` and `*.yaml`; writes YAML to `-o` or
    stdout; warnings to stderr as `file: message`; exit 1 on errors.
  - `hatchrun tests split [--by timings|name] [--timings FILE] [--index N] [--total N] [ITEMS]...`:
    items from arguments, else one per line from stdin; prints the
    node's items space-separated on one line. `--index`/`--total`
    default to `HATCHRUN_NODE_INDEX` / `HATCHRUN_NODE_TOTAL` (0 and 1
    when unset); `--timings` defaults to `HATCHRUN_TIMINGS_FILE`, a
    JSON object of item to seconds; `--by name` ignores timings.
    Plan 2's agent sets those three variables on parallel nodes.
  - `hatchrun --version` prints `hatchrun <version>`, with `build.rs`
    copied from `crates/scaler/build.rs` (`HATCHRUN_VERSION`).

- [ ] **Step 1: Write the failing tests** with
  `std::process::Command::new(env!("CARGO_BIN_EXE_hatchrun"))`:
  - `validate_ok_and_error`: the spec fixture exits 0 with no output;
    a file with `parallelism: lots` exits 1 and stdout contains
    `:3:18:`.
  - `convert_writes_yaml`: converting `fixtures/github/ci.yml` (copy
    it under `crates/cli/tests/fixtures/`) to `-o` a temp file exits 0,
    and `validate` on that file exits 0; stderr mentions `services`.
  - `split_reads_env`: with `HATCHRUN_NODE_INDEX=1`,
    `HATCHRUN_NODE_TOTAL=2` and the Task 7 timings in a temp
    `HATCHRUN_TIMINGS_FILE`, `tests split a b c d e` prints `d e`.
  - `version`: prints `hatchrun 0.1.0`.
- [ ] **Step 2: Run** `cargo test -p hatchrun-cli` (FAIL).
- [ ] **Step 3: Implement `main.rs`** with `clap` derive (`anyhow` for
  I/O errors).
- [ ] **Step 4: Run** `cargo test -p hatchrun-cli` (PASS) and
  `cargo clippy --locked --all-targets -- -D warnings` (clean).
- [ ] **Step 5: Commit** `feat(cli): add the hatchrun binary`

### Task 9: Shipping the binary

**Files:**
- Modify: `.github/workflows/release.yml` (build and Package steps, around lines 95-122)
- Modify: `.github/workflows/ci.yml` (add the wasm build)
- Modify: `README.md` (a short `hatchrun` section next to the agent and scaler)

**Interfaces:**
- Produces: release assets `hatchrun-<version>-linux-<arch>.tar.gz`
  holding `hatchrun`, `LICENSE`, `NOTICE`, `README.md`. Plan 2 installs
  it into the runner image from this asset.

- [ ] **Step 1:** In `release.yml`, add `-p hatchrun-cli` to the
  `cargo build`, check `"target/$TARGET/release/hatchrun" --version | grep -qx "hatchrun $VERSION"`,
  and package it as above, beside the scaler tarball.
- [ ] **Step 2:** In `ci.yml`, add
  `cargo build --locked -p hatchrun-workflow --target wasm32-unknown-unknown`
  to the job that runs clippy, so a dependency that breaks the Worker
  build fails CI.
- [ ] **Step 3:** Run `cargo build --locked --release -p hatchrun-cli`
  and `actionlint .github/workflows/release.yml .github/workflows/ci.yml`
  (if installed); both clean.
- [ ] **Step 4: Commit** `build(release): publish the hatchrun cli`
