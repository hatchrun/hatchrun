# Orchestrator Plan 3: Control Plane

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A push or pull request on a repository with
`.hatchrun/config.yml` runs its workflows on EC2 instances in the
hatchrun-owned AWS account, with one check run per job, live logs over
a WebSocket and finished logs in R2, all without GitHub Actions.

**Architecture:** A new Worker, `hatchrun-ci` (`crates/ci`), takes the
GitHub App's push, pull request and check run events, which the
existing `hatchrun-cloud` Worker forwards over a service binding. It
plans runs with `hatchrun-workflow`, keeps runs and jobs in the shared
Postgres, and gives each workflow run one Durable Object that holds the
job graph, live logs and the check run outbox. Ready jobs go on a launch
queue whose consumer starts an instance with the launcher code shared
from `hatchrun-cloud`. The agent claims a broker ticket, the control
plane turns instance metadata off, and the agent then fetches its
`JobDocument`, runs it with the Plan 2 executor and streams events back
to the run's Durable Object. Pure logic (event parsing, planning, the
graph, check run rendering, log assembly, tokens, cron) is plain Rust
tested natively; the Worker and Durable Object code only wire it to
bindings.

**Tech Stack:** Rust 1.99 (edition 2024), `worker` 0.8.7 (Durable
Objects, Queues, R2, Hyperdrive, Cron Triggers, WebSockets),
`tokio-postgres` through `hatchrun_cloud::db`, `hmac` 0.13 + `sha2`
0.11 (tokens, webhook HMAC), `flate2` (gzip, `rust_backend`),
`hatchrun-workflow`, `hatchrun-agent` (tokio, reqwest), `axum` 0.8 and
`tokio` for the dev-only fake GitHub and local launcher, React app in
`web/app`.

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`
(sections "Flow", "Control plane", "Storage", "Compute" Phase 1, "Calls
made in writing this up", "Secrets, the GitHub App and billing").

## Scope against the other plans

- **In:** GitHub App events (push, pull request with forks ignored,
  schedules from the default branch, manual dispatch from the app API,
  check run re-requests), the run Durable Object and job graph, check
  runs, the launch queue and the EC2 backend in the hatchrun-owned
  account, the agent job protocol, live and finished logs, log access,
  the reconciler, 30 day retention, a one-script end-to-end test.
- **Plan 4:** runner templates beyond one, the warm pool, secrets and
  vars (this plan sends empty `secrets` and `vars`), billing, caps and
  per-tenant retention (90 days for enterprise).
- **Plan 5:** cache, workspace and artifact storage (this plan sends
  `cache: None`), timings (`timings` stays empty), approval jobs.

## Rulings

Made by the controller before this plan; the plan keeps them unless the
line says otherwise.

1. A separate Worker crate `crates/ci` (package `hatchrun-ci`, Worker
   `hatchrun-ci`, its own `wrangler.toml`). It depends on
   `hatchrun-cloud` as an rlib with `default-features = false`.
   `crates/cloud/src/lib.rs` defines the `#[event(fetch)]`,
   `#[event(scheduled)]` and `#[event(queue)]` exports itself, so Task 1
   moves them into `src/entry.rs` behind a default `worker` feature.
2. One GitHub App, one webhook URL. `hatchrun-cloud` forwards the raw
   request of `push`, `pull_request` and `check_run` deliveries to
   `hatchrun-ci` over the `CI` service binding and returns its answer
   unchanged, so a failure there is a failed delivery that the existing
   redelivery pass (`crates/cloud/src/reconcile/deliveries.rs`) asks for
   again. `hatchrun-ci` checks the HMAC itself. Delivery ids are kept in
   Postgres.
3. Same Postgres through Hyperdrive. New tables live in
   `crates/cloud/migrations/0004_ci.sql`, prefixed `ci_`. New columns
   are `NOT NULL` without compat paths.
4. The agent side is `crates/hatchrun-agent/src/ci/`, reusing
   `exec::run_job` and the `Sink` trait. **Added:** the runner image's
   existing unit runs `hatchrun-agent` with no subcommand, so user data
   carries `"mode": "ci"` and the no-subcommand path hands such a boot to
   the CI path. hatchrun.io user data never sets `mode`, so its
   behaviour is unchanged. `hatchrun-agent ci --user-data FILE` is the
   same path for local runs.
