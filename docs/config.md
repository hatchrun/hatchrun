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

Filters are lists of glob patterns. `branches`/`tags` include, the
`-ignore` form excludes, and a pattern starting with `!` negates.

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
