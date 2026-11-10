# Security policy

hatchrun runs arbitrary jobs and holds the credentials those jobs use.
We would rather hear about a problem early than read about it later,
so reports are welcome even when you are not sure a bug is
exploitable.

## Reporting a vulnerability

Do not open a public issue. Report privately through GitHub's private
vulnerability reporting:

<https://github.com/hatchrun/hatchrun/security/advisories/new>

Include what you can:

- the component: `crates/ci`, `crates/cloud`, `crates/broker`,
  `crates/hatchrun-agent`, `crates/cli` or `web/app`;
- the version or commit you tested;
- what an attacker gains, and what they need to start;
- the steps to reproduce, with a proof of concept if you have one.

Test only against your own account, tenant and instances. Do not read,
change or delete other people's data, and do not run a denial of
service test against the hosted service.

## What to expect

- We acknowledge a report within 14 days, usually much sooner.
- We confirm or reject it, agree a severity with you, and keep you
  updated while we work on a fix.
- We aim to fix critical and high severity issues within 30 days, and
  sooner when an exploit is public.
- We publish a GitHub security advisory when a fix is available, and
  credit you unless you ask us not to.

## Scope

In scope: the code in this repository, the hosted service at
`hatchrun.io` and `app.hatchrun.io`, and the agent and CLI we publish.

Out of scope: vulnerabilities in GitHub, Cloudflare, AWS or another
service we depend on (report those to the provider), and findings that
need access to an account or tenant you do not own.

## Supported versions

Only the latest release is supported. hatchrun is pre-1.0 and does not
backport fixes to older versions; upgrade to the latest release.

## What we already do

CodeQL and `cargo audit` run on every pull request, Dependabot keeps
dependencies current, gitleaks scans the full history for secrets and
account IDs, and every release carries signed build provenance. These
reduce the chance of a known problem reaching you, but they are not a
substitute for a report.
