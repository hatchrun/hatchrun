# hatchrun orchestrator: design

Status: draft for review, 2026-10-19.

## Intent

hatchrun.io today gives GitHub Actions self-hosted runners in the
customer's own AWS account. GitHub still decides when jobs run, hands
them to runners and shows the results. The customer pays AWS for the
instances and hatchrun $0.001 per job minute.

The orchestrator is a second product next to it. It does what GitHub
Actions does: it reads workflow files, decides which jobs run and in
what order, runs them on compute hatchrun owns, and reports the results
on the pull request. GitHub Actions is not involved. The customer pays
hatchrun one per-minute price that includes the compute.

Why: hatchrun needs a CI service it controls end to end, so it can run
work for more than GitHub Actions later. It has to be much cheaper than
GitHub's hosted runners to be worth switching to.

hatchrun.io keeps working as it does. The two products share the
agent, the runner images and the EC2 launch code.

## Goals

- A customer installs the hatchrun GitHub App, adds `.hatchrun/config.yml`,
  and its workflows run on hatchrun on push, pull request, manual
  dispatch and schedule. Results show up as check runs on the commit
  and pull request.
- Every GitHub action (`uses:`) runs as a step, so the Marketplace
  ecosystem carries over even though the file format is our own.
  `.github/workflows/` stays GitHub's, so nothing runs twice and a
  repository can move one workflow at a time.
- A converter (`hatchrun convert`) turns `.github/workflows/*.yml` into
  `.hatchrun/config.yml` and flags what it cannot map.
- Price target: $0.002 per minute for a 2 vCPU Linux job, about a third
  of GitHub's $0.006 (Linux 2-core since 2026-01-01) and half of
  Blacksmith and Depot (~$0.004).
- Idle costs close to nothing. The control plane runs on Cloudflare
  Workers and pays per request. Compute scales to zero.
- Jobs start in about 10 to 20 seconds when the warm pool has an
  instance, and within about 30 seconds otherwise.

## Non-goals (for now)

- Code hosts other than GitHub. The event and status pieces are kept
  per host so GitLab, Bitbucket or plain webhooks can be added later
  with the same middle, but none are built now.
- Docker beyond `run:` steps at launch. Each job owns a whole VM and
  the runner image ships rootful Docker with Buildx, so `docker build`
  and `docker run` inside a step work from day one. `services:`,
  `container:` and Docker container actions need the engine to start
  containers, wire their network and mount the workspace; they come in
  a later phase. Until then a workflow that uses them fails before it
  starts with a message that says so.
- Windows and macOS runners.
- Running in the customer's cloud account. That is hatchrun.io.

## Workflow format

Owner's decision: one format of our own, `.hatchrun/config.yml`, with
CircleCI's structure and GitHub's reusable actions as steps. Each part
comes from whichever system does it better.

From CircleCI:

- `executors`, `commands` and parameterised `jobs` defined once in the
  file and reused.
- A `workflows:` section, separate from `jobs:`, that wires jobs with
  `requires`, branch and tag filters, and approval jobs
  (`type: approval`).
- `parallelism: N` on a job with timing-based test splitting
  (the runner provides it through the agent, fed by earlier runs),
  which GitHub has no equivalent for. Per-minute pricing makes parallel VMs
  cheap.
- Built-in `cache`, `workspace` and `test-results` steps backed by our
  own storage.

From GitHub:

- `uses:` steps that run any GitHub action (JavaScript and composite at
  launch, container actions in the Docker phase) with `with:` inputs
  and step outputs. The runner provides the `github` context and the
  `GITHUB_*` variables and files actions expect.
- `on:` triggers, set per workflow, with GitHub's event and filter
  vocabulary (branches, paths, pull request types, schedule, dispatch
  inputs). CircleCI's trigger model is weaker.
- `${{ }}` expressions, evaluated at run time, and `matrix`. CircleCI's
  `<< >>` parameters are substituted when the file is parsed; mixing
  the two would confuse, so parameters use `${{ parameters.x }}`.

An executor names a runner template (`runs-on: ubuntu-26.04-4core`).
Jobs run on VMs; `docker:` executors come with the Docker phase.

```yaml
executors:
  rust: { runs-on: ubuntu-26.04-4core }

commands:
  setup:
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
      - cache: { key: "deps-${{ hashFiles('Cargo.lock') }}", paths: [target] }

jobs:
  test:
    executor: rust
    parallelism: 4
    steps:
      - setup
      - run: cargo nextest run $(hatchrun-agent split --by timings tests/*.rs)
      - test-results: target/nextest
  deploy:
    executor: rust
    steps: [setup, { run: ./deploy.sh }]

workflows:
  ci:
    on: { push: { branches: [main] }, pull_request: {} }
    jobs:
      - test
      - hold: { type: approval, requires: [test] }
      - deploy: { requires: [hold], branches: [main] }
```

