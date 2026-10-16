# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Fills in jobs recorded before migration 0018 and gives each its run.

Jobs from before 0018 have no name, attempt or conclusion, and no row in
`runs`, so the app cannot list them by run. This asks GitHub about each,
as the App, and writes what it says to D1:

    python3 scripts/backfill-runs.py --app-id 123456 --app-key app.pem

Run it once, after the migration and the Worker that writes the new
columns are deployed. It prints the SQL and asks before running it;
--yes skips the question. Jobs GitHub no longer has (the repository is
gone or out of the App's reach) cannot be named or linked, and are
deleted. Needs wrangler signed in to the account and openssl.
"""

import argparse
import base64
import datetime
import json
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

DATABASE = "hatchrun-cloud"
API = "https://api.github.com"


def d1(sql):
    """Rows of one statement against the remote database."""
    out = subprocess.run(
        ["npx", "wrangler", "d1", "execute", DATABASE, "--remote", "--json", "--command", sql],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)[0]["results"]


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def app_jwt(app_id, key):
    """The App's JWT, signed RS256 with openssl."""
    now = int(time.time())
    header = b64(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    claims = b64(json.dumps({"iat": now - 60, "exp": now + 540, "iss": app_id}).encode())
    signing_input = f"{header}.{claims}".encode()
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", key],
        input=signing_input,
        check=True,
        capture_output=True,
    ).stdout
    return f"{header}.{claims}.{b64(signature)}"


def github(method, path, token, scheme="token"):
    """GitHub's answer as JSON, or None when GitHub will not show it. A
    rate limit raises, so no job is taken for gone because of one."""
    request = urllib.request.Request(
        API + path,
        method=method,
        headers={
            "Authorization": f"{scheme} {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "hatchrun-backfill",
        },
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        limited = err.code == 429 or (
            err.code == 403
            and (err.headers.get("X-RateLimit-Remaining") == "0" or err.headers.get("Retry-After"))
        )
        if not limited and err.code in (403, 404, 410):
            return None
        raise


def ms(value):
    """Unix milliseconds of GitHub's timestamp, or None."""
    if not value:
        return None
    return int(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)


def sql(value):
    """A SQL literal."""
    if value is None:
        return "NULL"
    if isinstance(value, int):
        return str(value)
    return "'" + str(value).replace("'", "''") + "'"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--app-id", required=True, help="the App's id or client id")
    parser.add_argument("--app-key", required=True, help="the App's private key, PEM")
    parser.add_argument("--yes", action="store_true", help="run the SQL without asking")
    args = parser.parse_args()

    jobs = d1(
        "SELECT id, installation_id, repository, run_id FROM jobs "
        "WHERE name = '' ORDER BY installation_id, run_id, id"
    )
    if not jobs:
        print("nothing to backfill", file=sys.stderr)
        return
    known = {row["id"] for row in d1("SELECT id FROM runs")}

    jwt = app_jwt(args.app_id, args.app_key)
    tokens = {}
    statements, gone, runs = [], [], {}
    for job in jobs:
        installation = job["installation_id"]
        if installation not in tokens:
            answer = github("POST", f"/app/installations/{installation}/access_tokens", jwt, "Bearer")
            tokens[installation] = answer and answer["token"]
        token = tokens[installation]
        seen = token and github("GET", f"/repos/{job['repository']}/actions/jobs/{job['id']}", token)
        if not seen:
            gone.append(job["id"])
            continue
        statements.append(
            "UPDATE jobs SET "
            f"name = {sql(seen['name'])}, run_attempt = {sql(seen.get('run_attempt') or 1)}, "
            f"conclusion = {sql(seen.get('conclusion') if seen['status'] == 'completed' else None)}, "
            f"started_at = {sql(ms(seen.get('started_at')) if seen['status'] != 'queued' else None)}, "
            f"completed_at = {sql(ms(seen.get('completed_at')) if seen['status'] == 'completed' else None)} "
            f"WHERE id = {job['id']};"
        )
        if job["run_id"] not in known and job["run_id"] not in runs:
            runs[job["run_id"]] = (job, seen, token)

    now = int(time.time() * 1000)
    for run_id, (job, seen, token) in runs.items():
        run = github("GET", f"/repos/{job['repository']}/actions/runs/{run_id}", token) or {}
        attempt = f"(SELECT MAX(run_attempt) FROM jobs WHERE run_id = {run_id})"
        queued_at = (
            f"(SELECT MIN(queued_at) FROM jobs WHERE run_id = {run_id} AND run_attempt = {attempt})"
        )
        statements.append(
            "INSERT INTO runs (id, installation_id, repository, workflow_name, head_branch, "
            "head_sha, attempt, queued_at, run_number, event, display_title, actor, path, "
            "details_at, created_at, updated_at) VALUES ("
            f"{run_id}, {job['installation_id']}, {sql(job['repository'])}, "
            f"{sql(seen.get('workflow_name') or run.get('name'))}, {sql(seen.get('head_branch'))}, "
            f"{sql(seen['head_sha'])}, {attempt}, {queued_at}, {sql(run.get('run_number'))}, "
            f"{sql(run.get('event'))}, {sql(run.get('display_title'))}, "
            f"{sql((run.get('actor') or {}).get('login'))}, {sql(run.get('path'))}, "
            f"{now}, {now}, {now}) ON CONFLICT (id) DO NOTHING;"
        )
    if gone:
        statements.append(f"DELETE FROM jobs WHERE id IN ({', '.join(map(str, gone))});")

    print("\n".join(statements))
    print(
        f"-- {len(jobs) - len(gone)} jobs named, {len(runs)} runs added, {len(gone)} jobs deleted",
        file=sys.stderr,
    )
    if not args.yes and input("Run this against the remote database? [y/N] ").strip().lower() != "y":
        return
    with tempfile.NamedTemporaryFile("w", suffix=".sql") as file:
        file.write("\n".join(statements))
        file.flush()
        subprocess.run(
            ["npx", "wrangler", "d1", "execute", DATABASE, "--remote", "--file", file.name],
            check=True,
        )


if __name__ == "__main__":
    main()
