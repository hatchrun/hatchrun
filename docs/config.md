# Workflow configuration

hatchrun reads `.hatchrun/config.yml` from the root of the repository,
at the commit the event names. The file is parsed and planned by
`hatchrun-workflow`, which does no I/O, so the same rules apply in the
Cloudflare Worker, the runner agent and `hatchrun validate`.

```sh
hatchrun validate .hatchrun/config.yml
```

## Top-level keys

| Key         | What it holds                                              |
| ----------- | ---------------------------------------------------------- |
| `executors` | Named runner choices, shared by jobs.                       |
| `commands`  | Named, parameterised lists of steps.                        |
| `jobs`      | The jobs workflows run.                                     |
| `workflows` | Workflows, each with its own triggers.                      |

All four are maps and may be omitted. Unknown keys are errors.

```yaml
executors:
  rust: { runs-on: ubuntu-26.04-4core }

commands:
  setup:
    steps:
      - uses: actions/checkout@v5
      - cache: { key: "deps-${{ hashFiles('Cargo.lock') }}", paths: [target] }

jobs:
  test:
    executor: rust
    steps:
      - setup
      - run: cargo test --locked
      - test-results: target/nextest

workflows:
  ci:
    on: { push: { branches: [main] }, pull_request: {} }
    jobs:
      - test
```

## executors

An executor is a named runner template. A job names one with
`executor:`, or names the template directly with `runs-on:`.

```yaml
executors:
  rust:
    runs-on: ubuntu-26.04-4core
```

`docker` is not accepted here; put a container on the job with
`container:` instead.

## jobs

A job is a list of steps that run on one runner.

| Key                 | Meaning                                                       |
| ------------------- | ------------------------------------------------------------- |
| `executor`          | A key under `executors`.                                      |
| `runs-on`           | A runner template label, instead of `executor`.               |
| `parameters`        | Values the steps read as `${{ parameters.x }}`.               |
| `parallelism`       | How many copies run, splitting tests between them.            |
| `env`               | Environment variables for every step.                         |
| `if`                | The job runs only when this expression is true.               |
| `timeout-minutes`   | Cancel the job after this many minutes.                       |
| `continue-on-error` | `true`, `false` or an expression; a failure then counts as a success. |
| `outputs`           | Values later jobs read as `${{ needs.NAME.outputs.x }}`.      |
| `permissions`       | What the job's token may do; replaces the workflow's.         |
| `environment`       | The deployment environment, which may need approval.          |
| `environment-url`   | Where the deployment can be seen.                             |
| `concurrency`       | At most one job of a group runs in the repository.            |
| `container`         | Run the steps inside this image instead of on the runner.     |
| `services`          | Containers that run beside the job, by hostname.              |
| `steps`             | The steps, in order.                                          |

`continue-on-error` may read `hatchrun`, `github`, `needs`, `matrix`,
`strategy`, `vars` and `inputs`.

### Steps

A step is one of:

- `- run: <script>`: a shell command. Options are `name`, `id`,
  `shell`, `working-directory`, `env`, `if`, `continue-on-error` and
  `timeout-minutes`.
- `- uses: owner/repo@ref`: a GitHub action, with `with`, `env`, `id`,
  `if`, `continue-on-error` and `timeout-minutes`.
- `- checkout`: check out the event's commit. Options are `ref`,
  `fetch-depth` (default `1`; `0` fetches all history), `submodules`
  (`false`, `true` or `recursive`), `lfs` and `path`.
- `- cache: { key, restore-keys, paths }`: restore a cache now and save
  it when the job ends.
- `- workspace: { persist: { root, paths } }` or
  `- workspace: { attach: { at } }`: pass files to later jobs.
- `- test-results: <path>`: upload JUnit XML from a file or directory.
- `- <command>` or `- <command>: { ... }`: run a command from
  `commands:`, with parameter values.

A checkout always clones the repository the event came from; no other
repository can be checked out.

### Containers

`container` and each entry of `services` take GitHub's keys: `image`
(a bare string is the image), `credentials` (`username`, `password`),
`env`, `ports`, `volumes` and `options` (more `docker create` flags).
Strings may hold `${{ }}` over `github`, `needs`, `matrix`, `strategy`,
`vars`, `secrets` and `inputs`, read when the job starts.

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    container: node:24
    services:
      postgres:
        image: postgres:17
        env: { POSTGRES_PASSWORD: postgres }
        ports: ["5432:5432"]
    steps:
      - run: npm test
