# hatchrun

[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/16537/badge)](https://www.bestpractices.dev/projects/16537)

hatchrun is a CI: workflows run on a fresh EC2 instance per job, in your
own AWS account.

This repository holds:

- **hatchrun-ci** ([`crates/ci`](crates/ci)): the Cloudflare Worker that
  runs workflows. It takes the hatchrun GitHub App's webhooks, plans
  runs, launches an instance per job and keeps logs, artifacts, caches,
  secrets and billing usage.
- **hatchrun.io** ([`crates/cloud`](crates/cloud)): the Cloudflare
  Worker in front of it. It owns the webhook endpoint, sign-in with
  GitHub, the organization and billing API of app.hatchrun.io and the
  OIDC issuer.
- **agent** ([`crates/hatchrun-agent`](crates/hatchrun-agent)): baked
  into the instance image. It starts early in boot, claims its boot
  document, runs exactly one job and powers the instance off.
- **hatchrun** ([`crates/cli`](crates/cli)): command line tool
  developers run locally to convert and validate workflow files,
  released as its own tarball.

## Documentation

The [reference](docs/README.md) covers the interfaces:

- [command line](docs/cli.md): `hatchrun validate` and
  `hatchrun convert`;
- [workflow configuration](docs/config.md): `.hatchrun/config.yml`,
  its jobs, workflows, triggers, steps and expressions;
- [HTTP API](docs/http-api.md): the Worker routes, the app's JSON API
  and the `/api/ci/` run API.

## Agent

`hatchrun-agent` is a static binary started by
[`packaging/systemd/hatchrun-agent.service`](packaging/systemd/hatchrun-agent.service).
It reads `"mode": "ci"`, a `broker` URL and a `ticket` in the
instance's JSON user data and claims its boot document from hatchrun-broker, which hands it out
once, retrying while the broker has nothing stored yet. The broker must
be HTTPS. When the job ends, or no document arrives within 10 minutes,
the unit powers the instance off.

The agent starts only on instances launched for a job: user data with
`"mode": "ci"`. `hatchrun-launch.service` checks that first, so an instance
launched any other way, by hand, boots
normally and stays up.

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
it exits. Images enable it with `systemctl enable hatchrun-agent`.

## hatchrun

`hatchrun` is a command line tool developers run locally on workflow
files. It has two commands:

- `hatchrun validate` checks a workflow file and reports every error.
- `hatchrun convert` converts GitHub Actions workflow files into a
  hatchrun config. Jobs that cannot convert are left out and reported,
  and the command then exits 1.

Each release has a static tarball for `amd64` and `arm64`, named
`hatchrun-<version>-linux-<arch>.tar.gz`, with checksums and build
provenance like the agent.

## Broker

[`crates/broker`](crates/broker) is a Cloudflare Worker that hands each
instance its boot document exactly once, so the document never sits in
user data. `hatchrun-ci` puts a random ticket in the instance's user
data and stores the boot document in the broker under that ticket. At
boot the agent claims it with the ticket. The claim deletes the
document, so the ticket is useless once the job has started, and an
unclaimed document is deleted when it expires (15 minutes by default).

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

[`crates/cloud`](crates/cloud) is the Cloudflare Worker behind
hatchrun.io. `POST /github/webhook` takes the hatchrun GitHub App's
deliveries: push, pull_request, check_run, repository, release, create,
delete, merge_group and repository_dispatch go on to `hatchrun-ci` as they came; `installation` events
link a tenant to its installation or unlink it. Everything under
`/api/ci/` on app.hatchrun.io is forwarded to `hatchrun-ci` too.
`GET /healthz` answers 200 while the database does and `hatchrun-ci` is
healthy.

```sh
npm --prefix web/app ci && npm --prefix web/app run build
cd crates/cloud
docker run -d --name hatchrun-pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:17
DATABASE_MIGRATE_URL=postgres://postgres:postgres@localhost:5432/postgres scripts/migrate.sh
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
random id in an `HttpOnly` cookie; the database keeps only its hash, and the
user's GitHub token sealed under a key derived from it.

A user token from the App sees only accounts the App is installed on,
so setup starts by installing it. The app links to GitHub's install
page with a state tied to the session, GitHub picks the organization,
and the App's setup URL, `https://app.hatchrun.io/api/github/setup`,
gets the installation id back. It makes the organization's tenant once
the App's JWT reads the installation, the user's token sees it in
`/user/installations`, the account is an organization and the user is
an active owner of it. Owners who already installed the App pick the
organization from the start page instead.

`GET /api/tenants/<org>` answers for any member of the organization:
`{ id, org: { id, login, avatarUrl }, installationId, installUrl, role }`,
where `role` is `admin` for an owner and `member` for anyone else, from
the user's membership on GitHub, which needs the App's organization
permission Members (read). Checkout and the customer portal need an
owner. The `installation` webhook unlinks a tenant when the App is
uninstalled or suspended and links it again when the App comes back.

Billing goes through Polar. `POST /api/tenants/<org>/checkout` opens a
checkout for the card, `POST /api/tenants/<org>/portal` the customer
portal, and Polar's webhooks at `/api/polar/webhook` keep the
`subscriptions` table current. `hatchrun-ci` sends each job's usage to
Polar's "CI usage" meter. A job runs for a tenant in the beta or with an
active or trialing subscription.

### OIDC issuer

hatchrun CI reaches its AWS account by signing a five-minute token
for `tenant:hatchrun-ci` and trading it for the role that
[`ci-account.yaml`](deploy/cloudformation/ci-account.yaml) creates,
through `AssumeRoleWithWebIdentity`. STS checks the token against the
keys the Worker serves on `oidc.hatchrun.io`:

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

## CI alerts

`hatchrun-ci` emails `ALERT_EMAIL` through Cloudflare Email Service,
from `alerts@notify.hatchrun.io`, when its reconciler fails five passes
in a row, a launch message reaches `hatchrun-ci-launches-dlq`, GitHub
rate limits an installation or leaves it under 2,000 requests, or the
database passes 80% of its connections. Each alert sends one email when
it fires, is listed in one digest a day while it lasts, and sends one
when it resolves; an email that fails is sent on the next pass. The
state is in `ci_alerts`. A Worker cannot see its own cron stop, so
`GET /healthz` on `hatchrun-ci` answers 503 when the last pass is over
five minutes old (or the database does not answer). A failing pass
answers 200 with its status in the body; email alerts report those. It
answers only the service binding, and `app.hatchrun.io/healthz` passes
CI's state on for the uptime monitor.

## Scaler core

[`crates/scaler-core`](crates/scaler-core) is the warm pool of
`hatchrun-ci`: stopped instances that booted once and read their files,
kept ready so a job starts without the slow first boot. It decides when
to launch, stop and terminate pool instances, adopts pool instances after
a restart by whether they are stopped, and backs off after launch
failures. It does no I/O and builds for `wasm32-unknown-unknown`.

## Releases

Merging to `main` releases automatically when there are `feat`, `fix`,
`perf` or `revert` commits since the last tag. The version comes from
those Conventional Commits (see `cliff.toml`): `fix` bumps the patch
version, `feat` and breaking changes bump the minor version until 1.0.

Instance images live in [hatchrun/images](https://github.com/hatchrun/images).

Status: early development, not ready for production.

## Before you commit

Run `pre-commit install` once after cloning. The hook runs gitleaks on
staged changes with the rules in `.gitleaks.toml` and stops the commit
when it finds a secret or a Cloudflare or AWS account ID. CI scans the
full history with the same rules. Pass account IDs through CI secrets
or variables; a finding that is not a secret goes in `.gitleaksignore`
by fingerprint, with a comment saying why.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to set up, build, test
and send a change. Report security problems privately, as described in
[SECURITY.md](SECURITY.md), not in a public issue.

## Licence

GNU Affero General Public License, version 3 only (AGPL-3.0-only). See
[LICENSE](LICENSE). If you run a modified hatchrun as a network service,
the AGPL requires you to offer your users the source of your changes.
