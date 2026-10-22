# Orchestrator Plan 2: Agent Executor

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `hatchrun-agent exec job.json` runs one compiled job on a
Linux machine: `run:` steps, JavaScript and composite actions, the
built-in `cache`, `workspace` and `test-results` steps, with the
contexts, `GITHUB_*` variables, files and workflow commands actions
expect, and secrets masked in every line it prints.

**Architecture:** The control plane will hand the agent a
`JobDocument`, a JSON wire type defined in `hatchrun-workflow` next to
`PlannedJob` so the Worker (Plan 3) and the agent agree on it. The
executor lives in `crates/hatchrun-agent/src/exec/` beside the
existing GitHub runner path, which stays unchanged. It reports through
a `Sink` trait; this plan ships a text sink that prints to stdout and a
JSON result file. Built-in steps store archives through a `Store`
trait; this plan ships a directory store. Plan 3 adds the HTTP sink,
the boot path through the broker and the agent protocol. Plan 5 adds
the remote store and records timings.

**Tech Stack:** Rust 1.99 (edition 2024), tokio (current-thread),
`nix` (process groups, users, signals), `reqwest` (action downloads),
`tar` + `flate2` (archives), `sha2` (`hashFiles`), `serde-saphyr`
(`action.yml`), `hatchrun-workflow` (expressions, globs, split).

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`
(sections "Workflow format" and "Workflow engine").

## Scope against the other plans

- **In:** the job document type, the `exec` and `split` subcommands,
  everything a job does on the machine, the `actions/cache` proxy for
  a job when the document names a cache service.
- **Plan 3:** how the agent gets the document (broker ticket, then a
  fetch the control plane answers only after it turns instance
  metadata off), log and step streaming, heartbeats, cancellation from
  the control plane, merging matrix outputs into `needs`.
- **Plan 5:** the remote `Store`, timings recorded from test results,
  `actions/upload-artifact` and `download-artifact`.

## Global Constraints

<!-- REUSE-IgnoreStart -->
- Every source file starts with the SPDX header used across the repo:
  `SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>`
  and `SPDX-License-Identifier: AGPL-3.0-only`.
<!-- REUSE-IgnoreEnd -->
- `[lints] workspace = true`; CI runs
  `cargo clippy --locked --all-targets -- -D warnings`.
- `hatchrun-workflow` still builds for `wasm32-unknown-unknown` with no
  dependency that needs `std::fs`, threads or a clock. Every new type
  in it is plain data.
- Running `hatchrun-agent` with no subcommand keeps today's behaviour
  for hatchrun.io exactly. No existing test changes.
- The directory layout matches GitHub's hosted runners, rooted at
  `--work-dir` (default `/home/runner/work`):
  `GITHUB_WORKSPACE` = `<work>/<repo>/<repo>`, `RUNNER_TEMP` =
  `<work>/_temp`, actions under `<work>/_actions/<owner>/<repo>/<ref>`,
  `RUNNER_TOOL_CACHE` = `/opt/hostedtoolcache`.
- Steps run as `--runner-user` (default `runner`) with the environment
  read from `--env-file` (default `/etc/environment`), reusing
  `runner::run_as` and `envfile`. An empty `--runner-user` keeps the
  agent's user, which is how tests run.
- A masked value prints as `***`.
- Docker-phase features fail the step with: `` `<uses>` is a Docker
  container action; Docker services and containers come in a later
  release ``.
- Nothing in this plan talks to the control plane. The only network
  calls are action downloads from `github.api_url` and, when the
  document has a cache service, the existing cache proxy's calls.
- No backward compatibility shims: new fields are required.
- Commits are Conventional Commits, GPG-signed and DCO signed off
  (`git commit -S -s`), with no AI attribution.

## Review Focus

1. **A secret reaching the log.** Split across two `write` calls,
   inside a multi-line value, JSON-escaped, base64-encoded, or echoed
   by the `::add-mask::` command line itself. The user expects `***`
   every time. Pinned in Task 3 (`masks_across_writes`,
   `masks_each_line_of_a_multiline_secret`, `masks_encoded_forms`,
   `add_mask_line_is_not_printed`) and Task 12 (`secret_never_printed`).
2. **A step that will not die.** It ignores SIGTERM, or forks a
   background process that holds stdout open. On timeout or cancel the
   user expects the step to end within the grace period and later
   `always()` steps to run. Pinned in Task 6
   (`timeout_kills_the_process_group`, `background_child_does_not_hold_the_step`).
3. **Output with no newlines.** A step that prints 100 MiB on one line,
   or binary. The user expects bounded memory and the output still
   shown, split into 64 KiB lines. Pinned in Task 3 (`long_line_is_split`).
4. **A hostile archive.** An action tarball or a cache/workspace
   archive with `../` entries, absolute paths, a symlink pointing out
   of the target, or a gzip bomb. The user expects the extraction to
   fail with a message and nothing written outside the target. Pinned
   in Task 7 (`rejects_entries_outside_the_root`, `stops_at_the_size_cap`).
5. **`GITHUB_ENV` edge cases.** A heredoc with no closing delimiter, a
   value that contains the delimiter, or a step setting `NODE_OPTIONS`.
   The user expects GitHub's behaviour: the first two fail the step
   with the file's line number; `NODE_OPTIONS` is refused with a
   warning. Pinned in Task 4.

---

### Task 1: The job document

**Files:**
- Create: `crates/workflow/src/job.rs`
- Modify: `crates/workflow/src/lib.rs`, `crates/workflow/src/plan.rs`,
  `crates/workflow/src/config.rs`, `crates/workflow/src/templates.rs`
- Test: `crates/workflow/tests/job.rs`

**Interfaces:**
- Produces, in `hatchrun_workflow::job` and re-exported from the crate
  root:

```rust
pub struct JobDocument {
    pub run_id: String,
    pub run_number: u64,
    pub run_attempt: u32,
    pub workflow: String,
    pub job: PlannedJob,
    pub event_name: String,
    pub event: Value,                       // webhook payload
    pub github: Github,
    pub needs: BTreeMap<String, Need>,      // keyed by PlannedJob::name
    pub secrets: BTreeMap<String, String>,
    pub vars: BTreeMap<String, String>,
    pub timings: BTreeMap<String, f64>,     // test name -> seconds
    pub cache: Option<CacheService>,
}
pub struct Github {
    pub repository: String, pub repository_owner: String,
    pub sha: String, #[serde(rename = "ref")] pub ref_: String,
    pub ref_name: String, pub ref_type: String,
    pub head_ref: String, pub base_ref: String, pub actor: String,
    pub server_url: String, pub api_url: String, pub graphql_url: String,
    pub token: String,
}
pub struct Need { pub result: Conclusion, pub outputs: BTreeMap<String, String> }
pub struct CacheService { pub url: String, pub token: String }
pub enum Conclusion { Success, Failure, Cancelled, Skipped }   // "success", ...
pub struct Annotation {
    pub level: AnnotationLevel,             // Error, Warning, Notice
    pub message: String, pub title: Option<String>, pub file: Option<String>,
    pub line: Option<u32>, pub end_line: Option<u32>,
    pub col: Option<u32>, pub end_column: Option<u32>,
}
pub struct StepSummary {
    pub name: String, pub outcome: Conclusion, pub conclusion: Conclusion,
    pub duration_ms: u64,
}
pub struct JobResult {
    pub conclusion: Conclusion,
    pub outputs: BTreeMap<String, String>,
    pub steps: Vec<StepSummary>,
    pub annotations: Vec<Annotation>,
}
```

  All derive `Debug, Clone, PartialEq, Serialize, Deserialize` with
  `rename_all = "snake_case"`. `Plan`, `PlannedJob`, `JobKind`,
  `PlannedStep`, `RunStep`, `UsesStep`, `CacheStep`, `WorkspaceStep`
  gain `Serialize, Deserialize` in the same JSON shape.

- [ ] **Step 1: Write the failing tests** in `tests/job.rs`:
  - `job_document_round_trips`: compile the spec example from
    `crates/cli/tests/fixtures/spec_example.yml` for `refs/heads/main`,
    wrap every job in a `JobDocument`, then
    `serde_json::from_str(&serde_json::to_string(&doc)?)? == doc`.
  - `job_kind_serializes_as_the_label`: `JobKind::Run` for
    `ubuntu-26.04-arm` serializes to
    `{"run":{"template":"ubuntu-26.04-arm"}}`.
  - `unknown_template_is_an_error`: deserializing
    `{"run":{"template":"ubuntu-24.04"}}` fails and the message names
    `ubuntu-24.04`.
  - `debug_hides_secrets`: `format!("{doc:?}")` with
    `secrets = {"K": "s3cr3t"}`, `github.token = "ghs_x"` and a cache
    token `"c_tok"` contains none of the three values.
- [ ] **Step 2: Run** `cargo test -p hatchrun-workflow --test job`.
  Expected: FAIL, `job` module not found.
- [ ] **Step 3: Implement.** `JobKind::Run { template }` serializes the
  template's first label and deserializes through `template(label)`.
  `RunStep` and `UsesStep` get a plain `Deserialize` derive that
  matches their `Serialize` output; the YAML path keeps its own
  deserializer for `Step`. Write `Debug` by hand for `JobDocument`,
  `Github` and `CacheService`, printing `[redacted]` for `secrets`
  values, `token`, and `event` (payloads can be large).
- [ ] **Step 4: Run** `cargo test -p hatchrun-workflow` and
  `cargo build -p hatchrun-workflow --target wasm32-unknown-unknown`.
  Expected: PASS, and the wasm build succeeds.
- [ ] **Step 5: Commit** `feat(workflow): add the job document the agent runs`.

---

### Task 2: The `exec` subcommand and the job skeleton

**Files:**
- Create: `crates/hatchrun-agent/src/exec/mod.rs`,
  `crates/hatchrun-agent/src/exec/layout.rs`,
  `crates/hatchrun-agent/src/exec/sink.rs`
- Modify: `crates/hatchrun-agent/src/main.rs`,
  `crates/hatchrun-agent/Cargo.toml` (add `hatchrun-workflow` path dep)
- Test: `crates/hatchrun-agent/tests/exec.rs`,
  `crates/hatchrun-agent/tests/fixtures/jobs/`

**Interfaces:**
- Consumes: `JobDocument`, `JobResult`, `StepSummary`, `Annotation`,
  `Conclusion` (Task 1).
- Produces:

```rust
// exec/mod.rs
pub struct Options {
    pub work_dir: PathBuf, pub runner_user: Option<String>,
    pub env_file: PathBuf, pub node: Option<PathBuf>,
    pub store: Option<PathBuf>, pub tool_cache: PathBuf,
}
pub async fn run_job(doc: JobDocument, opts: &Options, sink: &mut dyn Sink,
                     cancel: impl Future<Output = ()>) -> anyhow::Result<JobResult>;

