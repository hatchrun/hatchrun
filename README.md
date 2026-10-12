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
2. The SSM parameter named by the `hatchrun:jit-parameter` instance tag.
3. The token parameter of
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

Runner images live in [hatchrun/images](https://github.com/hatchrun/images)
and the Terraform module in
[hatchrun/terraform-aws-hatchrun](https://github.com/hatchrun/terraform-aws-hatchrun).

Status: early development, not ready for production.

## Licence

Apache License, Version 2.0. See [LICENSE](LICENSE).