```

## commands

A command is a reusable step list with optional parameters.

```yaml
commands:
  setup:
    parameters:
      node:
        type: string
        default: "24"
    steps:
      - uses: actions/checkout@v5
      - run: echo "node ${{ parameters.node }}"
```

A step that names a command passes values as its body:
`- setup: { node: "22" }`, or `- setup` for the defaults.

## workflows

A workflow has triggers, optional settings and an ordered list of jobs.

| Key           | Meaning                                                        |
| ------------- | -------------------------------------------------------------- |
| `on`          | The events that start the workflow. Required.                  |
| `run-name`    | The title of each run, over `hatchrun`, `github`, `inputs` and `vars`. |
| `permissions` | What every job's token may do unless the job sets its own.     |
| `concurrency` | At most one run of a group runs in the repository, one waits.  |
| `jobs`        | The jobs, wired together with `requires`.                      |

Each `jobs` entry is a bare job name, or a map with one key, the entry
name:

```yaml
workflows:
  ci:
    on: { push: { branches: [main] }, pull_request: {} }
    jobs:
      - test
      - hold: { type: approval, requires: [test] }
      - deploy: { requires: [hold], branches: [main] }
```

An entry's keys are:

- `requires`: entries that must end first. A bare name asks for
  `success`; a map or list gives the accepted end states: `success`,
  `failed`, `not_run`, `canceled` or `terminal`. The states are judged
  over the required entry and its ancestry.
- `type: approval`: the entry waits for a person. It names no job.
- `branches` / `tags`: filters for the entry.
- `matrix` / `strategy`: run copies of the entry.
- `name`: a display name, needed when a job appears twice.
- `call`: run a workflow instead of a job, with `with`, `secrets`,
  `permissions` and `if`. The value is a workflow key of this config,
  or `owner/repo/path.yml@ref`.
- Any other key is a parameter value for the job.

`if`, `permissions` and `secrets` belong to a `call` entry; on an entry
that runs a job, set them on the job under `jobs:`.

## Triggers

`on` is a map of events. Each may be written as `{}` for its defaults.

| Event                 | Filters                                                        |
| --------------------- | -------------------------------------------------------------- |
| `push`                | `branches`, `branches-ignore`, `tags`, `tags-ignore`, `paths`, `paths-ignore`. |
| `pull_request`        | `types` (default `opened`, `synchronize`, `reopened`), `branches`, `branches-ignore`, `paths`, `paths-ignore`. |
| `schedule`            | A list of `{ cron: "<five fields, UTC>" }`.                    |
| `workflow_dispatch`   | `inputs`, a map of parameters.                                 |
| `workflow_run`        | `workflows`, `types` (`requested`, `in_progress`, `completed`), `branches`, `branches-ignore`. |
| `release`             | `types` (default `published`), `tags`, `tags-ignore`.          |
| `create` / `delete`   | No filters.                                                    |
| `call`                | `inputs`, `secrets`, `outputs`; started by another workflow.   |
| `merge_group`         | `types` (default `checks_requested`).                          |
| `repository_dispatch` | `types`, the `event_type` values.                              |
| `issues`              | `types` (default all, see below).                              |
| `issue_comment`       | `types` (default `created`, `edited`, `deleted`).              |
| `command`             | Chat commands, see [Chat commands](#chat-commands).            |

Filters are lists of glob patterns. `branches`/`tags` include, the
`-ignore` form excludes, and a pattern starting with `!` negates.

## Issues, comments and chat commands

`issues` and `issue_comment` run the workflow on the default branch with
the repository's secrets, as on GitHub. The workflow file and its checkout
are the base repository's: a comment cannot change what runs, it can only
choose whether it runs. `github.event` is GitHub's payload, so
`github.event.issue.*` and `github.event.comment.*` work as documented. A
comment on a pull request is an `issue_comment` with
`github.event.issue.pull_request` set; the pull request's head is not
checked out unless the workflow does it.

`issues` types are `opened`, `edited`, `deleted`, `transferred`, `pinned`,
`unpinned`, `closed`, `reopened`, `assigned`, `unassigned`, `labeled`,
`unlabeled`, `locked`, `unlocked`, `milestoned`, `demilestoned`, `typed`,
`untyped`, `field_added` and `field_removed`. `issue_comment` types are
`created`, `edited` and `deleted`. A type may be written as a plain
string. A review comment on a diff line is not covered.

Text in these events is written by whoever opened the issue or comment.
`hatchrun validate` and `hatchrun convert` warn when a script interpolates
`github.event.issue.title`, `github.event.issue.body`,
`github.event.comment.body` or `github.event.command.line`, `args` or
`named` with `${{ }}`. Pass the value through `env:` and read it as a
shell variable. The same warning covers `pull_request_target`.

### ChatOps by hand

The portable form works on GitHub Actions too, and conversion keeps it:

```yaml
workflows:
  retest:
    on:
      issue_comment:
        types: [created]
    jobs:
      - retest

