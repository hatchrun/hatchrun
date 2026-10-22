#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

# Runs hatchrun-ci end to end on this machine: a push webhook goes in,
# hatchrun-agent ci runs the jobs, and check runs on a fake GitHub and
# logs in R2 come out. Run from anywhere in the repository:
#
#   crates/ci/scripts/e2e.sh
#
# What it starts, all on 127.0.0.1:
#
# - Postgres 17 in Docker on 55432 (E2E_PG_PORT), unless E2E_DATABASE_URL names one;
# - hatchrun-ci-dev fake-github on 9101, which also answers Polar's
#   calls, and launcher on 9102;
# - wrangler dev for crates/broker on 8789 and for crates/ci on 8788,
#   the latter built with the local launcher and run from a copy of its
#   wrangler.toml, crates/ci/wrangler.e2e.toml, whose Secrets Store
#   binding names a local store that holds the key for CI secrets.
#
# Scenarios, in order:
#
# - a push to main runs build then test, both pass, and the log and
#   step index land in R2;
# - a push to fail runs a job that fails;
# - a re-run request for the failed job runs it again;
# - a push to slow runs a job the script cancels;
# - a repository secret is set through the API, then a push to arm runs
#   a job on the ubuntu-26.04-arm template that echoes it;
# - that job's log in R2 shows the secret masked, and its value is in
#   no R2 object, Durable Object, KV entry, Worker log or fake GitHub
#   state, nor in the sealed row;
# - a push to bad fails the workflow whose runs-on names no template,
#   with a check run that says so, and runs the other workflow;
# - a push to wide runs six jobs: five at once, the sixth held with a
#   check run that says why until one ends; the fake Polar then holds a
#   usage event for each;
# - a tenant whose subscription was canceled gets one billing check
#   run and no run; a tenant that never subscribed gets nothing, and its
#   config is not read;
# - environments, after wide: environment settings, secrets and vars at
#   three levels, and an organization secret limited to other
#   repositories are set through the API; a push to guard fails its job
#   on a branch pattern without a launch; a push to deploy holds both
#   jobs for approval with no launch; one job is approved and runs with
#   the environment's secret and var and without the org-only secret,
#   the other is rejected; a second approval answers 409.
#
# Every instance must have had its metadata turned off, and the change
# seen applied, before its job ran.
#
# Needs docker (or E2E_DATABASE_URL and psql), cargo, node 22 or later,
# openssl, curl and jq. Everything started is stopped on exit. On
# failure the logs are printed and the exit code is 1. E2E_LOG_DIR, when
# set, receives every log either way.

set -euo pipefail
# Each background process gets a process group of its own, so the
# cleanup can stop it with everything it started.
set -m

WRANGLER="wrangler@4.145.0"
PG_PORT=${E2E_PG_PORT:-55432}
GITHUB_PORT=9101
LAUNCHER_PORT=9102
CI_PORT=8788
BROKER_PORT=8789
CI="http://127.0.0.1:${CI_PORT}"
GITHUB="http://127.0.0.1:${GITHUB_PORT}"
LAUNCHER="http://127.0.0.1:${LAUNCHER_PORT}"
BROKER="http://127.0.0.1:${BROKER_PORT}"
APP_URL="http://127.0.0.1:5173"
TENANT="e2etenant0000001"
INSTALLATION=42
# Installation 43: a tenant whose subscription was canceled.
# Installation 44: a tenant that never subscribed.
LAPSED_TENANT="e2etenant0000002"
NEVER_TENANT="e2etenant0000003"
POLAR_TOKEN="e2e-polar-token"
# The local Secrets Store that holds the key behind CI_SECRETS_KEY.
STORE_ID="e2e00000000000000000000000000000"
WEBHOOK_SECRET="e2e-webhook-secret"
BROKER_TOKEN="e2e-broker-token"
TOKEN_KEY="e2e-job-token-key"
# Long enough for the job's wait, short enough for a stuck run.
POLL_SECS=180

root="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$root"
fixtures="$root/crates/ci/tests/e2e"
# The physical path: the launcher names it so in agent command lines.
tmp="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/hatchrun-e2e.XXXXXX")" && pwd -P)"
logs="$tmp/logs"
mkdir -p "$logs"
pids=()
container=""
e2e_config="$root/crates/ci/wrangler.e2e.toml"

log() { echo "e2e: $*" >&2; }

die() {
  log "FAILED: $*"
  exit 1
}

stop_all() {
  local pid
  for pid in "${pids[@]+"${pids[@]}"}"; do
    kill -TERM -- "-$pid" 2>/dev/null || true
  done
  for _ in $(seq 1 50); do
    local alive=0
    for pid in "${pids[@]+"${pids[@]}"}"; do
      if kill -0 -- "-$pid" 2>/dev/null; then alive=1; fi
    done
    [[ $alive -eq 0 ]] && break
    sleep 0.1
  done
  for pid in "${pids[@]+"${pids[@]}"}"; do
    kill -KILL -- "-$pid" 2>/dev/null || true
  done
  # Agents run in process groups of their own; their command lines
  # name the work root.
  pkill -KILL -f -- "$tmp/instances" 2>/dev/null || true
  if [[ -n "$container" ]]; then
    docker rm -f "$container" >/dev/null 2>&1 || true
  fi
}

