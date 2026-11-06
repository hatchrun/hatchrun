# Fork pull requests, pull_request_target, issues and ChatOps

Status: draft for review, 2026-11-06.

## Intent

Two gaps keep hatchrun from running the workflows of a public project.

1. A pull request from a fork never runs. `intake::parse` returns
   `Ignored("pull request from a fork")` when the head repository is not
   the base repository. Most contributions to open source projects come
   from forks.
2. `pull_request_target`, `issues` and `issue_comment` are in the
   converter's `GITHUB_ONLY` list. A repository that labels pull
   requests, greets first-time contributors or reacts to `/retest`
   comments has to keep those workflows on GitHub Actions.

Owner decisions of 2026-11-06:

1. Fork PRs run on GitHub's model: no secrets, a read-only
   `GITHUB_TOKEN`, no OIDC token, and an approval for first-time
   contributors using GitHub's three levels.
2. hatchrun runs `pull_request_target`, `issues` and `issue_comment`
   itself. ChatOps (`/retest`, `/deploy`) is a goal.

This spec adds a trust level to every run, a repository policy, an
approval gate for fork runs, the three new events and a native
`on.command` trigger. It changes no workflow that runs today.

## Threat model

### What a fork run must never get

Each row names the enforcement point. The rule is enforced on the
control plane from the run row, so a modified agent or a modified
workflow cannot ask for more.

| Asset | Enforcement |
| --- | --- |
| Organization, repository and environment secrets | `open_values` returns empty maps for an untrusted run. The sealed rows are not read, so nothing is decrypted. |
| Write `GITHUB_TOKEN` | The token's permissions are cut to the fork ceiling (see "Token") before `cap_permissions`. The workflow's `permissions:` can only lower them. |
| OIDC identity tokens | The OIDC request document is never built (`agent.rs`, the `oidc::granted` branch) and `/v1/oidc/token` refuses a run whose trust is `fork`. Two checks, because the endpoint is reachable by any holder of a request token. |
| Cache writes into base scopes | The cache scope is `refs/pull/<n>/merge`, taken from `run.git_ref`. The cache service refuses a save when the scope of an untrusted run is anything else. |
| Artifacts visible to other PRs or other runs | Unchanged: artifacts are keyed by run id. A fork run's artifacts are marked `untrusted` in the run row so `workflow_run` consumers can see where they came from. |
| Environments and their secrets | `environments::gate` fails every job with `environment:` in an untrusted run. See the gap below. |
| State that survives the job | One VM per job, terminated at the end. A fork job never runs on an instance that ran another job. Warm pool instances are booted blank and used once. The runner has no persistent disk and no cache outside the cache service. |
| hatchrun's AWS credentials and instance metadata | Already an anti-goal for every job. Fork jobs get the same VM, so the same isolation applies. |

A gap in today's code: `branch_allowed` returns `true` for a
`pull_request` event when the environment has no branch patterns. An
environment without patterns would hand its secrets to a fork run. The
fork rule is a separate check that comes before the patterns.

Variables (`vars`) are not secrets by contract, and GitHub passes them to
fork runs. hatchrun does the same (decision 5).

### Residual exposure we accept

- A fork run restores caches saved on the base and default branches, as on
  GitHub, so a credential file cached by a trusted job leaks to it. The
  docs say so. Dropping the restore would make fork PRs slow enough that
  projects turn the feature off.
- Fork runs use hatchrun compute and can mine or scan. Approval, caps, a
  short timeout and the tenant's spend cap bound it (see "Billing and
  abuse").

### pull_request_target

`pull_request_target` runs the workflow from the base branch with the
base repository's secrets and a token that can write. It exists so
maintainers can label, comment on and triage fork PRs. The pwn request is
a workflow on this event that checks out the PR head and runs it:

(`actions/checkout` with `ref` set to the PR head sha, then `run: make test`) with the base secrets in the job.

hatchrun does what GitHub does: the run is trusted (secrets, write token,
OIDC if granted), the config is read from the base branch head, and the
workflow's own `if:` and `permissions:` rule. Differences from GitHub:

1. **Static warning.** `hatchrun convert`, `hatchrun validate` and the
   planner warn on a `pull_request_target` workflow that checks out
   the PR head. The planner puts the warning in the run summary and in
   the check run text. It does not block. Rule below.