jobs:
  retest:
    runs-on: ubuntu-latest
    if: >-
      github.event.issue.pull_request &&
      startsWith(github.event.comment.body, '/retest') &&
      contains(fromJSON('["OWNER","MEMBER","COLLABORATOR"]'),
               github.event.comment.author_association)
    steps:
      - run: ./retest.sh
```

It has two caveats:

1. `contains(github.event.comment.body, '/retest')` matches the command
   anywhere, including a quote of an earlier comment and a code block.
   Anchor it with `startsWith`.
2. `author_association` is not a permission level. `MEMBER` includes
   organization members with read access, and `COLLABORATOR` can be a
   read-only invitee. A command that deploys or spends money should not
   trust it.

`hatchrun validate` warns on an `issue_comment` job that has no `if:`
mentioning `author_association`, `actor` or a permission (a job that
needs a checked job is covered by it), and on a `contains` match against
`comment.body`.

### Chat commands

`on.command` fixes both caveats. The control plane finds the command in a
created comment, checks the commenter's repository permission before a
run exists, and replies in the comment only for the sha prompts below.

```yaml
workflows:
  retest:
    on:
      command:
        retest: {}                       # permission: write, in: [pull_request]
        deploy:
          permission: maintain
          args: [environment]            # names the words after the command
          sha: auto
    jobs:
      - go
```

`command: retest` and `command: [retest, deploy]` take the defaults.

| Option       | Meaning                                                           | Default          |
| ------------ | ----------------------------------------------------------------- | ---------------- |
| `permission` | Repository permission the commenter must hold: `read`, `triage`, `write`, `maintain`, `admin`. | `write`          |
| `in`         | Where the comment may be: `pull_request`, `issue`.                | `[pull_request]` |
| `sha`        | Whether the comment must name the head sha: `required`, `optional`, or `auto` (required when the pull request is from a fork). | `auto`           |
| `args`       | Names for the words after the command, read as `github.event.command.named.<name>`. | none             |
| `reaction`   | `default` leaves `+1` or `-1`; `rocket` leaves `rocket` or `confused`. | `default`        |

A command name is lowercase letters, digits, `-` and `_`, at most 32
characters. `approve-run` is reserved. `sha: required` needs `in` to
include `pull_request`, since an issue has no head commit.

A line of the comment is a command when it starts, after optional
whitespace, with `/<name>` and then ends or goes on after a space.
`/retestx` is not `/retest`. The first line of each name wins, lines in
fenced code blocks (``` or ~~~) and block quotes (`>`) never match, and at
most 3 commands are read from one comment. Edits and deletes never run a
command.

The words after the name are arguments, passed unchanged, at most 20
words of 200 characters each. They are untrusted text: read them through
`env:`. The word `--sha=<40 hex digits>` is the sha pin and is not an
argument. A short or non-hex value, or a second `--sha=`, refuses the
command with a reply.

The run's `github.event.command` has `name`, `line`, `args`, `named`,
`actor`, `permission` and, for a comment on a pull request,
`pull_request` with `number`, `head_sha`, `head_repo`, `base_ref` and
`fork`. A job that runs the pull request's code checks out the sha the
commenter named:

```yaml
jobs:
  go:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
        with:
          ref: ${{ github.event.command.pull_request.head_sha }}
      - run: ./deploy.sh "$ENVIRONMENT"
        env:
          ENVIRONMENT: ${{ github.event.command.named.environment }}
```

A command that needs a sha and has no `--sha=` starts nothing; the reply
quotes the line with the head sha to add, as in
`Add --sha=<head> to: /deploy staging`. A sha that is not the pull
request's current head starts nothing and the reply names the new head.
A match runs with `head_sha` set to that sha, and a push after that
changes nothing. On a plain issue `--sha=` is refused.

