#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

# Applies crates/cloud/migrations to the Postgres in DATABASE_MIGRATE_URL,
# in file name order, each file once. schema_migrations records the ones
# applied. Each file runs in its own transaction with its record, so a
# failing file changes nothing and the next run starts from it.
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

psql -c "SET client_min_messages = warning;
CREATE TABLE IF NOT EXISTS schema_migrations (
  name text PRIMARY KEY,
  applied_at timestamptz NOT NULL DEFAULT now()
)"

for file in "$dir"/*.sql; do
  name="$(basename "$file")"
  applied="$(psql -At -v name="$name" <<'SQL'
SELECT 1 FROM schema_migrations WHERE name = :'name'
SQL
)"
  if [[ -n "$applied" ]]; then
    continue
  fi
  echo "Applying $name"
  psql --single-transaction -v name="$name" -f "$file" -f - <<'SQL'
INSERT INTO schema_migrations (name) VALUES (:'name');
SQL
done