// exec/sink.rs
pub trait Sink {
    fn step_started(&mut self, index: usize, name: &str);
    fn line(&mut self, text: &str);          // already masked
    fn annotation(&mut self, annotation: &Annotation);
    fn step_finished(&mut self, index: usize, summary: &StepSummary);
}
pub struct TextSink<W: Write> { .. }        // "== <name>" headers, lines as-is

// exec/layout.rs
pub struct Layout { pub work: PathBuf, pub workspace: PathBuf, pub temp: PathBuf,
                    pub actions: PathBuf, pub tool_cache: PathBuf }
impl Layout { pub fn new(work: &Path, repository: &str, tool_cache: &Path) -> Self;
              pub fn create(&self) -> anyhow::Result<()>; }
```

- CLI: `hatchrun-agent exec <JOB_FILE> [--work-dir DIR] [--runner-user USER]
  [--env-file FILE] [--node PATH] [--store DIR] [--result FILE]`. The
  exit code is 0 when the job's conclusion is `success`, 1 otherwise,
  and 2 when the document cannot be read.

- [ ] **Step 1: Write the failing tests** in `tests/exec.rs`, running
  the binary with `--work-dir` in a temp dir, `--runner-user ""`,
  `--env-file` pointing at a fixture with `PATH=/usr/bin:/bin`:
  - `empty_job_succeeds`: a document whose job has no steps exits 0,
    and `--result` holds `{"conclusion":"success","outputs":{},"steps":[],"annotations":[]}`.
  - `layout_is_created`: after the run, `<work>/repo/repo`,
    `<work>/_temp` and `<work>/_actions` exist for
    `github.repository = "acme/repo"`.
  - `bad_document_exits_2`: a file holding `{}` exits 2 and stderr
    names the missing field.
  - `approval_job_is_refused`: a document whose job is
    `JobKind::Approval` exits 2 with
    `approval jobs do not run on a runner`.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent --test exec`.
  Expected: FAIL.
