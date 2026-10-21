# Converter Compatibility, Secrets and Variables

Two tracks that can run in parallel:

- **Track A** closes the conversion gaps measured on
  `hatchrun/workflow-corpus` (1,409 GitHub workflow files from 115
  public repositories, 3,315 jobs).
- **Track B** adds secrets, variables and environments: storage, API,
  app UI, config, CLI and job delivery.

**Spec:** `docs/superpowers/specs/2026-10-19-orchestrator-design.md`.
This plan changes it in four places, listed under *Spec changes*.

**Scope:** git hosts only. GitHub first, Codeberg (Forgejo) second.
SVN is out. Full GitHub compatibility is not a goal; each item below
says what is left out.

## Spec changes

| Spec says | Becomes |
| --- | --- |
| Org and repo secrets and vars | Org, repo and **environment** secrets and vars |
| Agent receives "a job's secrets" | Agent receives only the secrets the job's expressions name |
| GitHub is the only host | GitHub first, Codeberg second; hosts stay behind one adapter boundary |
| (nothing on job tokens) | Each job gets a short-lived `GITHUB_TOKEN` and can ask for an OIDC token |

## Dependencies

| Needs | Ready now | Waits for |
| --- | --- | --- |
| Track A | All of it | - |
| B1-B5, B7 | Storage, API, UI, config, resolution, CLI | - |
| B6, B8 | - | Plan 2 (agent executor) and Plan 3 (control plane) |

B6 and B8 are written here so Plans 2 and 3 build against a fixed
contract. Their code lands with those plans.

---

## Track A: Converter compatibility

Ranked by jobs affected in the corpus.

| # | Change | Corpus impact | Size |
| --- | --- | --- | --- |
| A1 | Shared `if:` helper with GitHub's implicit `success()` | 904 job and 2,670 step conditions | S |
| A2 | Drop workflows left with no events | 298 files, 534 jobs | S |
| A3 | Map third-party runner labels | ~300 jobs | S |
| A4 | `requires` statuses, `if:` split, skipped state | 533 jobs downstream of a skippable job | M |
| A5 | Matrix `exclude`, `include` expansion, per-leg `runs-on` | 206 jobs | M |
| A6 | Inline local reusable workflows | 253 of 321 calls | M |
| A7 | `workflow_run` and `release` events | 36 files | S |
| A8 | Host-neutral context, built-in `checkout`, `.forgejo/workflows` input | Codeberg prep | M |

### A1. Shared `if:` helper

- Move `condition()` from `crates/cli/tests/e2e.rs` into
  `crates/workflow/src/expr/mod.rs` as `pub fn condition`.
  It unwraps a whole `${{ }}`, interpolates mixed text, applies
  `truthy`, then ANDs in `success()` when `checks_status` is false.
- Export from `lib.rs`; e2e calls it.
- Tests in `tests/expr.rs`: bare, wrapped, mixed text, `always()`,
  `failure()`, the literal `'success()'`.

### A2. Drop event-less workflows

- In `Cx::workflow` (`convert.rs`), when `Triggers` is empty after
  `on:` converts, skip the jobs and do not insert the workflow.
- One warning naming the events:
  `` `x.yml` was dropped: it runs on `issues`, `issue_comment`; hatchrun runs on ... ``
- `workflow_call`-only files get their own message until A6, then are
  skipped silently as templates.
- Move `rank.py` and `split.py` into `workflow-corpus/sweep-scripts/`
  so the ranking can be rerun.

### A3. Third-party runner labels

- Table in `templates.rs` mapping known vendor labels
  (`blacksmith-*`, `depot-*`, `namespace-profile-*`, `ubicloud-*`,
  `warp-*`, `buildjet-*`, `ubuntu-slim`) to the template with the same
  vCPU and arch, rounding up; warn on each mapping.
- `runs-on: {group: ...}` and `{labels: ...}` objects: read `labels`;
  `group` stays an error.