2. **Cache scope.** GitHub scopes `pull_request_target` caches to the base
   branch, so a pwn request can poison `main`'s cache. hatchrun writes a
   `pull_request_target` run's caches to `refs/pull/<n>/target` and reads
   `refs/pull/<n>/target`, the base branch and the default branch. The run
   cannot write where a push to `main` restores from.
3. **OIDC subject.** `oidc::subject` already maps `pull_request_target` to
   `repo:<repo>:pull_request`, so a cloud trust on
   `repo:<repo>:ref:refs/heads/main` does not accept it. This stays. The
   token carries `event_name`, which trusts can pin.

The detector flags `actions/checkout` with a `ref` or `repository` input
containing `github.event.pull_request.head`, `github.head_ref` or
`refs/pull/`, and `run:` lines that call `gh pr checkout`. It is a
heuristic, so false negatives are possible. The repository setting
`pull_request_target_head_checkout` is `warn` (default) or `block`, which
fails the run at planning.

`workflow_run` has the same shape: it runs trusted from the default
branch and can download artifacts a fork run made. The planner applies
the same warning to a `workflow_run` workflow that downloads artifacts
and runs or sources them. `github.event.workflow_run.head_repository` is
filled so the workflow can tell a fork run from its own.

### Script injection

`issues`, `issue_comment` and `pull_request_target` carry attacker text
(titles, bodies, comments, branch names). `${{ github.event.comment.body }}`
inside `run:` lets the author run commands with the run's secrets. The
converter, `validate` and the planner flag such interpolations and point
to the fix: pass the value through `env:`. Warning only.

## Trust level

Every run gets one trust level at intake, stored on the run row:
`fork` for a `pull_request` whose head repository is not the base
repository, `trusted` for everything else (push, same-repo
`pull_request`, `pull_request_target`, `issues`, `issue_comment`,
commands, schedule, dispatch, release, create, delete, `workflow_run`).

The rest of the control plane reads `RunRow.trust` and never derives it
from a payload. A new `trust.rs` holds `Trust::of(&PullRequest)` and
`Trust::allows(Capability)`, which the secret, token, OIDC, cache and
environment code call. A new capability needs a new match arm, so the
compiler finds every caller.

A fork run may use every workflow feature minus those capabilities.
`${{ secrets.X }}` is the empty string, as on GitHub.

## Fork pull requests

### Intake

`intake::parse` stops returning `Ignored` for forks. It sets
`PullRequest.fork: Option<ForkInfo>` with the head repository id and full
name (`null` head repo, a deleted fork, counts as a fork with no name) and
`author_association`, read from `pull_request.author_association`.

The webhook turns one `pull_request` delivery into up to two groups of
runs:

- the `pull_request` workflows, read at the head sha as today,
  `trust = fork` for a fork, ref `refs/pull/<n>/merge`;
- the `pull_request_target` workflows, read at the base sha
  (`pull_request.base.sha`), `trust = trusted`, `GITHUB_REF` the base
  branch, `GITHUB_SHA` the base sha. These run from the base repository
  and need no fork contents.

The checkout of a fork run uses `refs/pull/<n>/merge` in the base
repository, which is where `actions/checkout` goes by default. The
installation token reaches it, so the job never needs the fork's
repository to be readable (a fork can be private).

Check runs go on the head sha in the base repository, as for same-repo PRs.

### Repository policy

A new table `ci_repo_policy` (one row per repository, created on first
write, otherwise defaults) holds:

| Column | Values | Default |
| --- | --- | --- |
| `fork_runs` | `off`, `on` | `on` for public repositories, `off` for private |
| `fork_approval` | `first_timers`, `first_time_contributors`, `all_outside` | `first_time_contributors` |
| `fork_max_running` | 1 to 10 | 2 |
| `fork_timeout_minutes` | 5 to 60 | 30 |
| `pull_request_target_head_checkout` | `warn`, `block` | `warn` |

The three approval levels use GitHub's `author_association` values:

| Level | Approval required when the author is |
| --- | --- |
| `first_timers` | `FIRST_TIMER` (new to GitHub) |
| `first_time_contributors` | `FIRST_TIMER` or `FIRST_TIME_CONTRIBUTOR` (first contribution to this repository) |
| `all_outside` | anyone but `OWNER`, `MEMBER` or `COLLABORATOR` |

`first_time_contributors` is the default, as on GitHub. A maintainer can
raise it to `all_outside` in one setting.

