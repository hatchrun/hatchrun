# Contributing

Thanks for helping. hatchrun is early software and moving quickly, so
a short issue before a large change saves us both time.

## Before you start

- Open an issue for anything larger than a typo or a small fix, so we
  can agree on the approach before you write it.
- Report security problems privately, as described in
  [SECURITY.md](SECURITY.md), never in a public issue.

## Setting up

You need Rust (the toolchain in `rust-toolchain.toml` installs itself),
Node 22 for `web/app`, Docker for the end-to-end tests, and
[pre-commit](https://pre-commit.com).

```sh
git clone https://github.com/hatchrun/hatchrun
cd hatchrun
pre-commit install
```

`pre-commit install` runs gitleaks on staged changes, with the rules in
`.gitleaks.toml`. It stops a commit that contains a secret or a
Cloudflare or AWS account ID. CI scans the full history with the same
rules. Pass account IDs through CI secrets or variables; a finding
that is not a secret goes in `.gitleaksignore` by fingerprint, with a
comment saying why.

## Build and test

Run these before you push. CI runs the same commands.

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests that switch users need root and a real account. CI runs them
in the `root-tests` job with `cargo test --locked -- --ignored`; run
the same in a container if you change that code.

The end-to-end suite runs a push webhook through `hatchrun-ci` to check
runs and logs on a fake GitHub. It needs Docker and Node 24:

```sh
crates/ci/scripts/e2e.sh
```

The `hatchrun-cloud` tests run their queries against Postgres 17. Set
`DATABASE_URL` to a database you can create schemas in, as CI does.

The app in `web/app`:

```sh
npm --prefix web/app ci
npm --prefix web/app run typecheck
npm --prefix web/app run lint
npm --prefix web/app run build
npm --prefix web/app audit --audit-level=high
```

Every file needs a licence and copyright header. `reuse lint` checks
them; markdown and configuration files are covered by `REUSE.toml`.

```sh
pipx run reuse==6.2.0 lint
```

## Tests

Major new functionality must come with tests in the same pull request,
and a bug fix should come with a regression test where one is
practical. Reviewers will ask for tests before approving a change that
adds behaviour.

## Style

- Rust: `rustfmt` with the defaults, and `clippy` with warnings as
  errors. Keep `crates/scaler-core` free of I/O so it runs in Workers.
- TypeScript: the ESLint config in `web/app` and `tsc -b` must pass.
- Prose and comments: match the spelling and tone of the file you are
  editing, and wrap prose at 72 columns where the file already does.

## Commits and pull requests

Use Conventional Commits: `feat:`, `fix:`, `perf:`, `revert:`,
`docs:`, `build:`, `ci:`, `test:`, `refactor:`, `chore:`. Releases are
cut from `feat`, `fix`, `perf` and `revert` commits, and the release
notes are generated from them.

Sign off every commit with the Developer Certificate of Origin:

```sh
git commit -s
```

The `dco` workflow rejects a pull request whose commits lack a
`Signed-off-by` line from their author. Fix it with
`git rebase --signoff -S <base>` and force-push.

Keep a pull request to one change. Say what it does and why in the
description, and link the issue it closes. If it changes the workflow
format, the CLI or the HTTP API, update the README and the docs in
`docs/`.

CI must pass before a pull request is reviewed, including `check`,
`e2e`, `app`, `root-tests`, `reuse`, `secrets`, `audit`,
`systemd-unit`, CodeQL and the DCO check. A maintainer reviews and
merges.

## Licence

hatchrun is AGPL-3.0-only (see [LICENSE](LICENSE)). By contributing you
agree that your contribution is licensed under the same terms, and the
DCO sign-off records that you have the right to submit it.