The parser compiles the file into one internal plan (a job graph with
steps), which the control plane schedules and the agent runs. The
converter targets the same format; act's and Forgejo's test workflows,
run through it, test the converter, action execution and expressions
together.

## Anti-goals

- Spot capacity. Interrupted jobs are not acceptable, so every job runs
  on on-demand capacity. Cheapness comes from the backend choice and,
  later, from packing jobs onto hosts.
- A control plane that costs money while nobody pushes.
- A job that can reach another tenant's job, hatchrun's AWS
  credentials or the instance metadata of the host it runs on.

## Constraints

- Everything is Rust: the control plane (on Workers through
  `workers-rs`), the workflow engine and the agent.
- AGPL-3.0-only, like the rest of hatchrun. GPLv3 and MIT code can be
  taken in, so Forgejo Actions, nektos/act (both Go) and Ubicloud (Ruby,
  AGPL) serve as reference and their logic may be ported.
- GitHub is the only source of events and the only place results
  appear.
- On-demand compute only.

## Decided

### Flow

1. The GitHub App sends `push`, `pull_request` and
   `workflow_dispatch` webhooks to the control plane. A Cron Trigger
   produces `schedule` events from the `on.schedule` entries of each
   repository's default branch.
2. The control plane reads `.hatchrun/config.yml` at the event's
   commit through the installation token, keeps the workflows whose
   `on:` matches, and creates one workflow run per match.
3. Each workflow run expands matrices and `parallelism`, orders jobs by
   `requires`, evaluates `if:` and creates a check run per job.
4. A job that is ready goes on the launch queue. The launcher starts a
   runner instance for it, and the agent on that instance fetches the
   job, runs its steps and streams back step results.
5. The control plane updates the check runs as steps finish, starts
   jobs whose `requires` are met, and closes the workflow run.

The existing hatchrun.io Worker already covers the first half of this
for GitHub: webhook verification, the GitHub App, D1, the launch queue
and the minute reconciler. The orchestrator follows the same patterns.

### Control plane

- Cloudflare Workers in Rust, with one Durable Object per workflow
  run holding its job graph and state. A run's events arrive in
  order at one place, and nothing runs between pushes.
- D1 for what is listed and searched: tenants, repositories, runs,
  jobs, minutes.
- Queues for launches, as hatchrun.io does since #101.
- A reconciler on a Cron Trigger, as in hatchrun.io, catches lost
  webhooks, stalled launches and instances nothing needs.

### Storage

