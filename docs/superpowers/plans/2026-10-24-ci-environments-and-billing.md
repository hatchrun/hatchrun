# Orchestrator: Environments, Repository Access and One Subscription

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** hatchrun CI bills on the hatchrun.io Polar subscription
instead of a product of its own, organization secrets and vars can be
limited to selected repositories, and jobs can name a repository
environment that restricts branches, asks an organization admin to
approve each job attempt, and carries its own secrets and vars.

**Architecture:** The CI card check reads any active or trialing row of
`subscriptions`, which after this plan holds only hatchrun.io product
subscriptions (the `product` column goes), and the app's CI page opens
hatchrun-cloud's existing checkout and portal. `ci_secrets` and
`ci_vars` gain a nullable `environment_id` and, for organization rows,
a nullable `repositories bigint[]`; resolution overlays organization,
repository and environment rows in that order. A job's `environment:`
is parsed by `hatchrun-workflow`, stored on the job row, and checked by
the launch consumer before billing: a missing environment or a ref the
branch patterns refuse fails the job, and an approval environment
holds it with the new hold reason `approval` until an admin approves or
rejects it through the API.

**Tech Stack:** Rust 1.99 (edition 2024), `worker` 0.8.7,
`tokio-postgres` through `hatchrun_cloud::db`, Polar
(`hatchrun_cloud::polar`), `hatchrun_workflow::Filter` for branch
globs, React 19 app in `web/app`, the e2e harness in
`crates/ci/scripts/e2e.sh`.

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`
(section "Secrets, the GitHub App and billing"), extended by the
decisions below. Where the spec and the decisions differ, the decisions
win; Task 2 and Task 11 write them into the spec.

## Decisions (the spec for this plan)

Made by the owner on 2026-10-24.

1. **One Polar subscription.** CI usage is billed on the existing
   hatchrun.io product (`POLAR_PRODUCT_ID`): the operator adds a metered
   price on the meter that sums `units` of `ci.usage` events to that
   product. `POLAR_CI_PRODUCT_ID` goes everywhere (both `wrangler.toml`,
   `env.rs` settings, `polar::product_kind`'s `ci`, tests), and so do
   hatchrun-ci's checkout and portal endpoints and their web client
   functions. The CI page's "Add a card" and "Manage billing" call
   hatchrun-cloud's checkout and portal, the functions `Billing.tsx`
   uses. The CI card is an `active` or `trialing` subscription to the
   hatchrun.io product. The Polar webhook calls `ci_wake::card_returned`
   when that subscription becomes licensed. CI page wording: "A card is
   on file." and "Add a card to run CI jobs. Jobs wait until you do."
   No operator wording reaches customers; when Polar is unconfigured the
   button shows the server's message, as today (an operator-only path).
   Usage stays per minute (`RECORD_USAGE`: whole minutes per attempt,
   rounded up, at least one).
2. **Organization secret and var repository access, as on GitHub.** An
   organization row has `repositories`: all (the default) or a selected
   list of the tenant's repositories, stored as a nullable `bigint[]`
   of `ci_repos` ids (NULL means all; a CHECK keeps it NULL on
   repository and environment rows). Resolution leaves out organization
   rows whose list lacks the job's repository. The org `PUT` body gains
   optional `repositories: ["owner/name", ...] | null`; unknown
   repositories are a `422` naming them; list answers carry
   `repositories` (names, or null). The org-scope `SecretsTable` gets an
   "All repositories" / "Selected repositories" control with a
   multi-select of the organization's repositories.
3. **Environments, as in GitHub Actions.** Per repository, an
   environment has a name (at most 255 characters, unique per
   repository ignoring case), `branches` (glob patterns matched against
   the run's branch name; empty allows any ref) and `approval` (when
   true an organization admin approves each job attempt that uses it).
   Environment secrets and vars live on `ci_secrets` and `ci_vars` with
   a new nullable `environment_id`; the sealed value's associated data
   includes the environment; precedence is environment, then
   repository, then organization. A job's `environment:` is a string or
   a map with `name` (`url` is dropped with a warning). A job naming an
   environment that does not exist fails with a clear check run message;
   a job whose run ref is not a branch matching the patterns fails with a
   message naming the environment and the ref (pull request runs never
   match a non-empty pattern list, tags never match). Approval holds the
   job with hold reason `approval` (it does not count against
   concurrency and blocks no other job); the run page shows Approve and
   Reject to admins; approve releases it to the normal launch path
   (billing holds still apply after), reject fails it with
   `Rejected by <login>`. The job row records who decided. Routes: CRUD
   for `/repos/{owner}/{repo}/environments[/{name}]`, environment secrets
   and vars under `/repos/{owner}/{repo}/environments/{name}/(secrets|vars)[/{name}]`,
   and `POST` approve and reject on a job. UI: an environments section
   on the repository settings page (list, create, edit branches and
   approval, delete with its secrets), `SecretsTable` reused for the
   environment scope, and approval buttons on the run view.
4. **Out of scope:** wait timers, required reviewers other than
   organization admins, deployment history, environment URLs, fork pull
   request handling.

## Rulings

Made while writing this plan; the plan keeps them.

1. **The `product` column is dropped.** After decision 1 every row the
   webhook writes is a hatchrun.io product subscription (a product not in
   `POLAR_PRODUCT_ID` is still not written), so nothing needs the column.
   The migration deletes any `product = 'ci'` rows first, so a stray CI
   subscription cannot license runners.
2. **`CiBilling` keeps its three states over the one subscription.** A
   tenant with no subscription row is `never` (ignored silently, no
   config read), one with only ended subscriptions is `lapsed`, one with
   an active or trialing subscription is `active`. Paying hatchrun.io
   runner tenants therefore become `active` for CI: each push costs one
   config read, and a repository without `.hatchrun/config.yml` gets
   nothing more. The beta does not count as a card.
3. **A beta tenant may add a card.** hatchrun-cloud's checkout refused
   any licensed tenant, beta included, with `409`; the CI page would then
   be stuck. The checkout now refuses only a tenant with an active or
   trialing subscription. Beta runner minutes stay unbilled (`billed` is
   `NOT beta AND ...`), so a beta tenant's card pays for CI only.
4. **Checkout and portal return to the CI page when asked.**
   `POST /api/tenants/{org}/checkout?return=ci` and `.../portal?return=ci`
   send Polar's return URL to `/o/{org}/ci`; without the query they
   return where they do today.
5. **Environment names are 1 to 255 characters of ASCII letters,
   digits, `-`, `_` and `.`.** GitHub allows more (spaces, other
   punctuation); this set keeps the name a plain URL path segment with
   no decoding. The workflow crate owns the rule
   (`hatchrun_workflow::valid_environment_name`), so the config check
   and the API agree.
6. **Expressions in an environment name are an error.** In
   `.hatchrun/config.yml`, `${{ parameters.x }}` is filled in at compile
   time like any job field (so a matrix over parameters can target
   several environments); any `${{` left after that is a compile error.
   In `convert`, a GitHub `environment:` with `${{` is an error that
   drops the job. Treating the text literally would run the job without
   the environment the author meant, skipping its approval.
7. **The environment is checked at launch, not at plan time.** The
   launch consumer reads the environment for every launch of a job that
   names one, so a change to branches or approval applies to queued
   jobs, and a re-run checks again. The check comes before billing. A
   failing check ends the job through the run object's `/failed`, with
   no claim, so no instance starts and nothing is metered.
8. **Approval is per attempt.** `ci_jobs.approval_attempt` records
   which attempt the decision is for; a re-run (a new attempt) needs a
   new approval. Approving sets `held = NULL` and sends a launch message;
   rejecting sets `held = NULL` and ends the job. Both set
   `next_launch_at` to now, so if the message or the end does not go,
   the reconciler's requeue of due queued jobs brings the job back to
   the consumer, which reads the recorded decision.
9. **Approval holds are woken only by a decision or an environment
   change.** The slow backstop (`ci_wake::STALE_HELD`) skips them.
   Deleting an environment, or turning its approval off, releases the
   jobs held for it (they go back to the consumer, which then fails them
   or launches them).
10. **Who may approve:** an active admin of the repository owner's
    organization, checked with `access::org_admin` as the org settings
    routes do. The run API does not say whether the viewer is one (that
    is a GitHub call per page view); the run page shows the buttons to
    any signed-in viewer and shows the `403` message to others.
11. **Org `PUT` without `repositories` means all repositories.** The
    `PUT` replaces the row, value and access together. Repository and
    environment `PUT` bodies with a `repositories` field get `400`. An
    empty selected list is allowed and reaches no repository. At most
    1,000 repositories per row.
12. **`GET /api/ci/orgs/{org}/repositories`** lists the tenant's
    repositories CI has seen (`ci_repos`, renamed-away rows left out),
    for the multi-select. A repository CI has never seen cannot be
    selected yet.
13. **Limits:** 100 environments per repository, 50 branch patterns per
    environment, 255 bytes per pattern, 100 secrets and 100 vars per
    environment (`secrets::MAX_PER_REPO`).
14. **A repository transfer leaves its environments behind**, as it
    already leaves its secrets and vars (`upsert_repo`).

## Global Constraints

<!-- REUSE-IgnoreStart -->
- Every new source file starts with the SPDX header used across the
  repository:
  `SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>`
  and `SPDX-License-Identifier: AGPL-3.0-only`.
<!-- REUSE-IgnoreEnd -->
- Rust 2024 workspace. `hatchrun-ci`, `hatchrun-cloud` and
  `hatchrun-workflow` build for `wasm32-unknown-unknown`.
- No backward compatibility code paths: new columns are required where
  it makes sense, everything deploys together, no data migration shims.
  There are no production CI rows yet.
- Schema changes go in one new file,
  `crates/cloud/migrations/0006_ci_environments.sql`. Task 1 creates it;
  Tasks 4 and 5 append to it. Add-only is not required.
- Comments, docs, UI copy and messages are plain English. No em-dash or
  en-dash characters anywhere (code, comments, docs, commit messages).
- Secret values never appear in logs, errors, check runs, `Debug`
  output, R2 objects or Durable Object storage. Errors name a secret,
  never its value.
- Postgres tests need `DATABASE_URL` pointing at Postgres 17. Locally:
  `docker run -d --rm --name hatchrun-test-pg -e POSTGRES_PASSWORD=postgres -p 127.0.0.1:5432:5432 postgres:17`
  and `export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres`.
  Every `cargo test` command below assumes it is set.
- Commits are Conventional Commits, GPG-signed and DCO signed off
  (`git commit -S -s`), with no AI attribution.

## Review Focus

1. **An environment deleted, or its approval turned off, while a job
   waits for approval.** Nothing else wakes an approval hold, so the
   user expects the job to fail ("does not exist") or launch, not wait
   forever. Pinned in Task 4 (`release_approval_holds_frees_the_jobs`)
   and Task 6 (`changes_that_release_holds`).
2. **An approval hold seen by the rest of the hold machinery.** The user
   expects a job waiting for approval to neither count as running nor
   keep younger jobs from launching, and the backstop not to wake it
   every few minutes. Pinned in Task 7
   (`an_approval_hold_blocks_nobody`, `the_backstop_skips_approval_holds`).
3. **A re-run of an approved job.** The user expects the new attempt to
   ask again, and the old approval to show as history only. Pinned in
   Task 7 (`a_decision_is_for_its_attempt`).
4. **An environment named in another case.** The workflow says
   `Production`, the repository has `production`. The user expects the
   same environment, its secrets, and one row after a `PUT` in another
   case. Pinned in Task 4 (`environment_lookup_ignores_case`).
5. **An empty selected list, and a selected repository of another
   tenant.** The user expects an organization secret with no selected
   repository to reach none, and a repository id that moved to another
   tenant not to receive the old tenant's secret. Pinned in Task 5
   (`an_empty_selection_reaches_no_repo`,
   `selection_never_crosses_tenants`).

## File map

| File | Responsibility |
| --- | --- |
| `crates/cloud/migrations/0006_ci_environments.sql` | every schema change |
| `crates/cloud/src/app/billing.rs` | webhook wakes CI, checkout refusal, return to the CI page |
| `crates/workflow/src/environment.rs` | the environment name rule |
| `crates/ci/src/environments.rs` | branch patterns, the launch gate, decisions, messages |
| `crates/ci/src/store.rs` | `Owner`, environments, scoped secrets and vars, approval SQL |
| `crates/ci/src/secrets.rs` | `Aad` over `Owner`, three-level overlay |
| `crates/ci/src/settings_api.rs` | environment, repository list and scoped secret routes |
| `crates/ci/src/launches.rs` | the environment gate before billing |
| `crates/ci/src/api.rs` | approve and reject, job JSON |
| `web/app/src/pages/ci/Environments.tsx` | the environments section |
| `web/app/src/pages/ci/RepoAccess.tsx` | all or selected repositories |

---

### Task 1 [standard]: Bill CI on the hatchrun.io subscription

**Files:**
- Create: `crates/cloud/migrations/0006_ci_environments.sql`
- Modify: `crates/cloud/src/polar.rs` (remove `product_kind` and its
  test), `crates/cloud/src/app/billing.rs`, `crates/cloud/src/app/mod.rs`
  (checkout and portal routes), `crates/cloud/src/usage.rs` (`PAID`,
  `MAY_LAUNCH`, tests), `crates/cloud/src/bench.rs`,
  `crates/cloud/src/launch.rs` (test insert), `crates/cloud/wrangler.toml`,
  `crates/ci/src/store.rs` (`TENANT_COLUMNS`, `HAS_CARD`, `STANDING`,
  the `subscribe` test helper and its tests),
  `crates/ci/src/settings_api.rs`, `crates/ci/src/env.rs`,
  `crates/ci/src/webhook.rs` (test inserts), `crates/ci/src/runs.rs`
  (comments), `crates/ci/wrangler.toml`, `crates/ci/scripts/e2e.sh`,
  `crates/ci-dev/src/github.rs`

**Interfaces:**
- Produces, in the migration:

```sql
-- SPDX header, then:
-- hatchrun CI environments, organization secret repository access, and
-- CI billed on the hatchrun.io subscription. Later tasks of the
-- environments plan append here.

-- CI is billed on the hatchrun.io product, so a subscription is a
-- subscription and the product column goes. Rows of the retired CI
-- product must not license runners.
DELETE FROM subscriptions WHERE product = 'ci';
ALTER TABLE subscriptions DROP COLUMN product;
```

- `hatchrun_cloud::app::billing`:

```rust
/// Whether a checkout or portal request asked to come back to the CI
/// page: its query holds `return=ci`.
pub(crate) fn back_to_ci(query: Option<&str>) -> bool;
pub(crate) async fn checkout(ctx: &Ctx<'_>, org: &str, back_to_ci: bool) -> Answer;
pub(crate) async fn portal(ctx: &Ctx<'_>, org: &str, back_to_ci: bool) -> Answer;
async fn settle(db: &Db, subscription: &Subscription, changed: u64) -> Result<Vec<ci_wake::Woken>>;
```

  `checkout` returns to `{app_url}/o/{org}/ci` when `back_to_ci`, else
  `{app_url}/o/{org}/settings` as today (the success URL keeps its
  `?checkout={CHECKOUT_ID}`); `portal` to `{app_url}/o/{org}/ci`, else
  `{app_url}/o/{org}`. `checkout` refuses with `409`
  `"{org} already has a card on file"` only when the tenant's
  subscription (`SUBSCRIPTION`, now without the product filter) has a
  status in `polar::LICENSED`; the beta no longer refuses.
  The webhook writes a subscription only when its `product_id` is in
  `POLAR_PRODUCT_ID` (else it logs
  `"polar webhook: subscription {id} is for product {product}, not in POLAR_PRODUCT_ID"`
  and answers `202`). `UPSERT_SUBSCRIPTION` loses `product` and `$10`.
  `settle`, for a changed and licensed subscription, clears the grace,
  rechecks launches and returns `ci_wake::card_returned(db, tenant)`;
  otherwise it does what the runners branch does today and returns no
  jobs.
- `hatchrun-ci`: every `subscriptions.product = 'ci'` filter goes from
  `TENANT_COLUMNS`, `HAS_CARD` and `STANDING`, so any active or trialing
  row is the card. `SettingsRoute::Checkout`, `SettingsRoute::Portal`,
  `polar_ready`, `polar_failed`, `checkout_refusal` and their routes and
  tests go; `Settings.polar_ci_product_id` goes.
  `settings_api` keeps `GET .../billing` and `PUT .../billing/cap`.
- `crates/cloud/wrangler.toml` loses `POLAR_CI_PRODUCT_ID`; the comment
  on `POLAR_PRODUCT_ID` adds that the product also carries a metered
  price of $0.002 per unit on the "CI usage" meter (sum of
  `metadata.units` of `ci.usage` events) and a meter credit of 500 units
  a month. `crates/ci/wrangler.toml` loses `POLAR_CI_PRODUCT_ID`, and
  its secrets comment says `POLAR_ACCESS_TOKEN` is for the usage events
  only.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/cloud/src/app/billing.rs
#[test] fn back_to_ci_reads_the_query() {
    assert!(back_to_ci(Some("return=ci")));
    assert!(back_to_ci(Some("a=1&return=ci")));
    assert!(!back_to_ci(Some("return=settings")));
    assert!(!back_to_ci(None));
}
#[test] fn a_licensed_subscription_wakes_jobs_held_for_the_card()
    // tenant t1; ci_repos 7 of t1; one ci_runs row; two ci_jobs queued
    // with held = 'card'. settle(active subscription of t1, changed 1)
    // returns both jobs (oldest first) and both rows now hold
    // held = 'concurrency'. settle(canceled subscription, 1) returns [].
#[test] fn a_delivery_writes_the_subscription() // existing, without the product value
// Delete: webhook_stores_the_product, ci_subscription_leaves_the_grace_alone,
// ci_only_tenant_shows_no_runners_subscription and the stored_product helper.

// crates/cloud/src/usage.rs: the_launch_rule_matches_the_constants
// drops `assert!(PAID.contains("product = 'runners'"))` and adds
assert!(!MAY_LAUNCH.contains("product"));

// crates/ci/src/store.rs (subscribe(db, id, status) loses `product`)
#[test] fn has_card_needs_an_active_subscription() {
    // none -> false; "s1" canceled -> false; "s2" active -> true;
    // s2 trialing -> true; tenant "nope" -> false
}
#[test] fn tenant_row_reads_the_billing_state() {
    // none -> Never; "s1" canceled -> Lapsed; "s2" active -> Active;
    // s2 canceled -> Lapsed; s2 trialing -> Active (by id and by installation)
}
// crates/ci/src/settings_api.rs: routes_parse loses the checkout and
// portal asserts; other_paths_and_methods_have_no_route adds
// ("orgs/acme/billing/checkout", Method::Post) and ("orgs/acme/billing/portal", Method::Post)
// and asserts find("orgs/acme/billing/checkout").is_none().
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-cloud billing usage && cargo test -p hatchrun-ci store settings_api`
Expected: FAIL: `back_to_ci` not defined; inserts still name `product`.

- [ ] **Step 3: Implement** the migration and the code above. In
  `app/mod.rs`, read the query once
  (`let query = req.url()?.query().map(str::to_owned);`) and pass
  `billing::back_to_ci(query.as_deref())` to `checkout` and `portal`.
  The test helper `delivery(...)` in `billing.rs` loses its
  `"runners"` value. Update every test insert into `subscriptions` that
  names `product`
  (`usage.rs`, `bench.rs` insert and its `billed` SQL, `launch.rs`,
  `webhook.rs`, `store.rs`). Comments that say "CI subscription" or "CI
  product" now say "subscription".
  In `crates/ci/scripts/e2e.sh`: drop `CI_PRODUCT` and the
  `--var POLAR_CI_PRODUCT_ID` line; the seed inserts both subscriptions
  without `product`, with `product_id` `e2e-product`; drop the
  `orgs/e2e2/billing/checkout` call and its check from the billing
  scenario; the header comment's billing bullet ends "gets one billing
  check run and no run; a tenant that never subscribed gets nothing,
  and its config is not read."
  In `crates/ci-dev/src/github.rs`: remove the `/v1/checkouts/` and
  `/v1/customer-sessions/` routes, their handlers, and their asserts in
  `fake_polar_keeps_events_once_and_wants_the_token`.

- [ ] **Step 4: Run the tests and builds**

Run: `cargo test -p hatchrun-cloud && cargo test -p hatchrun-ci && cargo test -p hatchrun-ci-dev && cargo check -p hatchrun-ci -p hatchrun-cloud --target wasm32-unknown-unknown && ! git grep -n "POLAR_CI_PRODUCT_ID\|polar_ci_product_id\|product_kind" -- crates`
Expected: PASS, and the grep prints nothing.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud crates/ci crates/ci-dev
git commit -S -s -m "feat(billing): bill ci on the hatchrun.io subscription"
```

---

### Task 2 [cheap]: The CI page opens the hatchrun.io checkout

**Files:**
- Modify: `web/app/src/api.ts`, `web/app/src/pages/ci/CiSettings.tsx`,
  `web/app/mock/api.ts`, `web/app/mock/ci-settings.ts`,
  `docs/superpowers/specs/2026-10-19-orchestrator-design.md`

**Interfaces:**
- Consumes: Task 1's `?return=ci`.
- Produces, in `api.ts`:

```ts
export const startCheckout = (org: string, back?: "ci") =>
  post<{ url: string }>(`${tenantPath(org)}/checkout${back ? "?return=ci" : ""}`);
export const openPortal = (org: string, back?: "ci") =>
  post<{ url: string }>(`${tenantPath(org)}/portal${back ? "?return=ci" : ""}`);
// removed: startCiCheckout, openCiPortal
```

  `BillingCard` shows "A card is on file." with a "Manage billing"
  button (`openPortal(org, "ci")`), or "Add a card to run CI jobs. Jobs
  wait until you do." with an "Add a card" primary button
  (`startCheckout(org, "ci")`). An error shows the server's message, as
  today.
- The spec's "Billing and abuse" bullet gains: "The card is the
  hatchrun.io subscription: CI minutes are billed on the same Polar
  product as runner minutes, through a metered price on the meter that
  sums the `units` of `ci.usage` events, with 500 free units a month as
  a meter credit. Decided 2026-10-24."

- [ ] **Step 1: Update the mocks.** `mock/api.ts`: the tenant checkout
  answers `/o/${org}/ci?checkout=mock` and the portal
  `/o/${org}/ci?portal=mock` when the query has `return=ci`.
  `mock/ci-settings.ts`: drop the `checkout` and `portal` branches and
  narrow its billing regex to `billing(?:\/cap)?`.

- [ ] **Step 2: Implement** the `api.ts`, `CiSettings.tsx` and spec
  changes.

- [ ] **Step 3: Check the app**

Run: `cd web/app && npm run typecheck && npm run lint && npm run build && ! grep -rn "startCiCheckout\|openCiPortal" src mock`
Expected: no errors, the grep prints nothing.

- [ ] **Step 4: Commit**

```bash
git add web/app docs/superpowers/specs/2026-10-19-orchestrator-design.md
git commit -S -s -m "feat(app): add a card for ci through the hatchrun.io checkout"
```

---

### Task 3 [standard]: `environment:` on jobs

**Files:**
- Create: `crates/workflow/src/environment.rs`
- Modify: `crates/workflow/src/lib.rs`, `crates/workflow/src/config.rs`
  (`Job`, `JobDe`), `crates/workflow/src/plan.rs` (`PlannedJob`,
  `expand_job`, the approval job literal),
  `crates/workflow/src/convert.rs`, the `PlannedJob` literals in
  `crates/workflow/src/secrets.rs`, `crates/ci/src/checks.rs`,
  `crates/ci/src/runs.rs`, `crates/ci/src/agent.rs`,
  `crates/hatchrun-agent/src/ci/client.rs`,
  `crates/hatchrun-agent/src/exec/context.rs`,
  `crates/hatchrun-agent/tests/fixtures/jobs/make.rs`
- Test: `crates/workflow/tests/plan.rs`, `crates/workflow/tests/parse.rs`,
  `crates/workflow/tests/convert.rs`

**Interfaces:**
- Produces, re-exported at the crate root:

```rust
// hatchrun_workflow::environment
/// The longest environment name.
pub const ENVIRONMENT_NAME_MAX: usize = 255;
/// Ok when `name` is 1 to 255 ASCII letters, digits, `-`, `_` or `.`.
/// # Errors
/// "Environment names cannot be empty", "Environment names are at most 255 characters",
/// "Use letters, digits, -, _ and . in environment names".
pub fn valid_environment_name(name: &str) -> Result<(), String>;
```

- `config::Job.environment: Option<String>` (YAML key `environment`, a
  string; `skip_serializing_if = "Option::is_none"`), and the same field
  on `JobDe`.
- `PlannedJob.environment: Option<String>`: the job's name after
  parameter substitution (`sub.opt`), `None` for approval jobs.
  Compile errors, pushed like the other `expand_job` errors:
  `"{owner}: `environment` can use only parameters; found `{value}`"`
  when `${{` is left after substitution, and
  `"{owner}: `environment` `{value}`: {rule message}"` when the name
  breaks the rule.
- `convert`: `environment` leaves the dropped-key list. A string, or a
  map's `name` string, becomes `Job.environment`. A map's `url` warns
  `"{at}: `environment.url` is not supported and was dropped"`; any
  other map key warns `"{at}: unknown key `environment.{key}` was dropped"`.
  Errors that drop the job: a value with `${{`
  (`"{at}: `environment` uses an expression; hatchrun reads environment names as written"`),
  a name that breaks the rule (`"{at}: `environment` `{name}`: {rule message}"`),
  and any other shape (`"{at}: `environment` must be a name or a map with `name`"`).

- [ ] **Step 1: Write the failing tests**

```rust
// environment.rs
#[test] fn environment_names() {
    assert!(valid_environment_name("production").is_ok());
    assert!(valid_environment_name("eu-west.1_b").is_ok());
    assert!(valid_environment_name(&"a".repeat(255)).is_ok());
    assert_eq!(valid_environment_name(""), Err("Environment names cannot be empty".into()));
    assert_eq!(valid_environment_name(&"a".repeat(256)),
        Err("Environment names are at most 255 characters".into()));
    assert_eq!(valid_environment_name("prod eu"),
        Err("Use letters, digits, -, _ and . in environment names".into()));
    assert!(valid_environment_name("a/b").is_err());
}
// tests/parse.rs
#[test] fn job_environment_round_trips()
    // jobs.deploy.environment: production parses to Some("production")
    // and to_yaml writes `environment: production` back
// tests/plan.rs
#[test] fn environment_reaches_the_planned_job()        // Some("production"); a job without one: None
#[test] fn environment_takes_parameters()
    // job deploy { parameters: { target: { type: string } }, environment: "${{ parameters.target }}" },
    // workflow entry `deploy: { matrix: { target: [staging, production] } }`
    // -> two planned jobs with environment Some("staging") and Some("production")
#[test] fn environment_with_a_run_time_expression_is_an_error()
    // environment: "${{ github.ref_name }}" -> error containing
    // "`environment` can use only parameters"
#[test] fn environment_with_a_bad_name_is_an_error()
    // environment: "prod eu" -> error containing "Use letters, digits, -, _ and . in environment names"
#[test] fn approval_jobs_have_no_environment()
// tests/convert.rs
#[test] fn environment_carries_over()
    // `environment: production` and `environment: { name: staging, url: https://x }`
    // -> Some("production"), Some("staging"); one warning
    // "`environment.url` is not supported and was dropped"; no "environment` is not supported" warning
#[test] fn environment_expressions_drop_the_job()
    // `environment: ${{ matrix.env }}` -> error containing
    // "`environment` uses an expression"; the job is not in the config
#[test] fn environment_of_another_shape_drops_the_job() // `environment: [a]` -> "must be a name or a map with `name`"
// Update dropped_job_keys_warn_and_name_the_file_and_job: `environment`
// is no longer in its expected warnings.
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-workflow environment`
Expected: FAIL, module and field not defined.

- [ ] **Step 3: Implement** the module, the fields and the checks.
  The `PlannedJob` literals outside `plan.rs` gain `environment: None`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-workflow && cargo test -p hatchrun-cli && cargo check --workspace --all-targets && cargo check -p hatchrun-workflow --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workflow crates/ci/src crates/hatchrun-agent
git commit -S -s -m "feat(workflow): read a job's environment"
```

---

### Task 4 [standard]: Environments and scoped secrets in the store

**Files:**
- Modify: `crates/cloud/migrations/0006_ci_environments.sql`,
  `crates/ci/src/store.rs`, `crates/ci/src/secrets.rs`,
  `crates/ci/src/settings_api.rs` (callers, no new routes),
  `crates/ci/src/agent.rs` (`read_values` callers, `None` for now),
  `crates/ci/src/reconcile.rs` (reseal's `Aad`), `crates/ci/src/runs.rs`
  (`NewJob.environment`)

**Interfaces:**
- Consumes: Task 3 (`PlannedJob.environment`).
- Produces, appended to the migration:

```sql
-- Environments of a repository. Names are unique per repository,
-- ignoring case; branches is a JSON array of glob patterns.
CREATE TABLE ci_environments (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
  branches jsonb NOT NULL CHECK (jsonb_typeof(branches) = 'array'),
  approval boolean NOT NULL,
  updated_at bigint NOT NULL
);
CREATE UNIQUE INDEX ci_environments_name ON ci_environments (repo_id, lower(name));

-- Environment secrets and vars: repo_id is the environment's repository.
ALTER TABLE ci_secrets
  ADD COLUMN environment_id bigint REFERENCES ci_environments (id) ON DELETE CASCADE,
  ADD CONSTRAINT ci_secrets_environment_repo CHECK (environment_id IS NULL OR repo_id IS NOT NULL),
  DROP CONSTRAINT ci_secrets_tenant_id_repo_id_name_key,
  ADD CONSTRAINT ci_secrets_scope_name UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, environment_id, name);
ALTER TABLE ci_vars
  ADD COLUMN environment_id bigint REFERENCES ci_environments (id) ON DELETE CASCADE,
  ADD CONSTRAINT ci_vars_environment_repo CHECK (environment_id IS NULL OR repo_id IS NOT NULL),
  DROP CONSTRAINT ci_vars_tenant_id_repo_id_name_key,
  ADD CONSTRAINT ci_vars_scope_name UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, environment_id, name);

-- The environment a job names, and the approval of one attempt.
ALTER TABLE ci_jobs
  ADD COLUMN environment text,
  ADD COLUMN approval_attempt bigint,
  ADD COLUMN approval_decision text CHECK (approval_decision IN ('approved', 'rejected')),
  ADD COLUMN approval_by text,
  ADD COLUMN approval_at bigint;
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval'));
```

- `store.rs`:

```rust
/// Where a secret or var row lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner { Org, Repo(u64), Environment { repo_id: u64, environment_id: u64 } }
impl Owner {
    pub fn repo_id(self) -> Option<u64>;
    pub fn environment_id(self) -> Option<u64>;
    /// From a row's columns: an environment row also has its repository.
    pub fn of(repo_id: Option<u64>, environment_id: Option<u64>) -> Owner;
}

// Every secret and var function takes `owner: Owner` where it took
// `repo_id: Option<u64>`: put_secret, delete_secret, secret_names,
// put_var, delete_var, vars. Their SQL adds
// `AND environment_id IS NOT DISTINCT FROM $n::bigint` next to the
// repo_id test, and ON CONFLICT names (tenant_id, repo_id, environment_id, name).

// SealedRow and StaleRow gain `environment_id: Option<u64>` (in Debug too)
// and `fn owner(&self) -> Owner`. reseal matches on environment_id as well.

/// The organization's, repository `repo_id`'s and (when set) environment
/// `environment_id`'s sealed secrets, named in `names` or all when None,
/// ordered by name, then organization, repository, environment.
pub async fn sealed_secrets(db: &Db, tenant: &str, repo_id: u64,
    environment_id: Option<u64>, names: Option<&[String]>) -> Result<Vec<SealedRow>>;
/// The organization's vars overlaid with the repository's, then the environment's.
pub async fn job_vars(db: &Db, tenant: &str, repo_id: u64,
    environment_id: Option<u64>) -> Result<BTreeMap<String, String>>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EnvironmentRow {
    pub id: u64,
    pub repo_id: u64,
    pub name: String,
    #[serde(deserialize_with = "json_strings")] // branches::text AS branches
    pub branches: Vec<String>,
    pub approval: bool,
    pub updated_at: u64,
}
pub enum PutEnvironment { Saved(EnvironmentRow), TooMany }
/// By lower(name).
pub async fn environments(db: &Db, repo_id: u64) -> Result<Vec<EnvironmentRow>>;
/// Ignoring case.
pub async fn environment(db: &Db, repo_id: u64, name: &str) -> Result<Option<EnvironmentRow>>;
/// Creates, or updates branches and approval of the row of that name in
/// any case (its stored name stays). A new name is refused at `limit`.
/// Runs under LOCK_TENANT like `put`.
#[allow(clippy::too_many_arguments)]
pub async fn put_environment(db: &Db, tenant: &str, repo_id: u64, name: &str,
    branches: &[String], approval: bool, limit: u32, now: u64) -> Result<PutEnvironment>;
/// Its secrets and vars go with it (ON DELETE CASCADE).
pub async fn delete_environment(db: &Db, repo_id: u64, name: &str) -> Result<bool>;

/// What the launch consumer and the claim need about a job's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnvironment {
    pub run_id: u64,
    pub repo_id: u64,
    /// `owner/repo`.
    pub full_name: String,
    pub event: String,
    pub git_ref: String,
    /// The environment the job names, `None` when the repository has none of that name.
    pub environment: Option<EnvironmentRow>,
}
/// `None` when the job does not exist.
pub async fn job_environment(db: &Db, job_id: u64) -> Result<Option<JobEnvironment>>;

/// Clears the approval holds of queued jobs in repository `repo_id` that
/// name environment `name` (ignoring case), due at `now`, and returns
/// them for the launch queue.
pub async fn release_approval_holds(db: &Db, repo_id: u64, name: &str, now: u64)
    -> Result<Vec<hatchrun_cloud::ci_wake::Woken>>;
```

  `NewJob.environment: Option<String>` (inserted by `insert_jobs`);
  `JobRow` gains `environment: Option<String>`,
  `approval_attempt: Option<u32>`, `approval_decision: Option<String>`,
  `approval_by: Option<String>`, `approval_at: Option<u64>`, all in
  `JOB_COLUMNS`. `runs.rs` fills `environment: job.environment.clone()`.
  `upsert_repo` also runs
  `DELETE FROM ci_environments WHERE repo_id = $1 AND tenant_id <> $2`.
  `job_environment`'s SQL:

```sql
SELECT ci_runs.id AS run_id, ci_runs.repo_id, ci_repos.full_name, ci_runs.event,
  ci_runs.ref AS git_ref, e.id AS environment_id, e.name AS environment_name,
  e.branches::text AS branches, e.approval, e.updated_at AS environment_updated_at
FROM ci_jobs JOIN ci_runs ON ci_runs.id = ci_jobs.run_id
  JOIN ci_repos ON ci_repos.id = ci_runs.repo_id
  LEFT JOIN ci_environments e ON e.repo_id = ci_runs.repo_id
    AND lower(e.name) = lower(ci_jobs.environment)
WHERE ci_jobs.id = $1
```

  `release_approval_holds`' SQL:

```sql
UPDATE ci_jobs SET held = NULL, next_launch_at = $3, updated_at = $3 FROM ci_runs
WHERE ci_runs.id = ci_jobs.run_id AND ci_runs.repo_id = $1
  AND lower(ci_jobs.environment) = lower($2)
  AND ci_jobs.status = 'queued' AND ci_jobs.held = 'approval'
RETURNING ci_jobs.id AS job_id, ci_jobs.attempt
```

- `secrets.rs`: `Aad { tenant: &str, owner: Owner, name: &str }`. The
  scope part of the associated data is `org` for `Owner::Org`, the id
  for `Owner::Repo(id)` (both as today), and
  `{repo_id}/env/{environment_id}` for an environment. `job_secrets`
  inserts organization rows, then repository rows, then environment
  rows, so the most specific wins.

- [ ] **Step 1: Write the failing tests**

```rust
// store.rs (Postgres; setup() has tenant t1 and repo 7 acme/app)
#[test] fn environments_are_unique_per_repo_ignoring_case() {
    // put "Production" then "production" (branches ["main"], approval true):
    // one row, named "Production", branches ["main"], approval true
}
#[test] fn environment_lookup_ignores_case()
    // environment(7, "PRODUCTION") finds "Production"; job_environment of a job
    // whose environment is "production" has Some(row); of a job naming "nope", None
#[test] fn put_environment_counts_against_the_limit()
    // limit 2: third new name -> TooMany; updating an existing one -> Saved
#[test] fn deleting_an_environment_deletes_its_secrets_and_vars()
#[test] fn environment_values_win_over_repo_and_org() {
    // var X: org "o", repo 7 "r", environment e of repo 7 "e"
    // job_vars(t1, 7, Some(e)) == {X: "e"}; job_vars(t1, 7, None) == {X: "r"};
    // sealed_secrets(t1, 7, Some(e), None) returns org, repo, env rows of
    // one name in that order; sealed_secrets(t1, 7, None, None) has no env row
}
#[test] fn another_environment_stays_out() // env f of repo 7: its rows never come with Some(e)
#[test] fn release_approval_holds_frees_the_jobs()
    // two queued jobs of repo 7 held 'approval' naming "Production" and "production",
    // one held 'card' naming "production", one of another environment:
    // release_approval_holds(7, "PRODUCTION", 50) returns the first two;
    // their held is NULL and next_launch_at 50; the others keep their hold
#[test] fn a_transferred_repo_leaves_its_environments()
    // upsert_repo(7 now of tenant t2) deletes t1's environments of repo 7
#[test] fn job_rows_carry_environment() // insert_jobs with environment Some("production") reads back

// secrets.rs
#[test] fn environment_values_are_bound_to_their_environment()
    // sealed for Environment{7, 1} opens with that owner, not with Repo(7)
    // nor Environment{7, 2}; Org and Repo(7) values still open as before
#[test] fn the_most_specific_row_wins() // job_secrets over org, repo and env rows of one name -> env value
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci store secrets`
Expected: FAIL.

- [ ] **Step 3: Implement** the migration, the store and `secrets.rs`
  changes, and update every caller and existing test to `Owner`
  (`settings_api::Target` holds `owner: Owner`; `agent.rs` passes
  `None` for the environment until Task 7; `reconcile.rs` builds the
  `Aad` with `row.owner()`). `json_strings` reads a JSON array of
  strings carried as text, like `json_text`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud/migrations/0006_ci_environments.sql crates/ci/src
git commit -S -s -m "feat(ci): store environments and their secrets and vars"
```

---

### Task 5 [standard]: Organization secrets for selected repositories

**Files:**
- Modify: `crates/cloud/migrations/0006_ci_environments.sql`,
  `crates/ci/src/store.rs`, `crates/ci/src/secrets.rs`,
  `crates/ci/src/settings_api.rs`

**Interfaces:**
- Consumes: Task 4 (`Owner`, `sealed_secrets`, `job_vars`).
- Produces, appended to the migration:

```sql
-- An organization secret or var reaches every repository (NULL) or the
-- ci_repos ids listed.
ALTER TABLE ci_secrets ADD COLUMN repositories bigint[]
  CHECK (repositories IS NULL OR (repo_id IS NULL AND cardinality(repositories) <= 1000));
ALTER TABLE ci_vars ADD COLUMN repositories bigint[]
  CHECK (repositories IS NULL OR (repo_id IS NULL AND cardinality(repositories) <= 1000));
```

- `secrets::MAX_SELECTED_REPOS: usize = 1000`.
- `store.rs`:

```rust
// put_secret and put_var gain `repositories: Option<&[u64]>` after `owner`,
// bound as text "{1,2}" (or NULL) and cast `$n::bigint[]`; the insert and
// the ON CONFLICT update both set it.
/// A listed secret: never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretItem { pub name: String, pub updated_at: u64, pub repositories: Option<Vec<String>> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarItem { pub name: String, pub value: String, pub updated_at: u64, pub repositories: Option<Vec<String>> }
pub async fn secret_names(db: &Db, tenant: &str, owner: Owner) -> Result<Vec<SecretItem>>;
pub async fn vars(db: &Db, tenant: &str, owner: Owner) -> Result<Vec<VarItem>>;
/// The tenant's repositories CI has seen, by lower(full_name), without
/// names that hold `#gone-`.
pub async fn tenant_repos(db: &Db, tenant: &str) -> Result<Vec<String>>;
/// The ids of the tenant's repositories named in `names` (ignoring case),
/// as (name as stored, id).
pub async fn repo_ids(db: &Db, tenant: &str, names: &[String]) -> Result<Vec<(String, u64)>>;
```

  The list SQL reads `repositories` as names:
  `CASE WHEN repositories IS NULL THEN NULL ELSE COALESCE((SELECT json_agg(r.full_name ORDER BY lower(r.full_name)) FROM ci_repos r WHERE r.id = ANY (s.repositories) AND r.tenant_id = s.tenant_id), '[]'::json)::text END AS repositories`.
  `sealed_secrets` and `job_vars` keep an organization row only when
  `repositories IS NULL OR $2::bigint = ANY (repositories)`.
- `settings_api.rs`:
  - The `PUT` body is
    `#[serde(deny_unknown_fields)] struct ValueBody { value: String, #[serde(default)] repositories: Option<Vec<String>> }`;
    `parse_value` becomes
    `fn parse_body(body: &[u8], max_body: usize) -> Result<ValueBody, Stop>`
    with the same checks and messages.
  - Org scope: `None` stores NULL. A list longer than
    `MAX_SELECTED_REPOS` is `422` `"Select at most 1000 repositories"`.
    Names `repo_ids` does not find are `422`
    `"Unknown repositories: {names joined with ", "}"`, in the order
    given. Repository and environment scope with `Some(_)`: `400`
    `"Only organization secrets and variables have repository access"`.
  - List answers: secrets `[{"name","updatedAt","repositories"}]`, vars
    `[{"name","value","updatedAt","repositories"}]`; `repositories` is
    `null` or the names.
  - New route `SettingsRoute::Repositories { org }`:
    `GET /api/ci/orgs/{org}/repositories`, org admin, answers
    `[{"name": "acme/api"}, ...]` from `tenant_repos`.

- [ ] **Step 1: Write the failing tests**

```rust
// store.rs
#[test] fn selected_org_rows_reach_only_their_repos() {
    // repos 7 and 8 of t1; org secret A for [8], org secret B for all;
    // sealed_secrets(t1, 7, None, None) names B only; (t1, 8, ...) names A and B;
    // the same for vars through job_vars
}
#[test] fn an_empty_selection_reaches_no_repo()     // repositories Some(&[]) -> neither 7 nor 8 gets it
#[test] fn selection_never_crosses_tenants()
    // t2 owns repo 9; a t1 org secret selecting id 9 is not returned by
    // sealed_secrets(t2, 9, None, None), and t1's listing shows repositories []
#[test] fn listing_names_the_selected_repos()
    // A for [8, 7] lists repositories Some(["acme/app", "acme/web"]); B lists None
#[test] fn a_repo_row_cannot_select_repos()         // raw INSERT with repo_id 7 and repositories '{7}' fails the CHECK
#[test] fn tenant_repos_leaves_out_gone_names()     // a row renamed by FREE_REPO_NAME is not listed
#[test] fn repo_ids_ignore_case()                   // ["ACME/APP", "acme/nope"] -> [("acme/app", 7)]
// settings_api.rs
#[test] fn bodies_with_repositories() {
    // {"value":"x","repositories":["a/b"]} -> Some(["a/b"]);
    // {"value":"x","repositories":null} and {"value":"x"} -> None;
    // {"value":"x","repositories":"a/b"} -> 400 starting with "expected"
}
#[test] fn routes_parse() // adds orgs/acme/repositories GET -> Repositories { org: "acme" }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci store settings_api`
Expected: FAIL.

- [ ] **Step 3: Implement** the column, the store functions and the
  route. Existing `value_too_large`, `escaped_value_fits` and
  `bad_bodies_are_refused_without_echoing_them` move to `parse_body`
  and keep their assertions.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cloud/migrations/0006_ci_environments.sql crates/ci/src
git commit -S -s -m "feat(ci): limit organization secrets and vars to selected repositories"
```

---

### Task 6 [standard]: Environment routes

**Files:**
- Create: `crates/ci/src/environments.rs`
- Modify: `crates/ci/src/lib.rs`, `crates/ci/src/settings_api.rs`,
  `crates/ci/src/api.rs` (`body_cap` only if needed)

**Interfaces:**
- Consumes: Task 4 store functions, Task 3 `valid_environment_name`.
- Produces, in `environments.rs` (Task 7 adds the gate here):

```rust
/// The most environments one repository holds.
pub const MAX_PER_REPO: u32 = 100;
/// The most branch patterns of one environment.
pub const MAX_BRANCH_PATTERNS: usize = 50;
/// The longest branch pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 255;
/// # Errors
/// "Set at most 50 branch patterns", "Branch patterns cannot be empty",
/// "Branch patterns are at most 255 bytes", "Branch patterns cannot hold control characters".
pub fn valid_branches(patterns: &[String]) -> Result<(), String>;
/// The JSON the API answers for an environment:
/// {"name", "branches", "approval", "updatedAt"}.
pub fn environment_json(row: &EnvironmentRow) -> serde_json::Value;
```

- `settings_api.rs`:

```rust
pub enum Scope<'a> {
    Org(&'a str),
    Repo { owner: &'a str, repo: &'a str },
    Environment { owner: &'a str, repo: &'a str, environment: &'a str },
}
pub enum SettingsRoute<'a> {
    // ... existing, plus:
    Environments { owner: &'a str, repo: &'a str },                 // GET
    Environment { owner: &'a str, repo: &'a str, name: &'a str },   // PUT, DELETE
    Repositories { org: &'a str },                                  // GET (Task 5)
}
```

  Paths after `/api/ci/`: `repos/{o}/{r}/environments`,
  `repos/{o}/{r}/environments/{env}`,
  `repos/{o}/{r}/environments/{env}/(secrets|vars)` and
  `repos/{o}/{r}/environments/{env}/(secrets|vars)/{name}`. `{env}` must
  pass `valid_environment_name`, else no route.
  All need repository admin (`Access::Admin`), as repository secrets do.
  - `GET .../environments` answers `[environment_json, ...]`.
  - `PUT .../environments/{env}` with
    `#[serde(deny_unknown_fields)] struct EnvironmentBody { branches: Vec<String>, approval: bool }`
    (a body that does not read: `400`
    `"expected {\"branches\": [...], \"approval\": false}"`; a pattern
    rule: `422` with its message; `TooMany`: `409`
    `"{owner}/{repo} has 100 environments, the most allowed"`) answers
    `200` with `environment_json` of the stored row. When the stored row
    has `approval: false`, it calls `release_approval_holds` and
    `launches::send_wakes` (a wake failure is logged, the answer stays
    `200`).
  - `DELETE .../environments/{env}`: `404` when missing, else deletes,
    then releases and wakes the held jobs as above, and answers `204`.
  - Environment scope secrets and vars: `target()` finds the
    repository, then `store::environment(repo.id, env)`; a missing one
    is `404` `"not found"`. `Owner::Environment { repo_id, environment_id }`,
    label `"{owner}/{repo} environment {name}"`, limit
    `secrets::MAX_PER_REPO`.
  - The module doc table lists the new routes.

- [ ] **Step 1: Write the failing tests**

```rust
// environments.rs
#[test] fn branch_pattern_rules() {
    assert!(valid_branches(&[]).is_ok());
    assert!(valid_branches(&["main".into(), "release/*".into(), "!release/old".into()]).is_ok());
    assert_eq!(valid_branches(&vec!["a".to_owned(); 51]), Err("Set at most 50 branch patterns".into()));
    assert_eq!(valid_branches(&["".into()]), Err("Branch patterns cannot be empty".into()));
    assert_eq!(valid_branches(&["a".repeat(256)]), Err("Branch patterns are at most 255 bytes".into()));
    assert_eq!(valid_branches(&["a\nb".into()]), Err("Branch patterns cannot hold control characters".into()));
}
#[test] fn environment_json_shape()
    // keys exactly name, branches, approval, updatedAt
// settings_api.rs
#[test] fn environment_routes_parse() {
    // repos/acme/api/environments GET -> Environments
    // repos/acme/api/environments/production PUT and DELETE -> Environment
    // repos/acme/api/environments/production/secrets GET ->
    //   Secrets(Scope::Environment { owner: "acme", repo: "api", environment: "production" })
    // repos/acme/api/environments/production/vars/X PUT -> Var(.., "X")
}
#[test] fn bad_environment_paths_have_no_route()
    // environments/prod%20eu, environments/ (empty), environments/production GET,
    // environments POST, environments/production/other
#[test] fn changes_that_release_holds()
    // fn releases(approval_after: bool, deleted: bool) -> bool, the rule both
    // handlers use: true for a delete or for approval false
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci environments settings_api`
Expected: FAIL.

- [ ] **Step 3: Implement** the module and routes, with
  `fn releases(approval_after: bool, deleted: bool) -> bool` in
  `settings_api.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci/src
git commit -S -s -m "feat(ci): add environment routes and environment secrets and vars"
```

---

### Task 7 [standard]: Check the environment at launch and claim

**Files:**
- Modify: `crates/ci/src/environments.rs`, `crates/ci/src/launches.rs`,
  `crates/ci/src/agent.rs`, `crates/cloud/src/ci_wake.rs`,
  `crates/ci/src/store.rs` (tests only)

**Interfaces:**
- Consumes: Task 4 (`job_environment`, `JobRow` approval fields,
  `sealed_secrets`, `job_vars`).
- Produces, in `environments.rs`:

```rust
/// A recorded decision on the job's current attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision<'a> { Approved(&'a str), Rejected(&'a str) }
/// The decision of `job.attempt`: `None` when there is none, or it was for another attempt.
pub fn decision(job: &JobRow) -> Option<Decision<'_>>;

/// Whether a run may use an environment with `patterns`: any run when
/// there are none; otherwise only a run whose event is not
/// `pull_request` and whose ref is `refs/heads/<branch>` with `branch`
/// passing `hatchrun_workflow::Filter::new(patterns)`.
pub fn branch_allowed(patterns: &[String], event: &str, git_ref: &str) -> bool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Go on to billing and the launch.
    Pass,
    /// End the job with this message.
    Fail(String),
    /// Hold the job for approval; the check run says this.
    Approval(String),
}
/// The check, in order: the environment exists, the run's ref is
/// allowed, the attempt was not rejected, an approval environment has an
/// approval for this attempt.
pub fn gate(asked: &str, found: &JobEnvironment, decision: Option<Decision<'_>>, app_url: &str) -> Gate;
```

  Messages, exactly:
  - missing: ``"Environment `{asked}` does not exist in {full_name}. Create it at {app_url}/ci/{full_name}/settings."``
  - ref refused: ``"Environment `{name}` allows only branches {patterns joined with ", "}; this run is on {git_ref}."``
  - rejected: `"Rejected by {login}"`
  - approval: ``"Waiting for approval to use environment `{name}`. An admin of {owner} can approve or reject it at {app_url}/ci/{full_name}/runs/{run_id}."``
    (`owner` is `full_name` before the `/`)
  - at claim, when the environment is gone:
    ``"Environment `{name}` was deleted before the job started."``
- `launches.rs`: `hold_job` becomes
  `async fn hold_job(env: &Env, db: &Db, job: &JobRow, reason: &str, message: &str, now: u64) -> Result<(), String>`;
  billing callers pass `Hold::as_str()` and `Hold::message(...)`.
  `end_job` becomes `pub(crate)`. In `launch`, for `Delivery::Launch`
  of a job whose `environment` is set, before the standing is read:
  `store::job_environment` (`None`: return `Ok(())`), then
  `environments::gate`: `Fail(m)` calls `end_job(env, &db, &job, &m)`
  with no claim; `Approval(m)` calls `hold_job(.., "approval", &m, now)`;
  `Pass` goes on to billing. `Delivery::GiveUp` skips the gate.
- `agent.rs`: `read_values` reads `store::job_environment` when
  `row.environment` is set; no environment row is
  `OpenFailure::EnvironmentGone(String)`, which `opened` turns into
  `Opened::EndJob` with the claim message above. Otherwise it passes
  the environment id to `sealed_secrets` and `job_vars`.
- `ci_wake::STALE_HELD` adds `AND ci_jobs.held <> 'approval'`.

- [ ] **Step 1: Write the failing tests**

```rust
// environments.rs (JobEnvironment and EnvironmentRow built in the test)
#[test] fn empty_patterns_allow_any_ref() {
    assert!(branch_allowed(&[], "push", "refs/tags/v1"));
    assert!(branch_allowed(&[], "pull_request", "refs/pull/3/merge"));
}
#[test] fn patterns_match_branch_names() {
    let p = vec!["main".to_owned(), "release/*".to_owned()];
    assert!(branch_allowed(&p, "push", "refs/heads/main"));
    assert!(branch_allowed(&p, "workflow_dispatch", "refs/heads/release/1.2"));
    assert!(!branch_allowed(&p, "push", "refs/heads/feature"));
    assert!(!branch_allowed(&p, "push", "refs/tags/main"));
    assert!(!branch_allowed(&p, "pull_request", "refs/heads/main"));
}
#[test] fn gate_order_and_messages() {
    // missing -> Fail("Environment `prod` does not exist in acme/app. Create it at https://app.hatchrun.io/ci/acme/app/settings.")
    // branches ["main"], ref refs/heads/x -> Fail("Environment `production` allows only branches main; this run is on refs/heads/x.")
    // approval, no decision -> Approval("Waiting for approval to use environment `production`. An admin of acme can approve or reject it at https://app.hatchrun.io/ci/acme/app/runs/12.")
    // approval, Approved("octo") -> Pass; Rejected("octo") -> Fail("Rejected by octo")
    // a refused ref wins over Approved; no approval and no decision -> Pass
}
#[test] fn a_decision_is_for_its_attempt()
    // JobRow attempt 2, approval_attempt Some(1), decision "approved" -> None;
    // approval_attempt Some(2) -> Some(Approved("octo"))
// agent.rs
#[test] fn a_gone_environment_ends_the_job()
    // opened(Err(OpenFailure::EnvironmentGone("production".into())))
    //   == Opened::EndJob("Environment `production` was deleted before the job started.".into())
// store.rs (ci_wake and claim tests live here)
#[test] fn the_backstop_skips_approval_holds()
    // a quiet job held 'approval' and one held 'card': ci_wake::stale returns only the card one
#[test] fn an_approval_hold_blocks_nobody()
    // older job held 'approval', newer job queued: claim_launch(newer, "t1", 5, ..) is Claimed;
    // standing().running counts neither held job
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci environments agent store`
Expected: FAIL.

- [ ] **Step 3: Implement** the gate, the consumer and claim changes
  and the backstop filter.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo test -p hatchrun-cloud && cargo check -p hatchrun-ci -p hatchrun-cloud --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci/src crates/cloud/src/ci_wake.rs
git commit -S -s -m "feat(ci): check a job's environment before it launches"
```

---

### Task 8 [standard]: Approve and reject a job

**Files:**
- Modify: `crates/ci/src/api.rs`, `crates/ci/src/store.rs`

**Interfaces:**
- Consumes: Task 7 (`environments::decision`, `launches::end_job`,
  `launches::send_wakes`).
- Produces:

```rust
// store.rs
/// Records the decision on attempt `attempt` of job `job_id` while it is
/// queued and held for approval: held cleared, next_launch_at and
/// updated_at set to `now`. Returns whether it applied.
pub async fn decide_approval(db: &Db, job_id: u64, attempt: u32, approved: bool,
    login: &str, now: u64) -> Result<bool>;
```

```sql
UPDATE ci_jobs SET approval_attempt = $2, approval_decision = $3, approval_by = $4,
  approval_at = $5, held = NULL, next_launch_at = $5, updated_at = $5
WHERE id = $1 AND attempt = $2 AND status = 'queued' AND held = 'approval'
```

```rust
// api.rs
pub enum Route<'a> { /* ... */ Approve(u64), Reject(u64) }   // POST jobs/{job}/approve, jobs/{job}/reject
```

  The handler `Cx::decide(&self, job_id: u64, approved: bool)`:
  no session: `401` `"sign in to approve or reject jobs"`; the job and
  run as `Cx::job` reads them (`Access::Read`, else its stop); the
  tenant of the repository; `access::org_admin(settings, session, org_login)`
  must answer an org id whose tenant is the repository's, else `403`
  `"Only admins of {org_login} can approve or reject jobs"` (GitHub's
  rate limit: `503` as elsewhere). `decide_approval` false: `409`
  `"This job is not waiting for approval"`. Approve then sends
  `Woken { job_id, attempt }` with `launches::send_wakes`; reject calls
  `launches::end_job(env, db, &job, &format!("Rejected by {login}"))`.
  Either failure is logged and the answer stays `204`: the reconciler
  brings the job back (Ruling 8).
  `job_json` adds `"held"`, `"environment"` and `"approval"`:
  `{"decision": "approved" | "rejected", "by": login, "at": ms}` when
  `environments::decision(row)` is some, else `null`.

- [ ] **Step 1: Write the failing tests**

```rust
// store.rs
#[test] fn a_decision_applies_once() {
    // job queued, attempt 1, held 'approval':
    // decide_approval(job, 1, true, "octo", 70) -> true; row: held NULL,
    // approval_attempt 1, decision "approved", by "octo", at 70, next_launch_at 70.
    // Again -> false. A job held 'card' -> false. attempt 2 asked of an attempt 1 job -> false.
}
// api.rs
#[test] fn routes_parse() // adds jobs/5/approve -> Approve(5), jobs/5/reject -> Reject(5)
#[test] fn only_dispatch_and_cancel_post() // renamed posts_are_dispatch_cancel_and_decisions; Approve and Reject answer POST
#[test] fn job_json_shows_the_approval()
    // held "approval", environment "production", approval null;
    // after a decision on the current attempt: approval {decision: "approved", by: "octo", at: 70};
    // a decision on an older attempt: approval null
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hatchrun-ci store api`
Expected: FAIL.

- [ ] **Step 3: Implement** the SQL, routes and handler. The module
  doc table of `api.rs` lists both routes as needing an org admin and
  CSRF.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hatchrun-ci && cargo check -p hatchrun-ci --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ci/src
git commit -S -s -m "feat(ci): let organization admins approve or reject a job"
```

---

### Task 9 [standard]: App: repository access and environments

**Files:**
- Create: `web/app/src/pages/ci/RepoAccess.tsx`,
  `web/app/src/pages/ci/Environments.tsx`
- Modify: `web/app/src/api.ts`, `web/app/src/pages/ci/SecretsTable.tsx`,
  `web/app/src/pages/ci/RepoSettings.tsx`, `web/app/mock/ci-settings.ts`

**Interfaces:**
- Consumes: the Task 5 and Task 6 routes.
- Produces, in `api.ts`:

```ts
export type CiScope =
  | { org: string }
  | { owner: string; repo: string }
  | { owner: string; repo: string; environment: string };
export interface CiSecret { name: string; updatedAt: number; repositories: string[] | null }
export interface CiVar extends CiSecret { value: string }
/** Org scope sends `repositories` (null for all); other scopes never send it. */
export const putCiItem = (scope: CiScope, kind: CiItemKind, name: string, value: string,
  repositories?: string[] | null) => ...;
export const listCiOrgRepos = (org: string) => adminOnly(() => get<{ name: string }[]>(`${orgCiPath(org)}/repositories`));
export interface CiEnvironment { name: string; branches: string[]; approval: boolean; updatedAt: number }
export const listCiEnvironments = (owner: string, repo: string) => ...;          // GET .../environments
export const putCiEnvironment = (owner: string, repo: string, name: string,
  body: { branches: string[]; approval: boolean }) => ...;                       // PUT, answers CiEnvironment
export const deleteCiEnvironment = (owner: string, repo: string, name: string) => ...;
```

  The environment scope path is
  `${repoPath(owner, repo)}/environments/${encodeURIComponent(environment)}`.
- `RepoAccess({ org, value, onChange }: { org: string; value: string[] | null; onChange: (v: string[] | null) => void })`:
  two radios, "All repositories" (`null`) and "Selected repositories"
  (an array), and with the second a checkbox list of
  `listCiOrgRepos(org)`; an empty org list says "No repositories have
  run CI yet."
- `SecretsTable` with an org scope: a "Repositories" column ("All", or
  "{n} selected" with the names in `title`), `RepoAccess` in the form,
  prefilled by Replace/Edit, sent with the save. Other scopes show
  neither.
- `Environments({ owner, repo })` on `RepoSettings` after the
  repository's variables: a table (Name; Branches: "Any branch" or the
  patterns joined with ", "; Approval: "Required" or "Not required";
  actions Edit, Secrets, Delete), a form (name, read-only when editing;
  a textarea "One pattern per line. Leave empty to allow any branch.";
  a checkbox "Require approval from an organization admin"), Delete
  after `window.confirm(`Delete the environment ${name}? Its secrets and variables are deleted too, and jobs that use it fail.`)`,
  and Secrets opening two `SecretsTable`s for
  `{ owner, repo, environment: name }`. API errors show as their text.
  `RepoSettings` subtitle: "Secrets, variables and environments for
  {owner}/{repo}. Repository admins only."

- [ ] **Step 1: Add mock data** in `mock/ci-settings.ts`: org
  repositories `acme/api` and `acme/web`; org secret `NPM_TOKEN`
  selected for `["acme/api"]`; environments `production` (branches
  `["main"]`, approval true) and `staging` (none, false) on
  `acme/demo`, with one secret on `production`; the new routes in the
  Task 5 and 6 shapes, including the `422` for an unknown repository.

- [ ] **Step 2: Implement** the API functions and components.

- [ ] **Step 3: Check the app**

Run: `cd web/app && npm run typecheck && npm run lint && npm run build`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add web/app
git commit -S -s -m "feat(app): manage environments and organization secret access"
```

---

### Task 10 [cheap]: App: approve and reject on the run page

**Files:**
- Modify: `web/app/src/api.ts`, `web/app/src/pages/ci/RunPage.tsx`,
  `web/app/mock/ci.ts`

**Interfaces:**
- Consumes: the Task 8 routes and job JSON.
- Produces: `CiJob` gains
  `held: string | null; environment: string | null; approval: { decision: "approved" | "rejected"; by: string; at: number } | null;`
  and `approveCiJob = (id: number) => post<undefined>(`/api/ci/jobs/${id}/approve`)`,
  `rejectCiJob` likewise. `RunDetailView` takes `session` and
  `reload`; under a job whose `held === "approval"` it shows
  "Waiting for approval to use {environment}" and, for a signed-in
  viewer, "Approve" and "Reject" buttons (Reject after
  `window.confirm(`Reject ${job.name}? The job fails.`)`), then reloads;
  an error shows its text next to the buttons. A job with
  `approval?.decision === "approved"` shows "Approved by {by}".

- [ ] **Step 1: Add mock data** in `mock/ci.ts`: a run of workflow
  `deploy` with job `deploy` queued, `held: "approval"`,
  `environment: "production"`, and routes
  `POST /api/ci/jobs/:id/approve` (held null, approval approved by the
  mock user) and `/reject` (completed, failure, approval rejected).

- [ ] **Step 2: Implement** the fields, functions and view.

- [ ] **Step 3: Check the app**

Run: `cd web/app && npm run typecheck && npm run lint && npm run build`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add web/app
git commit -S -s -m "feat(app): approve or reject jobs waiting on an environment"
```

---

### Task 11 [standard]: End-to-end scenarios and the spec

**Files:**
- Modify: `crates/ci/scripts/e2e.sh`,
  `crates/ci/tests/e2e/repo/.hatchrun/config.yml`,
  `docs/superpowers/specs/2026-10-19-orchestrator-design.md`

**Interfaces:**
- Consumes: every earlier task.
- Produces: fixture jobs and workflows:

```yaml
  guarded:
    runs-on: ubuntu-latest
    environment: staging
    steps:
      - run: echo "never runs"
  deploy-a:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - name: show the values
        run: |
          printf '%s' "${{ secrets.ENV_SECRET }}" | openssl dgst -sha256 -r | cut -d' ' -f1
          echo "var is ${{ vars.ENV_VAR }}"
          echo "org only is [${{ secrets.ORG_ONLY }}]"
  deploy-b:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - run: echo "never runs"
# workflows:
  guard:  { on: { push: { branches: [guard] } },  jobs: [guarded] }
  deploy: { on: { push: { branches: [deploy] } }, jobs: [deploy-a, deploy-b] }
```

  Scenarios, after the `wide` one and before the billing one, using
  `app_call`:
  1. **Setup.** Insert `ci_repos` row 1002 `e2e/other` of `$TENANT`
     (installation 42) with `psql_run`.
     `PUT repos/e2e/demo/environments/staging` `{"branches":["release/*"],"approval":false}`
     and `.../environments/production` `{"branches":[],"approval":true}`
     answer `200`. Values: org secret `ENV_SECRET=org-<hex>`, repo
     secret `ENV_SECRET=repo-<hex>`, environment `production` secret
     `ENV_SECRET=env-<hex>` (random hex each run), org var
     `ENV_VAR=org`, repo var `ENV_VAR=repo`, environment var
     `ENV_VAR=env`, all `204`. `PUT orgs/e2e/secrets/ORG_ONLY` with
     `{"value":"only-<hex>","repositories":["e2e/nope"]}` answers `422`
     with an error containing `e2e/nope`; with `["e2e/other"]` it
     answers `204`, and `GET orgs/e2e/secrets` lists `ORG_ONLY` with
     `repositories == ["e2e/other"]`.
  2. **Branch refused.** A push to `guard` fails `guard / guarded`
     without a launch; the check run's summary or text contains
     ``Environment `staging` allows only branches release/*; this run is on refs/heads/guard.``
  3. **Approval held.** A push to `deploy`: both jobs reach `queued`
     with `held == "approval"` in the run API, and the `deploy / deploy-a`
     check run's summary contains
     ``Waiting for approval to use environment `production`.`` No
     instance launches for them while held (the launcher's launch count
     is unchanged after 10 s).
  4. **Approve and reject.** `POST jobs/<deploy-a>/approve` and
     `POST jobs/<deploy-b>/reject` answer `204`; a second approve of
     deploy-a answers `409`. `deploy / deploy-a` ends `success`;
     `deploy / deploy-b` ends `failure` with `Rejected by e2e` in its
     summary or text. The run API shows deploy-a's
     `approval.by == "e2e"` and `approval.decision == "approved"`.
  5. **Values.** deploy-a's R2 log holds a line equal to
     `printf '%s' "$env_secret" | openssl dgst -sha256 -r | cut -d' ' -f1`,
     `var is env` and `org only is []`.

  The script's header comment lists these scenarios. The launch count
  the script waits for becomes 14 (deploy-a) and the passing exits 11.
- The spec's "Secrets, the GitHub App and billing" section gains two
  bullets, dated 2026-10-24: organization secrets and vars reach all
  repositories or a selected list; repository environments with branch
  patterns, admin approval per job attempt, and their own secrets and
  vars, which win over repository and organization values.

- [ ] **Step 1: Add the fixture jobs and workflows** and the spec
  bullets.

- [ ] **Step 2: Add the scenarios** to `e2e.sh`.

- [ ] **Step 3: Run it**

Run: `E2E_PG_PORT=55433 bash crates/ci/scripts/e2e.sh`
Expected: exit 0, ending with `e2e passed`.

- [ ] **Step 4: Commit**

```bash
git add crates/ci/scripts/e2e.sh crates/ci/tests/e2e docs/superpowers/specs/2026-10-19-orchestrator-design.md
git commit -S -s -m "test(ci): cover environments and repository access in e2e"
```

---

### Task 12 [cheap]: Full checks

**Files:**
- Modify: whatever the checks below ask to fix, in the task that owns
  the code (amend nothing; fix forward with `fix(...)` commits).

- [ ] **Step 1: Format and lint**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p hatchrun-ci --target wasm32-unknown-unknown -- -D warnings`
Expected: no output from fmt, no warnings.

- [ ] **Step 2: Tests**

Run: `DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: The app**

Run: `cd web/app && npm ci && npm run typecheck && npm run lint && npm run build`
Expected: no errors.

- [ ] **Step 4: End to end**

Run: `E2E_PG_PORT=55433 bash crates/ci/scripts/e2e.sh`
Expected: `e2e passed`.

- [ ] **Step 5: Dashes and leftovers**

Run: `git diff origin/main -- . | grep -nP "^\+.*(\x{2014}|\x{2013})"; git grep -n "POLAR_CI_PRODUCT_ID\|product = 'ci'\|startCiCheckout\|openCiPortal" -- crates web docs/superpowers/specs`
Expected: both print nothing (the migration's `DELETE FROM subscriptions
WHERE product = 'ci'` is the one allowed match of the second).

- [ ] **Step 6: Commit** any fixes as
  `fix(<scope>): <what>` with `git commit -S -s`.