- Windows and macOS stay errors.

### A4. Job status in `requires`

- Config: a `requires` entry can carry statuses:
  `requires: [build, {test: [failed, not_run]}, {lint: terminal}]`.
  Statuses: `success` (default), `failed`, `canceled`, `not_run`,
  `terminal`. They are checked over **all ancestors**, as GitHub's
  `failure()` is, so A failing makes `failure()` true for a grandchild.
- `JobStatus` gains `Skipped` for step conditions that read
  `needs.x.result`; the `needs` context reports `skipped`.
- Converter splits a job `if:` on top-level `&&`:

  | GitHub | `requires` status | `if:` kept |
  | --- | --- | --- |
  | `success() && X` | default | `X` |
  | `always() && X` | `terminal` | `X` |
  | `!cancelled() && X` | `success, failed, not_run` | `X` |
  | `failure() && X` | `failed` | `X` |

- Validation rejects status functions in a job `if:`; steps keep them.
- Unsplittable conditions (14 of 321 in the corpus) drop the job with
  an error.

### A5. Matrix

- Config gains `matrix.exclude` (list of partial combinations).
- Converter expands each `include` entry into an extra workflow entry
  with `with:` values and rewrites downstream `requires` to name every
  expanded entry.
- `runs_on` may be `${{ parameters.x }}`; `compile` resolves it per leg
  and checks it names a template. Removes the "one runner template per
  job" error.
- Left out: `fromJSON` matrices, computed labels.

### A6. Local reusable workflows

- A job with `uses: ./.github/workflows/x.yml` inlines the called
  file's jobs as `<caller>-<job>` with `parameters` from its `inputs`
  and `with:` on the entries.
- `needs` on the caller become `requires` on the callee's root jobs;
  jobs needing the caller require the callee's leaf jobs.
- `outputs` map through to `needs.<caller>.outputs`.
- `secrets: inherit` passes through; named `secrets:` become job env.
- Nesting up to 4 levels, cycles are an error.
- Remote calls stay dropped with a warning naming the callee.

### A7. Events

- `workflow_run` becomes "after workflow X finishes", internal to
  hatchrun and the same on every host.
- `release` (published, created) on GitHub and Forgejo.
- `pull_request_target`, `issues`, `issue_comment` stay unsupported;
  the warning says to keep them on GitHub Actions.

### A8. Codeberg prep

- A `hatchrun` context (ref, sha, actor, event, repository) that each
  host adapter fills; the GitHub and Forgejo adapters also fill
  `github`.
- Built-in `checkout` step; converter rewrites `actions/checkout@*`
  to it with `fetch-depth`, `ref`, `submodules`, `lfs` carried over.
- `hatchrun convert` also reads `.forgejo/workflows/` and
  `.gitea/workflows/`.

---

## Track B: Secrets, variables and environments

### Model

| Concept | What it is |
| --- | --- |
| **Secret** | Named encrypted value. Write-only after it is set. Masked in logs. |
| **Variable** | Named plain value. Readable in the app and in logs. |
| **Scope** | Organization, repository or environment. |
| **Environment** | Named group of secrets and vars with branch rules, optional required approval and reviewers. Attached to a workflow entry. |
| **Precedence** | environment, then repository, then organization. |

Names match `[A-Za-z_][A-Za-z0-9_]*`, are case-insensitive, and may not
start with `GITHUB_` or `HATCHRUN_`. Limits follow GitHub: 48 KB per
value, 1,000 per organization, 100 per repository, 100 per environment.

### B1. Storage and encryption

Migration `crates/cloud/migrations/0004_secrets.sql`:

