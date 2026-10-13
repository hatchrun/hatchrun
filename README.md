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

```sh
cd crates/broker
npx wrangler@4.143.0 secret put BROKER_TOKEN
npx wrangler@4.143.0 deploy
```

## hatchrun.dev

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
cd crates/cloud
npx wrangler@4.143.0 d1 migrations apply hatchrun-cloud --local
echo GITHUB_WEBHOOK_SECRET=dev > .dev.vars
npx wrangler@4.143.0 dev
```

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

## Licence

Apache License, Version 2.0. See [LICENSE](LICENSE).