- [ ] **Step 3: Implement** the subcommand, `Layout`, `TextSink` and a
  `run_job` that walks no steps yet. `exec` installs the same SIGTERM
  handler as the runner path and passes it as `cancel`. Add a
  `tests/fixtures/jobs/make.rs` helper module (not a fixture file per
  test) that builds `JobDocument`s in code.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): add the exec subcommand`.

---

### Task 3: Output lines, masking and workflow commands

**Files:**
- Create: `crates/hatchrun-agent/src/exec/mask.rs`,
  `crates/hatchrun-agent/src/exec/commands.rs`,
  `crates/hatchrun-agent/src/exec/lines.rs`

**Interfaces:**
- Produces:

```rust
// mask.rs
pub struct Masker { .. }
impl Masker {
    pub fn add(&mut self, value: &str);      // also adds encoded forms, each line
    pub fn mask<'a>(&self, line: &'a str) -> Cow<'a, str>;
}
// lines.rs: splits a byte stream into lines of at most MAX_LINE bytes
pub const MAX_LINE: usize = 64 << 10;
pub struct Lines { .. }
impl Lines { pub fn push(&mut self, bytes: &[u8]) -> Vec<String>;
             pub fn finish(self) -> Option<String>; }
// commands.rs
pub enum Command { AddMask(String), Annotation(Annotation), Debug(String),
                   Group(String), EndGroup, StopCommands(String), Resume(String),
                   Echo(bool), SetOutput { name: String, value: String },
                   Refused(&'static str) }
pub fn parse(line: &str) -> Option<Command>;
```

- [ ] **Step 1: Write the failing tests** (unit tests in each module):
  - `masks_across_writes`: `Lines::push(b"tok")`, `push(b"en=s3c")`,
    `push(b"ret\n")` gives one line, which masks to `token=***` with
    `s3cret` added. Masking runs on complete lines, so a secret split
    across writes is whole when it is masked.
  - `masks_each_line_of_a_multiline_secret`: adding `"a1b2\nc3d4"`
    masks `a1b2` and `c3d4` on separate lines.
  - `masks_encoded_forms`: adding `p@ss"w` masks `cEBzcyJ3`
    (base64) and `p@ss\"w` (JSON-escaped).
  - `longest_secret_wins`: with `abc` and `abcdef` added,
    `xabcdefx` masks to `x***x`.
  - `add_mask_line_is_not_printed`: `parse("::add-mask::v")` is
    `Some(AddMask("v"))`. Task 6 pins that the executor never prints
    the line.
  - `long_line_is_split`: pushing 200 KiB without a newline yields
    three 64 KiB lines and the rest on `finish`; a multi-byte UTF-8
    character is never cut, and invalid UTF-8 becomes U+FFFD.
  - `parses_annotation_properties`:
    `::error file=a.rs,line=3,col=7,title=Bad%2C really::boom%0Anext`
    gives an error annotation with `title = "Bad, really"` and message
    `"boom\nnext"`.
  - `stop_commands_until_token`: after `::stop-commands::tok`,
    `::add-mask::x` is plain text until `::tok::`.
  - `set_env_and_add_path_are_refused`: `::set-env name=A::b` and
    `::add-path::/x` give `Refused` with GitHub's message that the
    command is disabled and names the file to use instead.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent exec::`. Expected: FAIL.
- [ ] **Step 3: Implement.** The masker ignores empty values and values
  made only of whitespace. Replacement is longest match first, scanning
  left to right; use one Aho-Corasick-style pass or a sorted list,
  whichever stays linear in the line for up to 1,000 secrets.
  Unescaping follows GitHub: data `%25 %0D %0A`, properties also
  `%3A %2C`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): mask secrets and parse workflow commands`.

---

### Task 4: File commands

**Files:**
- Create: `crates/hatchrun-agent/src/exec/files.rs`

**Interfaces:**
- Produces:

```rust
pub struct StepFiles { pub env: PathBuf, pub output: PathBuf, pub state: PathBuf,
                       pub path: PathBuf, pub summary: PathBuf }
impl StepFiles { pub fn create(temp: &Path, step: usize) -> anyhow::Result<Self>; }
pub struct Written { pub env: Vec<(String, String)>, pub outputs: Vec<(String, String)>,
                     pub state: Vec<(String, String)>, pub path: Vec<String>,
                     pub summary: String, pub warnings: Vec<String> }
pub fn read(files: &StepFiles) -> Result<Written, FileError>;  // FileError names file and line
pub const MAX_SUMMARY: usize = 1 << 20;
```

- [ ] **Step 1: Write the failing tests:**
  - `reads_name_value_and_heredoc`: `A=1\nB<<EOF\nx\ny\nEOF\n` gives
    `A=1` and `B="x\ny"`.
  - `heredoc_without_end_fails`: `B<<EOF\nx\n` fails naming the env
    file and line 1.
  - `value_holding_the_delimiter_ends_early`: `B<<EOF\nEOF\nEOF\n`
    gives `B=""` and then fails on the stray `EOF` line, as GitHub
    does.
  - `node_options_is_refused`: `NODE_OPTIONS=--require x` is dropped
    with a warning.
  - `path_lines_prepend_in_order`: `/a\n/b\n` gives `["/a", "/b"]`;
    the executor puts `/b` before `/a`, then the old `PATH`, matching
    GitHub's prepend-each-line behaviour.
  - `summary_over_the_cap_is_dropped`: a summary over 1 MiB gives a
    warning and an empty summary.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent exec::files`. Expected: FAIL.
- [ ] **Step 3: Implement.** Each step gets fresh files under
  `RUNNER_TEMP/_runner_file_commands/`. Each file is read with a 16 MiB
  cap; larger fails the step.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): read GITHUB_ENV, OUTPUT, STATE, PATH and summary files`.

---

### Task 5: Contexts and `hashFiles`

**Files:**
- Create: `crates/hatchrun-agent/src/exec/context.rs`

**Interfaces:**
- Consumes: `hatchrun_workflow::{evaluate, interpolate, truthy, Functions, JobStatus}`,
  `hatchrun_workflow::trigger::Filter` (glob with `!` negation).
- Produces:

```rust
pub struct Contexts { .. }                  // github, env, matrix, needs, steps,
                                            // secrets, vars, runner, job, inputs
impl Contexts {
    pub fn new(doc: &JobDocument, layout: &Layout) -> Self;
    pub fn value(&self) -> &Value;           // the object evaluate() takes
    pub fn set_env(&mut self, env: &BTreeMap<String, String>);
    pub fn record_step(&mut self, id: &str, outcome: Conclusion,
                       conclusion: Conclusion, outputs: &BTreeMap<String, String>);
    pub fn set_status(&mut self, status: JobStatus);
    pub fn with_action(&self, action: ActionScope) -> Contexts; // composite and JS scope
}
pub struct Runtime<'a> { pub workspace: &'a Path, pub status: JobStatus }
impl Functions for Runtime<'_> { .. }
pub fn condition(expr: Option<&str>, ctx: &Contexts, rt: &Runtime) -> Result<bool, ExprError>;
```

- [ ] **Step 1: Write the failing tests:**
  - `github_context_has_the_runner_fields`: `github.workspace`,
    `github.event.pull_request.number`, `github.ref`, `github.run_id`
    read from a document.
  - `runner_context`: `runner.os == 'Linux'`, `runner.arch` is `X64`
    or `ARM64` from the template, `runner.temp` is the layout's temp.
  - `missing_step_reads_null`: `steps.nope.outputs.x` is `null`, so
    `${{ steps.nope.outputs.x }}` interpolates to `""`.
  - `default_condition_is_success`: `condition(None, ..)` is false
    once status is `Failure`; `always()` is true; a condition without
    a status function is wrapped as `success() && (...)`, decided with
    `checks_status`.
  - `hash_files_matches_github`: for files `a.lock` = `x` and
    `b/c.lock` = `y`, `hashFiles('**/*.lock')` is the hex SHA-256 of the
    concatenated raw SHA-256 digests of each file in path order; no
    match gives `""`; `!b/**` excludes `b/c.lock`; a path outside the
    workspace is never read.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent exec::context`. Expected: FAIL.
- [ ] **Step 3: Implement.** `parameters` is an empty object: the
  compiler substitutes every `${{ parameters.x }}` and refuses any other
  read, so run time has none. `secrets` includes `GITHUB_TOKEN` set to
  `github.token`. `hashFiles` walks the workspace without following
  symlinks out of it, stops after 100,000 files with an error, and
  matches paths relative to the workspace with `Filter`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): build expression contexts and hashFiles`.

---

### Task 6: `run:` steps

**Files:**
- Create: `crates/hatchrun-agent/src/exec/shell.rs`,
  `crates/hatchrun-agent/src/exec/step.rs`
- Modify: `crates/hatchrun-agent/src/exec/mod.rs`,
  `crates/hatchrun-agent/src/runner.rs` (make `run_as` and the
  environment reader `pub(crate)`)

**Interfaces:**
- Produces:

```rust
// shell.rs
pub struct Shell { pub program: String, pub args: Vec<String>, pub extension: &'static str }
pub fn resolve(shell: Option<&str>, has_bash: bool) -> Result<Shell, String>;
// step.rs: runs one process and feeds its output through Lines, commands, Masker, Sink
pub struct Process<'a> { pub program: &'a str, pub args: &'a [String], pub cwd: &'a Path,
                         pub env: &'a BTreeMap<String, String>, pub timeout: Option<Duration> }
pub enum Exit { Code(i32), TimedOut, Cancelled }
pub async fn spawn(p: Process<'_>, out: &mut StepOutput<'_>,
                   cancel: &mut (impl Future<Output = ()> + Unpin)) -> anyhow::Result<Exit>;
```

  Shells, from GitHub's table: unset is `bash -e {0}` when bash exists,
  else `sh -e {0}`; `bash` is `bash --noprofile --norc -eo pipefail {0}`;
  `sh` is `sh -e {0}`; `python` is `python {0}`; `pwsh` is
  `pwsh -command ". '{0}'"`; any other value must contain `{0}`.

- [ ] **Step 1: Write the failing tests** (`tests/exec.rs`):
  - `run_step_output_and_exit`: `echo hi; exit 3` prints `hi`, the step
    is `failure`, the job is `failure`, exit code 1.
  - `env_layers`: job env `A=job`, step env `A=step`, `B=${{ env.A }}`
    prints `step job` for `echo $A $B` (step env sees job env).
  - `github_env_and_path_reach_later_steps`: step 1 writes
    `X=1` to `$GITHUB_ENV` and `/opt/x` to `$GITHUB_PATH`; step 2
    prints `1` and a `PATH` that starts with `/opt/x`.
  - `step_outputs`: step `id: a` writes `v=7` to `$GITHUB_OUTPUT`;
    step 2 runs `echo ${{ steps.a.outputs.v }}` and prints `7`.
  - `continue_on_error`: a failing step with `continue-on-error`
    records outcome `failure`, conclusion `success`, and the job
    succeeds.
  - `if_always_runs_after_failure`: after a failing step, a step with
    `if: always()` runs and a step with no `if` is `skipped`.
  - `timeout_kills_the_process_group`: `trap '' TERM; sleep 600` with
    `timeout-minutes: 1` and `--minute 2s` ends within 15 seconds as `failure`, and a following
    `if: always()` step runs.
  - `background_child_does_not_hold_the_step`:
    `sleep 600 & echo done` finishes in under 5 seconds; the
    background `sleep` is killed when the step ends.
  - `working_directory_and_custom_shell`: `working-directory: sub`
    and `shell: python3 {0}` run `import os; print(os.getcwd())` and
    print `<workspace>/sub`.
  - `add_mask_hides_later_output`: `echo "::add-mask::v4lue"; echo v4lue`
    prints `***` once and no line containing `add-mask` or `v4lue`.
  - `bad_expression_fails_the_step`: `run: echo ${{ nope( }}` fails
    the step with a message containing the expression error offset.
- [ ] **Step 2: Run** the tests. Expected: FAIL.
- [ ] **Step 3: Implement.** Add a hidden `exec` flag
  `--minute <DURATION>` (default `60s`) that sets how long one
  `timeout-minutes` minute lasts, so tests can time out in seconds.
  The script is interpolated, written to
  `RUNNER_TEMP/<uuid>.<ext>`, and run with `setsid` so the step owns a
  process group. When the main process exits, the group is killed and
  output is read until EOF or 1 second, whichever comes first. Timeout
  and cancel follow GitHub: SIGINT to the group, 7.5 s, SIGTERM,
  2.5 s, SIGKILL. The step environment is: the env file, then the
  `GITHUB_*`/`RUNNER_*`/`CI=true`/`GITHUB_ACTIONS=true` variables,
  then `GITHUB_ENV` values so far, then job `env`, then step `env`.
  `PATH` is the `GITHUB_PATH` entries in front of the env file's.
  Lines go through `commands::parse`, then `Masker::mask`, then the
  sink; command lines that change state are not printed, and
  annotations are also printed as `Error: <message>`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): run shell steps`.

---

### Task 7: Fetching actions

**Files:**
- Create: `crates/hatchrun-agent/src/exec/action.rs`,
  `crates/hatchrun-agent/src/exec/archive.rs`
- Modify: `crates/hatchrun-agent/Cargo.toml` (add `tar`, `serde-saphyr`)

**Interfaces:**
- Produces:

```rust
// action.rs
pub enum ActionRef { Remote { owner: String, repo: String, path: String, git_ref: String },
                     Local(PathBuf) }
pub fn parse_ref(uses: &str) -> Result<ActionRef, String>;
pub struct Action { pub dir: PathBuf, pub meta: Metadata }
pub struct Metadata { pub name: String, pub inputs: BTreeMap<String, Input>,
                      pub outputs: BTreeMap<String, Output>, pub runs: Runs }
pub enum Runs { Node { version: String, main: String, pre: Option<String>, pre_if: Option<String>,
                       post: Option<String>, post_if: Option<String> },
                Composite { steps: Vec<CompositeStep> },
                Docker }
pub async fn fetch(r: &ActionRef, github: &Github, layout: &Layout) -> anyhow::Result<Action>;
// archive.rs
pub fn extract_tar_gz(src: impl Read, dest: &Path, strip: usize, max_bytes: u64) -> anyhow::Result<()>;
pub fn create_tar_gz(root: &Path, paths: &[String], dest: impl Write) -> anyhow::Result<u64>;
```

- [ ] **Step 1: Write the failing tests:**
  - `parses_refs`: `actions/checkout@v5`,
    `github/codeql-action/init@v3` (path `init`), `./.github/act`;
    `actions/checkout` without `@` is an error naming the step.
  - `downloads_and_caches` (wiremock): `GET /repos/o/r/tarball/v1`
    with `Authorization: Bearer <token>` serves a tarball whose top
    directory is stripped; a second fetch of `o/r@v1` makes no request.
  - `reads_action_yaml_or_yml`: either file name works; neither is an
    error naming both.
  - `docker_action_is_runs_docker`: `runs.using: docker` parses to
    `Runs::Docker`.
  - `rejects_entries_outside_the_root`: entries `../x`, `/etc/x`, and a
    symlink `l -> /etc` followed by `l/passwd` each fail and write
    nothing outside `dest`.
  - `stops_at_the_size_cap`: an archive that expands past `max_bytes`
    fails with a message naming the cap.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent exec::action exec::archive`.
  Expected: FAIL.
- [ ] **Step 3: Implement.** Downloads go to
  `<github.api_url>/repos/<owner>/<repo>/tarball/<ref>`, follow
  redirects, retry 3 times on 5xx or connect errors, and are capped at
  1 GiB compressed and 4 GiB expanded. `action.yml` is read with a
  1 MiB cap. Local actions are read from the workspace and must stay
  inside it.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): fetch actions and read action.yml`.

---

### Task 8: JavaScript actions

**Files:**
- Create: `crates/hatchrun-agent/src/exec/node.rs`
- Modify: `crates/hatchrun-agent/src/exec/mod.rs`

**Interfaces:**
- Consumes: `fetch`, `Runs::Node` (Task 7), `spawn` (Task 6), `Contexts::with_action` (Task 5).
- Produces: `pub fn node_binary(opts: &Options, version: &str) -> anyhow::Result<PathBuf>`;
  post steps queued on the job and run in reverse order after the
  main steps.

- [ ] **Step 1: Write the failing tests** (`tests/exec.rs`, skipped
  with a printed reason when no `node` is found):
  - `js_action_inputs_and_outputs`: a local action whose `index.js`
    prints `process.env.INPUT_WHO_IS` and appends `greeting=hi` to
    `GITHUB_OUTPUT`; called with `with: { who is: me }` it prints `me`,
    and a later step reads `steps.g.outputs.greeting`.
  - `input_defaults_are_evaluated`: an input default
    `${{ github.repository }}` reaches the action as `acme/repo`.
  - `pre_and_post_order`: two actions with `pre` and `post` print
    `pre1 pre2 main1 main2 post2 post1`, and `post` reads the state
    `main` saved through `GITHUB_STATE` as `STATE_x`.
  - `post_runs_after_failure`: a failing later step still runs the
    earlier action's `post` (default `post-if: always()`).
  - `docker_action_fails_the_step`: a local Docker action fails with
    the Docker-phase message from the Global Constraints.
- [ ] **Step 2: Run** the tests. Expected: FAIL.
- [ ] **Step 3: Implement.** Input names map to
  `INPUT_<NAME>` uppercased with spaces as `_`. `with:` values are
  interpolated; non-string values are written as their JSON text, as
  GitHub does. Unknown inputs print a warning; missing required inputs
  without a default print a warning. `node12`, `node16` and `node20`
  run on `node20` when present, everything else on `node24`. The binary
  is `--node` if given, else
  `/home/runner/externals/node<v>/bin/node`, else `node` on `PATH`.
  `github.action_path`, `GITHUB_ACTION_PATH`, `github.action` and
  `GITHUB_ACTION` are set per step.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): run javascript actions`.

