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
2. A `broker` URL and `ticket` in JSON user data. The agent claims the
   config from hatchrun-broker, which hands it out once, retrying while
   the broker has nothing stored yet. The broker must be HTTPS.
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
and stores the config in the broker under that ticket. At boot the
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
whose only `runs-on` label is `hatchrun` or starts with `hatchrun/`, in
D1, once per job. Options in the label pick the runner:

```yaml
runs-on: hatchrun/arm64/cpu=8/disk=100/spot
```

The options are `amd64` or `arm64`, `cpu=N`, `ram=N` (GiB), `disk=N`
(GiB), `spot` and `type=INSTANCE_TYPE`. A job with an unknown option, or
with other labels next to the hatchrun one, is skipped, and the App's
webhook delivery log says why.

```sh
npm --prefix web/app ci && npm --prefix web/app run build
cd crates/cloud
npx wrangler@4.143.0 d1 migrations apply hatchrun-cloud --local
echo GITHUB_WEBHOOK_SECRET=dev > .dev.vars
npx wrangler@4.143.0 dev
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

An owner of a GitHub organization sets it up in the app, which makes
the organization's tenant, then installs the App with a link that
carries a state tied to the session and the tenant. GitHub sends the
browser to the App's setup URL, `https://app.hatchrun.io/api/github/setup`,
which links the installation once it belongs to that organization and
the user still owns it. Every request on a tenant checks the user's
ownership with GitHub again.

A tenant launches runners only while it holds a licence: a Polar
subscription to the `POLAR_PRODUCT_ID` product, or a place in the
invite-only beta. The app opens Polar's checkout with the tenant id as
the external customer id, and Polar's webhooks, at
`https://app.hatchrun.io/api/polar/webhook` and checked against
`POLAR_WEBHOOK_SECRET`, keep the `subscriptions` table current. The
beta needs no payment. An owner redeems an invite code, or the operator
lists the organization:

```sh
code=$(openssl rand -hex 8)
hash=$(printf %s "$code" | shasum -a 256 | cut -d' ' -f1)
npx wrangler@4.143.0 d1 execute hatchrun-cloud --remote --command \
  "INSERT INTO invites (code_hash, note, uses_left, created_at) VALUES ('$hash', 'acme', 1, unixepoch() * 1000)"
echo "invite code: $code"

npx wrangler@4.143.0 d1 execute hatchrun-cloud --remote --command \
  "INSERT INTO beta_orgs (org_login, created_at) VALUES ('acme', unixepoch() * 1000)"
```

An owner connects AWS from the app. A Launch Stack link per region
opens the published `connect.yaml` with the tenant id filled in; the
owner pastes the stack's `RoleArn` output back. The Worker assumes the
role with a token for the tenant, finds the stack's launch template by
its `hatchrun:tenant` tag and the subnets on its `hatchrun:subnets`
tag, checks they exist, and only then saves them.

[`publish-template.yml`](.github/workflows/publish-template.yml)
renders `connect.yaml` with the image owner account and uploads it on
every change merged to main. It needs, once, the bucket and role from
[`templates-bucket.yaml`](deploy/cloudformation/templates-bucket.yaml),
the `TEMPLATES_ROLE_ARN` and `IMAGE_OWNER_ACCOUNT_ID` secrets and the
`TEMPLATES_BUCKET` variable in the `production` environment, and the
bucket's `TemplateUrl` output as `CONNECT_TEMPLATE_URL` in
`wrangler.toml`:

```sh
aws cloudformation deploy --region us-east-1 --stack-name hatchrun-templates \
  --template-file deploy/cloudformation/templates-bucket.yaml \
  --capabilities CAPABILITY_NAMED_IAM
```

Once every step is done the app shows the organization's recent jobs,
what each launch did and the `runs-on` syntax, and its settings: the
most runners at once and the regions runners may use, which the
Worker also checks before every launch.

### Reconciler

A Cron Trigger runs [`reconcile`](crates/cloud/src/reconcile) every
minute. It asks GitHub to redeliver the App's webhook deliveries that
failed (each event once), asks GitHub about open jobs that have had no
webhook for a while, and, per tenant with a verified role, describes
the instances tagged with the tenant: it terminates the ones whose job
is over, that never got a job, or that ran past the longest age, and
launches queued jobs again whose launch was lost or whose runner took
another job. It also deletes expired sessions and jobs past the
retention. The timeouts are the `RECONCILE_*` variables in
`wrangler.toml`. Each pass logs one summary line and keeps it in the
`reconciler` table:

```sh
npx wrangler@4.143.0 d1 execute hatchrun-cloud --remote --command \
  "SELECT finished_at, summary FROM reconciler"
```

`wrangler dev --test-scheduled` runs a pass on
`GET /__scheduled?cron=*+*+*+*+*`.

`npm run dev` in `web/app` serves the app with hot reload and sends
`/api` to `wrangler dev` on port 8787. `npm run dev:mock` answers `/api`
from [`web/app/mock/api.ts`](web/app/mock/api.ts) instead, with no
Worker, GitHub or AWS behind it.

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
`npx wrangler@4.143.0 secret delete NAME`.

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