5. **Changed:** a Worker cannot start processes, so the `Local` launcher
   backend calls a local launcher daemon (`hatchrun-ci-dev launcher`)
   that starts `hatchrun-agent ci` with the user data, ticket included,
   passed as a file. `Local` is compiled only under
   `--cfg hatchrun_local_launcher` (set through `RUSTFLAGS`, since
   wrangler's build command is fixed in `wrangler.toml`) or in tests,
   and is refused when `DEPLOY_ENV` is `production`. The end-to-end
   script posts the webhook to `hatchrun-ci` directly; the forwarding
   in `hatchrun-cloud` has its own test.
6. Plan 4 owns templates, secrets, billing. Plan 5 owns cache and
   artifacts.
7. **Added:** CI runs only for tenants with `tenants.ci_enabled = true`
   until Plan 4 adds the card requirement and caps. `runs::create`
   checks it, so webhooks, schedules and dispatch all respect it.
8. **Added:** the one template, `ubuntu-latest` / `ubuntu-26.04`
   (amd64, 2 vCPU, 8 GB), launches hatchrun.io's built-in profile
   `hatchrun-2cpu` (general purpose `m` family, 4 GiB per vCPU, on
   demand). The spec's `c8a`/`c7i` have 2 GiB per vCPU and would miss
   the 8 GB in the template table.
9. **Added:** the Worker gets AWS credentials the way hatchrun.io does,
   `AssumeRoleWithWebIdentity` with an `oidc.hatchrun.io` token, whose
   subject is `tenant:hatchrun-ci`. Tenant ids are `[a-z0-9]{16}`, so
   the hyphen keeps the subject apart from every tenant.
10. **Added:** approval jobs end as `skipped` with a message until Plan
    5; jobs that require them are skipped by the usual rule.
11. **Added:** the log key uses GitHub's repository id, which survives
    renames: `logs/<tenant>/<repo_id>/<run>/<job>.log.gz`, with the step
    index beside it as `.../<job>.index.json`.

## Global Constraints

<!-- REUSE-IgnoreStart -->
- Every source file starts with the SPDX header used across the repo:
  `SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>`
  and `SPDX-License-Identifier: AGPL-3.0-only`.
<!-- REUSE-IgnoreEnd -->
- Crates use `version = "0.1.0"`, `publish = false`, the workspace's
  `authors`, `edition`, `rust-version`, `license`, `repository`, and
  `[lints] workspace = true`. CI runs
  `cargo clippy --locked --all-targets -- -D warnings` and
  `cargo test --locked` with `DATABASE_URL` pointing at Postgres 17.
- `hatchrun-ci`, `hatchrun-cloud` (with and without default features),
  `hatchrun-workflow` and `hatchrun-broker` build for
  `wasm32-unknown-unknown`. New types in `hatchrun-workflow` are plain
  data with no I/O and no clock.
- Running `hatchrun-agent` with no subcommand and user data without
  `"mode": "ci"` behaves exactly as today. No existing test changes,
  except where a task moves a function and says so.
- Times are Unix milliseconds in `bigint` columns, as in the existing
  schema; ids of runs and jobs are Postgres identities; GitHub ids are
  `bigint`.
- GitHub calls per job attempt: one check run create, one update per
  step end and one at job end, never per log line (none at claim). A `403` or `429` from GitHub
  waits for `retry-after`, else for `x-ratelimit-reset` when
  `x-ratelimit-remaining` is `0`, else 60 s doubling per attempt up to
  900 s.
- Pull requests whose head repository differs from the base repository
  never run.
- Each job runs on a fresh on-demand instance with no IAM role. The
  control plane disables instance metadata (`HttpEndpoint=disabled`)
  and sees the change applied before it returns the job document.
- Tokens the agent holds are scoped to one job: the job token (HMAC,
  expires after the job's timeout plus 25 minutes) and a GitHub
  installation token limited to the job's repository with `contents`,
  `metadata`, `checks` and `pull_requests` read, revoked when the job
  ends.
- Log lines arrive masked (the executor masks them). Agents send
  batches about once a second (`FLUSH_INTERVAL_MS = 1000`) and a
  heartbeat at least every 10 s; a job silent for 120 s fails.
- Finished logs: `logs/<tenant>/<repo_id>/<run>/<job>.log.gz` in the
  `hatchrun-ci-logs` R2 bucket, plus `<job>.index.json`. Rows, logs and
  delivery ids are kept 30 days.
- Logs of public repositories are public. Private repository logs need a
  GitHub sign-in whose user can read the repository.
- The only runner template that launches is `ubuntu-latest` /
  `ubuntu-26.04`. A job on any other template fails before launch with:
  `` `<label>` runners come in a later release; use ubuntu-latest or ubuntu-26.04 ``.
- No backward compatibility shims: new fields are required.
- Commits are Conventional Commits, GPG-signed and DCO signed off
  (`git commit -S -s`), with no AI attribution.

## Review Focus

1. **A batch sent twice.** The agent times out waiting for an answer
   that the Durable Object already applied, and resends the batch. The
   user expects each line once in the live and the finished log and each
   step to end once. Pinned in Task 10 (`repeated_seq_is_ignored`) and
   Task 14 (`retries_with_the_same_seq`).
2. **GitHub's secondary rate limit during a wide matrix.** A run of 256
   jobs creates 256 check runs and GitHub answers `403` with no
   `retry-after`. The user expects every check run to reach its final
   state late, not an error loop or a lost update. Pinned in Task 6
   (`secondary_limit_without_headers_backs_off`) and Task 8
   (`outbox_keeps_only_the_latest_state_while_blocked`).
3. **A fork pull request whose head repository was deleted.**
   `pull_request.head.repo` is `null`. The user expects it ignored like
   any fork, never run. Pinned in Task 5
   (`fork_with_deleted_head_repo_is_ignored`).
4. **The agent claims while the metadata change is still pending.** The
   user expects the claim answered `503` until EC2 reports the change
   applied, never the document first. Pinned in Task 13
   (`claim_waits_until_metadata_is_applied`) and in the end-to-end
   script, which checks the launcher saw `metadata-off` before the job
   ran.
5. **Matrix outputs into `needs`.** Three matrix entries of `build` set
   output `v`; one fails. A job requiring `build` expects
   `needs.build.result == 'failure'` and the outputs merged in plan
   order with later non-empty values winning. Pinned in Task 7
   (`matrix_needs_merge_in_plan_order`).

---

### Task 1 [standard]: Share hatchrun-cloud with a second Worker

**Files:**
- Create: `crates/cloud/src/entry.rs`
- Modify: `crates/cloud/Cargo.toml`, `crates/cloud/src/lib.rs`,
  `crates/cloud/src/db.rs`, `crates/cloud/src/testdb.rs`,
  `crates/cloud/src/app/mod.rs`, `crates/cloud/src/tokens.rs`,
  `crates/cloud/src/fetch.rs`, `crates/cloud/src/http.rs`,
  `crates/cloud/src/reconcile/time.rs`, every file that builds a
  `http::Response { .. }` literal (find them with
  `rg -l 'Response \{' crates/cloud/src`), `.github/workflows/ci.yml`

**Interfaces:**
- Produces (all reachable from `hatchrun_cloud::`):
  - `http::Response` gains `pub headers: Vec<(String, String)>`
    (names lowercased) and
    `pub fn header(&self, name: &str) -> Option<&str>` (case-insensitive).
    `fetch::send` and `Counted::send` fill it from the fetch answer.
    Existing `Response { status, body }` literals add
    `headers: Vec::new()`.
  - `pub use reconcile::time::parse as parse_rfc3339;` with
    `parse_rfc3339(value: &str) -> Option<u64>` (Unix ms)
  - features: `default = ["worker"]`, `worker = []` (the `#[event]`
    exports), `testdb = ["dep:tokio", "tokio-postgres/runtime"]`
  - `pub mod db` with `pub struct Db`, `pub enum Param`, and
    `params!` exported with `#[macro_export]` (callers write
    `hatchrun_cloud::params![..]`; inside the crate
    `crate::db::params` keeps working through `pub use crate::params;`).
    `Db::{new, connect, run, all, first, rows, first_locked, batch}`
    become `pub`.
  - `pub mod tokens` with `Tokens`, `Kv`, `App`, `Store`, `Mint`,
    `Token`, `Error` public, and `Kv::new`, `App::{new, read}`,
    `Tokens::{new, token, send}`, `Token::fresh` public;
    `pub mod fetch` with `Sender`, `Counted` (and its `Default`),
    `send`; `pub mod webcrypto`
  - `pub use app::{Session, UserRow};` with `UserRow` and its fields
    public, `Session::find(req: &Request, db: &Db) -> Result<Option<Session>>`
    and `Session::csrf_ok(&self, req: &Request) -> Result<bool>` public,
    and fields `id`, `user`, `token` public
  - `pub mod testdb` when `any(test, feature = "testdb")` on non-wasm,
    with `url`, `Db::{new, connect, db, wait, run, try_run, batch, rows,
    first, all}` and every other `pub(crate)` item in it public

- [ ] **Step 1: Move the entry points.** Move `fetch`, `scheduled`,
  `queue` and the helpers only they use (`healthz`, `assets`,
  `github_webhook`, `record_queued`, `issuer`, `published_keys`,
  `error`, `SECURITY_HEADERS`, `WEBHOOK_SECRET`, `UNINSTALLED`,
  `INSTALLED`) into `src/entry.rs`, declared in `lib.rs` as
  `#[cfg(feature = "worker")] mod entry;`. Move their tests with them.

- [ ] **Step 2: Add the features and visibility** listed under
  Interfaces. `tokio` becomes an optional non-wasm dependency for
  `testdb` (same version and features as the dev-dependency). Gate
  `testdb` with
  `#[cfg(all(not(target_arch = "wasm32"), any(test, feature = "testdb")))]`.

- [ ] **Step 3: Write the failing header test** in `http.rs`:
  `header_lookup_ignores_case` (a `Response` with
  `("retry-after", "30")` answers `header("Retry-After") == Some("30")`).
  Run `cargo test -p hatchrun-cloud http`; expect FAIL, then add the
  field, the method and the `fetch` change.

- [ ] **Step 4: Check both builds and the tests**

Run:
```bash
cargo test -p hatchrun-cloud
cargo check -p hatchrun-cloud --target wasm32-unknown-unknown
cargo check -p hatchrun-cloud --no-default-features --target wasm32-unknown-unknown
```
Expected: all tests pass (only `Response` literals gain
`headers: Vec::new()`); both checks build. Add the
`--no-default-features` check to the wasm step of `.github/workflows/ci.yml`.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud .github/workflows/ci.yml
git commit -S -s -m "refactor(cloud): gate worker entry points behind a feature for reuse"
```

---

### Task 2 [standard]: Shared instance start and metadata off

**Files:**
- Create: `crates/cloud/src/start.rs`
- Modify: `crates/cloud/src/launch.rs`, `crates/cloud/src/lib.rs`,
  `crates/cloud/src/aws/ec2.rs`, `crates/cloud/src/tokens.rs` and
  `crates/cloud/src/app/aws.rs` (both import `app_jwt` /
  `tenant_credentials` from `crate::launch` today; they import from
  `crate::start` instead, and `app/aws.rs` calls `role_credentials`
  with `subject_tenant` = the tenant id)

**Interfaces:**
- Consumes: Task 1 visibility.
- Produces, in `hatchrun_cloud::start`:

```rust
pub use crate::reconcile::retry::{Cause, Failure};
pub struct Target<'a> {
    pub region: &'a str,
    pub endpoint: String,                 // aws::endpoint("ec2", region, override)
    pub credentials: &'a Credentials,
    pub launch_template_id: &'a str,
    pub subnets: Vec<Option<&'a str>>,    // in order; [None] = template network
    pub fleet: bool,
    pub offerings_scope: &'a str,         // unavailable_offerings.tenant_id
}
pub struct Instance<'a> {
    pub name: &'a str,                    // client token prefix, Name tag
    pub choice: &'a Choice,
    pub image_id: &'a str,
    pub root_device: &'a str,
    pub user_data: &'a str,
    pub tags: &'a [(&'a str, &'a str)],
    pub log_id: &'a str,                  // "job 123" in console lines
}
pub async fn start_instance(db: &Db, target: &Target<'_>, instance: &Instance<'_>) -> Result<String, Failure>;
pub async fn terminate_instance(target: &Target<'_>, instance_id: &str);
pub async fn app_jwt(app_id: &str, pkcs8: &[u8]) -> Result<String, String>;
pub async fn role_credentials(oidc_key: &[u8], oidc_kid: &str, aws_endpoint: Option<&str>,
    subject_tenant: &str, role_arn: &str, region: &str, session: &str)
    -> Result<Credentials, aws::Error>;
```
- Produces, in `hatchrun_cloud::aws::ec2`:
  - `pub fn metadata_off(endpoint: &str, instance_id: &str) -> Request`
    (`Action=ModifyInstanceMetadataOptions`, `InstanceId`,
    `HttpEndpoint=disabled`, `Version=2016-11-15`)
  - `pub fn describe_metadata(endpoint: &str, instance_id: &str) -> Request`
    (`DescribeInstances`, `InstanceId.1`)
  - `pub struct MetadataState { pub state: String, pub http_endpoint: String }`
  - `pub fn metadata_state(response: &Response) -> Result<Option<MetadataState>, Error>`
    (`None` when the instance is not listed)

- [ ] **Step 1: Write the failing tests** in `aws/ec2.rs`

```rust
#[test]
fn metadata_off_disables_the_endpoint() {
    let req = metadata_off("https://ec2.us-east-1.amazonaws.com", "i-1");
    let body = String::from_utf8(req.body).unwrap();
    assert!(body.contains("Action=ModifyInstanceMetadataOptions"));
    assert!(body.contains("InstanceId=i-1"));
    assert!(body.contains("HttpEndpoint=disabled"));
}
#[test]
fn reads_metadata_state() { /* XML with <metadataOptions><state>applied</state>
    <httpEndpoint>disabled</httpEndpoint> -> Some(applied, disabled);
    empty reservationSet -> None */ }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-cloud aws::ec2`
Expected: FAIL, `metadata_off` not defined.

- [ ] **Step 3: Implement** the ec2 functions, then move `fleet`,
  `fleets`, `run_instances`, `refusals`, `terminate`, `app_jwt` and
  `tenant_credentials` (renamed `role_credentials`, taking the subject
  tenant) from `launch.rs` into `start.rs` over `Target` and
  `Instance`. `launch.rs` builds a `Target` from its tenant row
  (`offerings_scope` = tenant id) and calls `start_instance`. Behaviour
  of hatchrun.io launches stays the same.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-cloud`
Expected: PASS, including every existing launch test.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud
git commit -S -s -m "refactor(cloud): share instance start and add metadata off requests"
```

---

### Task 3 [standard]: The hatchrun-ci crate, schema and store

**Files:**
- Create: `crates/ci/Cargo.toml`, `crates/ci/build.rs`,
  `crates/ci/wrangler.toml`, `crates/ci/src/lib.rs`,
  `crates/ci/src/store.rs`, `crates/cloud/migrations/0004_ci.sql`
- Modify: `Cargo.toml` (release profile `strip = "debuginfo"` for
  `hatchrun-ci`), `.github/workflows/ci.yml` (add `-p hatchrun-ci` to
  the wasm check)

**Interfaces:**
- Consumes: Task 1 (`hatchrun_cloud::db::Db`, `params!`, `testdb`).
- Produces: the schema below and, in `hatchrun_ci::store`:

```rust
pub struct TenantRow { pub id: String, pub ci_enabled: bool }
pub struct RepoRow { pub id: u64, pub tenant_id: String, pub installation_id: u64,
    pub full_name: String, pub default_branch: String, pub private: bool }
pub struct NewRun { pub tenant_id: String, pub repo_id: u64, pub workflow: String,
    pub delivery: String, pub event: String, pub git_ref: String, pub sha: String,
    pub actor: String, pub now: u64 }
pub struct RunRow { pub id: u64, pub run_number: u64, pub run_attempt: u32, pub status: String,
    pub conclusion: Option<String>, pub created_at: u64, pub completed_at: Option<u64>,
    /* NewRun fields */ }
pub struct NewJob { pub plan_id: String, pub name: String, pub timeout_minutes: u32 }
pub struct JobRow { pub id: u64, pub run_id: u64, pub plan_id: String, pub name: String,
    pub status: String, pub conclusion: Option<String>, pub instance_id: Option<String>,
    pub attempt: u32, pub launch_attempts: u32, pub next_launch_at: Option<u64>,
    pub timeout_minutes: u32, pub updated_at: u64, pub heartbeat_at: Option<u64>,
    pub started_at: Option<u64>, pub completed_at: Option<u64>,
    pub steps: serde_json::Value, pub log_key: Option<String> }
pub struct ScheduleRow { pub repo_id: u64, pub workflow: String, pub cron: String }

pub async fn tenant_for_installation(db: &Db, installation_id: u64) -> Result<Option<TenantRow>>;
pub async fn upsert_repo(db: &Db, repo: &RepoRow, now: u64) -> Result<()>;
pub async fn repo(db: &Db, repo_id: u64) -> Result<Option<RepoRow>>;
pub async fn repo_by_name(db: &Db, full_name: &str) -> Result<Option<RepoRow>>;
pub async fn delivery_seen(db: &Db, guid: &str) -> Result<bool>;
pub async fn record_delivery(db: &Db, guid: &str, event: &str, now: u64) -> Result<()>;
pub async fn insert_run(db: &Db, run: &NewRun) -> Result<Option<RunRow>>; // None on (delivery, workflow) conflict
pub async fn insert_jobs(db: &Db, run_id: u64, jobs: &[NewJob], now: u64) -> Result<Vec<(String, u64)>>; // status 'waiting'
pub async fn run(db: &Db, run_id: u64) -> Result<Option<RunRow>>;
pub async fn runs_for_repo(db: &Db, repo_id: u64, before: Option<u64>, limit: u32) -> Result<Vec<RunRow>>; // id < before, newest first
pub async fn jobs_for_run(db: &Db, run_id: u64) -> Result<Vec<JobRow>>;     // plan order (id)
pub async fn open_jobs(db: &Db, limit: u32) -> Result<Vec<JobRow>>;         // status <> 'completed', oldest updated_at first
pub async fn jobs_by_ids(db: &Db, ids: &[u64]) -> Result<Vec<JobRow>>;      // for orphan checks
pub async fn job(db: &Db, job_id: u64) -> Result<Option<JobRow>>;
pub async fn start_job(db: &Db, job_id: u64, now: u64) -> Result<()>;       // running, started_at = heartbeat_at = now
pub async fn reset_job(db: &Db, job_id: u64, attempt: u32, now: u64) -> Result<()>;
    // status 'waiting', attempt, launch_attempts 0, conclusion, instance_id, launch_error,
    // next_launch_at, heartbeat_at, started_at, completed_at and log_key NULL, steps '[]'
pub async fn run_started(db: &Db, run_id: u64, now: u64) -> Result<()>;     // queued -> in_progress
pub async fn reopen_run(db: &Db, run_id: u64, now: u64) -> Result<u32>;     // in_progress, conclusion NULL, run_attempt + 1; returns it
pub async fn set_job(db: &Db, job_id: u64, status: &str, conclusion: Option<&str>, now: u64) -> Result<()>;
pub async fn claim_launch(db: &Db, job_id: u64, now: u64) -> Result<Option<JobRow>>; // queued -> launching
pub async fn launched(db: &Db, job_id: u64, instance_id: &str, now: u64) -> Result<()>;
pub async fn launch_failed(db: &Db, job_id: u64, error: &str, retry_at: Option<u64>, now: u64) -> Result<()>;
pub async fn heartbeat(db: &Db, job_id: u64, now: u64) -> Result<()>;
pub async fn finish_job(db: &Db, job_id: u64, conclusion: &str, steps: &serde_json::Value,
    log_key: Option<&str>, now: u64) -> Result<()>;
pub async fn finish_run(db: &Db, run_id: u64, conclusion: &str, now: u64) -> Result<()>;
pub async fn replace_schedules(db: &Db, repo_id: u64, schedules: &[(String, String)]) -> Result<()>; // (workflow, cron)
pub async fn schedules(db: &Db, limit: u32) -> Result<Vec<ScheduleRow>>;
pub async fn lease(db: &Db, now: u64, until: u64) -> Result<bool>;         // takes ci_reconciler when lease_until < now
pub async fn last_minute(db: &Db) -> Result<Option<u64>>;
pub async fn set_last_minute(db: &Db, minute_ms: u64) -> Result<()>;
pub async fn sweep(db: &Db, now: u64) -> Result<Vec<u64>>;                 // ids of the runs it deleted
```

`crates/cloud/migrations/0004_ci.sql` (the plan fixes this schema):

```sql
ALTER TABLE tenants ADD COLUMN ci_enabled boolean NOT NULL DEFAULT false;

CREATE TABLE ci_repos (
  id bigint PRIMARY KEY,                       -- GitHub repository id
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  installation_id bigint NOT NULL,
  full_name text NOT NULL,
  default_branch text NOT NULL,
  private boolean NOT NULL,
  updated_at bigint NOT NULL
);
CREATE UNIQUE INDEX ci_repos_name ON ci_repos (lower(full_name));

CREATE TABLE ci_deliveries (
  guid text PRIMARY KEY,                       -- X-GitHub-Delivery, or schedule:/dispatch: keys
  event text NOT NULL,
  received_at bigint NOT NULL
);
CREATE INDEX ci_deliveries_age ON ci_deliveries (received_at);

CREATE TABLE ci_schedules (
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  cron text NOT NULL,
  PRIMARY KEY (repo_id, workflow, cron)
);

CREATE TABLE ci_run_numbers (
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  last bigint NOT NULL,
  PRIMARY KEY (repo_id, workflow)
);

CREATE TABLE ci_runs (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  run_number bigint NOT NULL,
  run_attempt bigint NOT NULL DEFAULT 1,
  delivery text NOT NULL,
  event text NOT NULL,
  ref text NOT NULL,
  sha text NOT NULL,
  actor text NOT NULL,
  status text NOT NULL CHECK (status IN ('queued', 'in_progress', 'completed')),
  conclusion text CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  completed_at bigint,
  expires_at bigint NOT NULL,                  -- created_at + 30 days
  UNIQUE (delivery, workflow)
);
CREATE INDEX ci_runs_repo ON ci_runs (repo_id, created_at DESC);
CREATE INDEX ci_runs_expiry ON ci_runs (expires_at);

CREATE TABLE ci_jobs (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  run_id bigint NOT NULL REFERENCES ci_runs (id) ON DELETE CASCADE,
  plan_id text NOT NULL,                       -- PlannedJob::id
  name text NOT NULL,                          -- PlannedJob::name
  attempt bigint NOT NULL DEFAULT 1,
  status text NOT NULL CHECK (status IN
    ('waiting', 'queued', 'launching', 'launched', 'running', 'completed')),
  conclusion text CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  timeout_minutes bigint NOT NULL,
  instance_id text,
  launch_attempts bigint NOT NULL DEFAULT 0,
  launch_error text,
  next_launch_at bigint,
  heartbeat_at bigint,
  started_at bigint,
  completed_at bigint,
  steps jsonb NOT NULL DEFAULT '[]',           -- Vec<StepSummary>
  log_key text,
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  UNIQUE (run_id, plan_id)
);
CREATE INDEX ci_jobs_open ON ci_jobs (status, updated_at) WHERE status <> 'completed';
CREATE INDEX ci_jobs_instance ON ci_jobs (instance_id) WHERE instance_id IS NOT NULL;

CREATE TABLE ci_reconciler (
  id bigint PRIMARY KEY CHECK (id = 1),
  lease_until bigint NOT NULL,
  last_minute bigint                           -- last schedule minute fired, Unix ms
);
```

`insert_run` takes the run number with
`INSERT INTO ci_run_numbers ... ON CONFLICT DO UPDATE SET last = last + 1 RETURNING last`
in the same transaction (`Db::batch`) as the run row, so a conflicting
duplicate does not use up a number.

`Cargo.toml` dependencies: `hatchrun-cloud = { path = "../cloud", default-features = false }`,
`hatchrun-workflow`, `worker = { version = "0.8.7", features = ["queue", "tokio-postgres"] }`,
`tokio-postgres` (as in cloud, with `js` on wasm), `serde`, `serde_json`,
`hmac` 0.13, `sha2` 0.11, `base64` 0.23.1,
`flate2 = { default-features = false, features = ["rust_backend"] }`,
`futures-util`, `wasm-bindgen`, `wasm-bindgen-futures`, `js-sys`;
non-wasm dev-dependencies `hatchrun-cloud` with `features = ["testdb"]`
and `tokio` as in cloud. `[lib] crate-type = ["cdylib", "rlib"]`.
`build.rs` prints `cargo::rustc-check-cfg=cfg(hatchrun_local_launcher)`.

`wrangler.toml`, with a comment on each binding as in
`crates/cloud/wrangler.toml`:

```toml
name = "hatchrun-ci"
main = "build/worker/shim.mjs"
compatibility_date = "2026-09-01"
routes = [{ pattern = "ci.hatchrun.io", custom_domain = true }]
[build]
command = "cargo install --locked worker-build --version 0.8.7 && worker-build --release"
[triggers]
crons = ["* * * * *"]
[[hyperdrive]]
binding = "HYPERDRIVE"
id = "hyperdrive-local"
localConnectionString = "postgres://postgres:postgres@localhost:5432/postgres"
[[kv_namespaces]]
binding = "TOKENS"
id = "local"
[[durable_objects.bindings]]
name = "RUNS"
class_name = "RunObject"
[[migrations]]
tag = "v1"
new_sqlite_classes = ["RunObject"]
[[r2_buckets]]
binding = "LOGS"
bucket_name = "hatchrun-ci-logs"
[[queues.producers]]
binding = "CI_LAUNCHES"
queue = "hatchrun-ci-launches"
[[queues.consumers]]
queue = "hatchrun-ci-launches"
max_batch_size = 5
max_batch_timeout = 1
max_retries = 3
max_concurrency = 10
dead_letter_queue = "hatchrun-ci-launches-dlq"
[[queues.consumers]]
queue = "hatchrun-ci-launches-dlq"
max_batch_size = 10
max_batch_timeout = 5
max_retries = 3
[vars]
DEPLOY_ENV = "production"
LAUNCHER = "ec2"
APP_URL = "https://app.hatchrun.io"
CI_URL = "https://ci.hatchrun.io"
BROKER_URL = "https://broker.hatchrun.io"
GITHUB_APP_ID = "Iv23liMh4C066YyxQB7R"
OIDC_KEY_ID = "2026-10-14"
CI_REGION = "us-east-1"
# Set by the deploy workflow from the CI account stack's outputs.
CI_ROLE_ARN = ""
CI_LAUNCH_TEMPLATE_ID = ""
CI_SUBNET_IDS = ""
# Secrets: GITHUB_WEBHOOK_SECRET, GITHUB_APP_PRIVATE_KEY,
# OIDC_SIGNING_KEY, BROKER_TOKEN, CI_TOKEN_KEY.
```

`lib.rs` holds the module list and a doc comment naming the routes;
the `#[event]` functions come in Tasks 9 to 16 as each route lands.
This task's `lib.rs` has `#[event(fetch)]` answering 404 for every path.

- [ ] **Step 1: Write the failing store tests** in `store.rs`, each on
  `hatchrun_cloud::testdb::Db::new()` with a tenant and repo inserted:

```rust
#[test] fn insert_run_dedupes_on_delivery_and_workflow()
    // two insert_run with delivery "d1", workflow "ci" -> second is None;
    // workflow "lint" with "d1" -> Some
#[test] fn run_numbers_count_per_workflow()
    // ci, ci, lint -> run_number 1, 2, 1
#[test] fn claim_launch_only_once()
    // job status 'queued': first claim Some with launch_attempts 1,
    // second None; status is 'launching'
#[test] fn sweep_deletes_expired_runs_and_old_deliveries()
    // run expires_at < now is gone with its jobs and its id returned;
    // delivery older than 30 days gone, newer kept
#[test] fn reset_job_clears_the_attempt()
    // after start_job + finish_job, reset_job(id, 2): status waiting,
    // attempt 2, launch_attempts 0, heartbeat_at None, steps []
#[test] fn start_job_sets_the_heartbeat()   // heartbeat_at == started_at == now
#[test] fn reopen_run_counts_attempts()     // completed run -> in_progress, returns 2
#[test] fn lease_is_taken_once()            // lease(now, now+55s) true, again false
#[test] fn replace_schedules_replaces_the_repo_rows()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci store`
Expected: FAIL, functions not defined.

- [ ] **Step 3: Write the migration and implement `store.rs`.** Keep
  each SQL string in a `const` with a doc comment saying what it binds,
  as `crates/cloud/src/jobs.rs` does. `sweep` deletes `ci_runs` with
  `expires_at < now` (jobs cascade) and `ci_deliveries` with
  `received_at < now - 30 days`, and returns the deleted run ids so the
  reconciler can purge their objects. The migration inserts the one
  `ci_reconciler` row (`id 1, lease_until 0`).

- [ ] **Step 4: Run the tests and the wasm build**

Run:
```bash
cargo test -p hatchrun-ci store && cargo test -p hatchrun-cloud
cargo check -p hatchrun-ci --target wasm32-unknown-unknown
```
Expected: PASS; the cloud tests also run the new migration.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ci crates/cloud/migrations/0004_ci.sql .github/workflows/ci.yml
git commit -S -s -m "feat(ci): add the hatchrun-ci worker crate and its tables"
```

---

### Task 4 [cheap]: Agent protocol types and job tokens

**Files:**
- Create: `crates/workflow/src/agent.rs`, `crates/ci/src/token.rs`
- Modify: `crates/workflow/src/lib.rs`, `crates/ci/src/lib.rs`
- Test: `crates/workflow/tests/agent.rs`

**Interfaces:**
- Produces, in `hatchrun_workflow::agent` (re-exported as
  `hatchrun_workflow::agent`, not at the root):

```rust
pub const CLAIM_PATH: &str = "/v1/agent/claim";
pub const EVENTS_PATH: &str = "/v1/agent/events";
pub const RESULT_PATH: &str = "/v1/agent/result";
pub const FLUSH_INTERVAL_MS: u64 = 1_000;
pub const HEARTBEAT_INTERVAL_MS: u64 = 10_000;
pub const HEARTBEAT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

/// What the broker hands a CI runner: {"ci": {"url": ..., "token": ...}}.
pub struct BootDocument { pub ci: CiBoot }
pub struct CiBoot { pub url: String, pub token: String }   // Debug redacts token

#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    StepStarted { index: usize, name: String },
    Line { text: String },
    Annotation { annotation: Annotation },
    StepFinished { index: usize, summary: StepSummary },
}
pub struct Batch { pub seq: u64, pub events: Vec<AgentEvent> }
pub struct BatchReply { pub cancel: u32 }
```

  Claim: `POST CLAIM_PATH` with `Authorization: Bearer <token>`, empty
  body; `200` with a `JobDocument`, `503` to retry after a second,
  `409` when the job ended (the agent exits). Events: `POST EVENTS_PATH`
  with a `Batch`, `200` with a `BatchReply`. Result: `POST RESULT_PATH`
  with a `JobResult`, `204`. Any `401` ends the agent.

- Produces, in `hatchrun_ci::token`:

```rust
pub struct Claims { pub job: u64, pub run: u64, pub exp: u64 }   // exp in Unix seconds
pub enum TokenError { Malformed, BadSignature, Expired }
pub const BOOT_SECS: u64 = 15 * 60;
pub const GRACE_SECS: u64 = 10 * 60;
pub fn lifetime_secs(timeout_minutes: u32) -> u64;   // timeout*60 + BOOT_SECS + GRACE_SECS
pub fn sign(key: &[u8], claims: &Claims) -> String;
pub fn verify(key: &[u8], token: &str, now_secs: u64) -> Result<Claims, TokenError>;
```

  Format: `v1.<b64url(claims JSON)>.<b64url(HMAC-SHA256(key, "v1." + b64url(claims JSON)))>`,
  base64url without padding; `verify` compares with `Mac::verify_slice`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/workflow/tests/agent.rs
#[test] fn events_round_trip_with_a_type_tag() {
    let e = AgentEvent::Line { text: "hi".into() };
    assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"type":"line","text":"hi"}"#);
}
#[test] fn boot_document_debug_hides_the_token() {
    let d = BootDocument { ci: CiBoot { url: "https://ci".into(), token: "secret".into() } };
    assert!(!format!("{d:?}").contains("secret"));
}
// crates/ci/src/token.rs
#[test] fn round_trips() { /* sign then verify at exp-1 -> same Claims */ }
#[test] fn rejects_expired() { /* verify at exp -> Expired */ }
#[test] fn rejects_a_changed_claim() { /* swap the middle part -> BadSignature */ }
#[test] fn rejects_another_key() { /* -> BadSignature */ }
#[test] fn lifetime_covers_the_timeout() { assert_eq!(lifetime_secs(360), 360 * 60 + 1500); }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow --test agent && cargo test -p hatchrun-ci token`
Expected: FAIL, types not defined.

