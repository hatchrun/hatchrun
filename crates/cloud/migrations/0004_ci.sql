-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The tables of hatchrun-ci (crates/ci), hatchrun's own replacement for
-- GitHub Actions. Times are Unix milliseconds. Runs, jobs and delivery
-- ids are kept 30 days.

-- CI runs only for tenants with this set, until billing lands.
ALTER TABLE tenants ADD COLUMN ci_enabled boolean NOT NULL DEFAULT false;

CREATE TABLE ci_repos (
  id bigint PRIMARY KEY,                       -- GitHub repository id
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  installation_id bigint NOT NULL,
  full_name text NOT NULL,
  default_branch text NOT NULL,
  private boolean NOT NULL,
  updated_at bigint NOT NULL
);
CREATE UNIQUE INDEX ci_repos_name ON ci_repos (lower(full_name));

CREATE TABLE ci_deliveries (
  guid text PRIMARY KEY,                       -- X-GitHub-Delivery, or schedule:/dispatch: keys
  event text NOT NULL,
  received_at bigint NOT NULL
);
CREATE INDEX ci_deliveries_age ON ci_deliveries (received_at);

CREATE TABLE ci_schedules (
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  cron text NOT NULL,
  PRIMARY KEY (repo_id, workflow, cron)
);

CREATE TABLE ci_run_numbers (
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  last bigint NOT NULL,
  PRIMARY KEY (repo_id, workflow)
);

CREATE TABLE ci_runs (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  run_number bigint NOT NULL,
  run_attempt bigint NOT NULL DEFAULT 1,
  delivery text NOT NULL,
  event text NOT NULL,
  ref text NOT NULL,
  sha text NOT NULL,
  actor text NOT NULL,
  status text NOT NULL CHECK (status IN ('queued', 'in_progress', 'completed')),
  conclusion text CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  completed_at bigint,
  expires_at bigint NOT NULL,                  -- created_at + 30 days
  UNIQUE (delivery, workflow)
);
CREATE INDEX ci_runs_repo ON ci_runs (repo_id, created_at DESC);
CREATE INDEX ci_runs_expiry ON ci_runs (expires_at);

CREATE TABLE ci_jobs (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  run_id bigint NOT NULL REFERENCES ci_runs (id) ON DELETE CASCADE,
  plan_id text NOT NULL,                       -- PlannedJob::id
  name text NOT NULL,                          -- PlannedJob::name
  attempt bigint NOT NULL DEFAULT 1,
  status text NOT NULL CHECK (status IN
    ('waiting', 'queued', 'launching', 'launched', 'running', 'completed')),
  conclusion text CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  timeout_minutes bigint NOT NULL,
  instance_id text,
  launch_attempts bigint NOT NULL DEFAULT 0,
  launch_error text,
  next_launch_at bigint,
  heartbeat_at bigint,
  started_at bigint,
  completed_at bigint,
  steps jsonb NOT NULL DEFAULT '[]',           -- Vec<StepSummary>
  log_key text,
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  UNIQUE (run_id, plan_id)
);
CREATE INDEX ci_jobs_open ON ci_jobs (status, updated_at) WHERE status <> 'completed';
CREATE INDEX ci_jobs_instance ON ci_jobs (instance_id) WHERE instance_id IS NOT NULL;

CREATE TABLE ci_reconciler (
  id bigint PRIMARY KEY CHECK (id = 1),
  lease_until bigint NOT NULL,
  last_minute bigint                           -- last schedule minute fired, Unix ms
);
INSERT INTO ci_reconciler (id, lease_until) VALUES (1, 0);
