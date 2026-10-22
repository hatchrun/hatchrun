# Orchestrator Plan 4: Accounts

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every runner template in the spec's table launches, a warm
pool per template shortens starts, jobs get the organization and
repository secrets and vars they reference and a `GITHUB_TOKEN` scoped
by `permissions:`, and an organization pays per minute with a card on
file, inside its free minutes, concurrency and spend caps.

**Architecture:** Prices and caps live in `hatchrun-workflow`'s
template table. The format crate gains `permissions:` and a scan for
the secrets a job references; both are resolved when a run is planned
and stored on the job row. The launch consumer reads the tenant's
billing standing before it claims a launch, holds the job queued with
a reason when a cap binds, and takes a stopped pool instance of the
job's template when one is ready. The reconciler re-checks held jobs,
keeps each template's pool at the size recent demand asks for (with
the pool logic moved out of `scaler-core`'s `Scaler`), sends usage to
Polar and re-seals secrets after a key rotation. Secret values are
sealed with AES-256-GCM under a key from Cloudflare Secrets Store; the
Worker's claim handler opens the referenced ones and adds them to the
job document after the run object built it, so values never reach the
object, its snapshots, logs or check runs.

**Tech Stack:** Rust 1.99 (edition 2024), `worker` 0.8.7 (Secrets
Store binding `Env::secret_store`, Queues, KV, Durable Objects),
`aes-gcm` 0.10.3 (already a `hatchrun-cloud` dependency), `base64`
0.23.1, `tokio-postgres` through `hatchrun_cloud::db`, Polar (the
billing provider hatchrun.io uses, `hatchrun_cloud::polar`),
`hatchrun-scaler-core`, React app in `web/app`.

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`
(sections "Compute" Phase 1, "Calls made in writing this up" (runner
templates, larger sizes, accounts), "Secrets, the GitHub App and
billing"). Plan 3, `docs/superpowers/plans/2026-10-22-orchestrator-control-plane.md`,
is built on branch `feat/control-plane` and this plan builds on it.

## Scope against the other plans

- **In:** the eight runner templates, a warm pool per template,
  organization and repository secrets and vars, `permissions:` for the
  job token, the card requirement, free minutes, the concurrency and
  spend caps, metering and Polar usage, removing `tenants.ci_enabled`,
  and the end-to-end script's new scenarios.
- **Not here:** per-tenant retention (Plan 3 left the enterprise 90
  days to Plan 4; this plan's scope does not list it, so rows and logs
  stay at 30 days).
- **Plan 5:** cache, artifacts, test timings (`timings` stays empty),
  approval jobs.
- **Later phases:** Oracle and microVM backends.

## Rulings

Made by the controller before this plan; the plan keeps them unless the
line says otherwise.

1. Secret values never touch logs, check runs, R2 state snapshots or
   the Durable Object's persisted state. **Checked against the code:**
   Plan 3 builds the job document at claim time in `RunObject::claim`
   (`run/decide.rs::job_document`) and does not store it. Plan 4 keeps
   that and goes further: the object still answers with empty `secrets`
   and `vars`, and the Worker's claim handler (`agent.rs`) fills them
   after the object answered, so no value enters the object at all.
   The object keeps the job's `GITHUB_TOKEN` in `JobState.github_token`,
   as Plan 3 does, to revoke it when the job ends; it is not in the
   graph or the R2 snapshot.
2. Minutes are counted from claim (`ci_jobs.started_at`) to result
   (`completed_at`), rounded up per started minute with at least one,
   as `hatchrun_cloud::usage::minutes` does. A job that was never
   claimed costs nothing.
3. Caps are checked in the launch consumer before an instance starts.
   A held job stays `queued` with `next_launch_at` 30 s out, and the
   reconciler's existing requeue of due queued jobs re-checks it.
4. Prices and cap amounts live in one place:
   `crates/workflow/src/templates.rs`, with the spec's values. SQL
   receives them as bound parameters.
5. **Added:** a `runs-on` label that names no template is caught when
   the config compiles, because labels are fixed once matrices expand
   at compile time. Nothing launches, and the existing
   `hatchrun / config` check run carries the message that lists every
   template. Plan 3's `planning::launchable` gate goes away.
6. **Added:** the 500 free minutes are minutes of the 2 vCPU template,
   so $1.00 of usage a month; a 4 vCPU minute uses two of them. The
   billing unit everywhere (Polar's meter, the cap) is one 2 vCPU
   minute, $0.002.
7. **Added:** "a card on file" is an `active` or `trialing` Polar
   subscription to a new CI product (`POLAR_CI_PRODUCT_ID`). Polar
   collects the card at checkout. `subscriptions` gains a `product`
   column so a CI subscription never licenses hatchrun.io runners and
   the reverse. `tenants.beta` does not apply to CI.
8. **Added:** a tenant without a card gets no run. A push or pull
   request gets one `hatchrun / billing` check run with conclusion
   `action_required`; schedules fire nothing. A tenant whose
   subscription ends after its runs were created has its queued jobs
   held with the card message.
9. **Added:** the spend cap counts the minutes of completed jobs this
   UTC month; jobs running when it is reached finish. The owner sets it
   in whole dollars from $50 to $10,000. The concurrency cap is a fixed
   5 jobs in `launching`, `launched` or `running`.
10. **Added:** the warm pool is shared by all tenants, one per template,
    and runs only with the EC2 backend; the local launcher has none, so
    the end-to-end script does not cover it.
11. **Added:** a job with no `permissions:` (at job or workflow level)
    keeps Plan 3's read-only token. `permissions: { id-token: write }`
    is refused at parse: OIDC tokens come in a later release.
12. **Added:** organization secrets and vars apply to every repository
    of the organization; a repository value of the same name wins.
    Every var goes in the job document (vars are not secret); secrets
    go only when referenced. Job-level `if:` still reads only `github`
    and `needs`.

## Global Constraints

<!-- REUSE-IgnoreStart -->
- Every source file starts with the SPDX header used across the repo:
  `SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>`
  and `SPDX-License-Identifier: AGPL-3.0-only`.
<!-- REUSE-IgnoreEnd -->
- CI runs `cargo clippy --locked --all-targets -- -D warnings` and
  `cargo test --locked` with `DATABASE_URL` pointing at Postgres 17.
  `hatchrun-ci`, `hatchrun-cloud` (with and without default features)
  and `hatchrun-workflow` build for `wasm32-unknown-unknown`.
- Runner templates, exactly the spec's table, 4 GB per vCPU, arm64 at
  the amd64 price:

  | `runs-on` | Arch | vCPU | Memory | Price |
  | --- | --- | --- | --- | --- |
  | `ubuntu-latest`, `ubuntu-26.04` | amd64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-arm` | arm64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-4core`, `ubuntu-26.04-4core-arm` | amd64, arm64 | 4 | 16 GB | $0.004/min |
  | `ubuntu-26.04-8core`, `ubuntu-26.04-8core-arm` | amd64, arm64 | 8 | 32 GB | $0.008/min |
  | `ubuntu-26.04-16core`, `ubuntu-26.04-16core-arm` | amd64, arm64 | 16 | 64 GB | $0.016/min |

  Each launches hatchrun.io's built-in profile `hatchrun-<vcpu>cpu`
  (amd64) or `hatchrun-<vcpu>cpu-arm64`, on demand.
- Billing: 500 free 2 vCPU minutes per organization and UTC month, at
  most 5 jobs running at once, a $50 monthly spend cap the owner can
  raise to $10,000. A card is required before the first job.
- All of the amounts above are the constants of Task 1 and nowhere
  else.
- Secret values never appear in console output, check runs, R2
  objects, Durable Object storage or error messages. Errors name the
  secret, never the value. `JobDocument`'s `Debug` already redacts
  `secrets`.
- Secrets are sealed with AES-256-GCM; each row stores the id of the
  key that sealed it; the associated data binds the value to its
  tenant, scope and name.
- GitHub calls per job attempt stay as Plan 3 has them, plus one check
  run update when a job's hold reason changes and one when a held job
  is claimed.
- One migration file for this plan,
  `crates/cloud/migrations/0005_ci_accounts.sql`. Task 4 creates it and
  later tasks append to it. New columns on tables that may hold rows
  are added `NOT NULL DEFAULT <value>` and the default is dropped in
  the same file, so the column is required from then on.
- No backward compatibility shims: new fields are required, and the
  agent and both Workers deploy together.
- Commits are Conventional Commits, GPG-signed and DCO signed off
  (`git commit -S -s`), with no AI attribution.

## Review Focus

1. **A warming pool instance looks like an orphan.** It runs with no
   `hatchrun:job-id` tag, which Plan 3's orphan sweep reads as "no job
   needs it". The user expects the pool to survive the sweep. Pinned in
   Task 13 (`orphans_skip_pool_instances`).
2. **The reconciler trims a pool instance a launch just took.** A
   consumer takes the last ready instance while the reconciler, working
   from rows it read earlier, decides the pool is too big. The user
   expects the job's instance never to be terminated or stopped under
   it. Pinned in Task 13 (`trim_skips_a_taken_instance`,
   `save_pool_never_brings_back_a_taken_row`).
3. **A secret read by a computed name.** `secrets[matrix.key]` or
   `toJSON(secrets)` names no secret the scan can see. The user expects
   the job to get every secret of its repository and organization, not
   an empty value. Pinned in Task 3
   (`dynamic_index_sends_every_secret`, `to_json_of_secrets_sends_every_secret`).
4. **A claim in the middle of a key rotation.** Rows sealed with the
   previous key are re-sealed a batch per reconciler pass; a job
   claimed before its row is re-sealed must still get the value. Pinned
   in Task 9 (`opens_a_value_sealed_with_the_previous_key`).
5. **A secret named in another case.** GitHub secret names are case
   insensitive, so `secrets.npm_token` reads `NPM_TOKEN`. The user
   expects the value either way. Pinned in Task 3
   (`names_are_case_insensitive`) and Task 9 (`names_are_uppercased`).

## File map

| File | Responsibility |
| --- | --- |
| `crates/workflow/src/templates.rs` | templates, prices, caps, units |
| `crates/workflow/src/permissions.rs` | `permissions:` and token scopes |
| `crates/workflow/src/secrets.rs` | `SecretRefs`, the scan of a job |
| `crates/ci/src/billing.rs` | standing, admission, hold messages, cap bounds |
| `crates/ci/src/secrets.rs` | keyring, seal and open, names |
| `crates/ci/src/pool.rs` | pool target rule, rows to `scaler_core::pool::Pool` |
| `crates/ci/src/settings_api.rs` | `/api/ci/orgs/*` and repository secrets and vars routes |
| `crates/scaler-core/src/pool.rs` | the warm pool, out of `Scaler` |
| `crates/cloud/migrations/0005_ci_accounts.sql` | every schema change |

---

### Task 1 [cheap]: Prices and caps in one place

**Files:**
- Modify: `crates/workflow/src/templates.rs`, `crates/workflow/src/lib.rs`

**Interfaces:**
- Produces, in `hatchrun_workflow::templates` (re-exported at the
  crate root like `Template`):

```rust
/// The price of one 2 vCPU minute, the billing unit, in millionths of a dollar.
pub const UNIT_PRICE_MICROS: u64 = 2_000;
/// Free units (2 vCPU minutes) per organization and UTC month.
pub const FREE_UNITS: u64 = 500;
/// Jobs one organization may have launching or running at once.
pub const MAX_RUNNING_JOBS: u32 = 5;
/// The monthly spend cap until the owner changes it: $50.
pub const DEFAULT_SPEND_CAP_MICROS: u64 = 50_000_000;
/// Lowest and highest cap the owner may set: $50 and $10,000.
pub const MIN_SPEND_CAP_MICROS: u64 = 50_000_000;
pub const MAX_SPEND_CAP_MICROS: u64 = 10_000_000_000;

impl Template {
    /// Billing units of `minutes` on this template.
    pub fn units(&self, minutes: u64) -> u64;
}
/// What a month of `units` costs past the free units, in micros.
pub fn spend_micros(units: u64) -> u64;
/// `$50.00` for 50_000_000.
pub fn dollars(micros: u64) -> String;
```

- [ ] **Step 1: Write the failing tests** in `templates.rs`:

```rust
#[test] fn the_table_is_the_spec() {
    let rows: Vec<_> = TEMPLATES.iter()
        .map(|t| (t.labels.to_vec(), t.arch, t.vcpu, t.memory_gib, t.price_micros_per_minute)).collect();
    assert_eq!(rows, vec![
        (vec!["ubuntu-latest", "ubuntu-26.04"], Arch::Amd64, 2, 8, 2_000),
        (vec!["ubuntu-26.04-arm"], Arch::Arm64, 2, 8, 2_000),
        (vec!["ubuntu-26.04-4core"], Arch::Amd64, 4, 16, 4_000),
        (vec!["ubuntu-26.04-4core-arm"], Arch::Arm64, 4, 16, 4_000),
        (vec!["ubuntu-26.04-8core"], Arch::Amd64, 8, 32, 8_000),
        (vec!["ubuntu-26.04-8core-arm"], Arch::Arm64, 8, 32, 8_000),
        (vec!["ubuntu-26.04-16core"], Arch::Amd64, 16, 64, 16_000),
        (vec!["ubuntu-26.04-16core-arm"], Arch::Arm64, 16, 64, 16_000),
    ]);
}
#[test] fn prices_follow_vcpus() // every t: price == vcpu * UNIT_PRICE_MICROS / 2, memory == 4 * vcpu
#[test] fn units_scale_with_price() {
    assert_eq!(template("ubuntu-latest").unwrap().units(10), 10);
    assert_eq!(template("ubuntu-26.04-4core-arm").unwrap().units(10), 20);
    assert_eq!(template("ubuntu-26.04-16core").unwrap().units(3), 24);
}
#[test] fn spend_starts_after_the_free_units() {
    assert_eq!(spend_micros(0), 0);
    assert_eq!(spend_micros(500), 0);
    assert_eq!(spend_micros(600), 200_000);
}
#[test] fn dollars_has_cents() { assert_eq!(dollars(50_000_000), "$50.00"); assert_eq!(dollars(200_000), "$0.20"); }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow templates`
Expected: FAIL, `units`, `spend_micros`, `dollars` not defined.

- [ ] **Step 3: Implement** the constants and functions. The doc
  comment at the top of the module says this file holds every price
  and cap amount.

- [ ] **Step 4: Run the tests and the wasm build**

Run: `cargo test -p hatchrun-workflow templates && cargo check -p hatchrun-workflow --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow/src/templates.rs crates/workflow/src/lib.rs
git commit -S -s -m "feat(workflow): keep prices and caps in one place"
```

---

### Task 2 [standard]: `permissions:` for the job token

**Files:**
- Create: `crates/workflow/src/permissions.rs`
- Modify: `crates/workflow/src/config.rs` (`Job`, `Workflow`),
  `crates/workflow/src/plan.rs` (`PlannedJob`, compile),
  `crates/workflow/src/convert.rs`, `crates/workflow/src/lib.rs`
- Test: `crates/workflow/tests/plan.rs`, `crates/workflow/tests/convert.rs`

**Interfaces:**
- Produces, in `hatchrun_workflow::permissions` (types re-exported at
  the root):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level { None, Read, Write }

/// `permissions:` as written: `read-all`, `write-all` or a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permissions { ReadAll, WriteAll, Scopes(BTreeMap<String, Level>) }  // custom Deserialize

/// Workflow keys and the GitHub App permission each one asks for.
pub const SCOPES: &[(&str, &str)] = &[
    ("actions", "actions"), ("attestations", "attestations"), ("checks", "checks"),
    ("contents", "contents"), ("deployments", "deployments"), ("discussions", "discussions"),
    ("issues", "issues"), ("packages", "packages"), ("pages", "pages"),
    ("pull-requests", "pull_requests"), ("repository-projects", "repository_projects"),
    ("security-events", "security_events"), ("statuses", "statuses"),
];
/// The token without any `permissions:`: Plan 3's read set.
pub const DEFAULT_SCOPES: &[(&str, Level)] = &[
    ("checks", Level::Read), ("contents", Level::Read),
    ("metadata", Level::Read), ("pull_requests", Level::Read),
];
/// App permission names and levels for a job's token. Always holds
/// `metadata: read`; `none` levels are left out.
pub fn token_scopes(p: Option<&Permissions>) -> BTreeMap<String, Level>;
```

- `config::Job.permissions: Option<Permissions>` and
  `config::Workflow.permissions: Option<Permissions>`.
- `PlannedJob.token_permissions: BTreeMap<String, Level>`: the job's
  own `permissions` when set, else its workflow's, else the default,
  through `token_scopes`. Required in serde.

- [ ] **Step 1: Write the failing tests** in `tests/plan.rs` (compile a
  small config and read the job's `token_permissions`) and in
  `permissions.rs`:

```rust
#[test] fn no_permissions_is_the_read_default()
    // {checks: read, contents: read, metadata: read, pull_requests: read}
#[test] fn job_permissions_replace_the_workflow_ones()
    // workflow {contents: write, issues: write}, job {pull-requests: write}
    // -> {metadata: read, pull_requests: write}
#[test] fn workflow_permissions_apply_to_jobs_without_their_own()
#[test] fn read_all_reads_every_scope()   // 13 scopes read plus metadata read, 14 entries
#[test] fn write_all_writes_every_scope() // 13 write, metadata read
#[test] fn empty_map_leaves_metadata()    // {} -> {metadata: read}
#[test] fn none_level_drops_the_scope()   // {contents: none} -> {metadata: read}
#[test] fn unknown_scope_is_an_error()
    // message: "`permissions` names unknown scope `foo`; use one of actions, attestations, checks, contents, deployments, discussions, issues, packages, pages, pull-requests, repository-projects, security-events, statuses"
#[test] fn unknown_level_is_an_error()   // "`permissions.contents` is `admin`; use read, write or none"
#[test] fn id_token_comes_later()        // "`id-token` permission: OIDC tokens come in a later release"
```

  In `tests/convert.rs`:

```rust
#[test] fn permissions_carry_over()
    // GitHub workflow and job `permissions` land on the hatchrun workflow and job; no warning
#[test] fn id_token_is_dropped_with_a_warning()
    // {id-token: write, contents: read} -> {contents: read}, warning
    // "`id-token` is not supported and was dropped"
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow permissions`
Expected: FAIL, module not defined.

- [ ] **Step 3: Implement.** Deserialize `Permissions` from a string
  (`read-all`, `write-all`) or a map; collect errors as diagnostics in
  the parse step, the way `config.rs` reports other key errors. In
  `convert.rs`, remove `permissions` from the dropped-key lists at the
  workflow and job level and copy the value over, dropping `id-token`
  with the warning above.

- [ ] **Step 4: Run the workflow and CLI tests**

Run: `cargo test -p hatchrun-workflow && cargo test -p hatchrun-cli`
Expected: PASS. Existing tests that build `PlannedJob` literals gain
`token_permissions`.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow
git commit -S -s -m "feat(workflow): read permissions for the job token"
```

---

### Task 3 [standard]: The secrets a job references

**Files:**
- Create: `crates/workflow/src/secrets.rs`
- Modify: `crates/workflow/src/expr/mod.rs`, `crates/workflow/src/lib.rs`

**Interfaces:**
- Produces:

```rust
// hatchrun_workflow::SecretRefs
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRefs {
    /// Every secret is needed: a name the scan cannot see.
    pub all: bool,
    /// Uppercase names.
    pub names: BTreeSet<String>,
}
/// Read by every job when set, as on GitHub.
pub const DEBUG_SECRETS: [&str; 2] = ["ACTIONS_RUNNER_DEBUG", "ACTIONS_STEP_DEBUG"];

// hatchrun_workflow::expr
/// The secrets one text reads. `bare` reads the whole text as one
/// expression (an `if:`); otherwise only `${{ }}` segments count.
pub fn secret_refs(text: &str, bare: bool) -> Result<SecretRefs, ExprError>;

impl PlannedJob { pub fn secret_refs(&self) -> SecretRefs; }
```

  Walk the parsed tree: `Access { base: Context "secrets", path[0]:
  Property(name) }` and `Index(Literal(String))` add the name,
  uppercased; any other use of the `secrets` context sets `all`.
  `GITHUB_TOKEN` is never added (it is the job token).
  `PlannedJob::secret_refs` scans every string in the job's `env` and
  `outputs` and in each step's fields (including `with` values), reads
  the job's and each step's `if` bare, adds `DEBUG_SECRETS`, and treats
  a text that does not parse as naming nothing (compile already
  reported it).

- [ ] **Step 1: Write the failing tests** in `secrets.rs`:

```rust
#[test] fn named_reference()            // "${{ secrets.NPM_TOKEN }}" -> {NPM_TOKEN}
#[test] fn index_with_a_literal()       // "${{ secrets['A_B'] }}" -> {A_B}
#[test] fn names_are_case_insensitive() // "${{ secrets.npm_token }}" -> {NPM_TOKEN}
#[test] fn dynamic_index_sends_every_secret()      // "${{ secrets[matrix.key] }}" -> all
#[test] fn to_json_of_secrets_sends_every_secret() // "${{ toJSON(secrets) }}" -> all
#[test] fn bare_if_is_read()            // secret_refs("secrets.DEPLOY != ''", true) -> {DEPLOY}
#[test] fn github_token_is_not_a_stored_secret() // "${{ secrets.GITHUB_TOKEN }}" -> {}
#[test] fn string_literal_is_not_a_reference()   // "${{ 'secrets.X' }}" -> {}
#[test] fn job_scan_covers_env_steps_with_and_if()
    // job env A: ${{ secrets.ONE }}, step with: {t: ${{ secrets.TWO }}},
    // step if: secrets.THREE -> {ONE, TWO, THREE} plus DEBUG_SECRETS, all false
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow secrets`
Expected: FAIL.

- [ ] **Step 3: Implement** `expr::secret_refs` next to `checks_status`
  (same segment handling) and `PlannedJob::secret_refs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-workflow && cargo check -p hatchrun-workflow --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow
git commit -S -s -m "feat(workflow): find the secrets a job references"
```

---

### Task 4 [standard]: Launch every runner template

**Files:**
- Create: `crates/cloud/migrations/0005_ci_accounts.sql`
- Modify: `crates/ci/src/launcher.rs`, `crates/ci/src/launches.rs`,
  `crates/ci/src/planning.rs`, `crates/ci/src/graph.rs`,
  `crates/ci/src/store.rs` (`NewJob`, `JobRow`, `insert_jobs`,
  `JOB_COLUMNS`), `crates/ci/src/runs.rs` (`start_plan`),
  `crates/ci-dev/src/launcher.rs`

**Interfaces:**
- Consumes: Task 2 (`PlannedJob.token_permissions`), Task 3
  (`PlannedJob::secret_refs`), `hatchrun_cloud::profiles::built_in`.
- Produces:

```rust
// launcher.rs (PROFILE goes away)
/// The built-in profile a template launches: hatchrun-<vcpu>cpu, -arm64 on arm64.
pub fn profile_for(template: &Template) -> Profile;
pub struct LaunchSpec { pub job_id: u64, pub name: String, pub user_data: String,
    pub template: &'static Template }

// store.rs
pub struct NewJob { pub plan_id: String, pub name: String, pub timeout_minutes: u32,
    pub template: Option<String>,                 // first label; None for approval jobs
    pub token_permissions: BTreeMap<String, Level>,
    pub secret_refs: SecretRefs }
// JobRow gains: template: Option<String>, token_permissions: serde_json::Value,
// secret_refs: serde_json::Value (read as text like `steps`)
```

  `0005_ci_accounts.sql` starts with:

```sql
ALTER TABLE ci_jobs ADD COLUMN template text;   -- first runs-on label, NULL for approval jobs
ALTER TABLE ci_jobs ADD COLUMN token_permissions jsonb NOT NULL DEFAULT '{"metadata":"read"}';
ALTER TABLE ci_jobs ALTER COLUMN token_permissions DROP DEFAULT;
ALTER TABLE ci_jobs ADD COLUMN secret_refs jsonb NOT NULL DEFAULT '{"all":false,"names":[]}';
ALTER TABLE ci_jobs ALTER COLUMN secret_refs DROP DEFAULT;
```

  The local launcher's POST body gains `"template": "<label>"` and
  `hatchrun-ci-dev launcher` records it in the `launch` event of
  `/_events`.

- [ ] **Step 1: Write the failing tests**

```rust
// launcher.rs
#[test] fn every_template_has_a_built_in_profile()
    // for t in TEMPLATES: p = profile_for(t); p.arch matches t.arch,
    // p.cpu_min == p.cpu_max == Some(t.vcpu), p.memory_min == t.memory_gib,
    // p.capacity == OnDemand, p.categories == ["m"]
#[test] fn arm_names_the_arm64_profile() // profile_for(ubuntu-26.04-8core-arm).name == "hatchrun-8cpu-arm64"
// graph.rs (replaces unsupported_template_fails_before_launch)
#[test] fn every_template_launches()     // a job on ubuntu-26.04-16core-arm -> [Launch(id)]
// planning.rs (replaces other_templates_are_not_launchable)
#[test] fn unknown_label_lists_the_templates()
    // runs-on: ubuntu-24.04 -> Selected::Invalid with a message containing
    // "names no runner template; use one of ubuntu-latest, ubuntu-26.04, ubuntu-26.04-arm,
    //  ubuntu-26.04-4core, ubuntu-26.04-4core-arm, ubuntu-26.04-8core, ubuntu-26.04-8core-arm,
    //  ubuntu-26.04-16core, ubuntu-26.04-16core-arm"
// store.rs (Postgres)
#[test] fn insert_jobs_keeps_template_permissions_and_refs()
    // round trip through insert_jobs and job(); approval job has template None
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci launcher graph planning store`
Expected: FAIL.

- [ ] **Step 3: Implement.** `Ec2::launch` uses `profile_for(spec.template)`
  in place of `built_in(PROFILE)`. The launch consumer resolves the
  row's template with `hatchrun_workflow::template`; a row with no
  template or an unknown one ends the job through the run object with
  `job has no runner template`. Delete `planning::launchable` and the
  graph's gate. Drop the Plan 3 constraint line about `ubuntu-latest`
  from the crate docs that repeat it.

- [ ] **Step 4: Run the crate tests and the wasm build**

Run:
```bash
cargo test -p hatchrun-ci -p hatchrun-ci-dev && cargo test -p hatchrun-cloud
cargo check -p hatchrun-ci --target wasm32-unknown-unknown
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/ci-dev crates/cloud/migrations/0005_ci_accounts.sql
git commit -S -s -m "feat(ci): launch every runner template"
```

---

### Task 5 [standard]: A card is the gate

**Files:**
- Modify: `crates/cloud/migrations/0005_ci_accounts.sql`,
  `crates/cloud/src/polar.rs`, `crates/cloud/src/app/billing.rs`,
  `crates/cloud/src/usage.rs`, `crates/cloud/wrangler.toml`,
  `crates/ci/src/store.rs`, `crates/ci/src/webhook.rs`,
  `crates/ci/src/runs.rs`, `crates/ci/src/reconcile.rs`,
  `crates/ci/scripts/e2e.sh` (the tenant insert)

**Interfaces:**
- Consumes: Task 1 (`FREE_UNITS`).
- Produces:

```sql
-- appended to 0005
ALTER TABLE tenants DROP COLUMN ci_enabled;
ALTER TABLE subscriptions ADD COLUMN product text NOT NULL DEFAULT 'runners'
  CHECK (product IN ('runners', 'ci'));
ALTER TABLE subscriptions ALTER COLUMN product DROP DEFAULT;
```

```rust
// hatchrun_cloud::polar
/// `ci` for the CI product, `runners` for anything else.
pub fn product_kind(product_id: &str, ci_product_id: &str) -> &'static str;
// hatchrun_ci::store
pub struct TenantRow { pub id: String, pub org_login: String }
/// An active or trialing subscription to the CI product.
pub async fn has_card(db: &Db, tenant_id: &str) -> Result<bool>;
// hatchrun_ci::runs
pub const CARD_CHECK: &str = "hatchrun / billing";
pub fn no_card_check_run(sha: &str, org: &str, app_url: &str) -> CheckRun;
```

  `no_card_check_run`: status `completed`, conclusion
  `action_required`, `details_url` `{app_url}/o/{org}/ci`, title
  `hatchrun needs a card`, summary
  `Add a card for {org} at {app_url}/o/{org}/ci to run this commit's workflows. Each organization gets 500 free minutes a month.`
  (500 from `FREE_UNITS`).

- [ ] **Step 1: Write the failing tests**

```rust
// cloud usage.rs sql tests
#[test] fn a_ci_subscription_does_not_license_runners()
    // tenant with only a 'ci' active subscription past its free minutes: may_launch false
#[test] fn the_launch_rule_matches_the_constants() // updated: PAID holds "product = 'runners'"
// cloud polar.rs
#[test] fn product_kind_names_the_ci_product() // ("p-ci", "p-ci") -> "ci"; ("p-run", "p-ci") -> "runners"
// cloud app/billing.rs
#[test] fn webhook_stores_the_product()       // subscription for POLAR_CI_PRODUCT_ID stored with product 'ci'
#[test] fn ci_subscription_leaves_the_grace_alone() // a 'ci' cancel does not start the runners grace period
// ci store.rs
#[test] fn has_card_needs_an_active_ci_subscription()
    // none -> false; runners active -> false; ci canceled -> false; ci active -> true; ci trialing -> true
// ci runs.rs
#[test] fn no_card_check_run_asks_for_a_card() // name, conclusion, details_url and summary exactly as above
// ci webhook.rs (replaces disabled_tenant_is_ignored)
#[test] fn unknown_installation_is_ignored()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-cloud usage polar billing && cargo test -p hatchrun-ci store runs webhook`
Expected: FAIL.

- [ ] **Step 3: Implement.** `PAID` and `MAY_LAUNCH` in `usage.rs` add
  `AND subscriptions.product = 'runners'`. The Polar webhook in
  `app/billing.rs` (next to `PRODUCT_ID`) binds the product from
  `POLAR_CI_PRODUCT_ID` (new var in cloud's
  `wrangler.toml`, the CI product's id) and, for a `ci` subscription,
  stores it and answers `202` without the grace and recheck calls.
  `hatchrun-ci`'s webhook gate checks only that the tenant exists.
  `runs::create` calls `has_card` first; without one it reports
  `no_card_check_run` once per commit (list by `CARD_CHECK` first, as
  `report_invalid` does) and starts nothing. The reconciler's schedule
  step skips tenants without a card and posts nothing. The e2e script's
  tenant insert loses `ci_enabled` and gains a `subscriptions` row
  (`product 'ci'`, `status 'active'`).

- [ ] **Step 4: Run both crates' tests**

Run: `cargo test -p hatchrun-cloud && cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS; `rg ci_enabled crates` finds nothing.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud crates/ci
git commit -S -s -m "feat(ci): make a card the gate for running jobs"
```

---

### Task 6 [standard]: Meter minutes from claim to result

**Files:**
- Create: `crates/ci/src/billing.rs`
- Modify: `crates/cloud/migrations/0005_ci_accounts.sql`,
  `crates/ci/src/store.rs`, `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 (`Template::units`, `spend_micros`,
  `DEFAULT_SPEND_CAP_MICROS`), Task 4 (`ci_jobs.template`), Task 5
  (`has_card` SQL).
- Produces:

```sql
-- appended to 0005
CREATE TABLE ci_usage (
  job_id bigint NOT NULL,                  -- no FK: usage outlives the 30 day job rows
  attempt bigint NOT NULL,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  template text NOT NULL,
  minutes bigint NOT NULL CHECK (minutes > 0),
  ended_at bigint NOT NULL,
  sent_at bigint,                          -- when Polar took it
  PRIMARY KEY (job_id, attempt)
);
CREATE INDEX ci_usage_month ON ci_usage (tenant_id, ended_at);
CREATE INDEX ci_usage_unsent ON ci_usage (ended_at) WHERE sent_at IS NULL;
CREATE TABLE ci_spend_caps (
  tenant_id text PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
  cap_micros bigint NOT NULL CHECK (cap_micros > 0),
  updated_at bigint NOT NULL
);
```

```rust
// store.rs
/// A tenant's billing state this month, read for the tenant of `job_id`.
pub struct Standing { pub tenant_id: String, pub org_login: String, pub card: bool,
    pub cap_micros: u64, pub minutes_by_template: Vec<(String, u64)>, pub running: u32 }
pub async fn standing(db: &Db, job_id: u64, month_start: u64, default_cap: u64) -> Result<Option<Standing>>;
pub async fn tenant_standing(db: &Db, tenant_id: &str, month_start: u64, default_cap: u64) -> Result<Option<Standing>>;
// finish_job keeps its signature and records usage in the same transaction.
// billing.rs
pub fn month_start(now_ms: u64) -> u64;            // UTC calendar month
pub fn units(standing: &Standing) -> u64;          // sum of Template::units; unknown labels count 0
pub fn spend_micros(standing: &Standing) -> u64;   // templates::spend_micros(units)
```

  `finish_job` runs its update and this insert through `Db::batch`: a
  `ci_usage` row from the job row when `started_at` and `template` are
  set, minutes `GREATEST(1, CEIL((completed_at - started_at) / 60000.0))`,
  `ON CONFLICT DO NOTHING`. `running` counts the tenant's jobs in
  `launching`, `launched` or `running`. `cap_micros` is the
  `ci_spend_caps` row or `default_cap`. `store::sweep` also deletes
  `ci_usage` rows sent more than 90 days ago.

- [ ] **Step 1: Write the failing tests**

```rust
// store.rs (Postgres)
#[test] fn finish_job_records_minutes_from_claim() // start_job at t, finish at t + 61_000 -> 2 minutes, template label
#[test] fn a_job_never_claimed_is_not_metered()     // finish without start_job -> no row
#[test] fn finishing_twice_records_once()
#[test] fn each_attempt_is_metered()                // reset_job to attempt 2, run again -> two rows
#[test] fn minutes_round_up_like_usage_minutes()    // durations 0, 1, 60_000, 60_001 -> 1, 1, 1, 2
#[test] fn standing_counts_this_month_and_running_jobs()
    // usage rows on Sep 30 and Oct 2, month_start Oct 1 -> only Oct; two running jobs -> running 2;
    // another tenant's rows and jobs not counted; no cap row -> default_cap
// billing.rs
#[test] fn month_start_is_utc() // month_start(1_792_676_700_000) == 1_790_812_800_000 (2026-10-22T13:45Z -> 2026-10-01)
#[test] fn spend_counts_after_free_units()
    // ubuntu-latest 400 min + 4core 100 min -> 600 units -> 200_000 micros
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci store billing`
Expected: FAIL.

- [ ] **Step 3: Implement** the migration lines, `standing` (one
  statement: tenant, card, cap, per-template minutes as `json_agg`,
  running count), the usage insert and the pure functions.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud/migrations/0005_ci_accounts.sql
git commit -S -s -m "feat(ci): meter job minutes from claim to result"
```

---

### Task 7 [standard]: Hold launches past the caps

**Files:**
- Modify: `crates/cloud/migrations/0005_ci_accounts.sql`,
  `crates/ci/src/billing.rs`, `crates/ci/src/store.rs`,
  `crates/ci/src/launches.rs`, `crates/ci/src/run/object.rs`,
  `crates/ci/src/run/mod.rs` (`JobState`), `crates/ci/src/checks.rs`,
  `crates/ci/src/reconcile.rs` (test only)

**Interfaces:**
- Consumes: Task 6 (`Standing`, `standing`, `spend_micros`,
  `month_start`), Task 1 (`MAX_RUNNING_JOBS`, `dollars`).
- Produces:

```sql
-- appended to 0005
ALTER TABLE ci_jobs ADD COLUMN held text CHECK (held IN ('card', 'spend_cap', 'concurrency'));
```

```rust
// billing.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold { Card, SpendCap, Concurrency }
impl Hold {
    pub fn as_str(self) -> &'static str;   // "card", "spend_cap", "concurrency"
    pub fn message(self, org: &str, app_url: &str, cap_micros: u64) -> String;
}
/// First binding cap, in this order: card, spend cap (spend >= cap), concurrency.
pub fn admission(standing: &Standing) -> Option<Hold>;
pub const HOLD_RECHECK_MS: u64 = 30_000;
// store.rs (claim_launch changes)
pub enum Claim { Claimed(JobRow), Busy, Gone }
/// Locks the tenant row (Db::first_locked), then queued -> launching only
/// while fewer than `max_running` of the tenant's jobs are launching,
/// launched or running. Clears `held`.
pub async fn claim_launch(db: &Db, job_id: u64, tenant_id: &str, max_running: u32, now: u64) -> Result<Claim>;
/// Keeps attempt `attempt` queued with `hold` until `retry_at`; bumps
/// updated_at. True when `held` changed.
pub async fn hold(db: &Db, job_id: u64, attempt: u32, hold: &str, retry_at: u64, now: u64) -> Result<bool>;
// JobState gains `held: Option<String>`; JobView gains `held: Option<&'a str>`.
// Run object route: POST /held?job=&attempt=&run=&tenant= with {"message": "..."}.
```

  Messages, exactly (`{cap}` is `dollars(cap_micros)`, `5` is
  `MAX_RUNNING_JOBS`):
  - `Card`: `Waiting for a card. Add one for {org} at {app_url}/o/{org}/ci.`
  - `SpendCap`: `Waiting: {org} reached its {cap} monthly spend cap. Raise it at {app_url}/o/{org}/ci.`
  - `Concurrency`: `Waiting: {org} has 5 jobs running, the most at once. This job starts when one of them ends.`

  In `launches::launch`, a `Delivery::Launch` reads `standing`; a hold,
  or `Claim::Busy` (as `Concurrency`), calls `store::hold` with
  `now + HOLD_RECHECK_MS` and, when it changed, posts `/held` to the
  run object; the message is acked either way. `Claim::Gone` acks. The
  object stores the message in `JobState.held` and marks the outbox
  only when the message differs. A claim by the agent clears `held` and
  marks the outbox when it was set. `render` shows a queued job with
  `held` as title `Waiting` and the message as the first summary line.
  The reconciler is unchanged: `fix` already requeues a queued job
  whose `next_launch_at` passed.

- [ ] **Step 1: Write the failing tests**

```rust
// billing.rs
#[test] fn admission_order()
    // no card + over cap + 5 running -> Card; card + spend == cap -> SpendCap;
    // card + under cap + 5 running -> Concurrency; card + under cap + 4 running -> None
#[test] fn hold_messages() // the three strings above for org "acme", app "https://app.hatchrun.io", cap 50_000_000
// store.rs (Postgres)
#[test] fn claim_launch_holds_the_sixth_job()
    // 5 jobs of tenant t1 launching/launched/running -> Busy, status still queued;
    // 5 running jobs of t2 do not count for t1
#[test] fn claim_clears_the_hold()
#[test] fn hold_reports_a_change_once() // hold(card) true, hold(card) false, hold(concurrency) true; next_launch_at set
#[test] fn hold_leaves_another_attempt_alone()
// checks.rs
#[test] fn held_job_says_why() // Queued + held -> status queued, title "Waiting", summary starts with the message
#[test] fn running_job_drops_the_hold() // Running + held -> title "Running", no message
// reconcile.rs
#[test] fn held_job_is_requeued_when_due() // queued, next_launch_at <= now -> Requeue
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci billing store checks reconcile`
Expected: FAIL.

- [ ] **Step 3: Implement** as described. `claim_launch` passes
  `("SELECT id FROM tenants WHERE id = $1 FOR UPDATE", [tenant])` as
  the lock of `first_locked`, so two consumers cannot both take the
  fifth slot.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud/migrations/0005_ci_accounts.sql
git commit -S -s -m "feat(ci): hold launches past the caps"
```

---

### Task 8 [standard]: Billing routes and Polar usage

**Files:**
- Create: `crates/ci/src/settings_api.rs`
- Modify: `crates/ci/src/api.rs` (dispatch `orgs/*` to the new module),
  `crates/ci/src/access.rs`, `crates/ci/src/env.rs`,
  `crates/ci/src/billing.rs`, `crates/ci/src/store.rs`,
  `crates/ci/src/reconcile.rs`, `crates/cloud/src/polar.rs`,
  `crates/ci/wrangler.toml`

**Interfaces:**
- Consumes: Task 6 (`tenant_standing`, `units`, `spend_micros`), Task
  1 constants, `hatchrun_cloud::github::{membership, parse_membership}`,
  `hatchrun_cloud::polar::{checkout, customer_session, checkout_url, portal_url}`.
- Produces:

| Route | Needs | Answer |
| --- | --- | --- |
| `GET /api/ci/orgs/{org}/billing` | org admin | `BillingView` |
| `POST /api/ci/orgs/{org}/billing/checkout` | org admin, CSRF | `{"url": ...}` for the CI product |
| `POST /api/ci/orgs/{org}/billing/portal` | org admin, CSRF | `{"url": ...}` |
| `PUT /api/ci/orgs/{org}/billing/cap` | org admin, CSRF | `204`; body `{"capDollars": 75}` |

```rust
// access.rs
/// Whether the session's user is an active admin of `org` (GitHub membership).
pub async fn org_admin(settings: &Settings, session: &Session, org: &str) -> Result<bool, Failure>;
// billing.rs
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub struct BillingView { pub card: bool, pub minutes: u64, pub units: u64, pub free_units: u64,
    pub spend_cents: u64, pub cap_dollars: u64, pub running: u32, pub max_running: u32 }
pub fn view(standing: &Standing) -> BillingView;
/// Cap in micros from whole dollars; error "Set a cap from $50 to $10,000".
pub fn cap_micros(dollars: u64) -> Result<u64, String>;
// store.rs
pub async fn set_spend_cap(db: &Db, tenant_id: &str, cap_micros: u64, now: u64) -> Result<()>;
pub struct UnsentUsage { pub job_id: u64, pub attempt: u32, pub tenant_id: String,
    pub template: String, pub minutes: u64, pub ended_at: u64 }
pub async fn unsent_usage(db: &Db, limit: u32) -> Result<Vec<UnsentUsage>>;
pub async fn usage_sent(db: &Db, keys: &[(u64, u32)], now: u64) -> Result<()>;
// hatchrun_cloud::polar
pub const CI_USAGE_EVENT: &str = "ci.usage";
pub struct CiUsage<'a> { pub job_id: u64, pub attempt: u32, pub tenant: &'a str,
    pub template: &'a str, pub minutes: u64, pub units: u64, pub at: String }
/// external_id "ci:{job}:{attempt}", metadata {units, minutes, template}.
pub fn ci_ingest(api: &str, token: &str, events: &[CiUsage<'_>]) -> Request;
// reconcile.rs
pub struct Shares { pub schedules: u32, pub stalled: u32, pub orphans: u32,
    pub pool: u32, pub retention: u32, pub usage: u32 }
```

  `Settings` gains `polar_api` (`POLAR_API_URL`, default
  `hatchrun_cloud::polar::API`), `polar_token` (secret
  `POLAR_ACCESS_TOKEN`) and `polar_ci_product_id`
  (`POLAR_CI_PRODUCT_ID`). The reconciler gains a sixth step, usage:
  up to `INGEST_BATCH` unsent rows go to `ci_ingest`, then
  `usage_sent`. `shares` keeps schedules at 5/9 up to 500 and splits the
  rest half to stalled and an eighth each to orphans, pool, retention
  and usage (the pool share is spent in Task 13).

- [ ] **Step 1: Write the failing tests**

```rust
// billing.rs
#[test] fn cap_bounds() // 49 -> Err("Set a cap from $50 to $10,000"); 50 -> 50_000_000; 10_000 -> Ok; 10_001 -> Err
#[test] fn view_from_standing() // 600 units, cap default -> spend_cents 20, cap_dollars 50, free_units 500, max_running 5
// settings_api.rs
#[test] fn routes_parse() // "orgs/acme/billing" GET, ".../checkout" POST, ".../portal" POST, ".../cap" PUT; others None
// polar.rs
#[test] fn ci_ingest_names_job_and_attempt()
    // event name "ci.usage", external_customer_id tenant, external_id "ci:7:2", metadata.units 20
// reconcile.rs (replaces the old shares test)
#[test] fn shares_split_the_budget() // shares(900) == {500, 200, 50, 50, 50, 50}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci billing settings_api reconcile && cargo test -p hatchrun-cloud polar`
Expected: FAIL.

- [ ] **Step 3: Implement.** The org is found by
  `lower(tenants.org_login)`; an org with no tenant or a user who is
  not an admin answers `404`. The checkout passes the tenant id and
  org as `polar::checkout` does for runners, with
  `success_url = {app_url}/o/{org}/ci`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud/src/polar.rs
git commit -S -s -m "feat(ci): add billing routes and send usage to polar"
```

---

### Task 9 [standard]: Sealed secrets and a rotating key

**Files:**
- Create: `crates/ci/src/secrets.rs`
- Modify: `crates/cloud/migrations/0005_ci_accounts.sql`,
  `crates/ci/src/store.rs`, `crates/ci/src/reconcile.rs`,
  `crates/ci/src/lib.rs`, `crates/ci/Cargo.toml` (`aes-gcm` as in
  cloud), `crates/ci/wrangler.toml`

**Interfaces:**
- Produces:

```sql
-- appended to 0005
CREATE TABLE ci_secrets (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint REFERENCES ci_repos (id) ON DELETE CASCADE,  -- NULL: organization level
  name text NOT NULL,                                          -- uppercase
  key_id text NOT NULL,
  sealed text NOT NULL,                                        -- base64 of nonce || ciphertext
  updated_at bigint NOT NULL,
  UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, name)
);
CREATE INDEX ci_secrets_key ON ci_secrets (key_id);
CREATE TABLE ci_vars (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint REFERENCES ci_repos (id) ON DELETE CASCADE,
  name text NOT NULL,
  value text NOT NULL,
  updated_at bigint NOT NULL,
  UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, name)
);
```

```rust
// secrets.rs
pub const MAX_VALUE_BYTES: usize = 48 * 1024;
pub const MAX_PER_REPO: u32 = 100;    // secrets, and vars, each
pub const MAX_PER_ORG: u32 = 1_000;
pub const NAME_MAX: usize = 100;
pub struct Key { pub id: String, pub bytes: [u8; 32] }       // Debug hides bytes
pub struct Keyring { pub current: Key, pub previous: Option<Key> }
impl Keyring {
    /// Secrets Store bindings CI_SECRETS_KEY and CI_SECRETS_KEY_OLD (base64
    /// of 32 bytes), ids from vars CI_SECRETS_KEY_ID and CI_SECRETS_KEY_OLD_ID.
    /// The old pair is optional.
    pub async fn load(env: &Env) -> Result<Keyring, String>;
}
pub struct Aad<'a> { pub tenant: &'a str, pub repo_id: Option<u64>, pub name: &'a str }
// associated data: "hatchrun-ci-secret\0{tenant}\0{repo_id or org}\0{name}"
pub fn seal(key: &Key, aad: &Aad<'_>, nonce: [u8; 12], value: &str) -> String;
pub fn open(keyring: &Keyring, key_id: &str, aad: &Aad<'_>, sealed: &str) -> Result<String, SecretError>;
pub enum SecretError { UnknownKey(String), Corrupt }        // never carries the value
/// Uppercases; letters, digits and `_`, not starting with a digit or
/// GITHUB_, at most NAME_MAX.
pub fn valid_name(name: &str) -> Result<String, String>;
// store.rs
pub enum Put { Saved, TooMany }
pub async fn put_secret(db: &Db, tenant: &str, repo_id: Option<u64>, name: &str,
    key_id: &str, sealed: &str, limit: u32, now: u64) -> Result<Put>;
pub async fn delete_secret(db: &Db, tenant: &str, repo_id: Option<u64>, name: &str) -> Result<bool>;
pub async fn secret_names(db: &Db, tenant: &str, repo_id: Option<u64>) -> Result<Vec<(String, u64)>>;
pub struct SealedRow { pub repo_id: Option<u64>, pub name: String, pub key_id: String, pub sealed: String }
/// Organization rows and the repository's rows; `names` None for all.
pub async fn sealed_secrets(db: &Db, tenant: &str, repo_id: u64, names: Option<&[String]>) -> Result<Vec<SealedRow>>;
pub async fn put_var(db: &Db, tenant: &str, repo_id: Option<u64>, name: &str, value: &str, limit: u32, now: u64) -> Result<Put>;
pub async fn delete_var(db: &Db, tenant: &str, repo_id: Option<u64>, name: &str) -> Result<bool>;
pub async fn vars(db: &Db, tenant: &str, repo_id: Option<u64>) -> Result<Vec<(String, String, u64)>>;
/// Organization vars overlaid with the repository's.
pub async fn job_vars(db: &Db, tenant: &str, repo_id: u64) -> Result<BTreeMap<String, String>>;
pub struct StaleRow { pub tenant: String, pub repo_id: Option<u64>, pub name: String,
    pub key_id: String, pub sealed: String }
pub async fn stale_secrets(db: &Db, current_key_id: &str, limit: u32) -> Result<Vec<StaleRow>>;
/// Replaces the value only while key_id and sealed are still `old`.
pub async fn reseal(db: &Db, row: &StaleRow, key_id: &str, sealed: &str) -> Result<bool>;
```

  The nonce is 12 bytes from `webcrypto::random_bytes` on wasm; tests
  pass fixed nonces. The reconciler gains a re-seal step (after usage):
  up to 200 rows of `stale_secrets`, opened with the keyring and sealed
  with the current key. `wrangler.toml` gets the bindings:

```toml
# The key that seals CI secrets, from the account's Secrets Store. To
# rotate: add the new secret to the store, point CI_SECRETS_KEY at it
# with a new CI_SECRETS_KEY_ID, move the old one to CI_SECRETS_KEY_OLD
# and CI_SECRETS_KEY_OLD_ID, deploy, and drop the old binding once no
# ci_secrets row has the old key_id.
[[secrets_store_secrets]]
binding = "CI_SECRETS_KEY"
store_id = "<the account's store id: wrangler secrets-store store list>"
secret_name = "ci-secrets-key-2026-10"
```

  and `CI_SECRETS_KEY_ID = "2026-10"`, `CI_SECRETS_KEY_OLD_ID = ""` in
  `[vars]`.

- [ ] **Step 1: Write the failing tests**

```rust
// secrets.rs
#[test] fn round_trips()                          // seal then open -> same text
#[test] fn another_row_cannot_open_it()           // same sealed, Aad name "B" -> Corrupt
#[test] fn opens_a_value_sealed_with_the_previous_key() // sealed with previous, keyring current+previous -> Ok
#[test] fn unknown_key_id_is_an_error()           // -> UnknownKey("2025-01")
#[test] fn errors_never_hold_the_value()          // format!("{err:?}") of every error lacks the plaintext
#[test] fn names_are_uppercased()                 // "npm_token" -> "NPM_TOKEN"
#[test] fn bad_names_are_refused()
    // "GITHUB_X" -> "Secret names cannot start with GITHUB_"; "1A" -> "Secret names cannot start with a digit";
    // "A-B" -> "Use letters, digits and _ in secret names"; 101 chars -> "Secret names are at most 100 characters"
// store.rs (Postgres)
#[test] fn repo_value_overrides_org()             // job_vars: org A=1,B=2; repo A=3 -> {A: 3, B: 2}
#[test] fn put_counts_against_the_limit()         // limit 2: third name -> TooMany; overwriting a name -> Saved
#[test] fn sealed_secrets_filters_by_name()
#[test] fn reseal_skips_a_row_that_changed()      // put_secret between stale_secrets and reseal -> false
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci secrets store`
Expected: FAIL.

- [ ] **Step 3: Implement** with `aes_gcm::Aes256Gcm` and
  `Payload { msg, aad }`, as `hatchrun_cloud::session` uses the crate.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud/migrations/0005_ci_accounts.sql Cargo.lock
git commit -S -s -m "feat(ci): store secrets and vars sealed with a rotating key"
```

---

### Task 10 [standard]: Secrets and vars routes

**Files:**
- Modify: `crates/ci/src/settings_api.rs`, `crates/ci/src/api.rs`
  (repository routes), `crates/ci/src/access.rs`,
  `crates/ci/src/github.rs` (`Permissions` gains `admin`),
  `crates/ci-dev/src/github.rs` (repository answer gains
  `"admin": true`)

**Interfaces:**
- Consumes: Task 9 (store functions, `valid_name`, `seal`, `Keyring`,
  limits), Task 8 (`org_admin`).
- Produces:

| Route | Needs | Answer |
| --- | --- | --- |
| `GET /api/ci/orgs/{org}/secrets` | org admin | `[{"name", "updatedAt"}]` |
| `PUT /api/ci/orgs/{org}/secrets/{name}` | org admin, CSRF | `204`; body `{"value": "..."}` |
| `DELETE /api/ci/orgs/{org}/secrets/{name}` | org admin, CSRF | `204`, `404` when absent |
| `GET /api/ci/orgs/{org}/vars` | org admin | `[{"name", "value", "updatedAt"}]` |
| `PUT`, `DELETE /api/ci/orgs/{org}/vars/{name}` | org admin, CSRF | as secrets |
| the same four under `/api/ci/repos/{owner}/{repo}/` | repository admin | as above |

```rust
// access.rs: Access gains Admin above Write; from_permissions gives Admin when p.admin.
```

  A bad name or value answers `400` with the error text; a value over
  `MAX_VALUE_BYTES` answers `413` with `Values are at most 48 KB`;
  `Put::TooMany` answers `409` with
  `{org or owner/repo} has 100 secrets, the most allowed` (or `1000`,
  or `vars`). Secret values are never answered, logged or echoed in an
  error.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn routes_parse()          // every row of the table, with name segments decoded
#[test] fn admin_needs_the_admin_permission() // from_permissions(true, admin) -> Admin; write only -> Write
#[test] fn value_too_large()       // 49_153 bytes -> 413 text above
#[test] fn too_many_message()      // ("acme", repo None, secrets, 1000) -> "acme has 1000 secrets, the most allowed"
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci settings_api access`
Expected: FAIL.

- [ ] **Step 3: Implement.** `PUT` of a secret uppercases the name with
  `valid_name`, seals with `Keyring::load(env)`'s current key and a
  fresh nonce, then `put_secret` with `MAX_PER_ORG` or `MAX_PER_REPO`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci -p hatchrun-ci-dev && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/ci-dev
git commit -S -s -m "feat(ci): add secrets and vars routes for the app"
```

---

### Task 11 [standard]: Hand jobs their secrets, vars and scoped token

**Files:**
- Modify: `crates/ci/src/agent.rs`, `crates/ci/src/github.rs`,
  `crates/ci/src/secrets.rs`, `crates/ci-dev/src/github.rs` (`GET /app/installations/{id}`)

**Interfaces:**
- Consumes: Task 4 (`JobRow.token_permissions`, `JobRow.secret_refs`),
  Task 9 (`sealed_secrets`, `job_vars`, `open`, `Keyring`).
- Produces:

```rust
// github.rs
pub fn installation(api: &str, app_jwt: &str, installation_id: u64) -> Request;
/// The installation's `permissions` map.
pub fn installation_permissions(response: &Response) -> Result<BTreeMap<String, String>, String>;
/// Each asked scope at the lower of the asked and held level; scopes the
/// App does not hold are left out; `metadata: read` is always kept.
pub fn cap_permissions(asked: &BTreeMap<String, String>, held: &BTreeMap<String, String>) -> BTreeMap<String, String>;
pub fn scoped_token(api: &str, installation_id: u64, app_jwt: &str, repo_id: u64,
    permissions: &BTreeMap<String, String>) -> Request;
// secrets.rs
/// The values `refs` names (all of them when refs.all), repository rows
/// over organization rows. A row that does not open fails the claim.
pub fn job_secrets(refs: &SecretRefs, rows: &[SealedRow], keyring: &Keyring, tenant: &str)
    -> Result<BTreeMap<String, String>, SecretError>;
// agent.rs
pub(crate) fn fill(doc: &mut JobDocument, secrets: BTreeMap<String, String>, vars: BTreeMap<String, String>);
```

  The installation's permissions are cached in the `TOKENS` KV for 600
  s under `ci:installation-permissions:{id}`, since App JWT calls share
  one budget across all installations. The claim mints the token with
  `cap_permissions(row.token_permissions, held)`. After the run object
  answers `200` with the document, the handler reads `sealed_secrets`
  (names from `secret_refs`, all when `all`), `job_vars`, `fill`s the
  document and answers it. A secret that does not open logs
  `job {id}: secret {name}: {err}` and answers `503`. Any other object
  answer passes through unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
// github.rs
#[test] fn cap_keeps_what_the_app_holds()
    // asked {contents: write, issues: write, metadata: read}, held {contents: read, metadata: read, checks: write}
    // -> {contents: read, metadata: read}
#[test] fn scoped_token_asks_for_the_capped_scopes() // body permissions equal the map, repository_ids [repo]
#[test] fn installation_permissions_reads_the_map()
// secrets.rs
#[test] fn repo_value_wins()               // org and repo NPM_TOKEN -> repo's
#[test] fn only_named_secrets_go()         // refs {A}, rows A, B -> {A}
#[test] fn all_refs_send_every_secret()
#[test] fn missing_secret_is_left_out()    // refs {A}, no rows -> {}
// agent.rs
#[test] fn fill_sets_secrets_and_vars()    // keeps every other field of the document
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci github secrets agent`
Expected: FAIL.

- [ ] **Step 3: Implement** the cache, the capped mint, the fill. The
  fake GitHub answers `/app/installations/{id}` with
  `{"permissions": {"contents": "write", "metadata": "read", "checks": "write", "pull_requests": "write"}}`
  and records the permissions of every access token request in
  `/_state`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci -p hatchrun-ci-dev && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/ci-dev
git commit -S -s -m "feat(ci): hand jobs their secrets, vars and scoped token"
```

---

### Task 12 [standard]: The warm pool as its own type in scaler-core

**Files:**
- Create: `crates/scaler-core/src/pool.rs`
- Modify: `crates/scaler-core/src/lib.rs`

**Interfaces:**
- Produces, in `hatchrun_scaler_core::pool` (times in seconds, as the
  crate uses):

```rust
#[derive(Clone, Debug)]
pub struct PoolConfig { pub name_prefix: String, pub size: u32, pub warmup: u64,
    pub max_age: u64, pub inventory_grace: u64 }
pub struct Pool { /* instances by id, names launching */ }
impl Pool {
    pub fn new(config: PoolConfig) -> Pool;
    /// A pool that already holds these instances, by id.
    pub fn restore(config: PoolConfig, instances: impl IntoIterator<Item = (String, PoolInstance)>) -> Pool;
    pub fn set_size(&mut self, size: u32);
    /// Takes in instances without a runner name; drops entries missing
    /// from the inventory past the grace.
    pub fn reconcile(&mut self, instances: &[Instance], now: u64);
    /// Stop actions for warmed-up instances, Terminate for old and excess ones.
    pub fn upkeep(&mut self, now: u64) -> Vec<Action>;
    /// LaunchPool actions up to the size.
    pub fn refill(&mut self, now: u64) -> Vec<Action>;
    pub fn take_ready(&mut self) -> Option<String>;
    pub fn launched(&mut self, name: &str, instance_id: String, now: u64);
    pub fn launch_failed(&mut self, name: &str);
    pub fn instances(&self) -> &BTreeMap<String, PoolInstance>;
}
```

  `Scaler` holds a `Pool` and calls it where it ran the same code
  inline; its public API and every existing test stay as they are.
  `PoolInstance`, `PoolState`, `Instance` and `Action` stay in
  `lib.rs`.

- [ ] **Step 1: Write the failing tests** in `pool.rs`:

```rust
#[test] fn refill_launches_up_to_size()         // size 3, empty -> 3 LaunchPool; again -> none
#[test] fn upkeep_stops_warmed_instances()      // launched at 0, warmup 180 -> Stop at 180, not at 179
#[test] fn shrinking_terminates_the_oldest()    // 3 ready, set_size(1) -> Terminate the two oldest
#[test] fn old_instances_are_replaced()         // age >= max_age -> Terminate, refill launches one
#[test] fn restore_keeps_entries_within_grace() // restored entry missing from inventory at since+119 kept, +120 dropped
#[test] fn take_ready_takes_the_oldest()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-scaler-core pool`
Expected: FAIL.