- [ ] **Step 3: Implement** both modules as specified.

- [ ] **Step 4: Run the tests and the wasm build**

Run: `cargo test -p hatchrun-workflow --test agent && cargo test -p hatchrun-ci token && cargo build -p hatchrun-workflow --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow crates/ci
git commit -S -s -m "feat(ci): define the agent job protocol and job tokens"
```

---

### Task 5 [standard]: Webhook intake and run planning

**Files:**
- Create: `crates/ci/src/intake.rs`, `crates/ci/src/planning.rs`
- Create: `crates/ci/testdata/push.json`, `pull_request.json`,
  `pull_request_fork.json`, `check_run_rerequested.json`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Produces, in `hatchrun_ci::intake`:

```rust
pub struct Repo { pub id: u64, pub full_name: String, pub default_branch: String, pub private: bool }
pub struct Push { pub installation_id: u64, pub repo: Repo, pub r#ref: String,
    pub after: String, pub actor: String, pub changed: Vec<String>, pub payload: serde_json::Value }
pub struct PullRequest { pub installation_id: u64, pub repo: Repo, pub action: String,
    pub number: u64, pub head_sha: String, pub head_ref: String, pub base_ref: String,
    pub actor: String, pub payload: serde_json::Value }
pub struct Rerequested { pub installation_id: u64, pub repo: Repo, pub job_id: u64 }
pub enum Trigger { Push(Push), PullRequest(PullRequest), Rerequested(Rerequested), Ignored(String) }
pub fn parse(event: &str, body: &[u8]) -> Result<Trigger, serde_json::Error>;
```

  Rules: `push` with `deleted: true` is `Ignored("ref deleted")`.
  `changed` is the sorted union of `added`, `removed` and `modified` of
  `commits`, empty when `commits` is empty. `pull_request` whose
  `head.repo` is `null` or whose `head.repo.id` differs from
  `repository.id` is `Ignored("pull request from a fork")`. `check_run`
  other than `rerequested`, or whose `external_id` does not parse with
  `checks::parse_external_id` (Task 8; here a local
  `strip_prefix("hatchrun-ci:")` and `parse::<u64>()`), is ignored. A
  payload without `installation` is `Ignored("no App installation")`.
  Any other event is `Ignored("<event> events are not used")`.

- Produces, in `hatchrun_ci::planning`:

```rust
pub enum Selected { Plans(Vec<Plan>), Invalid(Vec<Diagnostic>) }
pub fn select(source: &str, event: &hatchrun_workflow::Event, compile_ref: &str) -> Selected;
pub fn schedules(source: &str) -> Vec<(String, String)>;   // (workflow, cron); empty on a parse error
pub fn launchable(template: &Template) -> Result<(), String>;
```

  `select` parses the config, keeps the workflows (in name order) whose
  `on:` is `triggered` by `event`, compiles each with `compile_ref`, and
  drops plans with no jobs. Any parse or compile error makes the whole
  result `Invalid`. `launchable` accepts only the template whose
  `labels[0]` is `"ubuntu-latest"` and otherwise returns the message in
  Global Constraints, quoting `labels[0]`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn push_lists_changed_paths() // fixture -> Push with changed ["README.md", "src/a.rs"]
#[test] fn deleted_ref_is_ignored()
#[test] fn pull_request_from_same_repo_is_read() // head_sha, base_ref "main", action "opened"
#[test] fn fork_pull_request_is_ignored() // head.repo.id != repository.id
#[test] fn fork_with_deleted_head_repo_is_ignored() // head.repo: null
#[test] fn rerequested_names_the_job() // external_id "hatchrun-ci:42" -> job_id 42
#[test] fn foreign_check_run_is_ignored() // external_id "other"
#[test] fn selects_triggered_workflows_only()
    // config with `ci` on push main and `nightly` on schedule;
    // Event::Push { ref: "refs/heads/main" } -> one plan named "ci"
