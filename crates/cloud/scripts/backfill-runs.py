# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Fills in jobs recorded before migration 0018 and gives each its run.

Jobs from before 0018 have no name, attempt or conclusion, and no row in
`runs`, so the app cannot list them by run. This asks GitHub about each
run, as the App, and writes what it says to D1:

    python3 scripts/backfill-runs.py --app-id 123456 --app-key app.pem

Run it once, after the migration and the Worker that writes the new
columns are deployed. It asks once before writing (--yes skips the
question, --dry-run prints the SQL instead) and writes each
installation's jobs as it finishes them, so a run stopped by an error or
a rate limit picks up where it left off. Jobs GitHub no longer has (the
repository is gone, or the App uninstalled) cannot be named or linked,
and are deleted; a suspended installation is skipped. Needs wrangler
signed in to the account and openssl.
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


def rate_limited(err):
    return err.code == 429 or (
        err.code == 403
        and (err.headers.get("X-RateLimit-Remaining") == "0" or err.headers.get("Retry-After"))
    )


def github(method, path, token, scheme="token", gone=(403, 404, 410)):
    """GitHub's answer as JSON, or None when GitHub will not show it: a
    status in `gone`. A rate limit raises, so no job is taken for gone
    because of one."""
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
        if not rate_limited(err) and err.code in gone:
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


def run_jobs(repository, run_id, token):
    """Every job of a run, of every attempt, by id; empty for a run
    GitHub no longer has. Any other error raises, so no job is deleted
    because a page went missing."""
    jobs, page = {}, 1
    while True:
        answer = github(
            "GET",
            f"/repos/{repository}/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}",
            token,
            gone=(404, 410) if page == 1 else (),
        )
        listed = (answer or {}).get("jobs", [])
        jobs.update({job["id"]: job for job in listed})
        if len(listed) < 100:
            return jobs
        page += 1


def installation_token(app_id, key, installation):
    """A token of the installation, None when it is uninstalled. Raises
    for a suspended one."""
    # A fresh JWT each time: it lives nine minutes, less than a large
    # installation can take.
    path = f"/app/installations/{installation}/access_tokens"
    answer = github("POST", path, app_jwt(app_id, key), "Bearer", gone=(404,))
    return (answer or {}).get("token")


def statements_for(job_ids, run_id, repository, installation, known, token):
    """The SQL that names a run's jobs and records the run, and the jobs
    GitHub no longer has."""
    statements, gone = [], []
    seen_jobs = run_jobs(repository, run_id, token) if token else {}
    for job_id in job_ids:
        seen = seen_jobs.get(job_id)
        if not seen:
            gone.append(job_id)
            continue
        statements.append(
            "UPDATE jobs SET "
            f"name = {sql(seen['name'])}, run_attempt = {sql(seen.get('run_attempt') or 1)}, "
            f"conclusion = {sql(seen.get('conclusion') if seen['status'] == 'completed' else None)}, "
            f"started_at = {sql(ms(seen.get('started_at')) if seen.get('runner_name') else None)}, "
            f"completed_at = {sql(ms(seen.get('completed_at')) if seen['status'] == 'completed' else None)} "
            f"WHERE id = {job_id};"
        )
    named = [seen_jobs[j] for j in job_ids if j in seen_jobs]
    if named and run_id not in known:
        seen = named[0]
        run = github("GET", f"/repos/{repository}/actions/runs/{run_id}", token) or {}
        now = int(time.time() * 1000)
        attempt = f"(SELECT MAX(run_attempt) FROM jobs WHERE run_id = {run_id})"
        queued_at = (
            f"(SELECT MIN(queued_at) FROM jobs WHERE run_id = {run_id} AND run_attempt = {attempt})"
        )
        statements.append(
            "INSERT INTO runs (id, installation_id, repository, workflow_name, head_branch, "
            "head_sha, attempt, queued_at, run_number, event, display_title, actor, path, "
            "details_at, created_at, updated_at) VALUES ("
            f"{run_id}, {installation}, {sql(repository)}, "
            f"{sql(seen.get('workflow_name') or run.get('name'))}, {sql(seen.get('head_branch'))}, "
            f"{sql(seen['head_sha'])}, {attempt}, {queued_at}, {sql(run.get('run_number'))}, "
            f"{sql(run.get('event'))}, {sql(run.get('display_title'))}, "
            f"{sql((run.get('actor') or {}).get('login'))}, {sql(run.get('path'))}, "
            f"{now}, {now}, {now}) ON CONFLICT (id) DO NOTHING;"
        )
    return statements, gone


def write(statements, dry_run):
    if dry_run:
        print("\n".join(statements))
        return
    with tempfile.NamedTemporaryFile("w", suffix=".sql") as file:
        file.write("\n".join(statements))
        file.flush()
        subprocess.run(
            ["npx", "wrangler", "d1", "execute", DATABASE, "--remote", "--file", file.name],
            check=True,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--app-id", required=True, help="the App's id or client id")
    parser.add_argument("--app-key", required=True, help="the App's private key, PEM")
    parser.add_argument("--yes", action="store_true", help="write without asking")
    parser.add_argument("--dry-run", action="store_true", help="print the SQL, write nothing")
    args = parser.parse_args()

    jobs = d1(
        "SELECT id, installation_id, repository, run_id FROM jobs "
        "WHERE name = '' ORDER BY installation_id, run_id, id"
    )
    if not jobs:
        print("nothing to backfill", file=sys.stderr)
        return
    installations = {}
    for job in jobs:
        runs = installations.setdefault(job["installation_id"], {})
        runs.setdefault((job["repository"], job["run_id"]), []).append(job["id"])
    question = (
        f"Backfill {len(jobs)} jobs of {len(installations)} installations "
        "in the remote database? [y/N] "
    )
    if not (args.yes or args.dry_run) and input(question).strip().lower() != "y":
        return
    known = {row["id"] for row in d1("SELECT id FROM runs")}

    for installation, runs in installations.items():
        try:
            token = installation_token(args.app_id, args.app_key, installation)
        except urllib.error.HTTPError as err:
            if err.code != 403 or rate_limited(err):
                raise
            print(
                f"installation {installation}: skipped, GitHub answered 403 (suspended?)",
                file=sys.stderr,
            )
            continue
        statements, gone = [], []
        for (repository, run_id), job_ids in runs.items():
            named, lost = statements_for(job_ids, run_id, repository, installation, known, token)
            statements += named
            gone += lost
        if gone:
            statements.append(f"DELETE FROM jobs WHERE id IN ({', '.join(map(str, gone))});")
        count = sum(len(ids) for ids in runs.values())
        print(
            f"installation {installation}: {count - len(gone)} jobs named, {len(gone)} deleted",
            file=sys.stderr,
        )
        if statements:
            write(statements, args.dry_run)


if __name__ == "__main__":
    main()
