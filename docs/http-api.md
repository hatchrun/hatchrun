# HTTP API

hatchrun is served by Cloudflare Workers. One Worker, `crates/cloud`,
answers on `hatchrun.io`, `app.hatchrun.io` and `oidc.hatchrun.io`; it
forwards `/api/ci/` to the `hatchrun-ci` Worker over a service binding.
The routes below are the ones the web app and the GitHub App use. They
are not a versioned public API: they can change with the app, and
nothing outside this repository should depend on them.

## Routing

| Host               | What it serves                                            |
| ------------------ | --------------------------------------------------------- |
| `hatchrun.io`      | `POST /github/webhook`, `GET /healthz`, static pages.      |
| `app.hatchrun.io`  | The React app, `/api/`, `/api/ci/`, `GET /healthz`.        |
| `oidc.hatchrun.io` | The OIDC discovery document and JWKS.                     |

On any host, `POST /github/webhook` is the webhook endpoint, `/api` and
`/api/*` go to the app, `/healthz` is the health check, and anything
else is a static asset. `/api/ci/*` is forwarded to `hatchrun-ci` only
when the request arrives on the app's own host; other hosts get the
app's 404.

## Webhooks

`POST /github/webhook` takes the hatchrun GitHub App's deliveries. It
checks `X-Hub-Signature-256` against `GITHUB_WEBHOOK_SECRET`, then:

- `push`, `pull_request`, `check_run`, `repository`, `release`,
  `merge_group`, `repository_dispatch`, `create`, `delete` and
  `installation_repositories` are forwarded to `hatchrun-ci` with the
  same body and headers.
- `installation` events link a tenant to its installation, or unlink it
  when the App is uninstalled or suspended.

The answer is `hatchrun-ci`'s: 202 once the delivery is stored, 200 for
a copy of one already stored, and 503 when it could not be stored, so
GitHub or the operator sends it again. A delivery whose planning failed
can be redelivered from GitHub and is planned again.

## Health

`GET /healthz` answers `200` when the database answers and
`hatchrun-ci`'s reconciler has run recently, and `503` otherwise. The
body says which check failed. `hatchrun-ci` answers the same check for
itself, and the app passes its state on for the uptime monitor.

## OIDC issuer

`oidc.hatchrun.io` serves:

- `GET /.well-known/openid-configuration`: the discovery document.
- `GET /.well-known/jwks.json`: the public keys, matched by `kid`.

Both are cached for an hour. `hatchrun-ci` signs a five-minute token
for `tenant:hatchrun-ci` and trades it for an AWS role with
`AssumeRoleWithWebIdentity`; STS checks the signature against these
documents.

## App API

Apart from the sign-in routes, Polar's webhook and the internal image
route, everything under `/api` needs the session cookie set by sign-in.
A request that changes state also needs the `csrfToken` from
`GET /api/session`, sent as the `X-CSRF-Token` header. Answers are
JSON, and an error is `{"error": "<message>"}` with the status that
fits: `401` when signed out, `403` for a bad CSRF token or missing
access, `404` for something the caller may not see, `422` for a value
the server refuses.

| Method | Path                             | What it does                          |
| ------ | -------------------------------- | ------------------------------------- |
| `GET`  | `/api/auth/login`                | Start GitHub sign-in.                 |
| `GET`  | `/api/auth/callback`             | Finish sign-in; sets the cookie.      |
| `POST` | `/api/auth/logout`               | Clear the session.                    |
| `GET`  | `/api/session`                   | The signed-in user and a CSRF token.  |
| `GET`  | `/api/installations`             | The App installations the user sees.  |
| `GET`  | `/api/tenants`                   | The organizations the user belongs to.|
| `POST` | `/api/tenants`                   | Create the tenant for an installation.|
| `GET`  | `/api/tenants/<org>`             | One tenant, with the caller's role.   |
| `POST` | `/api/tenants/<org>/checkout`    | Open a Polar checkout.                |
| `POST` | `/api/tenants/<org>/portal`      | Open the Polar customer portal.       |
| `GET`  | `/api/github/setup`              | The App's setup URL after install.    |
| `POST` | `/api/polar/webhook`             | Polar's subscription webhooks.        |
| `POST` | `/api/internal/images`           | Internal image catalogue updates.     |

`GET /api/tenants/<org>` returns:

```json
{
  "id": "…",
  "org": { "id": 1, "login": "acme", "avatarUrl": "…" },
  "installationId": 2,
  "installUrl": "…",
  "role": "admin"
}
```

`role` is `admin` for an organization owner and `member` for anyone
else, from the user's GitHub membership. Checkout and the portal need
an owner.

## CI API

`/api/ci/` is `hatchrun-ci`'s JSON API, used by the app. It reads the
same session cookie from the shared database, so no identity travels in
a header a caller could set. Access is resolved per repository:
reading needs read access, and a change needs write access. A caller
who cannot see a repository gets `404`; one who can see but not change
it gets `403`.

### Runs

