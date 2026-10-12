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