`hatchrun validate` accepts the `head_sha` checkout without a warning when
every command of the workflow needs `write` or higher, and warns on
`github.head_ref`, `refs/pull/` refs, `head_repo` and the head of a
command that needs less than `write`.

`/approve-run --sha=<40 hex digits>` is a built-in command that needs no
workflow. A commenter with `write` or higher approves the waiting fork run
for exactly that sha. It follows the same pin rule: the sha is required and
a stale one approves nothing.

## Parameters

A parameter, in `parameters:` or `workflow_dispatch.inputs:`, has a type
and optionally a default and a description.

```yaml
parameters:
  target:
    type: string
    default: staging
    description: Where to deploy
  retries:
    type: integer
    default: 3
  dry_run:
    type: boolean
    default: false
  region:
    type: enum
    enum: [eu-west-1, us-east-1]
```

The types are `string`, `boolean`, `integer` and `enum`; an `enum` needs
its `enum:` values, and only an `enum` may have them.

A called workflow declares its inputs under `on.call.inputs` with
`type: string | boolean | number`, plus `default`, `required` and
`description`; its secrets under `on.call.secrets` with `required` and
`description`.

## Matrices

A matrix runs one copy of the entry per combination of values.

```yaml
workflows:
  ci:
    on: { push: { branches: [main] } }
    jobs:
      - test:
          matrix:
            os: [ubuntu-26.04-4core, ubuntu-26.04-arm]
            rust: [stable, beta]
            exclude:
              - { os: ubuntu-26.04-arm, rust: beta }
            include:
              - { os: ubuntu-26.04-4core, rust: nightly, experimental: true }
```

`strategy` controls the copies: `fail-fast` (default `true`) cancels the
others when one fails, `max-parallel` caps how many run at once, and
`group` names the matrix when several entries count as one.

A matrix part may be an expression read when the entry becomes ready,
after the jobs it requires have ended. Such a matrix is dynamic: the
whole `matrix`, a key's list, or `include` may be a `${{ }}` expression.

## Expressions

`${{ }}` expressions appear in `if`, `run`, `with`, `env`, `name`,
`concurrency`, `environment`, matrices and outputs. The contexts are:

| Context      | What it reads                                                  |
| ------------ | -------------------------------------------------------------- |
| `hatchrun`   | hatchrun's own values, such as the run and job ids.            |
| `github`     | The event, repository, ref, actor and similar.                 |
| `env`        | Environment variables.                                         |
| `matrix`     | The current matrix combination.                                |
| `needs`      | The outputs and results of required entries.                   |
| `steps`      | The outputs of earlier steps with an `id`.                     |
| `secrets`    | Secrets, never printed.                                        |
| `parameters` | The job's or command's parameter values.                       |
| `strategy`   | `fail-fast`, `job-index`, `job-total` and `max-parallel`.      |
| `job`        | The job's own status and container.                            |
| `vars`       | Repository and organization variables.                         |
| `inputs`     | The inputs of a manual or called workflow.                     |

Functions include `contains`, `startsWith`, `endsWith`, `format`,
`join`, `toJSON`, `fromJSON`, `hashFiles` and the status functions
`success`, `failure`, `cancelled` and `always`.

## Permissions

`permissions` is `read-all`, `write-all`, or a map of scopes to `none`,
`read` or `write`. The scopes are `actions`, `attestations`, `checks`,
`contents`, `deployments`, `discussions`, `issues`, `packages`, `pages`,
`pull-requests`, `repository-projects`, `security-events`, `statuses`
and `id-token`. `id-token: write` lets the job ask for an OIDC token;
the others map to GitHub App permissions.

## Concurrency

`concurrency` is a group string or a map:

```yaml
concurrency:
  group: deploy-${{ github.ref }}
  cancel-in-progress: true
```

Within one repository, runs (or jobs) that name the same group do not
run together: at most one runs and one waits. A newer arrival takes the
place of the one that waits. With `cancel-in-progress`, it also cancels
the one that runs; the value may be an expression.

## Validation

`hatchrun validate` reports every problem it finds, not only the first.
It checks the YAML shape, unknown keys, references to missing jobs,
commands and executors, parameter types, expression syntax and the
contexts each expression may read. A config that validates is not a
promise that a job will succeed, but it is a promise that the file
means what it says.