- **Relations in PlanetScale Postgres** (us-east-1, PS-10 arm64 HA)
  through a Hyperdrive binding and tokio-postgres (workers-rs ships an
  example of exactly this), shared with hatchrun.io once it moves off
  D1. The Worker connects as a role with only `pg_read_all_data` and
  `pg_write_all_data`; migrations run as the owner role from the deploy
  workflow. hatchrun.io has one active
  tenant, its own, so moving now costs no customer migration and
  replaces the D1 sharding plan (#104). Job SQL sits behind one store as
  #103 describes.
- **No step rows.** A job's steps (name, status, times) go in the job
  row as a small summary and in the log index in R2, so the database
  grows by about one row per job.
- **Live state and live logs in the run's Durable Object.** The agent
  sends log chunks to it about once a second, as Workers Builds does,
  with secret values already masked on the runner. Viewers in the app
  follow a running job over a WebSocket to the same object.
- **Finished logs in R2.** When a job ends, its log is written as one
  gzipped object, `logs/<tenant>/<repo>/<run>/<job>.log.gz`, with an
  index of where each step starts, and the Durable Object drops its
  copy.
- **Cache and artifacts in S3** in the runners' region, through a VPC
  gateway endpoint, so the bytes never leave AWS. Artifacts download
  through presigned URLs.
- **GitHub gets the summary.** Each job's check run carries its status,
  a summary, the tail of a failed step and a link to the full log in
  the app. `::error file=...` lines become check run annotations.
- **Who reads logs.** Anyone with read access to the repository, checked
  through GitHub sign-in. Logs of public repositories are public.
- **Retention** follows hatchrun.io (#99): 30 days by default, 90 for
  enterprise tenants, for rows, logs and artifacts. R2 and S3 lifecycle
  rules delete the objects.

### Workflow engine

- A shared crate parses `.hatchrun/config.yml` into the plan,
  evaluates `${{ }}` expressions and expands matrices. The control
  plane uses it to plan a run and the agent uses it to run steps, so
  both read a workflow the same way.
- The agent gets an executor that runs one job: `run:` steps, the
  built-in steps, and the JavaScript and composite actions that
  `uses:` names, with the `github`, `env`, `matrix`, `needs` (outputs
  of required jobs), `steps`, `secrets`, `parameters` and `runner`
  contexts. It sits next to the GitHub runner path in
  `hatchrun-agent`, which hatchrun.io keeps using.
- Actions are fetched from GitHub at the ref the workflow names.
- `hatchrun` is its own binary, separate from `hatchrun-agent`, built
  from its own crate on top of the shared format crate. Owner's
  decision. Customers run it locally (`hatchrun convert`,
  `hatchrun validate`). Releases publish it like the other binaries.
  Test splitting is not part of it; the runner provides that through
  the agent.
- act and Forgejo Actions are the reference for behaviour and their
  test workflows seed the compatibility suite.

### Compute

Phase 1, at launch: one fresh EC2 instance per job, on-demand, in AWS
accounts hatchrun owns. This reuses the hatchrun.io launcher: EC2 Fleet
over every fitting type of the newest generation, the capacity cache,
runner profiles, the broker and the Marketplace images. The warm pool
in `scaler-core` (instances booted once, then stopped) gives the fast
start. Its size follows recent demand and drops to zero when nothing
runs.

- Default types: Graviton (`c8g`/`c7g`, 2 real cores) for arm64, and
  `c8a` or `c7i` for amd64.
- Measured on hatchrun.io (smoke-test run 38433205230, 2026-10-13):
  workflow dispatched to job running took 26 s. The OS booted in 7 s; the GitHub runner listened at 11.8 s after
  kernel start and got its job at 15.6 s. Our own dispatch drops
  GitHub's runner registration and job assignment.

Phase 2: Oracle E6.Flex as a second backend behind the same `Cloud`
interface, once a spike shows it boots to a running job within about
30 s and a new tenancy bursts a few hundred instances without capacity
errors. E6 is a Turin chip at roughly half the EC2 price per job minute
(1 OCPU with 8 GB at ~$0.046/hr, third-party figure) with 10 TB a month
of free egress. It becomes the default amd64 backend if it passes, and
an overflow backend if not.

Phase 3: a microVM cluster for sub-second starts and higher margin,
when steady traffic can keep hosts busy. Hosts are on-demand EC2 with
nested virtualization (C7i/C8i/M8i, Intel only) or bare metal, and run
a microVM per job restored from a snapshot. Both host level and VM
level scale to zero. Self-hosted Ubicloud (Cloud Hypervisor) is the
starting point to evaluate before writing a host layer of our own.

Ruled out: Lambda and workerd (no shell, `sudo` or full Linux),
Lambda MicroVMs (~$0.0042/min plus snapshot fees per launch, arm64
only), DigitalOcean (slower CPUs, 5 to 25 droplet cap on new accounts),
hourly-billed clouds such as Hetzner Cloud for per-job VMs, and every
spot market.

### Cost at the target price (2 vCPU, on-demand)

| Backend | Cost per minute | Margin at $0.002 |
| --- | --- | --- |
| EC2 c7g.large + IPv4 | ~$0.0013 | ~1.5x |
| EC2 c7i.large + IPv4 | ~$0.0016 | ~1.25x |
| Oracle E6.Flex 1 OCPU, 8 GB | ~$0.00077 | ~2.6x |
| MicroVM hosts, 2x vCPU overcommit | ~$0.00075 | ~2.7x |

The margin on EC2 alone is thin. Phase 2 and Phase 3 are where it
widens.

## Calls made in writing this up

The owner did not decide these. They are the defaults a builder should
use unless the owner changes them.

- **Scope of the first plan.** Phase 1 only: EC2 on-demand plus the
  warm pool, with the launcher behind a backend interface shaped so a
  second cloud fits. The Oracle and microVM spikes are separate pieces
  of work. The Worker's launcher has no such interface today (the
  `Cloud` trait lives only in `crates/scaler`), and `profiles.rs`
  refuses `warm_pool`, so both get built here.
- **Accounts and region.** A new AWS account that only the orchestrator
  uses, separate from the accounts behind hatchrun.io, starting in one
  region (us-east-1, the cheapest list prices). Raising its on-demand
  vCPU quota to the concurrency we sell is a launch task.
- **Root and the network.** Jobs get passwordless `sudo`, as on
  GitHub's hosted runners and in the current images. Because root can
  undo anything inside the guest:
  - the instance has no IAM role;
  - the agent reads its broker ticket from user data at boot, and the
    control plane then turns instance metadata off with
    `ModifyInstanceMetadataOptions` before it hands over the job;
  - every token the agent holds is scoped to one job and expires with
    it.
  Outbound traffic is open except port 25. Each job is a fresh
  instance; there is no per-tenant network isolation in Phase 1.
- **Runner templates, not profiles.** Owner's decision: unlike
  hatchrun.io, customers cannot define runner profiles. `runs-on` picks
  from a fixed set of templates hatchrun publishes, named the way
  GitHub names its hosted runners:

  | `runs-on` | Arch | vCPU | Memory | Price |
  | --- | --- | --- | --- | --- |
  | `ubuntu-latest`, `ubuntu-26.04` | amd64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-arm` | arm64 | 2 | 8 GB | $0.002/min |
  | `ubuntu-26.04-4core` (and `-arm`) | either | 4 | 16 GB | $0.004/min |
  | `ubuntu-26.04-8core` (and `-arm`) | either | 8 | 32 GB | $0.008/min |
  | `ubuntu-26.04-16core` (and `-arm`) | either | 16 | 64 GB | $0.016/min |

  Each template maps to instance requirements in the same vocabulary as
  hatchrun.io's built-in profiles, so the launcher code is shared; only
  the customer-facing editing is absent. A label that names no template
  (such as `ubuntu-24.04`) fails the job before launch with a message
  listing the templates. The image is the existing hatchrun runner image
  (Ubuntu 26.04 with the hosted-runner layout and preinstalled
  toolchains). Gaps against GitHub's toolset come out of the
  compatibility suite.
- **Larger sizes.** Priced per vCPU from the 2 vCPU price, as in the
  template table. arm64 costs the same as amd64. Memory is 4 GB per
  vCPU, matching hatchrun.io's built-in profiles.

## Secrets, the GitHub App and billing

Decided by the owner on 2026-10-20.

- **Secrets.** Customers set organization and repository `secrets` and
  `vars` in the app. Values are stored in Postgres, encrypted with a
  key the Worker holds. That key lives in Cloudflare Secrets Store (one
  store per account, 100 secrets of at most 1 KB each, so it holds our
  keys, not customer values). The agent receives a job's secrets in its
  job document and masks them in every log line.
- **Pull requests from forks do not run.** A `pull_request` event whose
  head repository differs from the base repository is ignored.
- **One GitHub App.** The hatchrun.io App gets the extra permissions
  the orchestrator needs (contents read, checks write, pull requests
  read, and what `permissions:` in a workflow may ask for). Only
  hatchrun's own installation exists, so the re-approval costs
  nothing. Both products then share one installation token budget per
  organization: 5,000 REST requests an hour, more with many
  repositories or users, plus secondary limits of 900 points and 80
  content-creating requests a minute and 500 an hour. The orchestrator
  therefore creates one check run per job and updates it when a step
  ends, never per log line, and backs off on `403`/`429` with
  `retry-after`.
- **Billing and abuse.** A card is required before the first job.
  Each organization gets 500 free minutes a month, at most 5 jobs
  running at once to start, and a $50 monthly spend cap the customer
  can raise. Launches past a cap wait in the queue and the check run
  says why. The card is the hatchrun.io subscription: CI minutes are
  billed on the same Polar product as runner minutes, through a metered
  price on the meter that sums the `units` of `ci.usage` events, with
  500 free units a month as a meter credit. Decided 2026-10-24.

## Left to the builder

- Crate layout, Durable Object and D1 schemas, queue messages.
- The protocol between the agent and the control plane for fetching a
  job, streaming logs and step results, and heartbeats. The broker's
  one-time ticket is the model for how the agent gets its first
  document.
- How the warm pool size follows demand.
- Which parts of the format beyond those listed above make the
  first release, decided from the compatibility suite.
- How jobs reach the cache and artifact services that
  `actions/cache` and `actions/upload-artifact` expect. The hatchrun.io
  cache proxy on the runner is the starting point.
- **Organization secrets and vars** (2026-10-24). An organization's
  secrets and vars reach all of its repositories or a selected list of
  them.
- **Environments** (2026-10-24). A repository defines environments
  with branch patterns and, optionally, admin approval for each attempt
  of a job that names one. Environments hold their own secrets and
  vars, which win over repository values, which win over organization
  values.
