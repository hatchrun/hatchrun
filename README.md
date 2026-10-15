# hatchrun

Self-hosted GitHub Actions runners on your own AWS account, one fresh EC2
instance per job.

This repository holds the two Rust programs:

- **scaler**: long-polls GitHub's runner scale set API, launches an
  instance for every assigned job and cleans up the ones that finish or
  get stuck. Runs on AWS Fargate, Cloudflare Containers, Kubernetes or any
  machine with outbound internet access.
- **agent**: baked into the runner image. It starts early in boot, reads
  the instance's just-in-time runner config, runs exactly one job as the
  `runner` user and powers the instance off.

## Agent

`hatchrun-agent` is a static binary started by
[`packaging/systemd/hatchrun-agent.service`](packaging/systemd/hatchrun-agent.service).
It takes the instance's just-in-time runner config from the first source
that has one:

1. A `jitConfig` field in JSON user data, plain or gzip.
2. A `broker` URL and `ticket` in JSON user data. The agent claims its
   boot document from hatchrun-broker, which hands it out once, retrying
   while the broker has nothing stored yet. The broker must be HTTPS.
   The document holds the config and, on hatchrun.io launches, where to
   report metrics (see [Runner telemetry](#runner-telemetry)).
3. The SSM parameter named by the `hatchrun:jit-parameter` instance tag.
4. The token parameter of
   [github-aws-runners/terraform-aws-github-runner](https://github.com/github-aws-runners/terraform-aws-github-runner),
   found through its `ghr:ssm_config_path` tag. This lets the agent replace
   that module's user data script (`enable_userdata = false`).

Parameters are polled until they exist, then deleted. The launch template
must expose instance tags in metadata (`InstanceMetadataTags = enabled`).

The agent passes the config to the runner in `ACTIONS_RUNNER_INPUT_JITCONFIG`
rather than on the command line, so other processes cannot read it from
argv and job steps never see it. The runner runs as `runner` with all its
supplementary groups and the environment from `/etc/environment`. When the
runner exits, or no config arrives within 10 minutes, the unit powers the
instance off.

On hatchrun.io launches the agent samples the instance once a second
while the job runs and reports CPU, memory, disk and network in
ten-second windows to `telemetry.hatchrun.io`. Reporting never fails or
delays the job. Configs from the self-hosted scaler, user data or SSM
come without telemetry, and the agent reports nothing.

The agent starts only on instances launched as runners: user data with a
config or ticket, or one of the tags above. `hatchrun-launch.service`
checks that first, so an instance launched any other way, by hand or by
the AWS Marketplace scan, boots normally and stays up.

### Installing

Each [release](https://github.com/hatchrun/hatchrun/releases) has a
`.deb`, an `.rpm` and a tarball for `amd64` and `arm64`, a `SHA256SUMS`
file and a CycloneDX SBOM. Every file carries signed build provenance:

```sh
gh attestation verify hatchrun-agent_0.1.0_amd64.deb --repo hatchrun/hatchrun
sudo apt install ./hatchrun-agent_0.1.0_amd64.deb
```

The package installs `/usr/bin/hatchrun-agent` and the systemd unit but
leaves the unit disabled, because the agent powers the machine off when
it exits. Runner images enable it with `systemctl enable hatchrun-agent`.

## Scaler

`hatchrun-scaler` serves one scale set. On start it creates or updates
the scale set, opens a message session and long-polls it. For every
assigned job it registers a just-in-time runner and starts an instance
from your launch template. User data holds only the broker URL and a
single-use ticket; the scaler stores the runner config in the broker
once the instance exists, and the agent claims it at boot. Instances are tagged `hatchrun:scale-set-id`,
`hatchrun:runner-name` and `hatchrun:runner-id`. Every minute it lists
the tagged instances, which lets a restarted scaler pick up where the
last one stopped. On SIGTERM it closes the session and leaves running
jobs alone.

```sh
hatchrun-scaler \
  --github-url https://github.com/acme \
  --scale-set linux-x64 \
  --broker-url https://hatchrun-broker.acme.workers.dev \
  --launch-template lt-0123456789abcdef0 \
  --subnets subnet-aaa,subnet-bbb \
  --max-runners 20
```

Every flag also reads from a `HATCHRUN_*` environment variable, listed
in `--help`; pass the broker token as `HATCHRUN_BROKER_TOKEN`. Authenticate with a GitHub App (`--github-app-client-id`,
`--github-app-installation-id`, `--github-app-private-key`) with read
and write access to the organization's self-hosted runners (repository
administration for a repository scale set), or with a personal access
token (`--github-token`). AWS credentials come from the usual
SDK chain. The scaler needs `ec2:RunInstances`, `ec2:CreateTags`,
`ec2:DescribeInstances` and `ec2:TerminateInstances`, plus
`iam:PassRole` when the launch template sets an instance profile.

### Running the scaler

Each release has a static `hatchrun-scaler` tarball for `amd64` and
`arm64`, with checksums, an SBOM and build provenance like the agent,
and a multi-arch image built from the same binaries:

```sh
docker pull ghcr.io/hatchrun/hatchrun-scaler:0.1.0
gh attestation verify oci://ghcr.io/hatchrun/hatchrun-scaler:0.1.0 --repo hatchrun/hatchrun
docker run --rm --read-only \
  -e HATCHRUN_GITHUB_TOKEN -e HATCHRUN_BROKER_TOKEN \
  -e AWS_REGION -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  ghcr.io/hatchrun/hatchrun-scaler:0.1.0 \
  --github-url https://github.com/acme \
  --scale-set linux-x64 \
  --broker-url https://hatchrun-broker.acme.workers.dev \
  --launch-template lt-0123456789abcdef0 \
  --subnets subnet-aaa,subnet-bbb
```

The image is distroless with CA certificates and the binary and nothing
else. It runs as user 65532 and writes no files, so it works with a
read-only root filesystem, `runAsNonRoot` on Kubernetes and
`readonlyRootFilesystem` on Fargate. On Fargate or EKS give the task or
pod an IAM role instead of access keys.

The launch template sets the runner image, instance type and security
groups. The scaler sets the shutdown behaviour to terminate, so the
agent powering off ends the instance. Subnets are tried in order when
one is out of capacity.

### Warm pool

`--warm-pool N` keeps N instances ready for new runners. A pool instance
launches without user data, so the agent stays off, boots once and reads
the image's files off the snapshot, and is stopped after
`--warm-pool-warmup` (3 minutes). A new runner gets the oldest stopped
pool instance: the scaler sets its user data to the ticket, tags it as
the runner's and starts it, and launches a replacement. A runner from
the pool skips the launch and the slow first read of a fresh volume.
Pool instances older than `--warm-pool-max-age` (24 hours) are replaced,
so the pool picks up new images.

A stopped instance costs its volume and nothing else: a 10 GB gp3 root
volume is about $0.80 a month per pool instance. The pool needs an
on-demand launch template, because spot instances cannot be stopped
this way, and the scaler also needs `ec2:StopInstances`,
`ec2:StartInstances`, `ec2:ModifyInstanceAttribute` and
`ec2:DeleteTags`. Pool instances are tagged `hatchrun:pool`.

## Broker

[`crates/broker`](crates/broker) is a Cloudflare Worker that hands each
instance its runner config exactly once, so the config never sits in
user data. The scaler puts a random ticket in the instance's user data
and stores a boot document, `{"jitConfig": "..."}`, in the broker under
that ticket; hatchrun.io adds a `telemetry` field to it. At boot the
agent claims it with the ticket. The claim deletes the config, so the
ticket is useless once the runner has started, and an unclaimed config
is deleted when it expires (15 minutes by default). Every cloud has
user data, so this works the same on all of them.

Each ticket gets its own Durable Object, named by the ticket's SHA-256,
so two claims of one ticket cannot both succeed and nothing stored is
keyed by a live ticket. Storing needs the `BROKER_TOKEN` secret; a
claim needs only the ticket, a 256-bit random value.

Neither `wrangler.toml` names the Cloudflare account. The deploy
workflows pass it as the `CLOUDFLARE_ACCOUNT_ID` secret; run by hand,
wrangler reads it from `CLOUDFLARE_ACCOUNT_ID` or from the account the
login can reach, so export it when that login reaches more than one.

```sh
cd crates/broker
npx wrangler@4.143.0 secret put BROKER_TOKEN
npx wrangler@4.143.0 deploy
```

## hatchrun.io

[`crates/cloud`](crates/cloud) is the Cloudflare Worker behind the
hosted service. Instead of a scaler per scale set, it takes the
`workflow_job` webhooks of the hatchrun GitHub App and records every job
whose only `runs-on` label names a runner profile, in D1, once per job.
A profile holds requirements in Karpenter's terms: instance categories
(`c`, `m`, `r`, ...), the oldest generation, the architecture, vCPU and
memory bounds, a root volume, the capacity type (on-demand, Spot, or
Spot falling back to on-demand) and an image. Naming instance families
is an optional pin.

```yaml
runs-on: hatchrun-4cpu-arm64
```

Every organization has the built-in profiles `hatchrun-2cpu`,
`hatchrun-4cpu`, `hatchrun-8cpu` and `hatchrun-16cpu` (amd64), the same
with `-arm64` (Graviton), all general purpose (category `m`) with at
least 4 GiB per vCPU and on-demand, and `hatchrun`, which is
`hatchrun-2cpu`. Owners add their own profiles in the app, named
`hatchrun-` and anything, and a profile named like a built-in replaces
it for that organization.

One job can change its profile with overrides in the same label:

```yaml
runs-on: hatchrun-4cpu/disk=200/spot=true
```

The overrides are `cpu=N` (at least N vCPUs), `ram=N` (at least N GiB),
`disk=N` (GiB) and `spot=true` (Spot falling back to on-demand) or
`spot=false`. A job with an unknown
profile or option, or with other labels next to the hatchrun one, is
skipped, and the App's webhook delivery log says why. There is one
label on purpose: GitHub gives a job any runner whose labels include
all of the job's labels, so with label sets a job asking for less can
take the runner launched for a job asking for more and leave that job
waiting.

The instance types come from the profile and the region's instance
type catalog: of every family that fits, the smallest size the connect
stack allows, newest generation first and cheapest first within it.
GPU, accelerator, bare metal, burstable and flex types only come with a
pin. A launch hands a generation at a time to EC2 Fleet, which takes
the cheapest offering with capacity. The stack's `InstanceTypes`
(by default any size up to `8xlarge`), `ExcludedInstanceTypes` (by
default GPU, accelerator and bare metal families) and `VolumeSizeLimit`
stay the final word; when nothing they allow fits, the job fails and
the app says why. Until a region's catalog is first read, the families
[`scripts/ec2-catalog.py`](crates/cloud/scripts/ec2-catalog.py) wrote
from AWS's public price lists stand in for it.

```sh
npm --prefix web/app ci && npm --prefix web/app run build
cd crates/cloud
npx wrangler@4.145.0 d1 migrations apply hatchrun-cloud --local
echo GITHUB_WEBHOOK_SECRET=dev > .dev.vars
npx wrangler@4.145.0 dev
```

### app.hatchrun.io

The same Worker serves the app customers sign up with: the React app
in [`web/app`](web/app), built into static assets, and its API under
`/api`. Sign-in uses the hatchrun App's user authorization, GitHub's
OAuth web flow with a state and PKCE; the App lists
`https://app.hatchrun.io/api/auth/callback` as its callback URL and the
client secret is the `GITHUB_CLIENT_SECRET` Worker secret, stored in
the `production` environment as `APP_CLIENT_SECRET` (GitHub refuses
secret names that start with `GITHUB_`). A session is a
random id in an `HttpOnly` cookie; D1 keeps only its hash, and the
user's GitHub token sealed under a key derived from it.

A user token from the App sees only accounts the App is installed on,
so setup starts by installing it. The app links to GitHub's install
page with a state tied to the session, GitHub picks the organization,
and the App's setup URL, `https://app.hatchrun.io/api/github/setup`,
gets the installation id back. It makes the organization's tenant once
the App's JWT reads the installation, the user's token sees it in
`/user/installations`, the account is an organization and the user is
an active owner of it. Owners who already installed the App pick the
organization from the start page instead. Every request on a tenant
checks the user's ownership with GitHub again, which needs the App's
organization permission Members (read). The `installation` webhook
unlinks a tenant when the App is uninstalled or suspended and links it
again when the App comes back.

A tenant launches runners only while it holds a licence: a Polar
subscription to one of the `POLAR_PRODUCT_ID` products, or a place in
the invite-only beta. `POLAR_PRODUCT_ID` lists the products separated
by commas, one per billing interval, since a Polar product has a single
interval: monthly and yearly. The app opens Polar's checkout with all
of them, so the buyer picks one, and with the tenant id as the external
customer id, and Polar's webhooks, at
`https://app.hatchrun.io/api/polar/webhook` and checked against
`POLAR_WEBHOOK_SECRET`, keep the `subscriptions` table current. The
beta needs no payment. An owner redeems an invite code, or the operator
lists the organization:

```sh
code=$(openssl rand -hex 8)
hash=$(printf %s "$code" | shasum -a 256 | cut -d' ' -f1)
npx wrangler@4.145.0 d1 execute hatchrun-cloud --remote --command \
  "INSERT INTO invites (code_hash, note, uses_left, created_at) VALUES ('$hash', 'acme', 1, unixepoch() * 1000)"
echo "invite code: $code"

npx wrangler@4.145.0 d1 execute hatchrun-cloud --remote --command \
  "INSERT INTO beta_orgs (org_login, created_at) VALUES ('acme', unixepoch() * 1000)"
```

An owner connects AWS from the app. The account first subscribes to the
amd64 and arm64 runner images in AWS Marketplace (`MARKETPLACE_AMD64_URL`
and `MARKETPLACE_ARM64_URL` in `wrangler.toml`); EC2 refuses to launch
an image of a product the account has not subscribed to, with
`OptInRequired`, and the job says where to subscribe. A Launch Stack
link per region opens the published `connect.yaml` with the tenant id
filled in; the owner pastes the stack's `RoleArn` output back. The Worker assumes the
role with a token for the tenant, finds the stack's launch template by
its `hatchrun:tenant` tag and the subnets on its `hatchrun:subnets`
tag, checks they exist, and only then saves them.

Published stacks do not change when `connect.yaml` does. The template
tags its launch template with `hatchrun:template-version`; a change to
the template bumps that tag and `TEMPLATE_VERSION` in
[`connect.rs`](crates/cloud/src/connect.rs) together (a test fails
otherwise) and adds what the version brings to `CHANGES`. The
dashboard of a tenant on an older stack, or one still limited by the
old family list, says what an update brings and how to apply it in the
console or with the AWS CLI. The reconciler reads each tenant's launch
template again once a day, so an updated stack is picked up without
anyone verifying the role again, and jobs waiting on the setup launch.

[`publish-template.yml`](.github/workflows/publish-template.yml)
uploads `connect.yaml` on every change merged to main. It needs, once,
the bucket and role from
[`templates-bucket.yaml`](deploy/cloudformation/templates-bucket.yaml),
the `TEMPLATES_ROLE_ARN` secret and the `TEMPLATES_BUCKET` variable in
the `production` environment, and the
bucket's `TemplateUrl` output as `CONNECT_TEMPLATE_URL` in
`wrangler.toml`:

```sh
aws cloudformation deploy --region us-east-1 --stack-name hatchrun-templates \
  --template-file deploy/cloudformation/templates-bucket.yaml \
  --capabilities CAPABILITY_NAMED_IAM
```

Once every step is done the app shows the organization's recent jobs,
what each launch did, each job's CPU and memory (see
[Runner telemetry](#runner-telemetry)) and the `runs-on` syntax, its
runner profiles
with the instance type and on-demand price each gets in the tenant's
region, and its settings: the most runners at once and the regions
runners may use, which the Worker also checks before every launch.

### Runner images

Launches take the AMI for the tenant's region and the runner's
architecture from the `images` table. Every release of
[hatchrun/images](https://github.com/hatchrun/images) fills it: once
AWS Marketplace serves the AMIs, its `register.yml` job posts the
release's `manifest.json` to `POST https://app.hatchrun.io/api/internal/images`
with a GitHub Actions OIDC token for the audience
`https://app.hatchrun.io`. The Worker checks the token's signature
against GitHub's JWKS and accepts only that job, on the protected
`main` branch, in the `image-build` environment of hatchrun/images, on
a GitHub-hosted runner, started by a push, the schedule or by hand
([`images.rs`](crates/cloud/src/images.rs)). Each token posts once. It
writes every region and architecture in one batch, and refuses a
release older than the one it holds or whose version is far from the
token's issue time. `ACTIONS_OIDC_ISSUER` points the Worker at a local
issuer under `wrangler dev`, together with a local `APP_URL` such as
`--var APP_URL:http://localhost:8787`; on app.hatchrun.io it is
refused. To post the newest release again:

```sh
gh workflow run register.yml --repo hatchrun/images --ref main
```

### Reconciler

A Cron Trigger runs [`reconcile`](crates/cloud/src/reconcile) every
minute. It asks GitHub to redeliver the App's webhook deliveries that
failed (each event once), asks GitHub about open jobs that have had no
webhook for a while, and, per tenant with a verified role, describes
the instances tagged with the tenant: it terminates the ones whose job
is over, that never got a job, or that ran past the longest age, and
launches queued jobs again whose launch was lost or whose runner took
another job. A launch that failed on something that passes (an EC2 or Spot
capacity error, throttling, an AWS, GitHub or broker error, no image
yet for the region) is launched
again after 30 seconds, doubling up to 5 minutes, for as long as
GitHub keeps the job queued. A launch that failed on the
organization's setup (the role cannot be assumed or may not launch,
no allowed instance type fits the profile, no such profile) waits
until an owner verifies the role again, edits a profile, changes the
settings or gains a licence; [`retry.rs`](crates/cloud/src/reconcile/retry.rs)
lists every case. Once a day per region it refreshes the instance
type catalog in D1 through a tenant role there: every current
generation amd64 and arm64 type with the zones that offer it
(`DescribeInstanceTypes`, `DescribeInstanceTypeOfferings`), its Linux
on-demand price from AWS's public price list and its lowest Spot price
(`DescribeSpotPriceHistory`). It also deletes expired sessions and
jobs past the retention. The timeouts are the `RECONCILE_*` variables in
`wrangler.toml`. Each pass logs one summary line and keeps it in the
`reconciler` table:

```sh
npx wrangler@4.145.0 d1 execute hatchrun-cloud --remote --command \
  "SELECT finished_at, summary FROM reconciler"
```

`wrangler dev --test-scheduled` runs a pass on
`GET /__scheduled?cron=*+*+*+*+*`.

`npm run dev` in `web/app` serves the app with hot reload and sends
`/api` to `wrangler dev` on port 8787. `npm run dev:mock` answers `/api`
from [`web/app/mock/api.ts`](web/app/mock/api.ts) instead, with no
Worker, GitHub or AWS behind it.

### Runner telemetry

hatchrun.io shows each job's CPU and memory on its page in the app,
next to a link to the job on GitHub. Logs stay on GitHub; hatchrun
neither stores nor shows them.

The launch stores a boot document in the broker instead of the bare
config:

```json
{"jitConfig": "...", "telemetry": {"url": "https://telemetry.hatchrun.io", "token": "..."}}
```

The token is an EdDSA JWT naming the tenant, the installation, the job
and the runner, valid for nine hours. It is signed with
`TELEMETRY_SIGNING_KEY`, an Ed25519 key that signs nothing else; the
OIDC key buys credentials in customer accounts and never signs anything
a runner holds. The agent keeps the token in memory.

Every ten seconds the agent sums up its one-second samples into a
window: CPU busy share (lowest second, mean, highest second) and steal,
the highest memory in use and the instance's total, the peak memory of
the agent's cgroup and its OOM kills, disk used on the root file system,
and bytes received and sent. It posts the windows once a minute, at
most 60 at a time, and once more when the runner exits, with the
instance type, the architecture and its version. Nothing else leaves
the instance: no command lines, environment, file names or logs.

[`crates/telemetry`](crates/telemetry) is the Worker at
`telemetry.hatchrun.io` that takes the reports. It holds only the
Analytics Engine binding and the token public keys. It checks the
token's signature and expiry, takes the job and installation from the
token alone, refuses bodies over 64 KiB, unknown fields and numbers
out of range, and writes one data point per window to the
`hatchrun_runner_metrics` dataset: the job id as the index, the
installation id, instance type, architecture and agent version as
blobs, the window's numbers as doubles. Analytics Engine keeps data for
three months. The app reads it back through the Analytics SQL binding,
always for one installation, with a lower time bound and a limit.

Generate the key once and store both halves in the `production`
environment: the private key as a secret for hatchrun-cloud, the public
key, unpadded base64url, as a variable for hatchrun-telemetry.

```sh
openssl genpkey -algorithm ed25519 -out ~/telemetry-2026-10.pem
gh secret set TELEMETRY_SIGNING_KEY --env production < ~/telemetry-2026-10.pem
gh variable set TELEMETRY_PUBLIC_KEYS --env production --body "$(
  openssl pkey -in ~/telemetry-2026-10.pem -pubout -outform DER |
    tail -c 32 | base64 | tr '+/' '-_' | tr -d '=')"
```

To rotate, add the new public key to `TELEMETRY_PUBLIC_KEYS` after a
comma and deploy hatchrun-telemetry, then set the new private key and
deploy hatchrun-cloud. Nine hours later, when the last old token has
expired, drop the old public key. Under `wrangler dev`, put
`TELEMETRY_SIGNING_KEY` in `crates/cloud/.dev.vars` and
`TELEMETRY_PUBLIC_KEYS` in `crates/telemetry/.dev.vars`.

### OIDC issuer

The Worker reaches a customer's AWS account by signing a five-minute
token for `tenant:<id>` and trading it for the role that
[`connect.yaml`](deploy/cloudformation/connect.yaml) creates, through
`AssumeRoleWithWebIdentity`. STS checks the token against the keys the
Worker serves on `oidc.hatchrun.io`:

- `/.well-known/openid-configuration`, the discovery document;
- `/.well-known/jwks.json`, the public keys, matched by `kid`.

The signing key is an RSA private key, the `OIDC_SIGNING_KEY` secret of
the `production` environment, and `OIDC_KEY_ID` in
[`wrangler.toml`](crates/cloud/wrangler.toml) is its `kid`. The JWKS
derives the public key from the secret on every request, so there is
nothing to copy by hand and nothing to drift.

```sh
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out ~/oidc-2026-10-14.pem
gh secret set OIDC_SIGNING_KEY --env production < ~/oidc-2026-10-14.pem
```

Keep the PEM somewhere safe: Cloudflare and GitHub will not hand it
back, and rotating needs it once more. Both documents are served with
`Cache-Control: max-age=3600`, and STS keeps its own copy of the JWKS,
so a key is published well before it signs and stays published until
the tokens it signed have expired. Rotation takes three deploys, each a
change to `wrangler.toml` merged to main:

1. Publish the next key. Generate it, set it as `OIDC_NEXT_SIGNING_KEY`
   and add `OIDC_NEXT_KEY_ID = "2027-01"`. The JWKS now lists both
   keys. Wait a day, so every cached JWKS has the new key.
2. Sign with it. Set the new PEM as `OIDC_SIGNING_KEY` and the old one
   as `OIDC_PREVIOUS_SIGNING_KEY`, then set `OIDC_KEY_ID = "2027-01"`,
   `OIDC_PREVIOUS_KEY_ID = "2026-10-14"` and remove `OIDC_NEXT_KEY_ID`.
3. Retire the old key, at least five minutes later, when the last token
   it signed has expired: remove `OIDC_PREVIOUS_KEY_ID`.

A slot is published only while its `kid` is set, so the leftover
secrets do nothing. Delete them anyway, from GitHub with
`gh secret delete NAME --env production` and from the Worker with
`npx wrangler@4.145.0 secret delete NAME`.

## Scale set client

[`crates/scaleset`](crates/scaleset) talks to the GitHub Actions runner
scale set API, the same API Actions Runner Controller uses. It does no
I/O: every call returns an HTTP request to send and a parser for the
response. The scaler sends them with its own HTTP client, which is what
lets it run on tokio or inside a Cloudflare Worker. CI builds the crate
for `wasm32-unknown-unknown` to keep it that way.

## Scaler core

[`crates/scaler-core`](crates/scaler-core) decides what the scaler does.
It keeps one runner per assigned job, between `min_runners` and
`max_runners`, and tracks each runner from launch to termination. It
removes runners that wait for a job longer than the idle timeout (15
minutes by default), which covers instances that never started the
runner and jobs that went to another runner. Warm runners kept by
`min_runners` get recycled on the same timeout. On start, and whenever
the scaler compares its runners with the instances in the account, it
adopts instances it does not know and removes the registration of
runners whose instance is gone. It also keeps the warm pool at its size,
hands pool instances to new runners and adopts pool instances after a
restart by whether they are stopped. Like the client, it does no I/O
and builds for `wasm32-unknown-unknown`.

## Releases

Merging to `main` releases automatically when there are `feat`, `fix`,
`perf` or `revert` commits since the last tag. The version comes from
those Conventional Commits (see `cliff.toml`): `fix` bumps the patch
version, `feat` and breaking changes bump the minor version until 1.0.

Runner images live in [hatchrun/images](https://github.com/hatchrun/images)
and the Terraform module in
[hatchrun/terraform-aws-hatchrun](https://github.com/hatchrun/terraform-aws-hatchrun).

Status: early development, not ready for production.

## Before you commit

Run `pre-commit install` once after cloning. The hook runs gitleaks on
staged changes with the rules in `.gitleaks.toml` and stops the commit
when it finds a secret or a Cloudflare or AWS account ID. CI scans the
full history with the same rules. Pass account IDs through CI secrets
or variables; a finding that is not a secret goes in `.gitleaksignore`
by fingerprint, with a comment saying why.

## Licence

Apache License, Version 2.0. See [LICENSE](LICENSE).