| Method | Path                                          | What it returns                     |
| ------ | --------------------------------------------- | ----------------------------------- |
| `GET`  | `/api/ci/orgs/<org>/runs`                     | The organization's runs.            |
| `GET`  | `/api/ci/orgs/<org>/activity`                 | Runs and jobs over time.            |
| `GET`  | `/api/ci/orgs/<org>/needs-you`                | What waits for the caller.          |
| `GET`  | `/api/ci/repos/<owner>/<repo>`                | A repository and its counts.        |
| `GET`  | `/api/ci/repos/<owner>/<repo>/workflows`      | Workflows that ran.                 |
| `GET`  | `/api/ci/repos/<owner>/<repo>/branches`       | Branches that ran.                  |
| `GET`  | `/api/ci/repos/<owner>/<repo>/pulls`          | The repository's pull requests.     |
| `GET`  | `/api/ci/repos/<owner>/<repo>/runs`           | The repository's runs.              |
| `POST` | `/api/ci/repos/<owner>/<repo>/dispatch`       | Start a workflow by hand.           |
| `GET`  | `/api/ci/runs/<run>`                          | One run.                            |
| `POST` | `/api/ci/runs/<run>/cancel`                   | Stop a run.                         |
| `POST` | `/api/ci/runs/<run>/approve-fork`             | Run a fork pull request's run.      |
| `POST` | `/api/ci/runs/<run>/reject-fork`              | Cancel a waiting fork run.          |
| `POST` | `/api/ci/jobs/<job>/approve`                  | Let a held job run.                 |
| `POST` | `/api/ci/jobs/<job>/reject`                   | Fail a held job.                    |
| `GET`  | `/api/ci/jobs/<job>/log`                      | A job's log.                        |
| `GET`  | `/api/ci/jobs/<job>/log/index`                | The log's step and chunk index.     |
| `GET`  | `/api/ci/jobs/<job>/log/search?q=<text>`      | Search a stored log.                |
| `GET`  | `/api/ci/jobs/<job>/live`                     | A running job's log, as a WebSocket.|
| `GET`  | `/api/ci/runs/<run>/artifacts`                | A run's artifacts.                  |
| `GET`  | `/api/ci/runs/<run>/artifacts/<artifact>`     | Download one artifact.              |

`GET /api/ci/jobs/<job>/log` takes `offset` and `length` to read a
range of a stored log. The search takes `q` (at most 200 characters)
and an optional `cursor` from the previous answer.

`POST /api/ci/repos/<owner>/<repo>/dispatch` takes:

```json
{ "workflow": "ci", "ref": "main", "inputs": { "target": "staging" } }
```

`ref` is a branch or tag; a bare name is a branch and an empty one is
the default branch. The repository must be enabled for CI, or the
answer is `422`.

### Settings

Settings routes need admin access to the organization or repository.

| Method        | Path                                             | What it does                     |
| ------------- | ------------------------------------------------ | -------------------------------- |
| `GET`         | `/api/ci/orgs/<org>/billing`                     | Usage and spend.                 |
| `PUT`         | `/api/ci/orgs/<org>/billing/cap`                 | Set the monthly spend cap.       |
| `GET`         | `/api/ci/orgs/<org>/repos`                       | The installation's repositories. |
| `PATCH`       | `/api/ci/orgs/<org>/repos/enabled`               | Choose which repositories run.   |
| `GET`         | `/api/ci/orgs/<org>/audit`                       | Settings changes.                |
| `GET`         | `/api/ci/orgs/<org>/setup`                       | Setup progress.                  |
| `GET`         | `/api/ci/orgs/<org>/environments`                | Environments of all repos.       |
| `GET`         | `/api/ci/orgs/<org>/secrets`                     | Secret names.                    |
| `PUT`/`DELETE`| `/api/ci/orgs/<org>/secrets/<name>`              | Set or delete a secret.          |
| `GET`         | `/api/ci/orgs/<org>/vars`                        | Variables.                       |
| `PUT`/`DELETE`| `/api/ci/orgs/<org>/vars/<name>`                 | Set or delete a variable.        |
| `GET`         | `/api/ci/repos/<owner>/<repo>/secrets`           | Secret names.                    |
| `PUT`/`DELETE`| `/api/ci/repos/<owner>/<repo>/secrets/<name>`    | Set or delete a secret.          |
| `GET`         | `/api/ci/repos/<owner>/<repo>/vars`              | Variables.                       |
| `PUT`/`DELETE`| `/api/ci/repos/<owner>/<repo>/vars/<name>`       | Set or delete a variable.        |
| `GET`/`PUT`   | `/api/ci/repos/<owner>/<repo>/fork-policy`       | How fork pull requests are treated. |
| `GET`         | `/api/ci/repos/<owner>/<repo>/environments`      | The repository's environments.   |
| `PUT`/`DELETE`| `/api/ci/repos/<owner>/<repo>/environments/<name>` | Set or delete an environment.  |

A settings body is at most 64 KiB, and a secret or variable value at
most 128 KiB. A request that changes state, and the log WebSocket, must
come from the app's own origin, and a change also needs the CSRF token;
a request from another origin is refused.
