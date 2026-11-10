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

1. **Warnings.** `hatchrun convert`, `hatchrun validate` and the
   planner warn on a `pull_request_target` workflow that gets the PR
   head onto the runner. The planner puts the warnings in the summary of
   the run's check runs and on the approval card. They never refuse a
   run. Rules below.
2. **Approval for forks.** A repository can make every
   `pull_request_target` run of a fork's pull request wait for a
   maintainer, through the approval card of the fork runs. Rules below.
3. **Cache scope.** GitHub scopes `pull_request_target` caches to the base
   branch, so a pwn request can poison `main`'s cache. hatchrun writes a
   `pull_request_target` run's caches to `refs/pull/<n>/target` and reads
   `refs/pull/<n>/target`, the base branch and the default branch. The run
   cannot write where a push to `main` restores from.
4. **OIDC subject.** `oidc::subject` already maps `pull_request_target` to
   `repo:<repo>:pull_request`, so a cloud trust on
   `repo:<repo>:ref:refs/heads/main` does not accept it. This stays. The
   token carries `event_name`, which trusts can pin.

**What the lints read.** The planner reads the plan of each
`pull_request_target` workflow (commands and called workflows are
expanded in it) and reports:

- a *head checkout*: a job is tainted when anything in it (its env, a
  step's env, `with:`, a script, its container) reads the PR head, which
  is a path of the `github` context at or above
  `github.event.pull_request.head`, `github.head_ref`,
  `github.event.pull_request.merge_commit_sha`, `GITHUB_HEAD_REF` or a
  `refs/pull/<n>/head|merge` ref. A computed index in a path matches any
  name, and reading a parent such as `github.event` counts. An `if:`, a
  `concurrency:` or a `name:` only compares or labels and does not count.
  A job that reads an output of a tainted job through `needs` is tainted.
  A tainted job that has a checkout, or a script that runs `git fetch`,
  `checkout`, `clone`, `pull`, `switch` or `worktree`, `gh pr checkout`
  or `gh repo clone`, is reported. So is a script that fetches code and
  finds the pull request at run time (`github.event.number`,
  `github.event.pull_request.number`, `GITHUB_EVENT_PATH`, `gh api` of
  `pulls`), names a `refs/pull/` refspec (split and wildcard ones too) or
  downloads a tarball, zipball or archive, and a checkout whose `ref` or
  `repository` is an expression that is not provably the base
  (`github.sha`, `github.base_ref`, `github.event.pull_request.base.*`, a
  literal);
- *script injection*: an untrusted field (see below) interpolated into a
  `run:` script or an `actions/github-script` script.

These are warnings, never refusals. A script can compute the head at run
time in ways no reading of the plan finds (a lookup through the API, a
value passed between steps), and a guard such as an `if:` on the head
repository can make a finding a false alarm. What holds is the approval.

**The setting `pull_request_target_head_checkout`** is `warn` (default)
or `approve`:

- `warn`: the `pull_request_target` run starts at once, with the
  findings in the summary of its check runs.
- `approve`: the `pull_request_target` run of a pull request **from a
  fork** waits for approval whoever the author is, through the card of
  "Approval flow" (the same check run, button, app card, write-access
  check, expiry and supersede on a new push). The card is on the pull
  request's head commit and says the run has the repository's secrets,
  lists the findings, and notes that the base branch's workflow decides
  whether the pull request's code runs. The run's own commit is the base
  branch's; the approval, the card and the supersede are by the head,
  stored on the run as `head_sha`. A pull request from the same
  repository never waits. A workflow that cannot be asked for approval
  (the pull request is closed, or the repository is over its caps) is
  not run unapproved: a neutral check run on the pull request's head,
  named `<workflow> / pull_request_target`, says it needs approval and
  why it did not run. A delivery older than what is recorded starts
  nothing. A newer commit ends the waiting runs of older ones before
  anything is counted, and the fork run and the `pull_request_target`
  run of one delivery share a group and count once against the caps. The
  caps of fork runs (waiting runs per repository, deferred runs) apply;
  the per-author cap and the setting `fork_runs` do not, because the run
  is the base repository's own workflow. A rerun follows the policy as it
  is now: under `approve`, a fork's run that was approved reruns, and one
  whose approval ended, or that never had one (made under `warn`), is
  refused with "This pull_request_target run was not approved; push a new
  commit to ask again."
- Moving from `approve` to `warn` lets more run, so it needs the admin's
  access as GitHub says now, like the other loosening changes.

The base branch's config is read by the branch name to see whether it
has a `pull_request_target` workflow; a config that is missing, does not
parse or has none starts nothing and says nothing (the push that made it
was told), and that is the whole cost of a repository without one. For
one that has, the branch's head commit is resolved and the config is read
again at that commit, so the plan and `GITHUB_SHA` agree; the policy is
read then.

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
| `pull_request_target_head_checkout` | `warn`, `approve` | `warn` |

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
  full, and evicts only entries saved by `fork` runs (each entry records
  the trust of the run that saved it), oldest first. Entries of
  `refs/pull/<n>/target` scopes come from trusted runs, and neither they
  nor base and default branch entries are evicted by a fork run.
- PR scopes are deleted when the PR closes, and by the existing 7 day idle rule.

Artifacts: keyed by run, as today. A fork run's artifacts are readable by
its own later jobs and by people who can read the repository through the
app. Each artifact row records the trust of its run.

A `workflow_run` or `pull_request_target` job often needs the artifacts
of another run (`actions/download-artifact` with `run-id` and
`github-token`). That action talks to GitHub's REST API, which has no
copy of a hatchrun run's artifacts, so the agent's ArtifactService proxy
serves the path:

- Run ids and artifact ids shown to workflows (`github.run_id`,
  `github.event.workflow_run.id`, artifact ids in API answers and paths)
  are `4_000_000_000_000_000 + <row id>`, each from its own table. That
  is below 2^53, so JavaScript actions read them exactly, and far above
  any id GitHub issues. The proxy decides origin by range alone and
  never guesses from a small number: an id in the range is a hatchrun id
  and is never sent to GitHub, an id below it is a GitHub id and is never
  answered locally. Run-id paths and artifact-id paths are routed
  separately, each by the range of its own id.
- The agent points `GITHUB_API_URL` at a local proxy for the job. For
  hatchrun run ids the proxy answers these itself, through the control
  plane with the job token: `GET /repos/{repo}/actions/runs/{id}` (id,
  name, status, conclusion, event, head sha and branch, head repository,
  attempt, times, app URL), `.../runs/{id}/jobs` (job id, name, status,
  conclusion, times, step summary, app URL) and `.../runs/{id}/artifacts`.
  For hatchrun artifact ids it answers `GET .../actions/artifacts/{id}`
  and `.../artifacts/{id}/zip`. Any other path under a hatchrun run id
  (logs, cancel, rerun, approve) answers `404` locally; logs are in the
  app. Paths without a hatchrun id go to GitHub unchanged.
- Limitation: a workflow or action that hard-codes `https://api.github.com`
  instead of reading `GITHUB_API_URL` sends the hatchrun id to GitHub,
  which answers `404`. Such workflows must use `GITHUB_API_URL`.
  `actions/github-script`, `gh` (`GH_HOST` and the API URL) and
  `actions/download-artifact` read it. `hatchrun validate` warns on a
  literal `api.github.com` in a `run:` or `with:` of a job that handles a
  run id.
- Authentication is the job token the agent holds, never the workflow's
  `github-token` value, which only proves repository access on GitHub.
  The control plane endpoints are `GET /v1/artifacts/runs/{run}` (list)
  and `GET /v1/artifacts/{id}/download`, which answers a presigned S3 URL
  that the proxy returns as the zip's `302`.
- Who may read which run: a trusted job reads the artifacts of any run of
  its own repository, fork runs included, because that is what
  `workflow_run` is for. A fork job reads only its own run. A job never
  reads another repository's run or another tenant's.
- A trusted job that downloads from a `fork` run gets a warning line in its
  log naming the run and its head repository, and the planner warns on
  such workflows (see "pull_request_target").

### Billing and abuse

The base repository's tenant pays for fork runs at its normal rate. The
spend cap and `billing::admission` apply, so a fork flood stops at the cap.

Limits specific to fork runs:

- `fork_max_running` jobs per repository at a time (default 2). Jobs over
  it wait in the queue. The tenant-wide `MAX_RUNNING_JOBS` still applies.
- At most 20 runs per repository waiting for approval. A delivery over
  that creates the run as `deferred`: the planner expands it, the check run
  `hatchrun / approval` says `Waiting for a free approval slot`, and no
  approval card is shown. The reconciler admits deferred runs, oldest
  first, when a slot frees, and then creates the card. A maintainer can
  admit and approve one at once with `/approve-run` or the app's button.
  At most 100 deferred runs per repository; a delivery over that is
  dropped and recorded, and `/approve-run` on the PR starts a run for its
  current head. Deferred runs expire after 7 days. The oldest pending run
  is never cancelled, so a spammer cannot push a real PR out.
- At most 3 active runs per author login per repository.
- `fork_timeout_minutes` caps the job timeout (default 30).
- Runner templates for fork runs are limited to 4 vCPU. Larger templates
  fail at planning with a clear message.

## issues and issue_comment

### Events and payloads

Both events arrive on the `issues` and `issue_comment` webhooks. A new
GitHub App subscription and read permission are needed (see "GitHub App").

`issues` actions that start workflows, as on GitHub: `opened`, `edited`,
`deleted`, `transferred`, `pinned`, `unpinned`, `closed`, `reopened`,
`assigned`, `unassigned`, `labeled`, `unlabeled`, `locked`, `unlocked`,
`milestoned`, `demilestoned`, `typed`, `untyped`, `field_added`,
`field_removed`. The default when `types` is absent is
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
commit and no check run; the run shows in the app. These check runs are
named `{workflow} / {job} (issue_comment)` or `(issues)`, and
`{workflow} / {job} (/<command>)` for a command run, never the `{workflow} / {job}` of the pull request's own
runs: anyone who can comment starts them, and on the same name they could
replace a required check with `success`.

The platform reacts to command comments itself, with the App's own
identity: `eyes` when the delivery is accepted, then `+1` on success or
`-1` on failure, or `rocket` and `confused` for `/deploy`-style commands
(the command's `reaction: rocket` option picks the set). It posts comment text only for the sha prompts of `on.command`. A refused commenter gets no reaction. Reactions use
`POST /repos/{repo}/issues/comments/{id}/reactions`, which needs
`Issues: write` for comments on issues and on pull requests alike;
`Pull requests: write` does not grant it. Workflow
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
          sha: auto                      # auto (default), required, optional
    jobs:
      - go
```

Each key under `command` is a command name: lowercase letters, digits,
hyphen and underscore, up to 32 characters. Options:

| Option | Meaning | Default |
| --- | --- | --- |
| `permission` | the GitHub repository permission the commenter must hold: `read`, `triage`, `write`, `maintain`, `admin` | `write` |
| `in` | where the comment may be: `pull_request`, `issue` | `[pull_request]` |
| `sha` | whether the comment must name the PR head sha: `required`, `optional`, or `auto`, which is required when the PR is from a fork | `auto` |
| `args` | names for the positional words, read as `github.event.command.named.<name>`; `args` holds all of them in order | none |

A workflow may also write `command: retest` or `command: [retest, deploy]`
for the defaults.

### Matching

- Only `created` comments match. Edits and deletes never run a command.
- A command is a line of the comment that starts, after optional leading
  whitespace, with `/<name>` followed by the end of the line or a space.
  The first matching line of each name wins. Lines inside fenced code
  blocks and block quotes (`>`) never match.
- One comment can carry several commands, at most 3.
- The word `--sha=<40 hex digits>` anywhere on the line is the sha pin and
  is not an argument. Only the full sha is accepted: a short value, a
  non-hex value or a second `--sha=` word refuses the command with a
  reply. Every other word, including ones that start with `@` or `-`,
  is an argument unchanged.
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
  "line": "/deploy staging --sha=1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
  "args": ["staging"],
  "named": {"environment": "staging"},
  "actor": "alice",
  "permission": "maintain",
  "pull_request": {
    "number": 12, "head_sha": "<full sha the comment named>",
    "head_repo": "alice/repo", "base_ref": "main", "fork": true
  }
}
```

`pull_request` is present when the comment is on a PR, filled from one
`GET /pulls/{n}` call.

The job checks out the PR with
`ref: ${{ github.event.command.pull_request.head_sha }}`. A comment's
timestamp does not bind it to the code the maintainer read, since a fork
author can push between the comment and intake, so the commenter names
the sha:

1. A command that needs a sha (`required`, or `auto` on a fork PR) and has
   no `--sha=` starts nothing. The platform replies with one comment that
   quotes the maintainer's own line: `Add --sha=<head> to: /deploy staging`
   (the head sha in full, the command and arguments exactly as typed).
2. A command with a sha is compared with the PR's current head, fetched
   once at intake, as full strings. A different value starts nothing and
   gets a reply naming the new head and quoting the line again. A match
   runs with `head_sha` set to that sha.
3. The job checks out exactly that sha, so a push after intake changes
   nothing. For a fork PR the sha is fetched from the base repository's
   `refs/pull/<n>/head`, and the run fails if that ref no longer holds it.

On a plain issue there is no PR head. `--sha=` is not allowed there and
refuses the command with the reason `Issues have no head commit`. A
command with `sha: required` and `in: [issue]` is a validation error, and
one that lists both places refuses on issues with the same reason.

The validator accepts the `head_sha` form without the head-checkout
warning when every command requires `write` or higher and has `sha` set
to `required` or `auto` (with `optional` the pin can be left out), and
warns on `github.head_ref` and `refs/pull/` forms.

A command on a fork PR from a maintainer is deliberately trusted. The
maintainer chose to run it with secrets. This is the `/ok-to-test` and
`/deploy` pattern that projects use today, now with the permission check
and a sha the maintainer named.

### Built-in /approve-run

`/approve-run --sha=<40 hex digits>` needs no workflow. It follows the
pin rule of commands, full sha only and one `--sha=` word, and it
quotes the maintainer's line in its reply. The sha is required and is
compared as a full string with the PR's current head at intake. A
missing or stale sha approves nothing. A push after the maintainer's
comment therefore cannot be approved by it. Only the run for exactly
that sha is touched. The PR must be open: on a closed or merged PR the
command refuses with `The pull request is closed`.

The commenter needs `write` or higher (same collaborator permission
call). The command runs the rules of the repository policy first: with
`fork_runs = off` (the default for private repositories) it leaves the
`pull_request` runs alone and replies `Fork runs are off for this
repository` when it did nothing else; `pull_request_target` runs that wait
under `approve` are still approved and made again.

Then, for that sha:

- a pending or deferred run is approved (a deferred one skips the queue);
- with a run already running or approved, the command reacts `+1` and does
  nothing else;
- with only terminal runs (cancelled, expired, failed), for example a PR
  reopened at the same sha after its delivery was dropped, the command
  creates a new run, not a rerun. Earlier approvals do not carry over.
  The new run starts gated and this comment is its approval, recorded as
  `gate_by` the commenter;
- with no run at all, it creates one the same way.

Each workflow that listens is decided on its own, for `pull_request`
(the head's config) and for `pull_request_target` (the base branch's), from
its runs at the head; a listed workflow with no run is made too, so a retry
after a partial failure makes the missing ones. With `fork_runs = off` the
`pull_request` runs are never approved or made. A workflow is made again as the first of
`synchronize`, `opened` and `reopened` that it listens to; one that
listens to none of them (a `labeled`-only workflow) is not made again,
and when nothing could be, the reply says so.

A new run still counts against the pending, deferred and per-author caps.
`/approve-run` is the only built-in command, and the name is reserved.

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

One migration, `crates/cloud/migrations/0022_ci_fork_runs.sql`. There is
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

Migration `0024_ci_chatops.sql` adds nullable columns to `ci_runs` (a run
that is not from an issue or a comment has none, so nothing is
backfilled): `issue_number` (the issue or pull request, for the cap),
`comment_id`, `command_name` and `command_reaction` (`default` or
`rocket`), the last three set together for a run a chat command started.

## Phases and pull requests

Fewer, larger PRs with a deploy order, as every merge to main releases.

**PR 1: trust model and fork PRs** (`feat(ci): run pull requests from forks`)

- Migration 0022, `trust.rs`, intake and webhook changes for forks, the
  policy table and settings API, the approval gate, check run action,
  approval API, reconciler expiry, fork caps, agent, OIDC, environment and
  cache enforcement.
- App: approval card, repository policy page, "Needs you" count.
**PR 2: pull_request_target and the lints**
(`feat(ci): run pull_request_target workflows`)

- `Event::PullRequestTarget`, base branch config read, `refs/pull/<n>/target`
  cache scope, head-checkout and script-injection lints (warnings) in the
  planner, `validate` and `convert`, the `approve` setting and its use of
  the approval card for a fork's `pull_request_target` runs. The
  converter stops listing `pull_request_target` as GitHub-only.
- No App change. Can ship right after PR 1.

**PR 3: issues, issue_comment and on.command**
(`feat(ci): run issues and issue_comment, add on.command`)

- Intake arms, `Event` variants, default-branch config, command parser and
  permission check, check runs on the PR head, loop filter, per-issue cap,
  lints, converter change, docs with the ChatOps recipes.
- Needs the App change first.

**Deploy order**

1. Merge PR 1. The deploy workflow runs migration 0022 as the owner role,
   then deploys the Worker and the app together. The migration keeps the
   defaults of its new `not null` columns (`trusted`, no gate, empty
   author), because the old Worker runs between the two steps and its
   scheduled runs, pull request rows and cache saves insert without them.
   Everything the old Worker makes is the repository's own code, so the
   defaults are the right values. PR 2 drops them. Fork PRs start running
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
8. A PR-head checkout in `pull_request_target` warns by default. A
   repository setting makes every `pull_request_target` run of a fork's
   pull request wait for approval instead. The lints never refuse a run:
   reading a plan cannot find every way to get the head onto the runner,
   so the approval is what holds, and the findings are for the approver.