---

### Task 9: Composite actions

**Files:**
- Create: `crates/hatchrun-agent/src/exec/composite.rs`

**Interfaces:**
- Consumes: Tasks 5 to 8.
- Produces: `Runs::Composite` steps run through the same step runner,
  with their own `steps` context and `inputs`.

- [ ] **Step 1: Write the failing tests:**
  - `composite_inputs_steps_outputs`: a composite with input `n`,
    a step `id: s` writing `v=${{ inputs.n }}0` to `GITHUB_OUTPUT`, and
    output `out: { value: ${{ steps.s.outputs.v }} }`; called with
    `n: 4`, the caller reads `steps.c.outputs.out == '40'`.
  - `composite_steps_context_is_scoped`: a composite step cannot read
    the caller's `steps`, and the caller cannot read the composite's.
  - `composite_run_needs_shell`: a composite `run` step without
    `shell` fails with GitHub's message that `shell` is required.
  - `composite_calls_a_js_action`: a composite that `uses` a local JS
    action runs it, and that action's `post` runs at job end.
  - `nesting_is_capped`: a composite that uses itself fails at depth
    10 with a message naming the action.
- [ ] **Step 2: Run** the tests. Expected: FAIL.
- [ ] **Step 3: Implement.** Composite `if:` and `continue-on-error`
  work per inner step. The composite step's outcome is `failure` when
  any inner step's conclusion is `failure`. `env` from the composite
  step applies to every inner step.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): run composite actions`.

---

### Task 10: Built-in steps

**Files:**
- Create: `crates/hatchrun-agent/src/exec/builtin.rs`,
  `crates/hatchrun-agent/src/exec/store.rs`

**Interfaces:**
- Consumes: `create_tar_gz`, `extract_tar_gz` (Task 7).
- Produces:

```rust
pub trait Store {
    async fn get(&self, key: &StoreKey) -> anyhow::Result<Option<Box<dyn Read + Send>>>;
    async fn put(&self, key: &StoreKey, file: &Path) -> anyhow::Result<()>;
    async fn find_prefix(&self, kind: Kind, prefix: &str) -> anyhow::Result<Option<String>>;
}
pub enum Kind { Cache, Workspace, TestResults }
pub struct StoreKey { pub kind: Kind, pub scope: String, pub name: String }
pub struct DirStore { pub root: PathBuf }   // <root>/<kind>/<scope>/<name>.tar.gz
```

  Scopes: a cache is scoped to the repository; a workspace and test
  results to `<run_id>`, named by the job's `PlannedJob::name`.

- [ ] **Step 1: Write the failing tests:**
  - `cache_restore_then_save`: first job, key `k1`, path `target`:
    restore misses, the job writes `target/a`, and at job end `k1` is
    saved. A second job restores `target/a`.
  - `cache_restore_keys_take_the_newest_prefix_match`:
    `restore-keys: [deps-]` restores the most recently saved `deps-*`.
  - `cache_not_saved_on_failure_or_exact_hit`: a failed job saves
    nothing; an exact hit saves nothing.
  - `workspace_persist_attach`: job `build` persists `root: .`,
    `paths: [dist]`; job `test`, which requires it, attaches at
    `ws` and sees `ws/dist/x`. Attach without anything persisted is a
    no-op with a printed note.
  - `test_results_are_stored`: a `test-results` step pointing at a
    directory of `*.xml` stores them; a path with nothing fails the
    step with a message naming the path.
  - `no_store_fails_the_builtin_step`: without `--store` a `cache:`
    step fails with `this agent has no store for cache steps`.
- [ ] **Step 2: Run** the tests. Expected: FAIL.
- [ ] **Step 3: Implement.** Keys and paths are interpolated. Paths
  must stay inside the workspace after `..` and symlinks are resolved.
  Archives are written to `RUNNER_TEMP` first, then `put`. Cache saves
  run after post steps, in step order.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): add cache, workspace and test-results steps`.

