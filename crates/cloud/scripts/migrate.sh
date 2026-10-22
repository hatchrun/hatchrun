#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

# Applies crates/cloud/migrations to the Postgres in DATABASE_MIGRATE_URL,
# in file name order, each file once. schema_migrations records the ones
# applied.
#
# The whole pass runs in one transaction that first takes an ACCESS
# EXCLUSIVE lock on schema_migrations. Two runners at once (deploy-cloud
# and deploy-ci) therefore take turns: the second waits for the lock,
# then sees the first one's migrations as applied. PlanetScale Postgres
# has no advisory locks, so the table lock does that job. A failing file
# rolls the whole pass back and the next run starts again from the
# first unapplied file.
#
# The deploy workflow runs this as the migrate role before the Worker
# deploys. Locally:
#
#   DATABASE_MIGRATE_URL=postgres://postgres:postgres@localhost:5432/postgres \
#     crates/cloud/scripts/migrate.sh

set -euo pipefail

: "${DATABASE_MIGRATE_URL:?set DATABASE_MIGRATE_URL}"
dir="$(cd "$(dirname "$0")/../migrations" && pwd)"

psql() {
  command psql "$DATABASE_MIGRATE_URL" --no-psqlrc --quiet -v ON_ERROR_STOP=1 "$@"
}

# Two runners that find the table missing can both try to create it, and
# the loser fails on a catalog unique index. It finds the table on the
# second try.
for try in 1 2 3; do
  if psql -c "SET client_min_messages = warning;
CREATE TABLE IF NOT EXISTS schema_migrations (
  name text PRIMARY KEY,
  applied_at timestamptz NOT NULL DEFAULT now()
)"; then
    break
  elif [[ "$try" == 3 ]]; then
    exit 1
  fi
  sleep 1
done

# One psql session reads this script from stdin. \gset and \if choose,
# after the lock is held, which files still need to run.
{
  echo "BEGIN;"
  echo "LOCK TABLE schema_migrations IN ACCESS EXCLUSIVE MODE;"
  for file in "$dir"/*.sql; do
    name="$(basename "$file")"
    echo "\\set name '$name'"
    echo "SELECT EXISTS (SELECT 1 FROM schema_migrations WHERE name = :'name') AS applied \\gset"
    echo "\\if :applied"
    echo "\\else"
    echo "\\echo Applying $name"
    echo "\\i $file"
    echo "INSERT INTO schema_migrations (name) VALUES (:'name');"
    echo "\\endif"
  done
  echo "COMMIT;"
} | psql