An author who does not need approval still gets a `fork` run with no
secrets. Approval and capabilities are independent: approval controls
whether the code runs at all, trust controls what it gets.

`fork_runs = off` makes intake ignore the fork PR with the reason
`fork runs are off for this repository`. Writing the policy needs repo admin through the user's token.

### Approval flow

When a fork PR delivery needs approval:

1. Intake creates the run as today, with `gate = 'fork_approval'` and
   `gate_status = 'pending'`. The planner expands the jobs but the
   scheduler launches none while the gate is pending. Jobs are `queued`
   with the reason `Waiting for approval`.
2. The control plane creates one check run on the head sha named
   `hatchrun / approval` with `conclusion = action_required`, a summary
   (below) and one action: `{label: "Approve and run", identifier:
   "approve-fork-run"}`. A separate "Reject" action is not offered, a
   maintainer who does not approve closes the PR or ignores it.
3. A user clicks the button. GitHub sends `check_run` with
   `action = requested_action`. The webhook handler reads
   `requested_action.identifier` and `sender.login`, then checks that the
   sender has `write`, `maintain` or `admin` on the base repository through
   the installation token (`GET /repos/{repo}/collaborators/{login}/permission`).
   On success the gate flips to `approved`, the approval check run
   completes with `conclusion = neutral` and text `Approved by <login>`,
   and the jobs enter the launch queue. On failure the handler ignores the
   click and logs it. GitHub shows the button only to users who can write,
   and the handler does not rely on that.
4. The app shows the same run on its runs list under "Needs you" (the
   existing `needs-you` count includes pending fork approvals) with
   `Approve and run` and `Reject` buttons. `POST
   /api/ci/runs/{run}/approve-fork` and `.../reject-fork` need a signed-in
   user with write access to the repository (not org admin, which the
   environment approval endpoint requires), plus CSRF as the other
   approve endpoint has.

Who can approve: anyone with write, maintain or admin on the base
repository. The approver's login and time go on the run.

Per attempt: an approval covers one run at one head sha. A new push
(`synchronize`) creates a new run that needs approval again, and
cancels the older pending run of the same PR with the message
`Superseded by a newer commit`. A rerun of an approved run keeps the
approval, since the code is the same and a rerun already needs write
access. An author who becomes `CONTRIBUTOR` after a merge stops needing
approval under the first two levels. There is no sticky approval per
author (decision 3).

What the approver sees (check run summary and the app page):

- PR number, title, author login, `author_association`, head repository
  full name and head sha, base branch;
- the files changed, and a line `The pull request changes
  .hatchrun/config.yml` with a link to the diff when it does. This is the
  line that matters most: the PR's own workflow is what will run;
- each job that would start: workflow, job name, runner template, timeout;
- the list of what the run does not get: secrets, write token, OIDC;
- the approval level that triggered the gate.

Timeouts:

- A pending approval expires after 7 days. The reconciler cancels the run
  (`Approval expired`).
- A closed or merged PR cancels its pending runs.
- An approved run has the normal job timeout, clamped to
  `fork_timeout_minutes`.

### Token

A fork run gets an installation token limited to the base repository and
the ceiling `contents: read`, `pull_requests: read`, `metadata: read`.
The workflow's `permissions:` can ask for less, never more:
`token_permissions` is intersected with the ceiling in `agent.rs` before
`cap_permissions`.

`checks`, `statuses`, `issues`, `actions`, `packages`, `id_token` and the
write levels are removed. The runner still reports results through the
control plane, so no step needs `checks: write`. A step that calls
`gh pr comment` fails, as on GitHub.

`pull_requests: read` is on the ceiling because `gh pr view` and
paths-filter actions need it. It stays (decision 6).

The agent receives it the same way as today: the control plane mints it
when the agent claims the job and puts it in the job document. The token
lives an hour and the control plane revokes it when the job ends
(`github::revoke_token`). The agent does not receive the App key or the
job token's signing key.

### Cache and artifacts

Cache: unchanged model, with the guard written down.

- Write scope: `refs/pull/<n>/merge`, from `run.git_ref`.
- Read scopes: that scope, `refs/heads/<base>`, the default branch.
  `cache::read_scopes` already does this.