| Table | Columns |
| --- | --- |
| `environments` | `id`, `tenant_id`, `repo` (null = all repos), `name`, `branches` (json filter), `require_approval`, `reviewers` (json logins), `created_at`, `updated_at` |
| `secrets` | `id`, `tenant_id`, `scope` (`org`/`repo`/`env`), `repo`, `environment_id`, `name`, `ciphertext`, `nonce`, `key_id`, `updated_at`, `updated_by` |
| `variables` | same keys as `secrets`, with `value` instead of the crypto columns |
| `secret_events` | `tenant_id`, `actor`, `action` (`set`/`delete`), `kind` (`secret`/`variable`/`environment`), `scope`, `name`, `at` |

Unique on `(tenant_id, scope, repo, environment_id, lower(name))`.

Encryption (`crates/cloud/src/secrets.rs`):
- AES-256-GCM through WebCrypto (`webcrypto.rs`), 96-bit random nonce.
- Key from a Cloudflare Secrets Store binding, `SECRETS_KEY`, with a
  `key_id` column so keys can rotate: new writes use the current key,
  reads pick by `key_id`, a rotation job re-encrypts old rows.
- Additional data: `tenant_id|scope|repo|environment|name`, so a row
  copied to another scope or tenant fails to decrypt.
- Plaintext lives only inside the request that encrypts it and, later,
  the job document. Never logged, never returned by the API.

### B2. API

All routes under `/api/tenants/{org}`, owners only, CSRF-checked like
`settings`. Every write adds a `secret_events` row.

| Method | Path | Body / result |
| --- | --- | --- |
| GET | `/secrets?repo=&environment=` | names, scope, `updated_at`, `updated_by`; no values |
| PUT | `/secrets/{name}?repo=&environment=` | `{ value }` |
| DELETE | `/secrets/{name}?repo=&environment=` | - |
| GET | `/variables?repo=&environment=` | names and values |
| PUT / DELETE | `/variables/{name}?repo=&environment=` | `{ value }` |
| GET / POST | `/environments` | list / create |
| PUT / DELETE | `/environments/{name}?repo=` | branches, approval, reviewers |
| GET | `/repositories` | installation's repos, for the scope pickers |
| GET | `/secrets/events` | audit list, newest first, paged |

Routes added to the match in `crates/cloud/src/app/mod.rs`, handlers in
`crates/cloud/src/app/secrets.rs`.

### B3. App UI

New pages in `web/app/src/pages`, mocks in `web/app/mock`, entry in
the tenant settings navigation.