---

### Task 11: Test splitting

**Files:**
- Create: `crates/hatchrun-agent/src/split.rs`
- Modify: `crates/hatchrun-agent/src/main.rs`, `crates/hatchrun-agent/src/exec/mod.rs`

**Interfaces:**
- Consumes: `hatchrun_workflow::split`, `PlannedJob::node`,
  `JobDocument::timings`.
- Produces: `hatchrun-agent split --by timings [ITEM...]`. `exec`
  writes `doc.timings` to `RUNNER_TEMP/hatchrun-timings.json` and sets
  `HATCHRUN_TIMINGS`, `HATCHRUN_NODE_INDEX` and `HATCHRUN_NODE_TOTAL`
  for every step of a parallel node.

- [ ] **Step 1: Write the failing tests** (`tests/split.rs`, running the binary):
  - `splits_by_timing`: index 0 of 2 with timings `a=10,b=1,c=1`
    prints `a` and index 1 prints `b\nc`, one item per line.
  - `reads_items_from_stdin`: with no `ITEM` arguments, items come
    from stdin, one per line, blank lines ignored.
  - `outside_a_parallel_job_prints_everything`: with no
    `HATCHRUN_NODE_*` variables every item prints.
  - `missing_timings_file_splits_evenly`: no `HATCHRUN_TIMINGS` gives
    the same split as empty timings.
  - `only_timings_is_supported`: `--by name` exits 2 naming `timings`.