- The service refuses a save whose scope is not the run's. For an
  untrusted run it also refuses a save when the repository's fork cache
  quota (a share of the tenant's cache quota, default 10 percent) is
  full, and evicts only entries of `refs/pull/*` scopes, oldest first.
  A fork PR cannot evict `main`'s caches.
- PR scopes are deleted when the PR closes, and by the existing 7 day idle rule.

Artifacts: keyed by run, as today. Fork run artifacts are readable by
the run's own later jobs and by people who can read the repository
through the app. A `workflow_run` workflow downloads them with the
repository's own token and gets the warning described above. Retention
is the tenant's.

### Billing and abuse

The base repository's tenant pays for fork runs at its normal rate. The
spend cap and `billing::admission` apply, so a fork flood stops at the cap.

Limits specific to fork runs:

- `fork_max_running` jobs per repository at a time (default 2). Jobs over
  it wait in the queue. The tenant-wide `MAX_RUNNING_JOBS` still applies.
- At most 20 runs per repository waiting for approval. A 21st delivery
  creates no run and records `Too many pull requests waiting for
  approval`. The oldest pending run is not cancelled, so a spammer cannot
  push a real PR out.
- At most 3 active runs per author login per repository.
- `fork_timeout_minutes` caps the job timeout (default 30).
- Runner templates for fork runs are limited to 4 vCPU. Larger templates
  fail at planning with a clear message.
- Runner templates for fork runs are limited to 4 vCPU.

## issues and issue_comment

### Events and payloads

Both events arrive on the `issues` and `issue_comment` webhooks. A new
GitHub App subscription and read permission are needed (see "GitHub App").

`issues` actions that start workflows, as on GitHub: `opened`, `edited`,
`deleted`, `transferred`, `pinned`, `unpinned`, `closed`, `reopened`,
`assigned`, `unassigned`, `labeled`, `unlabeled`, `locked`, `unlocked`,
`milestoned`, `demilestoned`. The default when `types` is absent is
every one of them.

`issue_comment` actions: `created`, `edited`, `deleted`. The default
when `types` is absent is all three. A PR conversation comment arrives as
`issue_comment`, with `issue.pull_request` set. A review comment on a
diff line is `pull_request_review_comment`, not covered.

`intake::parse` gets two arms. `Trigger::Issues` and `Trigger::IssueComment`
carry the installation id, repository, action, issue number, sender login,
the payload and, for comments, the comment body and `author_association`.
`"issues"` and `"issue_comment"` join the list of events `parse` reads.

The workflow crate's `Event` enum gets `Issues { action }`,
`IssueComment { action }` and `PullRequestTarget { action, base_ref,
changed }` next to `PullRequest`. Matching reuses the type filters of
`pull_request`, and `pull_request_target` reuses the branch and path
filters.

### Which config and ref run

As on GitHub, an `issues` or `issue_comment` run uses the workflow on the
default branch:

- config read at the head of the default branch, at the time of the
  event;
- `GITHUB_REF = refs/heads/<default>`, `GITHUB_SHA` the default branch
  head;
- `github.event` is GitHub's payload, so `github.event.issue.*`,
  `github.event.comment.*` and `github.event.issue.pull_request` work as
  documented;
- trust is `trusted`: secrets, environments, a token with the workflow's
  permissions, OIDC if granted.

The workflow file and its checkout are the base repository's. A comment
cannot change what runs. The comment can only choose whether it runs,
through `if:` or `on.command`. For a PR comment, the PR head is not
checked out unless the workflow does it.

The cache scope is `refs/heads/<default>`, like a scheduled run. The
head-checkout detector applies to all three events and `workflow_run`.

### Check runs and replies

GitHub attaches no check run to these events. hatchrun attaches them to
an `issue_comment` on a PR, on the PR head sha (one `GET /pulls/{n}` at
intake), so `/retest` shows next to the other checks. A plain issue has no
commit and no check run; the run shows in the app.

The platform reacts to command comments itself, with the App's own
identity: `eyes` when the delivery is accepted, then `+1` on success or
`-1` on failure, or `rocket` and `confused` for `/deploy`-style commands
(the command's `reaction: rocket` option picks the set). It posts no
comment text. A refused commenter gets no reaction. Reactions use
`POST /repos/{repo}/issues/comments/{id}/reactions`, which needs
`Issues: write` (for comments on a PR GitHub accepts `Issues: write` or
`Pull requests: write`, so `Issues: write` alone covers both). Workflow
steps can still reply with the job token and `permissions:`.

### Loop and noise control

- A webhook whose `sender` is the hatchrun App's own bot starts no
  workflow, as `GITHUB_TOKEN` events do not on GitHub. A step's comment
  through the job token cannot start another run.
- `on.command` ignores every sender with `type: "Bot"`.
- Deliveries are idempotent per `X-GitHub-Delivery`.
- At most 5 active runs per issue or PR from comment events.

### Writing ChatOps by hand

The portable way, which converted workflows keep:

```yaml
on:
  issue_comment:
    types: [created]
jobs:
  retest:
    if: >-
      github.event.issue.pull_request &&
      contains(github.event.comment.body, '/retest') &&
      contains(fromJSON('["OWNER","MEMBER","COLLABORATOR"]'),
               github.event.comment.author_association)
```

Two problems with it, which the docs state next to the example:

1. `contains` matches `/retest` anywhere, including a quote of an earlier
   comment and a code block. Anchor with `startsWith(body, '/retest')`.
2. `author_association` is not a permission level. `MEMBER` includes
   organization members with read access, and `COLLABORATOR` can be a
   read-only invitee. A command that deploys or spends money should not
   trust it.

`hatchrun validate` warns on an `issue_comment` job that has no `if:`
mentioning `author_association`, `actor` or a permission, and on a
`contains` match against `comment.body`.

## Native commands: on.command

Recommendation: yes. It fixes the three problems above with a real
permission check done before the run exists, and it gives ChatOps an
identity the app can show.

### Syntax

```yaml
workflows:
  retest:
    on:
      command:
        retest: {}                       # permission: write, in: [pull_request]
        deploy:
          permission: maintain
          in: [pull_request]
          args: [environment]            # optional, names the words after the command
    jobs:
      - go
```

Each key under `command` is a command name: lowercase letters, digits,
hyphen and underscore, up to 32 characters. Options:

| Option | Meaning | Default |
| --- | --- | --- |
| `permission` | the GitHub repository permission the commenter must hold: `read`, `triage`, `write`, `maintain`, `admin` | `write` |
| `in` | where the comment may be: `pull_request`, `issue` | `[pull_request]` |
| `args` | names for the positional words, for `github.event.command.args.<name>` | none |

A workflow may also write `command: retest` or `command: [retest, deploy]`
for the defaults.

### Matching

- Only `created` comments match. Edits and deletes never run a command.
- A command is a line of the comment that starts, after optional leading
  whitespace, with `/<name>` followed by the end of the line or a space.
  The first matching line of each name wins. Lines inside fenced code
  blocks and block quotes (`>`) never match.
- One comment can carry several commands, at most 3.
- Words after the name on the same line, split on whitespace, are the
  arguments. They are untrusted strings. At most 20 words, 200 characters
  each.

### Permission check

At intake, before a run is created, the control plane calls `GET
/repos/{repo}/collaborators/{login}/permission` with the installation
token and compares the commenter's role with `permission`. Anyone below
it starts nothing. The control plane records the refusal in the delivery
log and posts nothing, so strangers cannot probe which commands exist. The
App permission this call needs is part of the "GitHub App" section.

### Run context

A command run is a trusted run on the default branch, like any
`issue_comment` run, with these additions to `github.event`:

```json
"command": {
  "name": "deploy",
  "line": "/deploy staging",
  "args": ["staging"],
  "named": {"environment": "staging"},
  "actor": "alice",
  "permission": "maintain",
  "pull_request": {
    "number": 12, "head_sha": "<sha at comment time>",
    "head_repo": "alice/repo", "base_ref": "main", "fork": true
  }
}
```

`pull_request` is present when the comment is on a PR, filled from one
`GET /pulls/{n}` call.

The job checks out the PR with `ref: ${{ github.event.command.pull_request.head_sha }}`.
That is a sha, so a push between the comment and the checkout cannot swap
the code: the maintainer who typed `/deploy` approved what the PR looked
like at that moment. The validator accepts this form without the
head-checkout warning when the same workflow's command requires
`write` or higher, and warns on `github.head_ref` and `refs/pull/` forms.
For a fork PR the sha is fetched from the base repository's
`refs/pull/<n>/head`.

A command on a fork PR from a maintainer is deliberately trusted. The
maintainer chose to run it with secrets. This is the `/ok-to-test` and
`/deploy` pattern that projects use today, now with the permission check
and the sha pinned.

### Built-in /approve-run

`/approve-run` needs no workflow. On a fork PR comment from a user with
`write` or higher (same collaborator permission call), the control plane
approves the pending run for the PR's current head sha, exactly as the
check run button does, and reacts `+1`. A comment with no pending run gets
`confused`. It is the only built-in command, and the name is reserved.

## GitHub App permission and event changes

| Change | Why | Prompts owners |
| --- | --- | --- |
| Repository permission `Issues: write` (includes read) | `issues` webhooks, issue API reads, reactions on command comments | yes |
| Webhook subscriptions `issues`, `issue_comment` | the two events | no, these follow the permission |
| `pull_request_target` | uses the existing `pull_request` webhook and the existing `Pull requests: read` permission | no |
| Fork runs | use the existing webhook, `Contents: read`, `Checks: write` | no |
| `Metadata: read` for the collaborator permission call | already held by every App | no |
| `Checks: write` for `requested_action` and `action_required` | the App already creates check runs | no |

A new permission puts a request in front of every installation owner, and
GitHub sends no event for an installation until the owner accepts. Until
then those installations get no `issues` or `issue_comment` events and the
rest works. A repository that depends on one of the new events needs
nothing more than the owner accepting.

`pull_request_target` and fork PRs need no GitHub-side change, so Phase 1
ships without an owner prompt.

## Data model changes

One migration, `crates/cloud/migrations/0019_ci_fork_runs.sql`. There is
no compatibility path: new columns are `not null` and the migration
backfills existing rows (`trusted`, no gate).

- `ci_runs`: `trust`, `head_repo_id`, `head_repo_name`, `author_login`,
  `author_association`, `gate` (`none`, `fork_approval`), `gate_status`
  (`none`, `pending`, `approved`, `expired`, `cancelled`), `gate_by`,
  `gate_at`, `gate_check_run_id`.
- `ci_pulls`: `author_association`, `head_repo_id`.
- `ci_repo_policy`: new table, columns in "Repository policy".
- `ci_jobs`: no change. Environment approval stays per job attempt.
  Fork approval lives on the run so one decision releases every job.

## Phases and pull requests

Fewer, larger PRs with a deploy order, as every merge to main releases.

**PR 1: trust model and fork PRs** (`feat(ci): run pull requests from forks`)

- Migration 0019, `trust.rs`, intake and webhook changes for forks, the
  policy table and settings API, the approval gate, check run action,
  approval API, reconciler expiry, fork caps, agent, OIDC, environment and
  cache enforcement.
- App: approval card, repository policy page, "Needs you" count.
**PR 2: pull_request_target and the lints**
(`feat(ci): run pull_request_target workflows`)

- `Event::PullRequestTarget`, base-sha config read, `refs/pull/<n>/target`
  cache scope, head-checkout and script-injection lints in the planner,
  `validate` and `convert`. The converter stops listing
  `pull_request_target` as GitHub-only.
- No App change. Can ship right after PR 1.

**PR 3: issues, issue_comment and on.command**
(`feat(ci): run issues and issue_comment, add on.command`)

- Intake arms, `Event` variants, default-branch config, command parser and
  permission check, check runs on the PR head, loop filter, per-issue cap,
  lints, converter change, docs with the ChatOps recipes.
- Needs the App change first.

**Deploy order**

1. Merge PR 1. The deploy workflow runs migration 0019 as the owner role,
   then deploys the Worker and the app together. Fork PRs start running
   for public repositories at once, behind the default first-time
   contributor gate.
2. Merge PR 2. Deploy the CLI release with it so `validate` and the
   Worker agree.
3. Change the GitHub App in its settings: add `Issues: write` and subscribe
   to `issues` and `issue_comment`. Record it in the ops repo (the App's
   permission list is part of its inventory).
4. Merge PR 3. Installations that accept the new permission begin to send
   the events.

PR 3 can merge before step 3. It then sees no events and breaks nothing.

## Decisions

Owner answers of 2026-11-06, applied above.

1. Default approval level is `first_time_contributors`, as GitHub's.
2. Fork runs on private repositories are off by default, with a
   repository setting to enable them.
3. Approval is per head sha. Each push from an author who needs approval
   asks again. There is no sticky approval per author.
4. The platform reacts to command comments itself (`eyes`, then result
   reactions), so the App holds `Issues: write`.
5. Fork runs get `vars`.
6. The fork token keeps `pull_requests: read`.
7. A built-in `/approve-run` command exists (below).
8. A PR-head checkout in `pull_request_target` warns by default, with a
   repository setting to block.
