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

The launch template sets the runner image, instance type and security
groups. The scaler sets the shutdown behaviour to terminate, so the
agent powering off ends the instance. Subnets are tried in order when
one is out of capacity.

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
runners whose instance is gone. Like the client, it does no I/O and
builds for `wasm32-unknown-unknown`.

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