#[test] fn compile_errors_make_the_result_invalid()
#[test] fn plan_with_every_job_filtered_out_is_dropped()
#[test] fn other_templates_are_not_launchable()
    // template("ubuntu-26.04-arm") -> Err containing "`ubuntu-26.04-arm` runners come in a later release"
#[test] fn schedules_lists_every_cron()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci`
Expected: FAIL.

- [ ] **Step 3: Implement** both modules.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): read GitHub events and select the workflows they start"
```

---

### Task 6 [standard]: GitHub requests

**Files:**
- Create: `crates/ci/src/github.rs`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `hatchrun_cloud::http::{Request, Response}`, with
  `Response::header` (Task 1) for the rate limit headers.
- Produces, in `hatchrun_ci::github` (requests as data, like
  `crates/cloud/src/github.rs`; every request sets
  `Accept: application/vnd.github+json`,
  `X-GitHub-Api-Version: 2022-11-28`, `User-Agent: hatchrun-ci`):

```rust
pub const CONFIG_PATH: &str = ".hatchrun/config.yml";
pub fn config_file(api: &str, repo: &str, sha: &str, token: &str) -> Request; // Accept: application/vnd.github.raw+json
pub fn config_text(response: &Response) -> Result<Option<String>, String>;    // 404 -> None; over MAX_SOURCE -> Err
pub fn pull_files(api: &str, repo: &str, number: u64, page: u32, token: &str) -> Request; // per_page=100
pub fn pull_file_names(response: &Response) -> Result<Vec<String>, String>;
pub fn commit_sha(api: &str, repo: &str, git_ref: &str, token: &str) -> Request;   // GET /repos/{repo}/commits/{ref}
pub fn sha_of(response: &Response) -> Result<String, String>;
pub fn scoped_token(api: &str, installation_id: u64, app_jwt: &str, repo_id: u64) -> Request;
    // POST /app/installations/{id}/access_tokens
    // {"repository_ids":[repo_id],"permissions":{"contents":"read","metadata":"read","checks":"read","pull_requests":"read"}}
pub fn revoke_token(api: &str, token: &str) -> Request;  // DELETE /installation/token
pub struct CheckRun { pub name: String, pub head_sha: String, pub external_id: String,
    pub details_url: String, pub status: String, pub started_at: Option<String>,
    pub conclusion: Option<String>, pub completed_at: Option<String>, pub output: Option<Output> }
pub struct Output { pub title: String, pub summary: String, pub text: Option<String>,
    pub annotations: Vec<GhAnnotation> }
pub struct GhAnnotation { pub path: String, pub start_line: u32, pub end_line: u32,
    pub start_column: Option<u32>, pub end_column: Option<u32>,
    pub annotation_level: String, pub message: String, pub title: Option<String> }
pub fn create_check_run(api: &str, repo: &str, token: &str, run: &CheckRun) -> Request;
pub fn update_check_run(api: &str, repo: &str, id: u64, token: &str, run: &CheckRun) -> Request; // PATCH, no head_sha
pub fn check_run_id(response: &Response) -> Result<u64, String>;
pub fn user_repo(api: &str, repo: &str, user_token: &str) -> Request;  // GET /repos/{repo}
pub struct Permissions { pub read: bool, pub write: bool }
pub fn permissions(response: &Response) -> Permissions; // non-200 -> both false; private:false -> read
pub fn throttle(response: &Response, now_secs: u64, attempt: u32) -> Option<u64>;
```

  `throttle` returns the seconds to wait for a `429`, or a `403` that
  has `retry-after`, has `x-ratelimit-remaining: 0`, or whose body
  mentions `rate limit`: `retry-after` when present, else
  `x-ratelimit-reset - now_secs` when remaining is `0`, else
  `min(60 * 2^attempt, 900)`. Every other answer returns `None`.
  Times on a `CheckRun` are RFC 3339 UTC; add
  `pub fn rfc3339(ms: u64) -> String` here.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn config_file_asks_for_raw_content_at_the_sha()
#[test] fn missing_config_is_none()           // 404 -> Ok(None)
#[test] fn scoped_token_names_one_repository()
#[test] fn update_leaves_out_head_sha()
#[test] fn throttle_prefers_retry_after()     // 403 + retry-after: 30 -> Some(30)
#[test] fn throttle_waits_for_reset()         // 403, remaining 0, reset now+90 -> Some(90)
#[test] fn secondary_limit_without_headers_backs_off()
    // 403, body "You have exceeded a secondary rate limit": attempt 0 -> 60, 3 -> 480, 9 -> 900
#[test] fn plain_forbidden_is_not_throttled() // 403 "Resource not accessible" -> None
#[test] fn public_repo_is_readable_without_permissions()
#[test] fn rfc3339_formats_utc()              // 1792676700000 -> "2026-10-22T13:45:00Z"
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci github`
Expected: FAIL.

- [ ] **Step 3: Implement** `github.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci github`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): build the GitHub requests the control plane sends"
```

---

### Task 7 [standard]: The job graph

**Files:**
- Create: `crates/ci/src/graph.rs`
- Modify: `crates/workflow/src/expr/mod.rs`, `crates/workflow/src/lib.rs`,
  `crates/hatchrun-agent/src/exec/context.rs`, `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `planning::launchable` (Task 5), `hatchrun_workflow::{PlannedJob, JobKind, Need, Conclusion, JobResult, JobStatus, Functions}`.
- Produces, in `hatchrun_workflow::expr` (moved from the agent's
  `exec::context::condition`, which now calls it):
  `pub fn job_condition(cond: Option<&str>, ctx: &Value, funcs: &dyn Functions) -> Result<bool, ExprError>`
  (no `cond` means `funcs.status() == Success`; a `${{ }}` wrapper is
  stripped; a condition that calls no status function runs as
  `success() && (cond)`).
- Produces, in `hatchrun_ci::graph` (all `Serialize`/`Deserialize`):

```rust
pub struct Graph { pub jobs: Vec<Node>, pub cancelled: bool }
pub struct Node { pub id: u64, pub job: PlannedJob, pub state: State, pub attempt: u32,
    pub result: Option<JobResult>, pub message: Option<String>, pub cancel: u32 }
pub enum State { Waiting, Queued, Running, Done(Conclusion) }
pub enum Action {
    Launch(u64),
    Finished { job: u64, conclusion: Conclusion },  // check run and row must follow
    Cancel { job: u64, count: u32 },                // tell the agent through BatchReply
    RunDone(Conclusion),
}
impl Graph {
    pub fn new(jobs: Vec<(u64, PlannedJob)>) -> Self;
    pub fn advance(&mut self, github_ctx: &Value) -> Vec<Action>;
    pub fn running(&mut self, job: u64);
    pub fn finished(&mut self, job: u64, result: JobResult, github_ctx: &Value) -> Vec<Action>;
    pub fn failed(&mut self, job: u64, message: String, github_ctx: &Value) -> Vec<Action>;
    pub fn cancel(&mut self, github_ctx: &Value) -> Vec<Action>;
    pub fn escalate(&mut self) -> Vec<Action>;      // running jobs with cancel 1 -> 2
    pub fn rerun(&mut self, job: u64, github_ctx: &Value) -> Vec<Action>;
    pub fn needs(&self, job: u64) -> BTreeMap<String, Need>;
    pub fn conclusion(&self) -> Option<Conclusion>; // None while any job is not Done
    pub fn node(&self, job: u64) -> Option<&Node>;
}
```

  `github_ctx` is the expression context's `github` object, built by
  `run::github_context` (Task 9): a JSON object with `repository`,
  `repository_owner`, `sha`, `ref`, `ref_name`, `ref_type`, `head_ref`,
  `base_ref`, `actor`, `event_name`, `event`, `workflow`, `run_id`,
  `run_number`, `run_attempt`, `server_url`, `api_url`, and no token. It
  is not `JobDocument.github` (the typed `Github` struct, which carries
  the token and is built separately by `run::github_struct`).

  `advance` looks at each `Waiting` node whose `requires` are all `Done`,
  in plan order:
  - `JobKind::Approval`: `Done(Skipped)`, message `Approval jobs come in a later release.`;
  - `launchable` fails: `Done(Failure)` with its message;
  - otherwise evaluate `job_condition(job.if_, ctx)` where `ctx` is
    `{"github": github_ctx, "needs": needs, "matrix": job.matrix, "vars": {}, "inputs": github_ctx.event.inputs or {}}`
    and `status()` is `Cancelled` when the graph is cancelled, else
    `Success` when every need succeeded, else `Failure`; `hashFiles`
    returns an error. `true` → `Queued` + `Launch`; `false` →
    `Done(Cancelled)` when the graph is cancelled, else `Done(Skipped)`;
    an error → `Done(Failure)` with `if: <error>`. As on GitHub, after a
    cancel only jobs whose `if:` calls `always()` or `cancelled()` and
    is true still run.
  Every `Done` set here also emits `Finished`. When all nodes are `Done`
  it emits `RunDone` once with: `Failure` if any failed, else
  `Cancelled` if any was cancelled, else `Success` if any succeeded,
  else `Skipped`.

  `needs(job)` groups the required nodes by `job.name`. A group's
  `result` is `Failure` if any failed, else `Cancelled` if any was
  cancelled, else `Success` if any succeeded, else `Skipped`; its
  `outputs` merge the members' `result.outputs` in plan order, a later
  non-empty value replacing an earlier one.

  `cancel` marks the graph cancelled, sets `cancel = 1` on `Running`
  nodes with a `Cancel` action, and re-evaluates each `Queued` node's
  `if:` with status `Cancelled`: false ends it as `Cancelled` (its queue
  message finds the row `completed` and is dropped), true keeps it.
  `Waiting` nodes stay and meet the same rule in `advance`. `rerun`
  resets the job and every job that requires it, directly or not, to
  `Waiting` with `attempt + 1`, `result`, `message` and `cancel`
  cleared, clears `cancelled` and the emitted `RunDone`, then advances,
  so `RunDone` fires again when the rerun ends.

- [ ] **Step 1: Write the failing tests** in `graph.rs`, building
  plans with `hatchrun_workflow::compile` from small inline configs:

```rust
#[test] fn launches_roots_first()             // a, b requires a -> [Launch(a)]
#[test] fn success_starts_dependents()        // finished(a, success) -> [Finished(a), Launch(b)]
#[test] fn failure_skips_dependents()         // b without if -> Done(Skipped), RunDone(Failure)
#[test] fn always_runs_after_failure()        // b if: always() -> Launch(b)
#[test] fn job_if_reads_github()              // if: github.ref == 'refs/heads/main', github.ref = refs/heads/dev -> Skipped
#[test] fn matrix_needs_merge_in_plan_order()
    // build matrix [1,2,3] outputs v = "1","","3", build(2) failed
    // -> needs["build"] = Need { result: Failure, outputs: {"v": "3"} }
#[test] fn approval_is_skipped_with_a_message()
#[test] fn unsupported_template_fails_before_launch()
#[test] fn cancel_ends_waiting_and_signals_running()
#[test] fn always_job_still_runs_after_cancel() // b requires a, if: always(); cancel while a runs; a cancelled -> Launch(b)
#[test] fn rerun_emits_run_done_again()
#[test] fn escalate_raises_cancel_to_two()
#[test] fn rerun_resets_dependents()          // a, b requires a; rerun(a) -> a attempt 2 Queued, b Waiting
#[test] fn run_done_is_emitted_once()
```

  And in `crates/workflow/tests/expr.rs`:
  `job_condition_defaults_to_success` and
  `job_condition_wraps_without_status_calls`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci graph && cargo test -p hatchrun-workflow --test expr job_condition`
Expected: FAIL.

- [ ] **Step 3: Move `condition`** into `job_condition` and keep the
  agent's tests passing, then implement `graph.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci graph && cargo test -p hatchrun-workflow && cargo test -p hatchrun-agent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow crates/hatchrun-agent crates/ci
git commit -S -s -m "feat(ci): order jobs, evaluate if and merge matrix needs"
```

---

### Task 8 [standard]: Check run rendering and the outbox

**Files:**
- Create: `crates/ci/src/checks.rs`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `github::{CheckRun, Output, GhAnnotation, rfc3339}` (Task 6),
  `graph::{Node, State}` (Task 7).
- Produces, in `hatchrun_ci::checks`:

```rust
pub const MAX_ANNOTATIONS: usize = 50;
pub const TAIL_LINES: usize = 50;
pub fn check_name(workflow: &str, plan_id: &str) -> String;     // "{workflow} / {plan_id}"
pub fn external_id(job_id: u64) -> String;                      // "hatchrun-ci:{job_id}"
pub fn parse_external_id(value: &str) -> Option<u64>;
pub fn details_url(app_url: &str, repo: &str, run_id: u64, job_id: u64) -> String;
    // "{app_url}/ci/{owner}/{repo}/runs/{run_id}/jobs/{job_id}"