- [ ] **Step 3: Move** the pool fields and code out of `Scaler`.

- [ ] **Step 4: Run every scaler test**

Run: `cargo test -p hatchrun-scaler-core -p hatchrun-scaler && cargo check -p hatchrun-scaler-core --target wasm32-unknown-unknown`
Expected: PASS, with no change to the existing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scaler-core
git commit -S -s -m "refactor(scaler-core): move the warm pool into its own type"
```

---

### Task 13 [standard]: A warm pool per template

**Files:**
- Create: `crates/ci/src/pool.rs`
- Modify: `crates/cloud/migrations/0005_ci_accounts.sql`,
  `crates/cloud/src/aws/ec2.rs`, `crates/cloud/src/aws/instances.rs`,
  `crates/ci/Cargo.toml` (`hatchrun-scaler-core`),
  `crates/ci/src/launcher.rs`, `crates/ci/src/launches.rs`,
  `crates/ci/src/reconcile.rs`, `crates/ci/src/store.rs`,
  `deploy/cloudformation/ci-account.yaml`

**Interfaces:**
- Consumes: Task 12 (`Pool`, `PoolConfig`), Task 4 (`profile_for`,
  `LaunchSpec.template`), Task 8 (`Shares.pool`).
- Produces:

```sql
-- appended to 0005
CREATE TABLE ci_pool (
  instance_id text PRIMARY KEY,
  template text NOT NULL,
  state text NOT NULL CHECK (state IN ('warming', 'stopping', 'ready')),
  launched_at bigint NOT NULL,
  since bigint NOT NULL
);
CREATE INDEX ci_pool_ready ON ci_pool (template, launched_at) WHERE state = 'ready';
CREATE INDEX ci_jobs_started ON ci_jobs (started_at) WHERE started_at IS NOT NULL;
```

```rust
// hatchrun_cloud::aws::ec2
pub const TAG_POOL: &str = "hatchrun:pool";
pub fn start_instances(endpoint: &str, instance_id: &str) -> Request;
pub fn stop_instances(endpoint: &str, instance_id: &str) -> Request;
pub fn set_user_data(endpoint: &str, instance_id: &str, user_data: &str) -> Request; // UserData.Value, base64
pub fn create_tags(endpoint: &str, instance_id: &str, tags: &[(&str, &str)]) -> Request;
pub fn delete_tags(endpoint: &str, instance_id: &str, keys: &[&str]) -> Request;
// hatchrun_cloud::aws::instances
pub fn describe_pool(endpoint: &str, tenant: &str, next_token: Option<&str>) -> Request; // pending..stopped, tag-key hatchrun:pool
// Described gains `pool: Option<String>` (the hatchrun:pool tag).
// launcher.rs: Backend gains
pub async fn launch_pool(&self, db: &Db, template: &'static Template, name: &str) -> Result<String, Failure>;
pub async fn start_pooled(&self, instance_id: &str, spec: &LaunchSpec) -> Result<(), Failure>;
pub async fn stop(&self, instance_id: &str) -> Result<(), String>;
pub async fn pool_instances(&self) -> Result<Vec<Described>, String>;
// (Local answers Err("the local launcher has no pool") to all four.)
// pool.rs
pub const POOL_MAX: u32 = 10;
pub const BUCKET_MS: u64 = 300_000;
pub const WINDOW_MS: u64 = 3_600_000;
/// The busiest five-minute bucket of the last hour, at most POOL_MAX; 0 when idle.
pub fn pool_target(claims_per_bucket: &[u64]) -> u32;
pub fn config(template: &Template) -> PoolConfig;  // prefix "ci-pool-<label>", warmup 180, max_age 86_400, grace 120
// store.rs
pub struct PoolRow { pub instance_id: String, pub template: String, pub state: String,
    pub launched_at: u64, pub since: u64 }
pub async fn pool_rows(db: &Db, template: &str) -> Result<Vec<PoolRow>>;
/// Deletes and returns the oldest ready row (FOR UPDATE SKIP LOCKED).
pub async fn take_pool(db: &Db, template: &str) -> Result<Option<String>>;
/// Deletes the row; false when a launch took it first.
pub async fn drop_pool(db: &Db, instance_id: &str) -> Result<bool>;
/// Updates existing rows, inserts `launched` ones, never re-inserts a missing row.
pub async fn save_pool(db: &Db, template: &str, rows: &[PoolRow], launched: &[PoolRow]) -> Result<()>;
/// Claims per (template, bucket) since `since`, from ci_jobs.started_at.
pub async fn claims_by_bucket(db: &Db, since: u64) -> Result<Vec<(String, u64, u64)>>;
```

  **Demand rule:** a template's pool size is the number of its jobs
  claimed in the busiest five-minute bucket of the last hour, capped at
  10. A template with no claim in the last hour has size 0, and
  `upkeep` terminates what it holds.

  The reconciler's pool step (after orphans, within `shares.pool`)
  skips when the backend is local. Per template: restore a `Pool` from
  `pool_rows`, `reconcile` with `pool_instances` whose `pool` tag is the
  template's first label (stopped means ready), `set_size(pool_target)`,
  run `upkeep` then `refill`. A `Terminate` or `Stop` acts only after
  `drop_pool` (terminate) or a state update `WHERE state = 'warming'`
  (stop) changed a row; otherwise a launch took the instance. Then
  `save_pool`. A pool launch starts the template's profile with empty
  user data and tags `hatchrun:tenant`, `hatchrun:pool` and `Name`; the
  image's `detect` finds no runner config, so the agent stays off until
  the instance is started for a job.

  In `launch_claimed`, before a fresh launch: `take_pool(template)`;
  with an instance, `start_pooled` sets the user data, tags
  `hatchrun:job-id`, `hatchrun:runner-name` and `Name`, deletes
  `hatchrun:pool` and starts it. A failure terminates that instance and
  falls through to the fresh launch.

  `orphans` leaves out every instance with a `pool` tag.

  `ci-account.yaml`: `ManageOwnInstances` adds `ec2:StartInstances`,
  `ec2:StopInstances` and `ec2:ModifyInstanceAttribute`; a new
  statement allows `ec2:CreateTags` and `ec2:DeleteTags` on instances
  with `aws:ResourceTag/hatchrun:tenant = hatchrun-ci` and
  `ForAllValues:StringEquals aws:TagKeys` in `Name`,
  `hatchrun:job-id`, `hatchrun:runner-name`, `hatchrun:pool`.

- [ ] **Step 1: Write the failing tests**

```rust
// pool.rs
#[test] fn pool_target_follows_the_busiest_bucket() // [0,0,1,3,0,0,0,0,0,0,0,1] -> 3
#[test] fn pool_target_is_zero_when_idle()          // twelve zeros -> 0
#[test] fn pool_target_caps_at_ten()                // [25] -> 10
// reconcile.rs
#[test] fn orphans_skip_pool_instances() // running, no job tag, pool "ubuntu-latest" -> not an orphan
// store.rs (Postgres)
#[test] fn take_pool_takes_ready_only()   // warming row -> None; ready rows -> oldest, then gone
#[test] fn trim_skips_a_taken_instance()  // take_pool then drop_pool on the same id -> false
#[test] fn save_pool_never_brings_back_a_taken_row()
#[test] fn claims_by_bucket_groups_by_template()
// cloud ec2.rs / instances.rs
#[test] fn set_user_data_is_base64()      // Action ModifyInstanceAttribute, InstanceId, UserData.Value
#[test] fn delete_tags_names_the_keys()
#[test] fn describe_pool_lists_stopped_instances() // states pending, running, stopping, stopped; tag-key filter
#[test] fn described_reads_the_pool_tag()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci pool reconcile store && cargo test -p hatchrun-cloud ec2 instances`
Expected: FAIL.

- [ ] **Step 3: Implement** as described. Validate the stack with
  `cfn-lint deploy/cloudformation/ci-account.yaml`.

- [ ] **Step 4: Run the tests**

Run:
```bash
cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud
cargo check -p hatchrun-ci --target wasm32-unknown-unknown
cfn-lint deploy/cloudformation/ci-account.yaml
```
Expected: PASS, no lint findings.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud deploy/cloudformation/ci-account.yaml Cargo.lock
git commit -S -s -m "feat(ci): keep a warm pool per runner template"
```

---

### Task 14 [standard]: App pages for billing, secrets and vars

**Files:**
- Create: `web/app/src/pages/ci/CiSettings.tsx`,
  `web/app/src/pages/ci/RepoSettings.tsx`,
  `web/app/src/pages/ci/SecretsTable.tsx`, `web/app/mock/ci-settings.ts`
- Modify: `web/app/src/App.tsx` (the `/o/:org/ci` section),
  `web/app/src/router.tsx` (`/ci/:owner/:repo/settings`),
  `web/app/src/api.ts`, the mock server setup in `web/app/mock`

**Interfaces:**
- Consumes: the Task 8 and Task 10 routes.
- Produces: `/o/:org/ci` with a billing card (card on file or an "Add
  a card" button that opens the checkout URL, "Manage billing" for the
  portal, minutes used of 500 free, spend this month, the cap with an
  edit field in whole dollars, running of 5) and the organization's
  secrets and vars; `/ci/:owner/:repo/settings` with the repository's
  secrets and vars. `SecretsTable` lists names with their update time,
  adds or replaces a value in a password field that is cleared after
  saving, and deletes after a confirm. Secret values are never shown.
  Vars show their values. Errors from the API show as their text.

- [ ] **Step 1: Add mock data** for a tenant with a card, 320 minutes,
  two org secrets, one var, and a repository with one secret.

- [ ] **Step 2: Implement** the pages and API functions, sending the
  CSRF token from `/api/session` on every write.

- [ ] **Step 3: Check the app**

Run: `cd web/app && npm run typecheck && npm run lint && npm run build`
Expected: no errors. Then `npm run dev:mock`, open `/o/acme/ci` and
`/ci/acme/demo/settings`: adding a secret clears the field and the name
appears; a 49 KB value shows `Values are at most 48 KB`.

- [ ] **Step 4: Commit**

```bash
git add web/app
git commit -S -s -m "feat(app): add CI billing, secrets and vars pages"
```

---

### Task 15 [cheap]: Deploy wiring

**Files:**
- Modify: `crates/ci/wrangler.toml` (comments, `POLAR_CI_PRODUCT_ID`,
  `POLAR_API_URL`), `crates/cloud/wrangler.toml` (comment for
  `POLAR_CI_PRODUCT_ID`), `.github/workflows/deploy-ci.yml` (the
  `POLAR_ACCESS_TOKEN` secret put, as `deploy-cloud.yml` does)

**Interfaces:**
- Produces: a deploy that needs, once, by hand:
  - a Secrets Store secret `ci-secrets-key-2026-10` holding base64 of
    32 random bytes (`openssl rand -base64 32`), scope `workers`, and
    the store id written into `crates/ci/wrangler.toml`;
  - a Polar meter `CI usage` that sums `metadata.units` of events named
    `ci.usage`, a product `hatchrun CI` with a metered price of $0.002
    per unit on it and a meter credit benefit of 500 units per month,
    and its id in both Workers' `POLAR_CI_PRODUCT_ID`;
  - the updated `ci-account.yaml` stack applied in the CI account
    (`us-east-1`);
  - migration `0005` run by the deploy workflow as `0004` was.

- [ ] **Step 1: Write the comments and vars,** naming for each value
  where it comes from, in the style of Plan 3's `wrangler.toml`.

- [ ] **Step 2: Check the configuration**

Run: `cd crates/ci && npx --yes wrangler@4.145.0 deploy --dry-run --outdir /tmp/ci-dry`
Expected: the dry run lists the `CI_SECRETS_KEY` Secrets Store binding,
the new vars, and no error.

- [ ] **Step 3: Commit**

```bash
git add crates/ci/wrangler.toml crates/cloud/wrangler.toml .github/workflows/deploy-ci.yml
git commit -S -s -m "chore(ci): wire the secrets key and polar for deploy"
```

---

### Task 16 [standard]: End-to-end scenarios

**Files:**
- Modify: `crates/ci/scripts/e2e.sh`,
  `crates/ci/tests/e2e/repo/.hatchrun/config.yml`,
  `crates/ci-dev/src/github.rs` (answers for a second installation)

**Interfaces:**
- Consumes: every earlier task.
- Produces four new scenarios, run after Plan 3's and in this order,
  plus a fifth label case:

  1. **Template label.** Branch `arm` runs workflow `arm` with job
     `arm-hello` on `ubuntu-26.04-arm`. The launcher's `/_events` show
     its launch with `"template": "ubuntu-26.04-arm"`, and the job
     passes.
  2. **Unknown label.** Branch `bad` has a workflow whose job asks for
     `ubuntu-24.04`. The commit gets a failed `hatchrun / config` check
     run whose summary contains
     `names no runner template; use one of ubuntu-latest` and no
     launch.
  3. **Masked secret.** Before the push the script sets
     `E2E_SECRET=e2e-secret-<16 random hex>` through
     `PUT /api/ci/repos/e2e/demo/secrets/E2E_SECRET` with the session
     cookie and CSRF token. `arm-hello` runs
     `echo "secret is ${{ secrets.E2E_SECRET }}"`. The R2 log holds
     `secret is ***`. `grep -rF "$secret" "$tmp/ci-state" "$logs"` finds
     nothing (R2 objects, Durable Object storage, KV, Worker logs, fake
     GitHub state), and the `ci_secrets` row's `sealed` column does not
     contain it.
  4. **Concurrency cap.** Branch `wide` runs a matrix job `nap` over
     `n: [1, 2, 3, 4, 5, 6]`, each `sleep 20`. While the first five run,
     the run's API shows five jobs `launching`, `launched` or `running`
     and one `queued`, and the queued job's check run summary contains
     `has 5 jobs running, the most at once`. After one ends, the script
     triggers the reconciler (`/__scheduled`) until the sixth runs; all
     six pass.
  5. **No card.** A second tenant `e2etenant0000002`, installation 43,
     no subscription. A push for its repository gets one
     `hatchrun / billing` check run with conclusion `action_required`,
     no run in Postgres and no launch.

  The key for sealing comes from the local Secrets Store before
  `wrangler dev` starts:
  `npx --yes "$WRANGLER" secrets-store secret create <store id from wrangler.toml> --name ci-secrets-key-2026-10 --value "$(openssl rand -base64 32)" --scopes workers --persist-to "$tmp/ci-state"`
  (run in `crates/ci`, local by default). The instance counts the
  script waits for grow by one for `arm` and six for `wide`.

- [ ] **Step 1: Add the workflows** `arm` and `wide` to the fixture
  config. The `bad` case cannot live there, since one bad label fails
  the whole config at compile and the fake GitHub serves one directory
  for every ref: the script appends the `bad` workflow to the repo copy
  right before that push and restores the file after the check run
  arrives.

- [ ] **Step 2: Add the scenarios** to `e2e.sh` and the header comment
  that lists them.

- [ ] **Step 3: Run it**

Run: `crates/ci/scripts/e2e.sh`
Expected: exit 0, ending with `e2e passed`.

- [ ] **Step 4: Commit**

```bash
git add crates/ci/scripts/e2e.sh crates/ci/tests/e2e crates/ci-dev
git commit -S -s -m "test(ci): cover templates, secrets, caps and the card in e2e"
```