- [ ] **Step 2: Run** `cargo test -p hatchrun-agent --test split`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): split tests across parallel nodes`.

---

### Task 12: Finishing a job

**Files:**
- Modify: `crates/hatchrun-agent/src/exec/mod.rs`,
  `crates/hatchrun-agent/src/main.rs`
- Test: `crates/hatchrun-agent/tests/exec.rs`

**Interfaces:**
- Consumes: everything above; `cache::Proxy` for
  `JobDocument::cache`.
- Produces: the final `JobResult`.

- [ ] **Step 1: Write the failing tests:**
  - `job_outputs_are_evaluated`: job `outputs: { v: ${{ steps.a.outputs.v }} }`
    lands in the result.
  - `job_if_false_skips_everything`: job `if: false` gives conclusion
    `skipped` and runs no step.
  - `cancel_runs_always_steps`: SIGTERM to `exec` while step 1 sleeps
    ends step 1 as `cancelled`, runs a later `if: always()` step and a
    queued `post`, and the job is `cancelled`.
  - `job_timeout`: job `timeout-minutes: 1` with `--minute 3s` cancels
    the running step and the job is `failure`.
  - `secret_never_printed`: a document with secret `hunter2hunter2`
    whose steps `echo` it plainly, from a JS action, from a composite,
    split across `printf` calls, and inside `::error::`; neither
    stdout nor the result file contains it.
  - `annotations_in_result`: `::warning file=a,line=2::w` lands in
    `annotations`.
  - `cache_service_starts_the_proxy` (wiremock as the cache service):
    with `cache` set, steps see `ACTIONS_RESULTS_URL` pointing at the
    proxy and `ACTIONS_CACHE_SERVICE_V2=true`; without it, neither is
    set.
- [ ] **Step 2: Run** the tests. Expected: FAIL.
- [ ] **Step 3: Implement.** The job `if` is evaluated first with
  `needs` from the document. Job timeout and SIGTERM both cancel the
  running step through the same path as a step timeout. After the main
  steps: post steps in reverse order, then cache saves, then job
  outputs, masked. The proxy listens on `127.0.0.1` and
  `ACTIONS_RUNTIME_TOKEN` is set to a random per-job value, which the
  proxy does not check for cache calls.
- [ ] **Step 4: Run** `cargo test -p hatchrun-agent` and
  `cargo clippy --locked --all-targets -- -D warnings`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(agent): finish jobs, cancel them and report results`.