pub struct StepView { pub index: usize, pub name: String, pub summary: Option<StepSummary> }
pub struct JobView<'a> {
    pub workflow: &'a str, pub node: &'a Node, pub head_sha: &'a str, pub details_url: String,
    pub steps: &'a [StepView],
    pub annotations: &'a [Annotation],   // every annotation of the attempt, as stored
    pub file_annotations_sent: usize,    // how many with a file GitHub already has
    pub failed_tail: Option<&'a [String]>, pub started_at: Option<u64>, pub completed_at: Option<u64>,
}
pub fn render(view: &JobView<'_>) -> (CheckRun, usize);   // the request and the new file_annotations_sent
pub fn annotation(a: &Annotation) -> Option<GhAnnotation>;

pub struct Outbox { /* serde */ }
impl Outbox {
    pub fn mark(&mut self, job: u64);
    pub fn due(&self, now_ms: u64) -> Vec<u64>;     // empty while blocked
    pub fn sent(&mut self, job: u64);               // clears the mark and the attempt count
    pub fn throttled(&mut self, now_ms: u64, wait_secs: u64);
    pub fn attempt(&self) -> u32;
    pub fn next_alarm(&self, now_ms: u64) -> Option<u64>;
}
```

  `render`: `Waiting`/`Queued` → status `queued`; `Running` →
  `in_progress` with `started_at`; `Done(c)` → `completed` with
  conclusion `success`/`failure`/`cancelled`/`skipped`. Output title is
  `Succeeded`, `Failed`, `Cancelled`, `Skipped`, `Running` or `Queued`,
  with ` in <m>m <s>s` when both times are known. The summary is the
  node's `message` (if any) as a paragraph, then a Markdown table
  `| Step | Result | Time |` with one row per step. `text` is the failed
  tail in a fenced block, if any. Every render works from the whole
  stored list. Annotations without `file` are listed in the summary
  (`- **error**: message`) on every update, since each PATCH replaces
  `output.summary`. Annotations with a file go in `output.annotations`,
  but only those past `file_annotations_sent`, because GitHub appends
  the annotations of each request to the ones it has; resending them
  would duplicate them. At most `MAX_ANNOTATIONS` with a file are sent
  per job attempt. Levels map
  `Error` → `failure`, `Warning` → `warning`, `Notice` → `notice`;
  `start_line` defaults to 1, `end_line` to `start_line`; columns are
  sent only when `start_line == end_line`.

  The outbox keeps the set of marked jobs, never a queue of states, so a
  job marked many times while blocked sends its latest state once.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn names_and_external_ids_round_trip()
#[test] fn running_job_is_in_progress()
#[test] fn failed_job_shows_the_tail()
#[test] fn error_with_a_file_becomes_an_annotation()
    // Annotation { level: Error, file: Some("src/a.rs"), line: Some(3), .. } -> path src/a.rs, start 3, end 3, failure
#[test] fn annotation_without_a_file_goes_in_the_summary()
#[test] fn caps_annotations_at_fifty()
#[test] fn later_updates_keep_every_summary_annotation()
    // two file-less and one file annotation; render with sent 1 -> summary lists both
    // file-less ones, output.annotations empty, returned sent still 1
#[test] fn outbox_keeps_only_the_latest_state_while_blocked()
    // mark(1); throttled(now, 60); mark(1); mark(1); due(now+59s) empty; due(now+60s) == [1]
#[test] fn next_alarm_is_the_unblock_time()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci checks`
Expected: FAIL.

- [ ] **Step 3: Implement** `checks.rs`; replace the local external id
  parsing in `intake.rs` with `parse_external_id`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): render check runs and coalesce their updates"
```

---

### Task 9 [standard]: Run Durable Object: graph, check runs and launches

**Files:**
- Create: `crates/ci/src/run.rs`, `crates/ci/src/env.rs`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `graph`, `checks`, `github`, `store` (Tasks 3 to 8),
  `hatchrun_cloud::tokens::{Tokens, Kv, App}`,
  `hatchrun_cloud::fetch::Counted`.
- Produces, in `hatchrun_ci::env`:
  `pub struct Settings { app_id, app_key: Vec<u8>, github_api: String, app_url: String, ci_url: String, broker_url: String, broker_token: String, token_key: Vec<u8>, deploy_env: String, launcher: String, local_launcher_url: Option<String>, ci_region: String, ci_role_arn: String, ci_launch_template_id: String, ci_subnet_ids: Vec<String>, oidc_key: Vec<u8>, oidc_kid: String, aws_endpoint: Option<String> }`
  with `Settings::read(env: &Env) -> Result<Settings, String>` (secrets
  as in Task 3's `wrangler.toml`; `GITHUB_API_URL` and
  `AWS_ENDPOINT_URL` override for development, as in cloud), and
  `pub fn run_stub(env: &Env, run_id: u64) -> Result<Stub>` (DO named
  `run_id.to_string()` in `RUNS`). `Settings::read` accepts empty
  `CI_ROLE_ARN`, `CI_LAUNCH_TEMPLATE_ID` and `CI_SUBNET_IDS`; the `Ec2`
  backend refuses to launch with them empty.
- Produces, in `hatchrun_ci::run`: `#[durable_object] pub struct RunObject`
  and its internal routes, called only by this Worker through
  `run_stub` (host `https://run`):

| Route | Body | Answer |
| --- | --- | --- |
| `POST /start` | `Start { run: RunInfo, jobs: Vec<(u64, PlannedJob)>, event: Value }` | `204` |
| `POST /claim?job=ID` | `{ "token": "<scoped GitHub token>" }` | `200 JobDocument`, `409` ended |
| `POST /events?job=ID` | `Batch` | `200 BatchReply`, `409` ended |
| `POST /result?job=ID` | `JobResult` | `204` |
| `POST /failed?job=ID` | `{ "message": "..." }` | `204` |
| `POST /cancel` | none | `204` |
| `POST /rerun?job=ID` | none | `204` |
| `POST /purge` | none | `204` (deletes all storage; idempotent) |
| `GET /state` | none | `200 RunState` (graph nodes, step views) |

```rust
pub struct RunInfo { pub run_id: u64, pub run_number: u64, pub run_attempt: u32,
    pub tenant_id: String, pub installation_id: u64, pub repo: Repo, pub workflow: String,
    pub event_name: String, pub git_ref: String, pub sha: String, pub actor: String,
    pub head_ref: String, pub base_ref: String }
```

  Storage keys: `info` (RunInfo), `graph` (Graph), `outbox` (Outbox),
  `deadlines` (Deadlines), `event` (payload; when its JSON is over
  1 MiB the `commits` array is replaced by `[]`), `job:<id>`
  (`JobState { check_run_id: Option<u64>, steps: Vec<StepView>,
  annotations: Vec<Annotation>, file_annotations_sent: usize,
  started_at, completed_at, last_seq: u64, github_token:
  Option<String>, heartbeat_written_at: u64 }`).

```rust
/// Everything the object's single alarm waits for.
pub struct Deadlines { pub escalate_at: Option<u64>, pub upload_retry_at: Option<u64>,
    pub purge_at: Option<u64> }
pub(crate) fn next_alarm(deadlines: &Deadlines, outbox: &Outbox, now_ms: u64) -> Option<u64>; // earliest of all
pub(crate) fn github_context(info: &RunInfo, event: &Value, api: &str) -> Value;   // graph's github_ctx
pub(crate) fn github_struct(info: &RunInfo, api: &str, token: &str) -> Github;     // JobDocument.github
```

  A Durable Object has one alarm. Every handler and `alarm()` end by
  setting it to `next_alarm(..)` (or deleting it when `None`).
  `alarm()` runs, in order, each part whose time has come: the outbox
  flush, cancel escalation (`escalate_at`), R2 upload retries (Task 10)
  and the purge.

  Behaviour:
  - `/start` stores everything, marks every job in the outbox (creates
    its check run), calls `store::run_started`, runs `advance` with
    `github_context(..)` and executes the actions. The same value is
    passed to every graph call.
  - `Launch(job)` → `store::set_job(job, "queued")` and
    `CI_LAUNCHES.send({"job_id": job})`.
  - `Finished` → `store::finish_job` (steps JSON, log key from Task 10),
    mark the outbox, revoke the job's GitHub token if one is held.
  - `RunDone` → `store::finish_run`, then, once no job has a pending
    upload (Task 10) and the outbox is empty, the object writes
    `{info, graph, event}` to R2 at `state/<tenant>/<run>.json` and sets
    `purge_at` to now; the purge deletes all storage
    (`storage.delete_all()`). A later route on an empty object (only
    `/rerun` and `/state` can arrive) first loads that snapshot back.
    The reconciler's 30 day sweep calls `/purge` as a backstop.
  - `/claim` builds the `JobDocument`: `run_id` = `run_id` as text,
    `job` = the node's `PlannedJob`, `needs` = `graph.needs(job)`,
    `secrets`/`vars`/`timings` empty, `cache: None`, `github` from
    `RunInfo` with `server_url` `https://github.com`, `api_url` and
    `graphql_url` from `Settings.github_api`, `token` = the body's
    token (kept in `job:<id>`), with `github = github_struct(..)`. It
    moves the node to `Running` and calls `store::start_job`, which sets
    `started_at` and `heartbeat_at`. It does not mark the outbox: the
    check run shows `in_progress` from the first step end, which keeps
    GitHub calls to the per-step budget.
  - `/events` (step part): `StepStarted`/`StepFinished` update
    `steps`, and `StepFinished` marks the outbox; `Annotation` appends.
    Batches for a node that is not `Running` answer `409`. Batches with
    `seq <= last_seq` are ignored and answered with the current cancel
    count. `store::heartbeat` is written at most every
    30 s per job. The `Line` part is Task 10's.
  - `/result` → `graph.finished`; `/failed` → `graph.failed`; `/cancel`
    → `graph.cancel` and `escalate_at = now + 5 min`, whose alarm calls
    `escalate`.
  - `/rerun` → `graph.rerun`. For each reset job the object replaces
    `job:<id>` with a fresh `JobState` (so `last_seq` is 0,
    `check_run_id` is `None`, steps and annotations are empty, the old
    token is revoked first), deletes its `LogWriter` and `log:<id>:*`
    chunks, and calls `store::reset_job` with the new attempt. It calls
    `store::reopen_run` and stores the returned `run_attempt` in
    `info`. The new attempt's log overwrites the old R2 object at the
    same key; earlier attempts' logs are not kept. The outbox marks the
    reset jobs, so each gets a new check run.
  - The outbox flush: for each due job, render (Task 8) and send
    `create_check_run` when there is no id, else `update_check_run`,
    using the installation token from `Tokens`, then store the returned
    `file_annotations_sent`. A throttled answer calls
    `Outbox::throttled` with `github::throttle(response, now,
    outbox.attempt())` and stops the flush.

- [ ] **Step 1: Write the failing tests.** The object itself only runs
  in a Worker; its decisions are plain functions in `run.rs` tested
  natively:

```rust
#[test] fn github_context_for_a_push()      // github_context(&info, &event, api) has ref, sha, event_name, repository, event, no token
#[test] fn github_struct_carries_the_token() // github_struct(&info, api, "t").token == "t"
#[test] fn job_document_for_a_claim()       // job_document(&info, &graph, job, token, api) needs and token set, secrets empty
#[test] fn next_alarm_is_the_earliest_deadline() // outbox due at 50, escalate 40, upload 60 -> Some(40); all None -> None
#[test] fn fresh_state_after_rerun_takes_seq_one() // apply_steps on a reset JobState with seq 1 is applied
#[test] fn repeated_batch_does_not_finish_a_step_twice() // apply_steps(state, batch seq 3) twice -> one StepFinished applied
#[test] fn heartbeat_written_every_thirty_seconds()       // should_write_heartbeat(last, now)
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci run`
Expected: FAIL.

- [ ] **Step 3: Implement** `env.rs` and `run.rs` (`github_context`,
  `github_struct`, `next_alarm`, `job_document`, `apply_steps`,
  `should_write_heartbeat` as `pub(crate)`
  functions, the object on top).

- [ ] **Step 4: Run the tests and the wasm build**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): hold each run's job graph and check runs in a durable object"
```

---

### Task 10 [standard]: Run Durable Object: logs, WebSockets and R2

**Files:**
- Create: `crates/ci/src/logs.rs`
- Modify: `crates/ci/src/run.rs`

**Interfaces:**
- Consumes: `RunObject` (Task 9), `agent::AgentEvent` (Task 4).
- Produces, in `hatchrun_ci::logs`:

```rust
pub const MAX_LOG_BYTES: u64 = 32 << 20;
pub const CHUNK_BYTES: usize = 64 << 10;
pub const BACKLOG_BYTES: u64 = 2 << 20;
pub struct StepMark { pub index: usize, pub name: String, pub offset: u64 }
pub struct LogIndex { pub bytes: u64, pub steps: Vec<StepMark>, pub dropped_lines: u64 }
pub struct LogWriter { /* serde: bytes, steps, dropped_lines, open chunk text, chunk count */ }
impl LogWriter {
    pub fn step_started(&mut self, index: usize, name: &str);  // writes "== {name}\n", records offset
    pub fn line(&mut self, text: &str);                        // "{text}\n"; past MAX_LOG_BYTES counts a drop
    pub fn take_full_chunks(&mut self) -> Vec<(u32, String)>;  // chunks of >= CHUNK_BYTES, numbered
    pub fn finish(&mut self) -> (Option<(u32, String)>, LogIndex); // last chunk; a drop note line first
}
pub fn log_key(tenant: &str, repo_id: u64, run_id: u64, job_id: u64) -> String;
pub fn index_key(tenant: &str, repo_id: u64, run_id: u64, job_id: u64) -> String;
pub fn gzip(chunks: impl IntoIterator<Item = String>) -> Vec<u8>;
pub fn tail(text: &str, from: u64, lines: usize) -> Vec<String>;   // last lines from offset `from`
```

  The drop note is `[hatchrun] <n> lines were dropped: the log is over 32 MiB`.
  `log_key` = `logs/{tenant}/{repo_id}/{run_id}/{job_id}.log.gz`,
  `index_key` the same with `.index.json`.

  In the object: `Line` events go through the job's `LogWriter`; full
  chunks are stored as `log:<job>:<n:08>`, and the open chunk is kept in
  the writer. Every applied batch is sent to the job's WebSockets as
  `{"type":"events","events":[...]}`. `GET /live?job=ID` accepts a
  WebSocket with the hibernation API, tagged with the job id, and first
  sends `{"type":"backlog","text":...}` with up to `BACKLOG_BYTES` of
  the newest stored text. `GET /log?job=ID` answers the stored text of a
  running job. When a job finishes (`Finished`), the object writes the
  gzipped log to `LOGS` at `log_key` with
  `Content-Type: text/plain; charset=utf-8` and
  `Content-Encoding: gzip`, the `LogIndex` JSON at `index_key`, sends
  `{"type":"end","conclusion":...}` and closes the job's sockets,
  deletes the `log:<job>:*` keys, and passes `log_key` to
  `store::finish_job`. The failed tail for the check run (Task 8) comes
  from `tail` over the failed step. When the R2 write fails the chunks
  stay, `upload_pending` is set in `job:<id>`, and
  `Deadlines.upload_retry_at` is set to now + 30 s; the alarm (Task 9's
  `next_alarm`) retries every pending upload then.

- [ ] **Step 1: Write the failing tests** in `logs.rs`

```rust
#[test] fn index_points_at_step_headers()   // offsets of "== a" and "== b" in the joined text
#[test] fn chunks_split_at_the_chunk_size()
#[test] fn drops_past_the_cap_and_says_so()
#[test] fn gzip_round_trips()
#[test] fn keys_follow_the_layout()         // log_key("t1", 9, 7, 3) == "logs/t1/9/7/3.log.gz"
#[test] fn tail_takes_the_last_lines_of_a_step()
```

  And in `run.rs`: `repeated_seq_is_ignored` (applying the same batch
  twice writes its lines once).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci`