print_logs() {
  local file
  for file in "$logs"/*.log "$logs"/*.json "$tmp"/instances/*/agent.log; do
    [[ -f "$file" ]] || continue
    echo "----- ${file#"$tmp"/} (last 150 lines) -----" >&2
    tail -n 150 "$file" >&2
  done
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "${launcher_up:-}" ]]; then
    curl -fsS "$GITHUB/_state" >"$logs/github-state.json" 2>/dev/null || true
    curl -fsS "$LAUNCHER/_events" >"$logs/launcher-events.json" 2>/dev/null || true
  fi
  if [[ $status -ne 0 ]]; then
    print_logs
  fi
  stop_all
  if [[ -n "${E2E_LOG_DIR:-}" ]]; then
    mkdir -p "$E2E_LOG_DIR"
    cp -R "$logs/." "$E2E_LOG_DIR/" 2>/dev/null || true
    for dir in "$tmp"/instances/*/; do
      [[ -f "$dir/agent.log" ]] && cp "$dir/agent.log" "$E2E_LOG_DIR/$(basename "$dir").log"
    done
  fi
  rm -rf "$tmp"
  rm -f "$e2e_config"
  if [[ $status -ne 0 ]]; then
    exit 1
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# Starts "$@" in the background with its output in $logs/$name.log.
start() {
  local name=$1
  shift
  "$@" >"$logs/$name.log" 2>&1 </dev/null &
  pids+=("$!")
  # The cleanup stops it; bash need not report that it did.
  disown "$!"
}

# Waits up to $2 seconds for $1 to answer anything at all.
wait_http() {
  local url=$1 secs=$2 what=$3
  local deadline=$((SECONDS + secs))
  until curl -s -o /dev/null --max-time 5 "$url"; do
    if ((SECONDS >= deadline)); then
      die "$what did not answer at $url within ${secs}s"
    fi
    sleep 1
  done
}

for tool in cargo node npx openssl curl jq; do
  command -v "$tool" >/dev/null || die "$tool is not installed"
done

# 1. Postgres.
if [[ -n "${E2E_DATABASE_URL:-}" ]]; then
  database_url="$E2E_DATABASE_URL"
  command -v psql >/dev/null || die "psql is needed with E2E_DATABASE_URL"
  psql_run() { psql "$database_url" --no-psqlrc --quiet -v ON_ERROR_STOP=1 "$@"; }
  migrate_url="$database_url"
else
  command -v docker >/dev/null || die "docker is not installed and E2E_DATABASE_URL is not set"
  container="hatchrun-e2e-$$"
  database_url="postgres://postgres:postgres@127.0.0.1:${PG_PORT}/postgres"
  log "starting postgres:17 on $PG_PORT"
  # The migrations are mounted at the same path, so psql in the
  # container reads the files migrate.sh names.
  docker run -d --rm --name "$container" \
    -e POSTGRES_PASSWORD=postgres \
    -p "127.0.0.1:${PG_PORT}:5432" \
    -v "$root/crates/cloud/migrations:$root/crates/cloud/migrations:ro" \
    postgres:17 >/dev/null
  deadline=$((SECONDS + 60))
  until docker exec "$container" pg_isready -q -h 127.0.0.1 -U postgres 2>/dev/null; do
    ((SECONDS < deadline)) || die "postgres did not start"
    sleep 1
  done
  inside="postgres://postgres:postgres@127.0.0.1:5432/postgres"
  psql_run() { docker exec -i "$container" psql "$inside" --no-psqlrc --quiet -v ON_ERROR_STOP=1 "$@"; }
  migrate_url="$inside"
  # migrate.sh calls psql; without one here, the container's answers.
  if ! command -v psql >/dev/null; then
    mkdir -p "$tmp/bin"
    cat >"$tmp/bin/psql" <<EOF
#!/bin/sh
exec docker exec -i "$container" psql "\$@"
EOF
    chmod +x "$tmp/bin/psql"
    export PATH="$tmp/bin:$PATH"
  else
    migrate_url="$database_url"
  fi
fi
log "applying migrations"
DATABASE_MIGRATE_URL="$migrate_url" crates/cloud/scripts/migrate.sh >"$logs/migrate.log" 2>&1 ||
  die "migrations failed: $(cat "$logs/migrate.log")"
now_ms=$(($(date +%s) * 1000))
psql_run <<SQL
INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at)
VALUES ('$TENANT', 1, 'e2e', $INSTALLATION, $now_ms, $now_ms),
       ('$LAPSED_TENANT', 2, 'e2e2', 43, $now_ms, $now_ms),
       ('$NEVER_TENANT', 3, 'e2e3', 44, $now_ms, $now_ms);
INSERT INTO subscriptions (id, tenant_id, status, customer_id, product_id,
                           cancel_at_period_end, modified_at, updated_at)
VALUES ('sub-e2e', '$TENANT', 'active', 'cus-e2e', 'e2e-product', false,
        '2026-01-01T00:00:00Z', $now_ms),
       ('sub-e2e2', '$LAPSED_TENANT', 'canceled', 'cus-e2e2', 'e2e-product', false,
        '2026-01-01T00:00:00Z', $now_ms);
SQL

# 2. Binaries and keys.
log "building hatchrun-agent and hatchrun-ci-dev"
cargo build --locked -p hatchrun-agent -p hatchrun-ci-dev >"$logs/cargo.log" 2>&1 ||
  die "cargo build failed: $(tail -n 40 "$logs/cargo.log")"
target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
agent="$target_dir/debug/hatchrun-agent"
dev="$target_dir/debug/hatchrun-ci-dev"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$tmp/key.pem" 2>/dev/null
key="$(cat "$tmp/key.pem")"

# A session for the cancel, as the app's sign-in would leave it.
session_json="$("$dev" session --github-token ghu_e2e)"
cookie="$(jq -r .cookie <<<"$session_json")"
csrf="$(jq -r .csrf <<<"$session_json")"
psql_run <<SQL
INSERT INTO users (id, login, avatar_url, created_at, updated_at)
VALUES (7, 'e2e', 'https://example.invalid/a.png', $now_ms, $now_ms);
INSERT INTO sessions (key, user_id, github_token, created_at, expires_at)
VALUES ('$(jq -r .key <<<"$session_json")', 7, '$(jq -r .sealed <<<"$session_json")',
        $now_ms, $((now_ms + 86400000)));
SQL

# 3. Services.
repo="$tmp/repo"
cp -R "$fixtures/repo" "$repo"
start fake-github "$dev" fake-github --port "$GITHUB_PORT" --repo-dir "$repo"
start launcher env RUST_LOG="${RUST_LOG:-info,hatchrun_agent=debug}" "$dev" launcher --port "$LAUNCHER_PORT" --agent "$agent" --work-root "$tmp/instances"

# Secrets go in env files: the keys span lines.
{
  echo "BROKER_TOKEN=\"$BROKER_TOKEN\""
} >"$tmp/broker.env"
{
  echo "GITHUB_WEBHOOK_SECRET=\"$WEBHOOK_SECRET\""
  echo "BROKER_TOKEN=\"$BROKER_TOKEN\""
  echo "CI_TOKEN_KEY=\"$TOKEN_KEY\""
  printf 'GITHUB_APP_PRIVATE_KEY="%s"\n' "$key"
  printf 'OIDC_SIGNING_KEY="%s"\n' "$key"
  echo "POLAR_ACCESS_TOKEN=\"$POLAR_TOKEN\""
} >"$tmp/ci.env"

# The key behind the CI_SECRETS_KEY binding. crates/ci/wrangler.toml
# names the account's store, which the owner fills in; the copy names a
# fixed local store instead, and the key goes in wrangler's local
# Secrets Store under that id and the binding's secret name, in the
# state directory hatchrun-ci's wrangler dev is given with --persist-to.
secret_name="$(sed -n 's/^secret_name = "\(.*\)"$/\1/p' "$root/crates/ci/wrangler.toml")"
[[ -n "$secret_name" ]] || die "no secrets store binding in crates/ci/wrangler.toml"
sed "s/^store_id = \".*\"$/store_id = \"$STORE_ID\"/" "$root/crates/ci/wrangler.toml" >"$e2e_config"
grep -q "^store_id = \"$STORE_ID\"$" "$e2e_config" || die "no store_id in crates/ci/wrangler.toml"
log "creating the local secrets store key"
(cd "$root/crates/ci" && npx --yes "$WRANGLER" secrets-store secret create "$STORE_ID" \
  --name "$secret_name" --value "$(openssl rand -base64 32)" --scopes workers \
  --persist-to "$tmp/ci-state") >"$logs/secrets-store.log" 2>&1 ||
  die "creating the local secrets store key failed: $(cat "$logs/secrets-store.log")"

log "starting wrangler dev for the broker and hatchrun-ci (the first build takes a while)"
start broker env -C "$root/crates/broker" npx --yes "$WRANGLER" dev --local \
  --ip 127.0.0.1 --port "$BROKER_PORT" --inspector-port 9230 \
  --persist-to "$tmp/broker-state" --env-file "$tmp/broker.env" --show-interactive-dev-session=false
# The local launcher is compiled in only with this cfg. Its own target
# directory keeps the release build's cache apart.
start ci env -C "$root/crates/ci" \
  RUSTFLAGS="--cfg hatchrun_local_launcher" \
  CARGO_TARGET_DIR="$target_dir/e2e" \
  CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_HYPERDRIVE="$database_url" \
  npx --yes "$WRANGLER" dev -c wrangler.e2e.toml --local \
  --ip 127.0.0.1 --port "$CI_PORT" --inspector-port 9231 \
  --persist-to "$tmp/ci-state" --env-file "$tmp/ci.env" --show-interactive-dev-session=false \
  --test-scheduled \
  --var DEPLOY_ENV:development \
  --var LAUNCHER:local \
  --var LOCAL_LAUNCHER_URL:"$LAUNCHER" \
  --var GITHUB_API_URL:"$GITHUB" \
  --var BROKER_URL:"$BROKER" \
  --var CI_URL:"$CI" \
  --var APP_URL:"$APP_URL" \
  --var GITHUB_APP_ID:Iv1e2e \
  --var OIDC_KEY_ID:e2e \
  --var POLAR_API_URL:"$GITHUB" \
  --var CI_SECRETS_KEY_ID:e2e \
  --var CI_SECRETS_KEY_OLD_ID:""

wait_http "$GITHUB/_state" 30 "fake-github"
wait_http "$LAUNCHER/_events" 30 "the launcher"
launcher_up=1
wait_http "$BROKER/" 600 "the broker"
wait_http "$CI/" 900 "hatchrun-ci"
log "everything is up"

# 4. Webhooks.

# Posts the delivery $2 of event $1, signed, and prints the answer's
# body; fails on a status other than $3. A 503 is delivered again, as
# GitHub's redelivery would.
deliver() {
  local event=$1 body=$2 want=$3
  local signature status delivery tries=0
  signature="sha256=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" -r | cut -d' ' -f1)"
  delivery="$(openssl rand -hex 16)"
  while :; do
    status="$(printf '%s' "$body" | curl -sS -o "$tmp/answer" -w '%{http_code}' \
      -X POST "$CI/github/webhook" \
      -H "Content-Type: application/json" \
      -H "X-GitHub-Event: $event" \
      -H "X-GitHub-Delivery: $delivery" \
      -H "X-Hub-Signature-256: $signature" \
      --data-binary @-)"
    if [[ "$status" == 503 ]] && ((tries++ < 30)); then
      sleep 2
      continue
    fi
    break
  done
  [[ "$status" == "$want" ]] || die "$event delivery answered $status, not $want: $(cat "$tmp/answer")"
  cat "$tmp/answer"
}

# Pushes $2 to branch $1 and prints the run id.
push() {
  local branch=$1 sha=$2 body run
  body="$(jq -c --arg ref "refs/heads/$branch" --arg sha "$sha" \
    '.ref = $ref | .after = $sha' "$fixtures/push.json")"
  run="$(deliver push "$body" 202 | jq -r '.runs[0] // empty')"
  [[ -n "$run" ]] || die "the push to $branch started no run"
  echo "$run"
}

# The check runs on commit $1.
checks() {
  curl -fsS "$GITHUB/_state" | jq --arg sha "$1" '[.check_runs[] | select(.head_sha == $sha)]'
}

# Waits until commit $1 has $2 check runs and $3 of them, by jq filter
# over the list, hold.
wait_checks() {
  local sha=$1 count=$2 filter=$3
  local deadline=$((SECONDS + POLL_SECS))
  while :; do
    if checks "$sha" | jq -e --argjson n "$count" "length == \$n and ($filter)" >/dev/null; then
      return 0
    fi
    if ((SECONDS >= deadline)); then
      die "check runs on $sha after ${POLL_SECS}s: $(checks "$sha")"
    fi
    sleep 1
  done
}

check_of() {
  checks "$1" | jq -c --arg name "$2" '[.[] | select(.name == $name)] | last'
}

api() {
  curl -fsS "$CI/api/ci/$1"
}

job_id() {
  api "runs/$1" | jq -r --arg name "$2" '.jobs[] | select(.name == $name) | .id'
}

# Waits until run $1's job $2 reaches status $3 in the API.
wait_job() {
  local run=$1 name=$2 status=$3
  local deadline=$((SECONDS + POLL_SECS))
  until api "runs/$run" | jq -e --arg name "$name" --arg status "$status" \
    '.jobs[] | select(.name == $name) | .status == $status' >/dev/null; do
    ((SECONDS < deadline)) || die "job $name of run $run is not $status: $(api "runs/$run")"
    sleep 1
  done
}

# Scenario: a push to main runs build, then test, and both pass.
main_sha="$(printf 'a%.0s' $(seq 40))"
run="$(push main "$main_sha")"
log "push to main started run $run"
wait_checks "$main_sha" 2 'all(.status == "completed")'
build="$(check_of "$main_sha" "ci / build")"
test="$(check_of "$main_sha" "ci / test")"
[[ "$(jq -r .conclusion <<<"$build")" == success ]] || die "ci / build: $build"
[[ "$(jq -r .conclusion <<<"$test")" == success ]] || die "ci / test: $test"
[[ "$(jq -r .annotations <<<"$build")" == 1 ]] || die "ci / build should have 1 annotation: $build"
api "runs/$run" >"$tmp/run.json"
jq -e '[.jobs[] | select(.status == "completed")] | length == 2' "$tmp/run.json" >/dev/null ||
  die "run $run: $(cat "$tmp/run.json")"
build_job="$(jq -r '.jobs[] | select(.name == "build") | .id' "$tmp/run.json")"
# The log is read from R2 once the upload is done; until then from the
# run object. Wait for the index, which only R2 has.
deadline=$((SECONDS + 60))
until curl -fsS -o "$tmp/index.json" "$CI/api/ci/jobs/$build_job/log/index" 2>/dev/null; do
  ((SECONDS < deadline)) || die "job $build_job has no log index"
  sleep 1
done
jq -e '.. | objects | select(.name? == "say hello")' "$tmp/index.json" >/dev/null ||
  die "the index does not name the step: $(cat "$tmp/index.json")"
# R2 holds the log gzipped and it is served as stored, with
# Content-Encoding: gzip, so one gunzip gives the text.
curl -fsS -D "$tmp/log.headers" -o "$tmp/build.log.gz" "$CI/api/ci/jobs/$build_job/log"
cp "$tmp/log.headers" "$tmp/index.json" "$logs/"
grep -qi '^content-encoding: *gzip' "$tmp/log.headers" ||
  die "the stored log is not served gzipped: $(cat "$tmp/log.headers")"
gzip -dc <"$tmp/build.log.gz" >"$tmp/build.log" || die "the stored log is not one gzip stream"
cp "$tmp/build.log" "$logs/"
[[ "$(head -n 1 "$tmp/build.log")" == "== say hello" ]] ||
  die "the log does not start with the step: $(head -c 300 "$tmp/build.log" | od -c | head -n 5)"
grep -qx "hello from e2e" "$tmp/build.log" || die "the log lacks the echo: $(head -c 2000 "$tmp/build.log")"
test_job="$(jq -r '.jobs[] | select(.name == "test") | .id' "$tmp/run.json")"
curl -fsS --compressed "$CI/api/ci/jobs/$test_job/log" | grep -qx "build said success" ||
  die "the test job did not see build's result"
log "main: both jobs passed, the log and index are in R2"

# Scenario: a push to fail runs a job that fails.
fail_sha="$(printf 'b%.0s' $(seq 40))"
failed_run="$(push fail "$fail_sha")"
log "push to fail started run $failed_run"
wait_checks "$fail_sha" 1 'all(.status == "completed")'
broken="$(check_of "$fail_sha" "failing / broken")"
[[ "$(jq -r .conclusion <<<"$broken")" == failure ]] || die "failing / broken: $broken"
jq -e '.text // "" | contains("about to fail")' <<<"$broken" >/dev/null ||
  die "the failed check run lacks the log tail: $broken"
log "fail: the job failed"

# Scenario: a re-run request for the failed job runs it again. GitHub
# sends back the check run's external id.
external_id="$(jq -r .external_id <<<"$broken")"
rerun_body="$(jq -c --arg id "$external_id" '{
  action: "rerequested",
  check_run: { external_id: $id },
  repository: .repository,
  installation: .installation,
  sender: .sender
}' "$fixtures/push.json")"
deliver check_run "$rerun_body" 202 >/dev/null
deadline=$((SECONDS + POLL_SECS))
until api "runs/$failed_run" |
  jq -e '.jobs[] | select(.name == "broken") | .attempt == 2 and .status == "completed"' >/dev/null; do
  ((SECONDS < deadline)) || die "the re-run did not finish attempt 2: $(api "runs/$failed_run")"
  sleep 1
done
wait_checks "$fail_sha" "$(checks "$fail_sha" | jq length)" \
  '(map(select(.name == "failing / broken")) | last | .status == "completed" and .conclusion == "failure")'
broken_job="$(job_id "$failed_run" broken)"
deadline=$((SECONDS + 60))
until curl -fsS -o /dev/null "$CI/api/ci/jobs/$broken_job/log/index" 2>/dev/null; do
  ((SECONDS < deadline)) || die "attempt 2 of job $broken_job has no log index"
  sleep 1
done
curl -fsS --compressed "$CI/api/ci/jobs/$broken_job/log" | grep -qx "about to fail" ||
  die "attempt 2 has no log"
log "rerun: attempt 2 ran and failed again"

# Scenario: a push to slow runs a job that the script cancels.
slow_sha="$(printf 'c%.0s' $(seq 40))"
slow_run="$(push slow "$slow_sha")"
log "push to slow started run $slow_run"
wait_job "$slow_run" sleeper running
# Wait for the step to start, so the cancel reaches a running step.
deadline=$((SECONDS + POLL_SECS))
until curl -fsS "$CI/api/ci/jobs/$(job_id "$slow_run" sleeper)/log" 2>/dev/null | grep -q waiting; do
  ((SECONDS < deadline)) || die "the sleeper never started its step"
  sleep 1
done
# A viewer of the live log gets the backlog, then the end when the
# cancel lands. The app stops reading at the end, and so does this
# viewer: wrangler dev leaves a closed hibernatable socket half open.
sleeper_job="$(job_id "$slow_run" sleeper)"
start live node --input-type=module -e '
  const [url, origin] = process.argv.slice(1);
  const socket = new WebSocket(url, { headers: { Origin: origin } });
  socket.onmessage = (event) => {
    console.log(event.data);
    if (JSON.parse(event.data).type === "end") process.exit(0);
  };
  socket.onerror = (event) => { console.log("error " + (event.message ?? "")); process.exit(1); };
  socket.onclose = (event) => { console.log("close " + event.code); process.exit(1); };
' "ws://127.0.0.1:${CI_PORT}/api/ci/jobs/$sleeper_job/live" "$APP_URL"
live_type() {
  jq -R 'fromjson? | select(.type == $type)' --arg type "$1" "$logs/live.log" 2>/dev/null
}
deadline=$((SECONDS + 30))
until [[ -n "$(live_type backlog)" ]]; do
  ((SECONDS < deadline)) || die "no backlog on the live socket: $(cat "$logs/live.log")"
  sleep 0.5
done
live_type backlog | jq -e '.text | contains("waiting")' >/dev/null ||
  die "the backlog lacks the step's output: $(cat "$logs/live.log")"
status="$(curl -sS -o "$tmp/answer" -w '%{http_code}' -X POST "$CI/api/ci/runs/$slow_run/cancel" \
  -H "Origin: $APP_URL" \
  -H "Cookie: __Host-hatchrun_session=$cookie" \
  -H "X-CSRF-Token: $csrf")"
[[ "$status" == 204 ]] || die "cancel answered $status: $(cat "$tmp/answer")"
wait_checks "$slow_sha" 1 'all(.status == "completed" and .conclusion == "cancelled")'
deadline=$((SECONDS + 30))
until [[ -n "$(live_type end)" ]]; do
  ((SECONDS < deadline)) || die "the live socket got no end: $(cat "$logs/live.log")"
  sleep 0.5
done
live_type end | jq -e '.conclusion == "cancelled"' >/dev/null ||
  die "the live socket ended wrong: $(cat "$logs/live.log")"
live_type events | jq -se 'map(.events[]) | any(.type == "step_finished")' >/dev/null ||
  die "the live socket got no step end: $(cat "$logs/live.log")"
log "cancel: the job was cancelled"

# A request from the app, signed in, with the CSRF token. Prints the
# status; the body goes to $tmp/answer. $3, when set, is the body.
app_call() {
  local method=$1 path=$2 body=${3-}
  local args=(-sS -o "$tmp/answer" -w '%{http_code}' -X "$method" "$CI/api/ci/$path"
    -H "Origin: $APP_URL"
    -H "Cookie: __Host-hatchrun_session=$cookie"
    -H "X-CSRF-Token: $csrf")
  if [[ -n "$body" ]]; then
    printf '%s' "$body" | curl "${args[@]}" -H "Content-Type: application/json" --data-binary @-
  else
    curl "${args[@]}"
  fi
}

# Waits until job $2 of run $1 has its log in R2 and prints the log.
r2_log() {
  local run=$1 name=$2 job deadline
  job="$(job_id "$run" "$name")"
  deadline=$((SECONDS + 60))
  until curl -fsS -o /dev/null "$CI/api/ci/jobs/$job/log/index" 2>/dev/null; do
    ((SECONDS < deadline)) || die "job $job ($name) has no log index"
    sleep 1
  done
  curl -fsS --compressed "$CI/api/ci/jobs/$job/log"
}

# Scenario: a repository secret is set, then a push to arm runs a job
# on the arm64 template that echoes it.
secret="e2e-secret-$(openssl rand -hex 8)"
status="$(app_call PUT repos/e2e/demo/secrets/E2E_SECRET "$(jq -nc --arg value "$secret" '{value: $value}')")"
[[ "$status" == 204 ]] || die "setting the secret answered $status: $(cat "$tmp/answer")"
arm_sha="$(printf 'd%.0s' $(seq 40))"
arm_run="$(push arm "$arm_sha")"
log "push to arm started run $arm_run"
wait_checks "$arm_sha" 1 'all(.status == "completed")'
arm="$(check_of "$arm_sha" "arm / arm-hello")"
[[ "$(jq -r .conclusion <<<"$arm")" == success ]] || die "arm / arm-hello: $arm"
curl -fsS "$LAUNCHER/_events" | jq -e 'any(.[]; startswith("launch ") and endswith(" ubuntu-26.04-arm"))' >/dev/null ||
  die "no launch on ubuntu-26.04-arm: $(curl -fsS "$LAUNCHER/_events")"
log "arm: the job ran on ubuntu-26.04-arm and passed"

# Scenario: the secret is masked in the log and stored nowhere in the
# clear. R2 keeps logs gzipped, so gzipped files are searched unpacked.
r2_log "$arm_run" arm-hello >"$tmp/arm.log"
grep -qxF 'secret is ***' "$tmp/arm.log" || die "the log lacks the masked secret: $(head -c 2000 "$tmp/arm.log")"
curl -fsS "$GITHUB/_state" >"$logs/github-state.json"
if grep -rlF -- "$secret" "$tmp/ci-state" "$logs" "$tmp/arm.log" >&2; then
  die "the secret's value is in the files above"
fi
while IFS= read -r -d '' file; do
  if [[ "$(head -c 2 "$file" | od -An -tx1 | tr -d ' \n')" == 1f8b ]] &&
    gzip -dc <"$file" 2>/dev/null | grep -qF -- "$secret"; then
    die "the secret's value is in $file, gzipped"
  fi
done < <(find "$tmp/ci-state" -type f -print0)
sealed="$(psql_run -At <<SQL
SELECT count(*) FILTER (WHERE strpos(sealed, '$secret') > 0) || ' ' || count(*)
FROM ci_secrets WHERE tenant_id = '$TENANT' AND repo_id = 1001 AND name = 'E2E_SECRET';
SQL
)"
[[ "$sealed" == "0 1" ]] || die "the sealed secret row: $sealed (rows holding the value, rows)"
log "secret: masked in the log, in the clear nowhere"

# Scenario: a push to bad fails the workflow whose runs-on names no
# template and runs the other one.
bad_sha="$(printf 'e%.0s' $(seq 40))"
bad_run="$(push bad "$bad_sha")"
log "push to bad started run $bad_run"
wait_checks "$bad_sha" 2 'all(.status == "completed")'
bad="$(check_of "$bad_sha" "bad / config")"
[[ "$(jq -r .conclusion <<<"$bad")" == failure ]] || die "bad / config: $bad"
jq -e '.summary // "" | contains("names no runner template; use one of ubuntu-latest")' <<<"$bad" >/dev/null ||
  die "bad / config does not name the templates: $bad"
bad_ok="$(check_of "$bad_sha" "bad-ok / bad-ok-hello")"
[[ "$(jq -r .conclusion <<<"$bad_ok")" == success ]] || die "bad-ok / bad-ok-hello: $bad_ok"
log "bad: the bad label failed its workflow, the other one passed"

# Scenario: a push to wide runs six jobs, one more than run at once.
# The sixth waits with a check run that says why, and starts once one
# of the five ends and the reconciler wakes it.
wide_sha="$(printf 'f%.0s' $(seq 40))"
wide_run="$(push wide "$wide_sha")"
log "push to wide started run $wide_run"
deadline=$((SECONDS + POLL_SECS))
until api "runs/$wide_run" >"$tmp/wide.json" && jq -e '
  [.jobs[] | select(.status == "launching" or .status == "launched" or .status == "running")] | length == 5
' "$tmp/wide.json" >/dev/null; do
  ((SECONDS < deadline)) || die "wide never had five jobs running: $(api "runs/$wide_run")"
  sleep 1
done
jq -e '[.jobs[] | select(.status == "queued")] | length == 1' "$tmp/wide.json" >/dev/null ||
  die "wide should have one job queued: $(cat "$tmp/wide.json")"
held="$(jq -r '.jobs[] | select(.status == "queued") | .plan_id' "$tmp/wide.json")"
deadline=$((SECONDS + 60))
until check_of "$wide_sha" "wide / $held" |
  jq -e '.summary // "" | contains("has 5 jobs running, the most at once")' >/dev/null; do
  ((SECONDS < deadline)) || die "the held job's check run does not say why: $(check_of "$wide_sha" "wide / $held")"
  sleep 1
done
log "wide: five jobs run, $held waits"
deadline=$((SECONDS + POLL_SECS))
until api "runs/$wide_run" | jq -e --arg id "$held" \
  '.jobs[] | select(.plan_id == $id) | .status == "running" or .status == "completed"' >/dev/null; do
  ((SECONDS < deadline)) || die "$held never ran: $(api "runs/$wide_run")"
  curl -fsS -o /dev/null "$CI/__scheduled?cron=*+*+*+*+*" || die "the reconciler pass failed"
  sleep 3
done
wait_checks "$wide_sha" 6 'all(.status == "completed" and .conclusion == "success")'
log "wide: all six passed"
# Each completed job's billed seconds go to Polar, keyed by job and attempt,
# on a reconciler pass.
usage_ids="$(api "runs/$wide_run" | jq -c '[.jobs[] | "ci:\(.id):1"]')"
deadline=$((SECONDS + 60))
until curl -fsS "$GITHUB/_state" | jq -e --argjson ids "$usage_ids" --arg tenant "$TENANT" '
  [.polar_events[] | select(.name == "ci.usage" and .external_customer_id == $tenant
    and .metadata.template == "ubuntu-latest" and .metadata.seconds >= 60 and .metadata.units == .metadata.seconds) | .external_id] as $sent
  | all($ids[]; . as $id | $sent | index([$id]))
' >/dev/null; do
  ((SECONDS < deadline)) || die "Polar lacks usage for $usage_ids: $(curl -fsS "$GITHUB/_state" | jq -c .polar_events)"
  curl -fsS -o /dev/null "$CI/__scheduled?cron=*+*+*+*+*" || die "the reconciler pass failed"
  sleep 2
done
log "wide: Polar has the usage of all six"

# Whether the last app_call answered status $1 with a body that jq
# filter $2 holds for.
answered() {
  local want=$1
  shift
  [[ "$status" == "$want" ]] && jq -e "$@" "$tmp/answer" >/dev/null
}

# Scenario: environments. Setup first: a second repository of the
# tenant, the environments, and values at the three levels.
psql_run <<SQL
INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, private, updated_at)
VALUES (1002, '$TENANT', $INSTALLATION, 'e2e/other', 'main', false, $now_ms);
SQL
put_ok() {
  local want=$1 path=$2 body=$3
  status="$(app_call PUT "$path" "$body")"
  [[ "$status" == "$want" ]] || die "PUT $path answered $status, not $want: $(cat "$tmp/answer")"
}
put_ok 200 repos/e2e/demo/environments/staging '{"branches":["release/*"],"approval":false}'
put_ok 200 repos/e2e/demo/environments/production '{"branches":[],"approval":true}'
env_hex="$(openssl rand -hex 8)"
env_secret="env-$env_hex"
put_ok 204 orgs/e2e/secrets/ENV_SECRET "$(jq -nc --arg v "org-$(openssl rand -hex 8)" '{value: $v}')"
put_ok 204 repos/e2e/demo/secrets/ENV_SECRET "$(jq -nc --arg v "repo-$(openssl rand -hex 8)" '{value: $v}')"
put_ok 204 repos/e2e/demo/environments/production/secrets/ENV_SECRET "$(jq -nc --arg v "$env_secret" '{value: $v}')"
put_ok 204 orgs/e2e/vars/ENV_VAR '{"value":"org"}'
put_ok 204 repos/e2e/demo/vars/ENV_VAR '{"value":"repo"}'
put_ok 204 repos/e2e/demo/environments/production/vars/ENV_VAR '{"value":"env"}'
only_value="only-$(openssl rand -hex 8)"
status="$(app_call PUT orgs/e2e/secrets/ORG_ONLY "$(jq -nc --arg v "$only_value" '{value: $v, repositories: ["e2e/nope"]}')")"
[[ "$status" == 422 ]] && jq -e '.error | contains("e2e/nope")' "$tmp/answer" >/dev/null ||
  die "an unknown repository answered $status: $(cat "$tmp/answer")"
put_ok 204 orgs/e2e/secrets/ORG_ONLY "$(jq -nc --arg v "$only_value" '{value: $v, repositories: ["e2e/other"]}')"
status="$(app_call GET orgs/e2e/secrets)"
answered 200 --arg name ORG_ONLY '[.[] | select(.name == $name)] | length == 1 and .[0].repositories == ["e2e/other"]' ||
  die "ORG_ONLY is not listed for e2e/other: $status $(cat "$tmp/answer")"
log "environments: set up"

# A push to guard: the staging environment allows release/* only.
guard_sha="$(printf '3%.0s' $(seq 40))"
launches_before="$(curl -fsS "$LAUNCHER/_events" | jq '[.[] | select(startswith("launch "))] | length')"
guard_run="$(push guard "$guard_sha")"
log "push to guard started run $guard_run"
wait_checks "$guard_sha" 1 'all(.status == "completed")'
guard="$(check_of "$guard_sha" "guard / guarded")"
[[ "$(jq -r .conclusion <<<"$guard")" == failure ]] || die "guard / guarded: $guard"
jq -e '((.summary // "") + (.text // "")) | contains("Environment `staging` allows only branches release/*; this run is on refs/heads/guard.")' \
  <<<"$guard" >/dev/null || die "guard / guarded lacks the branch message: $guard"
[[ "$(curl -fsS "$LAUNCHER/_events" | jq '[.[] | select(startswith("launch "))] | length')" == "$launches_before" ]] ||
  die "the refused job launched an instance"
log "guard: the branch pattern refused the job"

# A push to deploy: both jobs wait for approval, nothing launches.
deploy_sha="$(printf '4%.0s' $(seq 40))"
launches_before="$(curl -fsS "$LAUNCHER/_events" | jq '[.[] | select(startswith("launch "))] | length')"
deploy_run="$(push deploy "$deploy_sha")"
log "push to deploy started run $deploy_run"
deadline=$((SECONDS + POLL_SECS))
until api "runs/$deploy_run" | jq -e '[.jobs[] | select(.status == "queued" and .held == "approval")] | length == 2' >/dev/null; do
  ((SECONDS < deadline)) || die "the deploy jobs are not held for approval: $(api "runs/$deploy_run")"
  sleep 1
done
deadline=$((SECONDS + 60))
until check_of "$deploy_sha" "deploy / deploy-a" |
  jq -e '.summary // "" | contains("Waiting for approval to use environment `production`.")' >/dev/null; do
  ((SECONDS < deadline)) || die "deploy-a's check run does not ask for approval: $(check_of "$deploy_sha" "deploy / deploy-a")"
  sleep 1
done
sleep 10
[[ "$(curl -fsS "$LAUNCHER/_events" | jq '[.[] | select(startswith("launch "))] | length')" == "$launches_before" ]] ||
  die "a held job launched an instance"
log "deploy: both jobs wait for approval, nothing launched"

# Approve one job and reject the other.
deploy_a="$(job_id "$deploy_run" deploy-a)"
deploy_b="$(job_id "$deploy_run" deploy-b)"
status="$(app_call POST "jobs/$deploy_a/approve")"
[[ "$status" == 204 ]] || die "approve answered $status: $(cat "$tmp/answer")"
status="$(app_call POST "jobs/$deploy_b/reject")"
[[ "$status" == 204 ]] || die "reject answered $status: $(cat "$tmp/answer")"
status="$(app_call POST "jobs/$deploy_a/approve")"
[[ "$status" == 409 ]] || die "a second approve answered $status, not 409: $(cat "$tmp/answer")"
wait_checks "$deploy_sha" "$(checks "$deploy_sha" | jq length)" \
  '(map(select(.name == "deploy / deploy-a")) | last | .status == "completed")
   and (map(select(.name == "deploy / deploy-b")) | last | .status == "completed")'
deploy_a_check="$(check_of "$deploy_sha" "deploy / deploy-a")"
[[ "$(jq -r .conclusion <<<"$deploy_a_check")" == success ]] || die "deploy / deploy-a: $deploy_a_check"
deploy_b_check="$(check_of "$deploy_sha" "deploy / deploy-b")"
[[ "$(jq -r .conclusion <<<"$deploy_b_check")" == failure ]] || die "deploy / deploy-b: $deploy_b_check"
jq -e '((.summary // "") + (.text // "")) | contains("Rejected by e2e")' <<<"$deploy_b_check" >/dev/null ||
  die "deploy / deploy-b lacks the rejection: $deploy_b_check"
api "runs/$deploy_run" | jq -e '.jobs[] | select(.name == "deploy-a")
  | .approval.by == "e2e" and .approval.decision == "approved"' >/dev/null ||
  die "deploy-a has no approval: $(api "runs/$deploy_run")"
log "deploy: deploy-a approved and passed, deploy-b rejected"

# The approved job saw the environment's secret and var, and not the
# organization secret limited to another repository.
r2_log "$deploy_run" deploy-a >"$tmp/deploy-a.log"
want_hash="$(printf '%s' "$env_secret" | openssl dgst -sha256 -r | cut -d' ' -f1)"
grep -qxF "$want_hash" "$tmp/deploy-a.log" || die "deploy-a lacks the environment secret's hash: $(head -c 2000 "$tmp/deploy-a.log")"
grep -qxF 'var is env' "$tmp/deploy-a.log" || die "deploy-a lacks the environment var: $(head -c 2000 "$tmp/deploy-a.log")"
grep -qxF 'org only is []' "$tmp/deploy-a.log" || die "deploy-a saw the org-only secret: $(head -c 2000 "$tmp/deploy-a.log")"
log "environments: the environment's values won, the org-only secret stayed out"

# Scenario: a tenant whose subscription was canceled gets one billing
# check run for a push and nothing runs; a tenant that never subscribed
# gets nothing at all, not even a config read.
tenant_push() {
  local installation=$1 org=$2 org_id=$3 repo_id=$4 sha=$5
  jq -c --arg sha "$sha" --arg org "$org" \
    --argjson installation "$installation" --argjson org_id "$org_id" --argjson repo_id "$repo_id" '
    .after = $sha
    | .installation.id = $installation
    | .repository.id = $repo_id
    | .repository.full_name = "\($org)/demo"
    | .repository.owner = { login: $org, id: $org_id }
  ' "$fixtures/push.json"
}
lapsed_sha="$(printf '1%.0s' $(seq 40))"
deliver push "$(tenant_push 43 e2e2 2 2001 "$lapsed_sha")" 202 | jq -e '.runs == []' >/dev/null ||
  die "the lapsed tenant's push started a run: $(cat "$tmp/answer")"
wait_checks "$lapsed_sha" 1 \
  'all(.name == "hatchrun / billing" and .status == "completed" and .conclusion == "action_required")'
runs_2001="$(psql_run -At -c "SELECT count(*) FROM ci_runs WHERE repo_id = 2001")"
[[ "$runs_2001" == 0 ]] || die "the lapsed tenant has $runs_2001 runs"
never_sha="$(printf '2%.0s' $(seq 40))"
deliver push "$(tenant_push 44 e2e3 3 3001 "$never_sha")" 200 | jq -e '.ignored' >/dev/null ||
  die "the never subscribed tenant's push was not ignored: $(cat "$tmp/answer")"
curl -fsS "$GITHUB/_state" >"$tmp/state.json"
jq -e --arg sha "$never_sha" '[.check_runs[] | select(.head_sha == $sha)] | length == 0' "$tmp/state.json" >/dev/null ||
  die "the never subscribed tenant got check runs: $(checks "$never_sha")"
jq -e '(.contents | index("e2e3/demo")) == null and (.contents | index("e2e2/demo")) != null' \
  "$tmp/state.json" >/dev/null || die "config reads: $(jq -c .contents "$tmp/state.json")"
# The card state as the billing page reads it.
status="$(app_call GET orgs/e2e/billing)"
answered 200 '.card == true' || die "the billing view of e2e answered $status: $(cat "$tmp/answer")"
status="$(app_call GET orgs/e2e2/billing)"
answered 200 '.card == false' || die "the billing view of e2e2 answered $status: $(cat "$tmp/answer")"
log "billing: the card state of both tenants read as expected"

# Fourteen instances: build, test, broken twice, sleeper, arm-hello,
# bad-ok-hello, the six naps and deploy-a. Wait for the last agent to exit.
launches=14
deadline=$((SECONDS + 60))
until curl -fsS "$LAUNCHER/_events" | jq -e --argjson n "$launches" '[.[] | select(startswith("exit "))] | length >= $n' >/dev/null; do
  ((SECONDS < deadline)) || die "agents still running: $(curl -fsS "$LAUNCHER/_events")"
  sleep 1
done

# Every instance had its metadata turned off and the change seen
# applied before the job ran. The claim answers the job document only
# after an applied read, so the order of these events shows it.
curl -fsS "$LAUNCHER/_events" >"$tmp/events.json"
jq -e '
  def at($e): (index([$e]) // -1);
  . as $ev
  | [$ev[] | select(startswith("launch ")) | split(" ")[1]] as $ids
  | ($ids | length) > 0
    and all($ids[]; . as $id
      | ($ev | at("metadata-off " + $id)) as $off
      | ($ev | at("metadata-read " + $id)) as $read
      | ([$ev | to_entries[] | select(.value | startswith("exit " + $id + " ")) | .key] | first // -1) as $exit
      | $off >= 0 and $read > $off and $exit > $read)
' "$tmp/events.json" >/dev/null || die "metadata order: $(cat "$tmp/events.json")"
# Exit codes: 0 for passing jobs, 1 for the failed and cancelled ones.
jq -e '[.[] | select(startswith("exit ")) | split(" ")[2]] | all(. == "0" or . == "1")' \
  "$tmp/events.json" >/dev/null || die "an agent exited badly: $(cat "$tmp/events.json")"
[[ "$(jq '[.[] | select(startswith("launch "))] | length' "$tmp/events.json")" == "$launches" ]] ||
  die "expected $launches launches: $(cat "$tmp/events.json")"
jq -e '[.[] | select(endswith(" 0") and startswith("exit "))] | length == 11' "$tmp/events.json" >/dev/null ||
  die "expected the eleven passing jobs to exit 0: $(cat "$tmp/events.json")"
# The App's permissions are read once per installation and kept in KV,
# so the claims did not each read them.
reads="$(curl -fsS "$GITHUB/_state" | jq '[.installation_reads[] | select(. == 42)] | length')"
((reads >= 1 && reads < launches)) || die "installation 42 was read $reads times for $launches claims"

# One reconciler pass over what the scenarios left: nothing is stuck,
# so it has nothing to fix, and it must not fail.
curl -fsS -o /dev/null "$CI/__scheduled?cron=*+*+*+*+*" || die "the reconciler pass failed"
sleep 2
if grep -a 'ERROR' "$logs/ci.log" "$logs/broker.log" >/dev/null; then
  die "the Workers logged errors: $(grep -a -A3 'ERROR' "$logs/ci.log" "$logs/broker.log")"
fi

echo "e2e passed"