| Page | Contents |
| --- | --- |
| **Secrets and variables** | Tabs: Organization, Repository (repo picker), Environment (environment picker). Two tables per tab: secrets (name, updated, by) and variables (name, value, updated). "New secret" / "New variable" dialog. Secrets: "Update" replaces, never reveals. Delete asks to confirm with the name. Inherited entries from wider scopes shown greyed with their scope, so overrides are visible. |
| **Environments** | List with repo, branch rules, approval. Detail: branch patterns editor (same syntax as `branches:` filters), "Require approval" toggle, reviewers picker (tenant members), and its own secrets and variables tables. |
| **Activity** | Audit list from `secret_events`. |
| **Run page** | A job that waits for environment approval shows "Waiting for approval to deploy to `prod`" with Approve / Reject for reviewers (lands with Plan 3's approval jobs). |

Write-only values: the dialog sends the value once and clears the
field; the API never returns it, so the UI cannot show it.

### B4. Config and converter

- `WorkflowJob` gains `environment: Option<String>`; `ENTRY_KEYS` adds
  `environment`. Validation: the name is non-empty; whether it exists
  is checked by the control plane, since the config cannot see the
  app.
- An entry with an environment that requires approval gets an implicit
  approval step in front of it at plan time (reusing approval entries).
- Converter: job `environment: prod` or `{ name: prod, url: ... }`
  moves to the entry; `url` is dropped with a warning. Removes the
  `environment` warning (262 jobs).
- `hatchrun convert` ends with the names each workflow needs:
  `ci: secrets CODECOV_TOKEN, DOCKERHUB_TOKEN; vars REGISTRY`.
  `hatchrun validate --needs` prints the same for a native config.
- `expr::secrets_read(job) -> Needed` returns `Names(set)` or `All`
  (for `secrets[...]`, `toJSON(secrets)`, `secrets: inherit`), built on
  `checked_contexts`.

### B5. Resolution

Pure function in `crates/cloud/src/secrets.rs`, unit-tested now,
called by Plan 3:

`resolve(tenant, repo, ref, environment, needed) -> Result<Resolved, Refusal>`

1. If the entry names an environment: it must exist for this repo (or
   all repos), and `ref` must match its branch rules; otherwise the job
   fails with "`prod` does not allow branch `x`".
2. For each needed name: environment, then repo, then org. Missing
   names resolve to empty and are listed for a check run warning.
3. `GITHUB_TOKEN` is not stored; B6 mints it.

Pull requests from forks never reach resolution (spec). Dependabot
PRs get no secrets, as on GitHub.

### B6. Delivery to jobs (with Plans 2 and 3)

- The job document carries `secrets: {name: value}` for the resolved
  names only, plus `vars`.
- The agent masks every value (and its base64 and URL-encoded forms)
  in log lines before sending them, and honours `::add-mask::`.
- `GITHUB_TOKEN`: the control plane mints an installation token per
  job, limited to the job's repository and to the permissions from
  `permissions:` (default: contents read, metadata read), and revokes
  it when the job ends. Exposed as `secrets.GITHUB_TOKEN` and
  `github.token`. Codeberg gets a bot-user token when its adapter is
  built.
- Check run summary lists missing secret names.

### B7. CLI

- Tenant API tokens: `api_tokens` table (hashed, named, scoped to one
  tenant, expiry), created and revoked on a **Tokens** page in
  settings.
- `hatchrun login` stores a token in the OS config dir.
- `hatchrun secrets set NAME [--repo R] [--env E]` reads the value
  from stdin; `list`, `rm`. Same for `hatchrun vars`.
- `hatchrun secrets import --from-convert` reads the names `convert`
  printed and prompts for each value, for moving off GitHub.

### B8. Job OIDC tokens (with Plans 2 and 3)

- Reuse the `oidc.hatchrun.io` issuer (`crates/cloud/src/oidc.rs`) with
  its own key id.
- Subjects `repo:<owner>/<repo>:ref:<ref>` and
  `repo:<owner>/<repo>:environment:<name>`; never `tenant:`, so job
  tokens cannot assume a tenant's `ControlPlaneRole`.
- Requires `permissions: id-token: write`. The agent sets
  `ACTIONS_ID_TOKEN_REQUEST_URL` and `_TOKEN` so
  `aws-actions/configure-aws-credentials` and similar work unchanged.
- Docs page with AWS and GCP trust policy examples.

---

## Order of PRs

| PR | Contents | Depends on |
| --- | --- | --- |
| 1 | A1 helper, A2 event-less drop | - |
| 2 | B1 storage and encryption, B2 API | - |
| 3 | B3 UI: secrets and variables, activity | 2 |
| 4 | B4 config `environment`, converter, needed names; B5 resolution | 2 |
| 5 | B3 UI: environments | 2, 4 |
| 6 | A3 vendor labels | - |
| 7 | A4 `requires` statuses and `if:` split | 1 |
| 8 | A5 matrix | - |
| 9 | B7 API tokens and CLI | 2 |
| 10 | A6 local reusable workflows | 4, 7 |
| 11 | A7 events | 2 |
| 12 | A8 Codeberg prep | - |
| with Plan 2/3 | B6 delivery and `GITHUB_TOKEN`, B8 OIDC, approval UI on runs | 4, 5 |

## Open decisions

| Question | Default in this plan |
| --- | --- |
| Who can manage secrets | Tenant owners only. Repo admins later. |
| Environment reviewers | Tenant members, picked by GitHub login |
| Environments per repo or org-wide | Either: `repo` null means every repo |
| Same-repo PR branches get secrets | Yes, as on GitHub; forks never run |