Expected: FAIL.

- [ ] **Step 3: Implement** `logs.rs` and the object's log routes.

- [ ] **Step 4: Run the tests and the wasm build**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): stream live logs and store finished logs in r2"
```

---

### Task 11 [standard]: Webhook handler and run creation

**Files:**
- Create: `crates/ci/src/webhook.rs`, `crates/ci/src/runs.rs`
- Modify: `crates/ci/src/lib.rs`, `crates/cloud/src/entry.rs`,
  `crates/cloud/wrangler.toml`

**Interfaces:**
- Consumes: `intake`, `planning`, `github`, `store`, `checks`,
  `env::{Settings, run_stub}`, `run::{RunInfo, Start}`,
  `hatchrun_cloud::webhook::verify`.
- Produces, in `hatchrun_ci::runs`:

```rust
pub struct Request<'a> {
    pub tenant_id: &'a str, pub installation_id: u64, pub repo: &'a Repo,
    pub delivery: &'a str, pub event_name: &'a str, pub event: &'a Value,
    pub trigger: &'a hatchrun_workflow::Event, pub compile_ref: &'a str,
    pub git_ref: &'a str, pub sha: &'a str, pub actor: &'a str,
    pub head_ref: &'a str, pub base_ref: &'a str,
}
pub async fn create(env: &Env, db: &Db, settings: &Settings, tokens: &Tokens<'_, Kv, App, Counted>,
    req: &Request<'_>, now: u64) -> worker::Result<Vec<u64>>;   // run ids created
```

  `create` first checks the tenant: missing or `ci_enabled = false`
  returns no runs and logs `hatchrun CI is not enabled for tenant <id>`.
  The webhook, the schedules (Task 15) and dispatch (Task 16) all start
  runs through `create`, so the check holds for each. It then reads the
  config at `sha` (no config: no runs), selects the
  plans, and for `Invalid` creates one completed check run named
  `hatchrun / config` with conclusion `failure`, the diagnostics as
  annotations on `.hatchrun/config.yml` (line and column from the
  `Diagnostic`) and in the summary. For each plan it calls
  `store::insert_run` (skips on conflict), `store::insert_jobs` with
  status `waiting`, and `POST /start` on the run's object.

- Produces, in `hatchrun_ci::webhook`:
  `pub async fn handle(req: worker::Request, env: &Env) -> worker::Result<Response>`
  on `POST /github/webhook`:
  1. `401` unless `webhook::verify` passes with `GITHUB_WEBHOOK_SECRET`;
  2. `200 {"duplicate": guid}` when `store::delivery_seen`;
  3. `intake::parse`; `Ignored(reason)` answers `200 {"ignored": reason}`;
  4. the tenant by installation; missing answers
     `200 {"ignored": "no tenant for this installation"}`, and
     `ci_enabled = false` answers
     `200 {"ignored": "hatchrun CI is not enabled for this organization"}`
     (an early answer; `create` checks again);
  5. `store::upsert_repo`;
  6. Push: `create` with `Event::Push { ref, changed }`, `compile_ref` =
     ref. When the ref is `refs/heads/<default_branch>`,
     `store::replace_schedules` with `planning::schedules` of the config
     at `after`. Pull request: changed paths from up to 3 pages of
     `pull_files` (`MAX_CHANGED` = 300), `Event::PullRequest`,
     `compile_ref` = `refs/heads/<base_ref>`, `sha` = head sha,
     `git_ref` = `refs/pull/<n>/merge`. Rerequested: `POST /rerun` on the
     job's run object (job and run looked up with `store::job` and
     `store::run`), only when the run's `repo_id` equals the event's
     repository id, else `Ignored("check run of another repository")`;
  7. `store::record_delivery`, then `202 {"runs": [ids]}`.
  Any error answers `500`, so GitHub records a failed delivery.

- Produces, in `crates/cloud/src/entry.rs`: in `github_webhook`, before
  the signature check, events `push`, `pull_request` and `check_run` are
  forwarded with `env.service("CI")?.fetch_request(...)` as a new
  `POST https://ci/github/webhook` with the same body and the
  `X-Hub-Signature-256`, `X-GitHub-Event` and `X-GitHub-Delivery`
  headers; the answer goes back unchanged. With no `CI` binding it
  answers `202 {"ignored": "no CI service"}`. `crates/cloud/wrangler.toml`
  gets `[[services]] binding = "CI"` / `service = "hatchrun-ci"`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/ci/src/runs.rs, pure helpers
#[test] fn config_failure_check_run_lists_diagnostics()
    // invalid_config_check_run(&[Diagnostic{line 3, column 5, ..}], sha) -> completed, failure, annotation at 3
#[test] fn pull_request_compiles_against_the_base_branch()  // compile_ref_for(&pr) == "refs/heads/main"
// crates/ci/src/webhook.rs, against testdb for the store part
#[test] fn disabled_tenant_is_ignored()                     // decide(tenant ci_enabled false) -> Ignored
// crates/cloud/src/entry.rs
#[test] fn forwards_orchestrator_events()                   // forwarded("push"), ("pull_request"), ("check_run") true; ("workflow_job") false
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud forwards`
Expected: FAIL.

- [ ] **Step 3: Implement** `runs.rs`, `webhook.rs`, the
  `#[event(fetch)]` route in `lib.rs`, and the forwarding in cloud.

- [ ] **Step 4: Run the tests and both wasm builds**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud && cargo check -p hatchrun-ci -p hatchrun-cloud --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud
git commit -S -s -m "feat(ci): start runs from push, pull request and re-request events"
```

---

### Task 12 [standard]: Launcher and launch queue

**Files:**
- Create: `crates/ci/src/launcher.rs`, `crates/ci/src/launches.rs`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `hatchrun_cloud::start::{Target, Instance, start_instance,
  terminate_instance, role_credentials, Failure, Cause}`,
  `hatchrun_cloud::{profiles::built_in, instance::{choose, static_offerings, Limits}, aws::{ec2, instances}}`,
  `token` (Task 4), `store` (Task 3), `env::Settings` (Task 9).
- Produces, in `hatchrun_ci::launcher`:

```rust
pub const TENANT: &str = "hatchrun-ci";        // OIDC subject tenant and hatchrun:tenant tag
pub const PROFILE: &str = "hatchrun-2cpu";
pub struct LaunchSpec { pub job_id: u64, pub name: String, pub user_data: String }
pub enum Kind { Ec2, Local }
pub fn kind(launcher: &str, deploy_env: &str) -> Result<Kind, String>;
pub enum Backend { Ec2(Ec2), #[cfg(any(test, hatchrun_local_launcher))] Local(Local) }
impl Backend {
    pub async fn new(settings: &Settings) -> Result<Backend, String>;
    pub async fn launch(&self, db: &Db, spec: &LaunchSpec) -> Result<String, Failure>;
    pub async fn metadata_off(&self, instance_id: &str) -> Result<(), String>;
    pub async fn metadata_applied(&self, instance_id: &str) -> Result<bool, String>;
    pub async fn terminate(&self, instance_id: &str);
    pub async fn instances(&self) -> Result<Vec<instances::Described>, String>;
}
pub fn user_data(broker_url: &str, ticket: &str) -> String;
    // {"mode":"ci","broker":"<url>","ticket":"<ticket>"}
```

  `kind`: `"ec2"` → `Ec2`; `"local"` → `Local` only when
  `deploy_env != "production"` and the build has
  `cfg(any(test, hatchrun_local_launcher))`, else
  `Err("the local launcher is refused in production")` or
  `Err("this build has no local launcher")`; anything else is an error.

  `Ec2` gets credentials with `role_credentials(oidc_key, oidc_kid,
  aws_endpoint, TENANT, ci_role_arn, ci_region, "hatchrun-ci-<job>")`,
  chooses types with `choose(&built_in(PROFILE), region,
  &static_offerings(region), &Limits::from_columns(None, None, None, None))`,
  reads the image from the `images` table for `(ci_region, 'amd64')`,
  and calls `start_instance` with `fleet: true`, `offerings_scope:
  TENANT`, subnets from `CI_SUBNET_IDS`, and tags
  `hatchrun:tenant=hatchrun-ci`, `hatchrun:job-id=<job>`,
  `hatchrun:runner-name=<name>`. `metadata_off` sends
  `ec2::metadata_off`; `metadata_applied` is true when
  `ec2::metadata_state` reports `applied` and `disabled`; `instances`
  pages `instances::describe(endpoint, TENANT, ..)`.

  `Local` talks to `LOCAL_LAUNCHER_URL`: `POST /instances`
  `{"name","job_id","user_data"}` → `{"instance_id"}`;
  `POST /instances/{id}/metadata-off` → `204`; `GET /instances/{id}`
  → `{"metadata": "applied"}`; `DELETE /instances/{id}` → `204`;
  `GET /instances` → `Described` list as JSON.

- Produces, in `hatchrun_ci::launches`:
  `pub struct LaunchMessage { pub job_id: u64 }` and
  `pub async fn consume(batch: MessageBatch<LaunchMessage>, env: Env)`.
  Per message: `store::claim_launch` (none: ack); ticket =
  base64url of 32 random bytes; job token with `token::sign` and
  `lifetime_secs(timeout_minutes)`; `Backend::launch` with name
  `ci-<job>-<launch_attempts>`; store
  `{"config": <serde_json::to_string(&BootDocument)>, "expiresIn": 900}`
  (the broker's `config` is a string and it refuses unknown fields) at
  `PUT {BROKER_URL}/v1/tickets/{ticket}` with `Authorization: Bearer
  BROKER_TOKEN` (terminate the instance when that fails);
  `store::launched`. A failure with `Cause::NoCapacity` or
  `Cause::Unreachable` and fewer than 3 launch attempts is
  `store::launch_failed(retry_at = now + 30 s)` and back to `queued`;
  any other failure, or the third, posts `/failed` to the run object
  with the failure's message. The dead-letter consumer posts `/failed`
  with `the launch did not finish`. The decision is

```rust
pub enum Next { Retry { at_ms: u64 }, Fail }
pub fn next_step(cause: &Cause, launch_attempts: u32, now_ms: u64) -> Next;
    // NoCapacity | Unreachable with launch_attempts < 3 -> Retry { now_ms + 30_000 }; else Fail
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn local_is_refused_in_production()   // kind("local", "production") is Err with "refused in production"
#[test] fn local_is_allowed_in_development()  // kind("local", "development") == Ok(Local) under cfg(test)
#[test] fn ec2_is_the_default()
#[test] fn user_data_selects_the_ci_path()    // JSON has mode "ci", broker, ticket
#[test] fn the_profile_is_two_vcpu_amd64_on_demand()
    // choose(&built_in(PROFILE).unwrap(), "us-east-1", ...) -> vcpus 2, memory_gib 8, arch Amd64
#[test] fn retries_capacity_twice_then_fails()
    // next_step(&Cause::NoCapacity, 1, 0) == Retry { at_ms: 30_000 }; (.., 2, ..) Retry; (.., 3, ..) Fail
#[test] fn broker_body_holds_the_document_as_text() // broker_body(&doc)["config"] is a string that parses as BootDocument
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci`
Expected: FAIL.

- [ ] **Step 3: Implement** both modules and the `#[event(queue)]`
  entry in `lib.rs`.

- [ ] **Step 4: Run the tests and the wasm builds**

Run:
```bash
cargo test -p hatchrun-ci
cargo check -p hatchrun-ci --target wasm32-unknown-unknown
RUSTFLAGS="--cfg hatchrun_local_launcher" cargo check -p hatchrun-ci --target wasm32-unknown-unknown
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): launch job instances from a queue in the ci account"
```

---

### Task 13 [standard]: Agent API in the Worker

**Files:**
- Create: `crates/ci/src/agent.rs`
- Modify: `crates/ci/src/lib.rs`

**Interfaces:**
- Consumes: `token::verify`, `launcher::Backend`, `github::scoped_token`,
  `hatchrun_cloud::start::app_jwt`, `env::run_stub`, `store::job`,
  `hatchrun_workflow::agent` paths.
- Produces: `pub async fn handle(req: worker::Request, env: &Env) -> worker::Result<Response>`
  for `CLAIM_PATH`, `EVENTS_PATH` and `RESULT_PATH`:
  - every route: `401` unless the bearer token verifies with
    `CI_TOKEN_KEY` at the current time;
  - claim: a row in `launching` (the instance can boot before
    `store::launched` runs, and always does with the local launcher)
    or in `queued` answers `503`; `launched` or `running` with an
    instance goes on; `waiting` or `completed` answers `409`. Call
    `metadata_off`, then poll
    `metadata_applied` every 500 ms for up to 10 s; still pending
    answers `503`. Then mint a token with `scoped_token` and pass it to
    the object's `/claim`, answering its document;
  - events: forward the body to `/events?job=`; result: to
    `/result?job=`. Bodies over 1 MiB answer `413`.
  - `pub(crate) fn claim_step(applied: bool, waited_ms: u64) -> ClaimStep`
    with `enum ClaimStep { Ready, Wait, Retry }` holds the poll decision
    (`Wait` below 10 000 ms, `Retry` at or past it).
  - `pub(crate) fn authorize(header: Option<&str>, key: &[u8], now_secs: u64) -> Result<Claims, TokenError>`
    reads `Bearer <token>` and calls `token::verify`.
  - `pub(crate) fn claim_answer(row: &JobRow, claims: &Claims) -> ClaimAnswer`
    with `enum ClaimAnswer { Go, Retry, Ended, Refused }` (`Refused`
    when `claims.run != row.run_id`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn claim_waits_until_metadata_is_applied()
    // claim_step(false, 0) == Wait; claim_step(false, 10_000) == Retry; claim_step(true, 0) == Ready
#[test] fn bad_token_is_unauthorized()   // authorize("Bearer x", key, now) is Err
#[test] fn job_of_another_run_is_refused() // claim_answer(row run 1, claims run 2) == Refused
#[test] fn launching_job_is_retried()      // claim_answer(status launching) == Retry; completed == Ended
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci agent`
Expected: FAIL.

- [ ] **Step 3: Implement** `agent.rs` and route `/v1/agent/` in
  `#[event(fetch)]`.

- [ ] **Step 4: Run the tests and the wasm build**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci
git commit -S -s -m "feat(ci): hand jobs to agents once instance metadata is off"
```

---

### Task 14 [standard]: `hatchrun-agent ci`

**Files:**
- Create: `crates/hatchrun-agent/src/ci/mod.rs`,
  `crates/hatchrun-agent/src/ci/client.rs`,
  `crates/hatchrun-agent/src/ci/sink.rs`
- Modify: `crates/hatchrun-agent/src/main.rs`,
  `crates/hatchrun-agent/src/source.rs`

**Interfaces:**
- Consumes: `hatchrun_workflow::agent` (Task 4), `exec::{run_job, Options, sink::Sink}`,
  `broker::{HttpBroker, https_url}`, `source::Broker`, `imds::Imds`.
- Produces:
  - `source.rs`: `UserData` gains `#[serde(default)] mode: String`;
    `pub(crate) fn ci_ticket(raw: &[u8]) -> Result<Option<(String, String)>>`
    returns `(broker, ticket)` when `mode == "ci"`. `run()` (no
    subcommand) checks it first and calls `ci::run` with defaults.
  - `ci::Client`: `new(url: &str, token: String) -> Result<Client>`
    (URL through `https_url`), `claim(&self) -> Result<Claim>` with
    `enum Claim { Job(Box<JobDocument>), Ended }` (retries `503` and
    connection errors every second until `--claim-timeout`), `events(&self, batch: &Batch) -> Result<BatchReply>`,
    `result(&self, result: &JobResult) -> Result<()>` (retries for up
    to 5 minutes).
  - `ci::sink::HttpSink` implements `Sink` by pushing `AgentEvent`s into
    a shared buffer; `HttpSink::start(client, cancel: watch::Sender<u32>) -> (HttpSink, Flusher)`
    and `Flusher::close(self) -> Result<()>` (final flush, retried up to
    2 minutes). The flusher posts every `FLUSH_INTERVAL_MS` at most
    `MAX_BATCH_BYTES` of events with `seq` starting at 1, keeps a batch
    and its `seq` until it is acknowledged, posts an empty batch when
    nothing was sent for `HEARTBEAT_INTERVAL_MS`, and raises the cancel
    watch to `reply.cancel` when that is higher. Pending events are
    capped at 8 MiB; past it `Line`s are dropped and counted, and the
    next batch starts with `Line { text: "[hatchrun] <n> lines were dropped while the control plane was unreachable" }`.
    Step and annotation events are never dropped.
  - `pub struct CiOptions { pub exec: exec::Options, pub claim_timeout: Duration }`
    built from the `Ci` flags. The flusher stops sending on `409` (the
    job ended or was reset) and on `401`, and the job is then cancelled
    through the watch (count 2).
  - `ci::run(user_data: Vec<u8>, opts: CiOptions) -> Result<i32>`:
    ticket → `HttpBroker::claim` loop → `BootDocument` → `Client::claim`
    → `run_job` with the `HttpSink` → `Flusher::close` →
    `Client::result`. Exit code 0 on success, 1 otherwise; `Ended`
    exits 0. The unit powers the instance off when the agent exits, as
    for hatchrun.io runners.
  - `main.rs`: subcommand `Ci { --user-data FILE (optional; IMDS when
    absent), --work-dir, --runner-user, --env-file, --node, --state-dir,
    --claim-timeout (default 10m) }`, with the same defaults as `Exec`.

- [ ] **Step 1: Write the failing tests** with `wiremock` and
  `tokio::time::pause` in `ci/sink.rs` and `ci/client.rs`

```rust
#[tokio::test] async fn batches_lines_once_a_second()     // 3 lines -> one POST with seq 1 after 1 s
#[tokio::test] async fn retries_with_the_same_seq()      // first POST 500, second has seq 1 and the same events
#[tokio::test] async fn cancel_from_reply_reaches_the_watch() // reply {"cancel":1} -> watch == 1
#[tokio::test] async fn heartbeat_when_idle()            // no events for 10 s -> empty batch posted
#[tokio::test] async fn unauthorized_stops_the_job()     // events answered 401 -> watch == 2, no more POSTs
#[tokio::test] async fn drops_lines_past_the_cap_and_says_so()
#[tokio::test] async fn claim_retries_until_ready()      // 503, 503, 200 -> Job
#[tokio::test] async fn ended_job_is_not_run()           // 409 -> Claim::Ended
#[test] fn ci_mode_user_data_names_the_ticket()          // {"mode":"ci","broker":"https://b","ticket":"t"}
#[test] fn runner_user_data_is_not_ci()                  // {"broker":..,"ticket":..} -> None
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-agent ci`
Expected: FAIL.

- [ ] **Step 3: Implement** the module and the subcommand.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-agent`
Expected: PASS, with every existing test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/hatchrun-agent
git commit -S -s -m "feat(agent): run control plane jobs with hatchrun-agent ci"
```

---

### Task 15 [standard]: Schedules and the reconciler

**Files:**
- Create: `crates/workflow/src/cron.rs`, `crates/ci/src/reconcile.rs`
- Modify: `crates/workflow/src/lib.rs`, `crates/workflow/src/plan.rs`
  (`validate` reports bad crons), `crates/ci/src/lib.rs`
- Test: `crates/workflow/tests/cron.rs`

**Interfaces:**
- Produces, in `hatchrun_workflow::cron` (re-exported at the root):

```rust
pub struct UtcMinute { pub year: i32, pub month: u32, pub day: u32,
    pub hour: u32, pub minute: u32, pub weekday: u32 /* 0 = Sunday */ }
impl UtcMinute { pub fn from_unix(secs: u64) -> UtcMinute; }
pub struct Cron { /* one bit set per field */ }
impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String>;
    pub fn matches(&self, t: &UtcMinute) -> bool;
}
```

  Five fields: minute 0-59, hour 0-23, day of month 1-31, month 1-12 or
  `JAN`-`DEC`, day of week 0-7 or `SUN`-`SAT` (7 is Sunday). Each field
  is `*` or a comma list of `n`, `a-b`, `*/s` or `a-b/s`. When both day
  fields are restricted a time matches if either matches. `from_unix`
  uses the civil-from-days algorithm (Howard Hinnant).
  `validate` reports `on.schedule` entries that do not parse as
  `invalid cron "<expr>": <reason>`.

- Produces, in `hatchrun_ci::reconcile`:
  `pub async fn run(env: Env)` from `#[event(scheduled)]`, under a lease
  on `ci_reconciler` (`lease_until = now + 55 s`). Each pass, in order:
  1. **Schedules.** For each minute after `last_minute` up to the
     current one (at most 5), every `ci_schedules` row whose `Cron`
     matches starts a run through `runs::create` with event name
     `schedule`, `Event::Schedule { cron }`, event payload
     `{"schedule": cron, "repository": {"full_name": ..}}`, ref and sha of the default
     branch head (`commit_sha`), delivery
     `schedule:<repo>:<workflow>:<cron>:<minute ms>`; at most 200 per
     pass. Then `last_minute` advances.
  2. **Stalled launches.** `queued` with `next_launch_at <= now` or
     `updated_at < now - 60 s`: send to `CI_LAUNCHES` again.
     `launching` with `updated_at < now - 600 s`: `/failed` with
     `the launch did not finish`. `launched` with
     `updated_at < now - 600 s`: terminate and `/failed` with
     `the runner did not start`.
  3. **Lost runners.** `running` with
     `COALESCE(heartbeat_at, started_at) < now - 120 s` (`start_job`
     sets both at claim):
     terminate and `/failed` with `lost contact with the runner`.
  4. **Orphans.** `Backend::instances()`: terminate each instance whose
     job is unknown, completed more than 180 s ago, or whose launch time
     is more than 7 hours ago. `Described.launch_time` is read with
     `hatchrun_cloud::parse_rfc3339` (Task 1); one that does not parse
     counts as old.
  5. **Retention.** `store::sweep`, then `POST /purge` on the object of
     each run id it returns.
  Each step's decisions are a pure function over rows and `now`
  (`stalled(&[JobRow], now) -> Vec<Fix>`, `orphans(&[Described], &[JobRow], now) -> Vec<String>`)
  with `enum Fix { Requeue(u64), Fail(u64, &'static str), TerminateAndFail(u64, String, &'static str) }`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/workflow/tests/cron.rs
#[test] fn from_unix_known_dates() {
    assert_eq!(UtcMinute::from_unix(1_792_676_700), UtcMinute { year: 2026, month: 10, day: 22, hour: 13, minute: 45, weekday: 4 });
    assert_eq!(UtcMinute::from_unix(1_835_481_540), UtcMinute { year: 2028, month: 2, day: 29, hour: 23, minute: 59, weekday: 2 });
}
#[test] fn every_fifteen_minutes()    // "*/15 * * * *" matches :45, not :44
#[test] fn day_fields_or_when_both_set() // "0 0 1 * MON" matches the 1st and every Monday
#[test] fn names_and_sunday_seven()   // "0 12 * JAN-MAR 7" == "0 12 * 1-3 0"
#[test] fn rejects_bad_fields()       // "60 * * * *", "* * *", "*/0 * * * *" are Err
#[test] fn validate_reports_bad_crons()
// crates/ci/src/reconcile.rs
#[test] fn requeues_a_quiet_queued_job()
#[test] fn fails_a_silent_runner()
#[test] fn terminates_unknown_and_finished_instances_only()
#[test] fn fires_each_missed_minute_up_to_five()
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow --test cron && cargo test -p hatchrun-ci reconcile`
Expected: FAIL.

- [ ] **Step 3: Implement** `cron.rs` and `reconcile.rs`.

- [ ] **Step 4: Run the tests and the wasm builds**

Run: `cargo test -p hatchrun-workflow && cargo test -p hatchrun-ci && cargo check -p hatchrun-ci -p hatchrun-workflow --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow crates/ci
git commit -S -s -m "feat(ci): fire schedules and reconcile launches, runners and instances"
```

---

### Task 16 [standard]: Read API, dispatch, cancel and log access

**Files:**
- Create: `crates/ci/src/api.rs`, `crates/ci/src/access.rs`
- Modify: `crates/ci/src/lib.rs`, `crates/cloud/src/entry.rs`

**Interfaces:**
- Consumes: `hatchrun_cloud::Session`, `github::{user_repo, permissions, commit_sha}`,
  `store`, `runs::create`, `env::run_stub`, `logs::{log_key, index_key}`.
- Produces, in `hatchrun_ci::access`:
  `pub enum Access { None, Read, Write }`,
  `pub async fn check(req: &worker::Request, db: &Db, settings: &Settings, repo: &RepoRow) -> Result<Access>`
  (a public repository is at least `Read` with no session; otherwise
  the session user's token asks `user_repo`; `write` needs `push`),
  and `pub fn from_permissions(private: bool, p: Option<Permissions>) -> Access`.
- Produces, in `hatchrun_ci::api`:
  `pub async fn handle(req: worker::Request, env: &Env) -> worker::Result<Response>`
  under `/api/ci/`, every answer `Cache-Control: no-store`:

| Route | Needs | Answer |
| --- | --- | --- |
| `GET /api/ci/repos/{owner}/{repo}/runs?before=ID` | Read | 50 runs, newest first |
| `GET /api/ci/runs/{run}` | Read | run row, jobs with status, conclusion, steps; running jobs from the object's `/state` |
| `GET /api/ci/jobs/{job}/log` | Read | finished: the R2 object streamed with its headers; running: the object's `/log` |
| `GET /api/ci/jobs/{job}/log/index` | Read | the index JSON; `404` while running |
| `GET /api/ci/jobs/{job}/live` | Read | WebSocket to the object's `/live` |
| `POST /api/ci/repos/{owner}/{repo}/dispatch` | Write + CSRF | `{workflow, ref, inputs}` → `202 {"runs": [...]}` |
| `POST /api/ci/runs/{run}/cancel` | Write + CSRF | `204` |

  No access answers `404`, so private repositories are not revealed.
  Dispatch resolves the ref with `commit_sha`, and calls `runs::create`
  with event name `workflow_dispatch`, `Event::Dispatch`, the event
  `{"ref","inputs","workflow","repository":{"full_name"},"sender":{"login"}}`,
  delivery `dispatch:<random hex>`, and actor the session's login. The
  workflow must exist in the config and have `workflow_dispatch`, else
  `422`. `crates/cloud/src/entry.rs` forwards `/api/ci/*` on
  `app.hatchrun.io` to the `CI` binding with the request unchanged
  (cookies and WebSocket upgrade included), before `app::handle`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn public_repo_is_readable_signed_out()  // from_permissions(false, None) == Read
#[test] fn private_repo_needs_pull()             // (true, None) == None; (true, Some{read}) == Read
#[test] fn push_permission_writes()
#[test] fn routes_parse()                        // route("/api/ci/jobs/7/log") == Route::Log(7) ...
#[test] fn dispatch_needs_workflow_dispatch()    // dispatchable(config, "ci") is Err when on: has no workflow_dispatch
// crates/cloud/src/entry.rs
#[test] fn forwards_ci_api_paths()               // forwards_api("/api/ci/runs/1") true, ("/api/session") false
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud forwards_ci`
Expected: FAIL.

- [ ] **Step 3: Implement** both modules, the routes in `lib.rs` and
  the forwarding in cloud.

- [ ] **Step 4: Run the tests and the wasm builds**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud && cargo check -p hatchrun-ci -p hatchrun-cloud --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci crates/cloud
git commit -S -s -m "feat(ci): serve runs and logs to readers and dispatch runs from the app"
```

---

### Task 17 [standard]: App pages for runs and logs

**Files:**
- Create: `web/app/src/pages/ci/RunPage.tsx`, `web/app/src/pages/ci/JobLog.tsx`,
  `web/app/src/pages/ci/DispatchDialog.tsx`, `web/app/mock/ci.ts`
- Modify: `web/app/src/router.tsx`, `web/app/src/api.ts`, the mock
  server setup in `web/app/mock`

**Interfaces:**
- Consumes: the Task 16 routes.
- Produces: routes `/ci/:owner/:repo` (runs list with a "Run workflow"
  button opening `DispatchDialog`, which posts with the CSRF token from
  `/api/session`), `/ci/:owner/:repo/runs/:run` (jobs with status and
  time) and `/ci/:owner/:repo/runs/:run/jobs/:job` (the URL check runs
  link to). `JobLog` fetches the finished log and its index and shows
  one collapsible section per step; for a running job it opens
  `/api/ci/jobs/:job/live`, renders the backlog, appends `events`
  messages, and on `end` loads the finished log. Lines render as text,
  never HTML. Signed-out viewers of a private repository see the
  sign-in prompt the app already has.

- [ ] **Step 1: Add mock data** for a finished and a running job in
  `web/app/mock/ci.ts` and a mock WebSocket that replays three events
  and `end`.

- [ ] **Step 2: Implement** the pages and the API functions.

- [ ] **Step 3: Check the app**

Run: `cd web/app && npm run typecheck && npm run lint && npm run build`
Expected: no errors. Then `npm run dev:mock`, open
`/ci/e2e/demo/runs/1/jobs/1` and `/ci/e2e/demo/runs/1/jobs/2`: the
first shows steps with their lines, the second streams lines and
switches to the finished log after `end`.

- [ ] **Step 4: Commit**

```bash
git add web/app
git commit -S -s -m "feat(app): show orchestrator runs and live job logs"
```

---

### Task 18 [standard]: CI account stack, deploy and retention

**Files:**
- Create: `deploy/cloudformation/ci-account.yaml`,
  `.github/workflows/deploy-ci.yml`, `crates/ci/r2-lifecycle.json`
- Modify: `.github/workflows/deploy-cloud.yml` (deploy after
  `hatchrun-ci` exists, comment only), `crates/ci/wrangler.toml`
  (comments naming where each id comes from)

**Interfaces:**
- Produces: a stack for the hatchrun-owned CI account in `us-east-1`
  with outputs `RoleArn`, `LaunchTemplateId`, `SubnetIds`:
  - an IAM OIDC provider for `https://oidc.hatchrun.io` (audience
    `sts.amazonaws.com`) and a role trusted only for
    `oidc.hatchrun.io:sub` = `tenant:hatchrun-ci`, allowed
    `ec2:RunInstances`, `ec2:CreateFleet`, `ec2:CreateTags`,
    `ec2:CreateLaunchTemplateVersion`, `ec2:DeleteLaunchTemplateVersions`,
    `ec2:DescribeInstances`, `ec2:DescribeLaunchTemplateVersions`,
    `ec2:DescribeSubnets`, and `ec2:TerminateInstances` plus
    `ec2:ModifyInstanceMetadataOptions` only on instances tagged
    `hatchrun:tenant=hatchrun-ci`; no `iam:PassRole`;
  - a VPC `10.40.0.0/16` with public subnets in three zones that map
    public IPv4 addresses, an internet gateway, and a security group
    with no ingress and egress for all TCP except port 25 (ranges 0-24
    and 26-65535), all UDP and ICMP;
  - a launch template with no instance profile, `HttpTokens: required`,
    `HttpPutResponseHopLimit: 1`, `InstanceInitiatedShutdownBehavior:
    terminate`, the security group, and a gp3 root volume of 30 GiB.
- Produces: `deploy-ci.yml`, modelled on `deploy-broker.yml`
  (`environment: production`, pinned actions, `WRANGLER:
  wrangler@4.145.0`), which runs `crates/cloud/scripts/migrate.sh`,
  applies `r2-lifecycle.json` with
  `wrangler r2 bucket lifecycle set hatchrun-ci-logs --file r2-lifecycle.json`,
  sets the `CI_*` vars from repository variables, and deploys
  `crates/ci`. `r2-lifecycle.json` expires objects under `logs/` and
  `state/` after 30 days.

- [ ] **Step 1: Write the stack** and lint it.

Run: `cfn-lint deploy/cloudformation/ci-account.yaml`
Expected: no errors.

- [ ] **Step 2: Write the workflow** and lint it.

Run: `actionlint .github/workflows/deploy-ci.yml`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add deploy/cloudformation/ci-account.yaml .github/workflows crates/ci
git commit -S -s -m "build(ci): add the ci account stack and the hatchrun-ci deploy"
```

Launch tasks outside the repo, listed in the PR body: create the AWS
account, subscribe it to the amd64 Marketplace image, raise its
on-demand vCPU quota, create the R2 bucket, the queues and the
`ci.hatchrun.io` domain, and deploy `hatchrun-ci` once by hand before
the workflow's first run.

---

### Task 19 [standard]: End-to-end harness

**Files:**
- Create: `crates/ci-dev/Cargo.toml`, `crates/ci-dev/src/main.rs`,
  `crates/ci-dev/src/github.rs`, `crates/ci-dev/src/launcher.rs`,
  `crates/ci/scripts/e2e.sh`, `crates/ci/tests/e2e/repo/.hatchrun/config.yml`,
  `crates/ci/tests/e2e/push.json`
- Modify: `.github/workflows/ci.yml` (an `e2e` job on `ubuntu-24.04`
  with the Postgres service, running the script)

**Interfaces:**
- Consumes: the `Local` protocol (Task 12), `hatchrun-agent ci
  --user-data` (Task 14), the Task 11, 13 and 16 routes.
- Produces: binary `hatchrun-ci-dev` (package `hatchrun-ci-dev`,
  native only, `axum` 0.8 + `tokio`) with two subcommands:
  - `fake-github --port 9101 --repo-dir DIR`:
    `POST /app/installations/{id}/access_tokens` → `{"token":"ghs_e2e_<n>","expires_at":<now + 1 h>}`;
    `GET /repos/{o}/{r}/contents/.hatchrun/config.yml` → the file from
    `DIR` as raw text; `GET /repos/{o}/{r}/pulls/{n}/files` → `[]`;
    `GET /repos/{o}/{r}/commits/{ref}` → `{"sha":"<40 x e>"}`;
    `GET /repos/{o}/{r}` → `{"private":false,"permissions":{"pull":true,"push":true}}`;
    `POST /repos/{o}/{r}/check-runs` → `{"id":<n>}`;
    `PATCH /repos/{o}/{r}/check-runs/{id}` → `200`;
    `DELETE /installation/token` → `204`;
    `GET /_state` → every check run's latest `name`, `status`,
    `conclusion` and annotation count.
  - `launcher --port 9102 --agent PATH --work-root DIR`: the `Local`
    protocol. `POST /instances` writes the user data to
    `DIR/<id>/user-data.json` and starts
    `PATH ci --user-data <file> --work-dir DIR/<id>/work --runner-user "" --state-dir DIR/<id>/state --env-file /dev/null`;
    `DELETE` kills the process group; `GET /_events` lists
    `launch <id>`, `metadata-off <id>`, `metadata-read <id>` (each
    `GET /instances/{id}` that answered `applied`) and
    `exit <id> <code>` in the order they happened.
- Produces: `crates/ci/scripts/e2e.sh`, run from the repository root
  with no arguments on Linux:
  1. Postgres: `E2E_DATABASE_URL`, else start `postgres:17` in Docker
     on port 55432; apply `crates/cloud/scripts/migrate.sh`; insert a
     tenant (`e2etenant0000001`, org 1, login `e2e`, installation 42,
     `ci_enabled` true).
  2. `cargo build -p hatchrun-agent -p hatchrun-ci-dev`; generate an RSA
     key with `openssl genpkey` for `GITHUB_APP_PRIVATE_KEY` and
     `OIDC_SIGNING_KEY`.
  3. Start `fake-github`, `launcher`, `wrangler dev` for
     `crates/broker` on 8789, and
     `RUSTFLAGS="--cfg hatchrun_local_launcher" wrangler dev` for
     `crates/ci` on 8788 with
     `CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_HYPERDRIVE` set and
     `--var` for `DEPLOY_ENV:development`, `LAUNCHER:local`,
     `LOCAL_LAUNCHER_URL`, `GITHUB_API_URL`, `BROKER_URL`, `CI_URL`,
     `APP_URL`, `GITHUB_APP_ID`, and the secrets. A `trap` stops every
     process and the container on exit.
  4. Post `push.json` (installation 42, repository id 1001
     `e2e/demo`, public, `refs/heads/main`) to
     `http://127.0.0.1:8788/github/webhook`, signed with
     `openssl dgst -sha256 -hmac`, and read the run id from the `202`.
  5. Poll `fake-github /_state` for up to 180 s until both check runs
     are `completed`.
  6. Assert: both conclusions are `success`; `ci / build` has 1
     annotation; `GET /api/ci/runs/<run>` lists both jobs `completed`;
     `GET /api/ci/jobs/<build job>/log` contains `hello from e2e`;
     its `/log/index` names the step `say hello`; the launcher's events
     show, for every launched instance, `metadata-off <id>` before
     `metadata-read <id>` (a `GET /instances/{id}` answered `applied`)
     and both before `exit <id> 0`. The claim needs an `applied` answer
     before it returns the document, so this order shows the job ran
     with metadata off; each
     agent exited 0. Print `e2e passed` and exit 0; on failure print
     the `wrangler dev` and launcher logs and exit 1.

  The fixture config:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - name: say hello
        run: |
          echo "hello from e2e"
          echo "::error file=README.md,line=1::e2e annotation"
  test:
    runs-on: ubuntu-latest
    steps:
      - run: echo "build said ${{ needs.build.result }}"
workflows:
  ci:
    on: { push: { branches: [main] } }
    jobs:
      - build
      - test: { requires: [build] }
```

- [ ] **Step 1: Write `hatchrun-ci-dev`** with a unit test per
  subcommand: `fake_github_serves_the_config`,
  `launcher_records_metadata_off`.

Run: `cargo test -p hatchrun-ci-dev`
Expected: PASS.

- [ ] **Step 2: Write the script and fixtures**, then run it.

Run: `crates/ci/scripts/e2e.sh`
Expected: last line `e2e passed`, exit code 0, in under 5 minutes on a
warm cargo cache.

- [ ] **Step 3: Break it on purpose** to see the script catch it: set
  `LAUNCHER:ec2` in the script and run again.

Expected: exit code 1 with the launch failure in the printed log. Undo
the change.

- [ ] **Step 4: Add the CI job** and commit.

```bash
git add Cargo.toml Cargo.lock crates/ci-dev crates/ci/scripts crates/ci/tests .github/workflows/ci.yml
git commit -S -s -m "test(ci): drive a push through to check runs and an r2 log end to end"
```

---

## Self-review

- **Spec coverage.** Flow 1 to 5: Tasks 5, 11, 15 (events, config at the
  commit, schedules, dispatch in 16), 7 and 9 (graph, `if:`, matrix and
  parallelism through `compile`), 8 and 9 (check runs per job, per step
  end, backoff), 12 (launch queue, EC2 on-demand, shared launcher), 13
  and 14 (ticket, metadata off, job token, document, events,
  heartbeats, cancel), 10 and 16 (live logs over WebSocket, R2, step
  index, read access), 15 (reconciler), 3 and 18 (30 day retention).
  "Docker beyond run:" is already refused by the format crate. Fork
  pull requests: Task 5. One App and the shared token budget: Rulings 2
  and Task 6's `throttle`. Port 25 and no IAM role: Task 18. Secrets,
  templates, warm pool and billing are Plan 4 by scope.
- **Types.** `RunInfo`, `Start`, `JobState` (Task 9), `LaunchMessage`
  (Task 12), `Backend` (Tasks 12, 13, 15), `checks::parse_external_id`
  (Tasks 5, 8), `logs::log_key` (Tasks 10, 16) and the protocol constants
  (Tasks 4, 13, 14) are named the same in every task that uses them.
- **Proportion.** Code blocks are signatures, the schema the plan fixes,
  config files and test names; bodies are left to the implementer.
