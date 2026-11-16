-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Partition ci_runs, ci_jobs, ci_usage and ci_audit so retention drops
-- whole months instead of deleting rows one by one.
--
-- ci_runs and ci_jobs are partitioned by LIST on a tier column (30
-- or 90, the retention days the tenant had when the run was recorded),
-- each tier sub-partitioned by RANGE on the month of the row's
-- creation time in Unix milliseconds. Jobs take the tier and the month
-- of their run, so a run and its jobs always sit in partitions that
-- drop together. ci_usage keeps 90 days for every tenant and ci_audit
-- keeps 400 days, so both are partitioned by RANGE on the month of
-- their own time only.
--
-- Primary and unique keys grow the partition columns, as Postgres
-- requires. The foreign keys into and between these tables cannot keep
-- them and are dropped; the reconciler deletes the dependents of a
-- dropped month in the same pass:
--
--   ci_jobs_run_id_fkey (ci_jobs.run_id to ci_runs.id)
--   ci_artifacts_run_id_fkey (ci_artifacts.run_id to ci_runs.id)
--   ci_artifacts_job_id_fkey (ci_artifacts.job_id to ci_jobs.id)
--   ci_concurrency_run_id_fkey (ci_concurrency.run_id to ci_runs.id)
--   ci_concurrency_job_id_fkey (ci_concurrency.job_id to ci_jobs.id)
--
-- Each table is rebuilt under a _new name, loaded from the old one
-- (the tier comes from expires_at minus created_at, which the insert
-- set from the tenant's tier), swapped by rename, and reindexed. The
-- other tables of 0009 and 0010 stay plain: ci_cache_entries keys by
-- tenant and repository with its own idle eviction, while ci_artifacts
-- and ci_concurrency rows name their run, so the upkeep deletes the
-- rows of a dropped month before it drops the partition.

-- Month boundaries are UTC, as the reconciler's upkeep makes them, so
-- the partitions made here never overlap the ones it adds later.
SET LOCAL TIME ZONE 'UTC';

ALTER TABLE ci_jobs DROP CONSTRAINT IF EXISTS ci_jobs_run_id_fkey;
ALTER TABLE ci_artifacts DROP CONSTRAINT IF EXISTS ci_artifacts_run_id_fkey;
ALTER TABLE ci_artifacts DROP CONSTRAINT IF EXISTS ci_artifacts_job_id_fkey;
ALTER TABLE ci_concurrency DROP CONSTRAINT IF EXISTS ci_concurrency_run_id_fkey;
ALTER TABLE ci_concurrency DROP CONSTRAINT IF EXISTS ci_concurrency_job_id_fkey;

-- ci_runs, by tier, then by month of created_at. The tier is what
-- expires_at minus created_at says: the insert set expires_at from the
-- tenant's tier, so the row keeps it.
CREATE TABLE ci_runs_new (
  id bigint NOT NULL,
  tenant_id text NOT NULL CONSTRAINT ci_runs_tenant_id_fkey REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL CONSTRAINT ci_runs_repo_id_fkey REFERENCES ci_repos (id) ON DELETE CASCADE,
  workflow text NOT NULL,
  run_number bigint NOT NULL,
  run_attempt bigint NOT NULL DEFAULT 1,
  delivery text NOT NULL,
  event text NOT NULL,
  ref text NOT NULL,
  sha text NOT NULL,
  actor text NOT NULL,
  status text NOT NULL CONSTRAINT ci_runs_status_check CHECK (status IN ('queued', 'in_progress', 'completed')),
  conclusion text CONSTRAINT ci_runs_conclusion_check CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  completed_at bigint,
  tier smallint NOT NULL CONSTRAINT ci_runs_tier_check CHECK (tier IN (30, 90)),
  message text,
  base_ref text,
  pull_number bigint,
  head_ref text,
  trust text NOT NULL CONSTRAINT ci_runs_trust_check CHECK (trust IN ('trusted', 'fork')),
  head_repo_id bigint,
  head_repo_name text,
  author_login text NOT NULL,
  author_association text NOT NULL,
  gate text NOT NULL CONSTRAINT ci_runs_gate_check CHECK (gate IN ('none', 'fork_approval')),
  gate_status text NOT NULL
    CONSTRAINT ci_runs_gate_status_check CHECK (gate_status IN ('none', 'pending', 'deferred', 'approved', 'expired', 'cancelled')),
  gate_by text,
  gate_at bigint,
  gate_check_run_id bigint,
  gate_card_dirty boolean NOT NULL DEFAULT false,
  gate_scanned_at bigint NOT NULL DEFAULT 0,
  head_sha text,
  lints text,
  issue_number bigint,
  comment_id bigint,
  command_name text,
  command_reaction text,
  PRIMARY KEY (id, tier, created_at),
  UNIQUE (delivery, workflow, tier, created_at),
  CONSTRAINT ci_runs_command_reaction_known CHECK (command_reaction IN ('default', 'rocket')),
  CONSTRAINT ci_runs_command_whole CHECK ((comment_id IS NULL) = (command_name IS NULL)
    AND (comment_id IS NULL) = (command_reaction IS NULL))
) PARTITION BY LIST (tier);

CREATE TABLE ci_runs_t30 PARTITION OF ci_runs_new FOR VALUES IN (30) PARTITION BY RANGE (created_at);
CREATE TABLE ci_runs_t90 PARTITION OF ci_runs_new FOR VALUES IN (90) PARTITION BY RANGE (created_at);

DO $$
DECLARE
  t int;
  first_ts timestamptz;
  last_ts timestamptz;
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  FOREACH t IN ARRAY ARRAY[30, 90] LOOP
    SELECT date_trunc('month', to_timestamp(MIN(created_at) / 1000.0)),
           date_trunc('month', to_timestamp(MAX(created_at) / 1000.0))
      INTO first_ts, last_ts
      FROM ci_runs
      WHERE CASE WHEN (expires_at - created_at) >= 90 * 86400000::bigint THEN 90 ELSE 30 END = t;
    IF first_ts IS NULL THEN CONTINUE; END IF;
    cur := first_ts;
    WHILE cur <= last_ts LOOP
      lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
      hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
      EXECUTE format('CREATE TABLE %I PARTITION OF %I FOR VALUES FROM (%s) TO (%s)',
        'ci_runs_t' || t || '_' || to_char(cur, 'YYYYMM'), 'ci_runs_t' || t, lo, hi);
      cur := cur + INTERVAL '1 month';
    END LOOP;
  END LOOP;
END $$;

INSERT INTO ci_runs_new
  (id, tenant_id, repo_id, workflow, run_number, run_attempt, delivery, event, ref, sha,
   actor, status, conclusion, created_at, updated_at, completed_at, tier, message, base_ref,
   pull_number, head_ref, trust, head_repo_id, head_repo_name, author_login, author_association,
   gate, gate_status, gate_by, gate_at, gate_check_run_id, gate_card_dirty, gate_scanned_at,
   head_sha, lints, issue_number, comment_id, command_name, command_reaction)
  SELECT id, tenant_id, repo_id, workflow, run_number, run_attempt, delivery, event, ref, sha,
   actor, status, conclusion, created_at, updated_at, completed_at,
   CASE WHEN (expires_at - created_at) >= 90 * 86400000::bigint THEN 90 ELSE 30 END,
   message, base_ref, pull_number, head_ref, trust, head_repo_id, head_repo_name, author_login,
   author_association, gate, gate_status, gate_by, gate_at, gate_check_run_id, gate_card_dirty,
   gate_scanned_at, head_sha, lints, issue_number, comment_id, command_name, command_reaction
  FROM ci_runs;

-- The ids the old sequence handed out stay used even when retention
-- deleted their rows: run and job ids name Durable Objects and R2
-- snapshots, so the new sequence starts past the old high-water mark.
CREATE TEMP TABLE ci_runs_id_mark AS
  SELECT GREATEST(
    COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ci_runs', 'id')::regclass), 0),
    COALESCE((SELECT MAX(id) FROM ci_runs), 0)) AS hw;
DROP TABLE ci_runs;
ALTER TABLE ci_runs_new RENAME TO ci_runs;
ALTER TABLE ci_runs RENAME CONSTRAINT ci_runs_new_pkey TO ci_runs_pkey;
ALTER TABLE ci_runs RENAME CONSTRAINT ci_runs_new_delivery_workflow_tier_created_at_key
  TO ci_runs_delivery_workflow_key;

CREATE SEQUENCE ci_runs_id_seq;
SELECT setval('ci_runs_id_seq', GREATEST((SELECT hw FROM ci_runs_id_mark), 1),
  (SELECT hw FROM ci_runs_id_mark) > 0);
DROP TABLE ci_runs_id_mark;
ALTER TABLE ci_runs ALTER COLUMN id SET DEFAULT nextval('ci_runs_id_seq');

CREATE INDEX ci_runs_repo ON ci_runs (repo_id, created_at DESC);
CREATE INDEX ci_runs_tenant_created ON ci_runs (tenant_id, created_at);
CREATE INDEX ci_runs_tenant_completed ON ci_runs (tenant_id, completed_at)
  WHERE completed_at IS NOT NULL;
CREATE INDEX ci_runs_pull ON ci_runs (repo_id, pull_number, created_at DESC)
  WHERE pull_number IS NOT NULL;
CREATE INDEX ci_runs_gate ON ci_runs (repo_id, created_at, id)
  WHERE gate_status IN ('pending', 'deferred');
CREATE INDEX ci_runs_gate_unfinished ON ci_runs (repo_id, gate_scanned_at, gate_at, id)
  WHERE (gate_status IN ('expired', 'cancelled') AND status <> 'completed')
     OR (gate_status IN ('approved', 'expired', 'cancelled')
         AND gate_check_run_id IS NOT NULL AND gate_card_dirty);
CREATE INDEX ci_runs_gate_commit ON ci_runs (repo_id, pull_number, (COALESCE(head_sha, sha)))
  WHERE gate_status IN ('pending', 'deferred');
CREATE INDEX ci_runs_issue_active ON ci_runs (repo_id, issue_number)
  WHERE issue_number IS NOT NULL AND status <> 'completed';

-- ci_jobs, by the tier and the creation month of their run.
CREATE TABLE ci_jobs_new (
  id bigint NOT NULL,
  run_id bigint NOT NULL,
  tier smallint NOT NULL CONSTRAINT ci_jobs_tier_check CHECK (tier IN (30, 90)),
  run_created_at bigint NOT NULL,
  plan_id text NOT NULL,
  name text NOT NULL,
  attempt bigint NOT NULL DEFAULT 1,
  status text NOT NULL CONSTRAINT ci_jobs_status_check CHECK (status IN
    ('waiting', 'queued', 'launching', 'launched', 'running', 'completed')),
  conclusion text CONSTRAINT ci_jobs_conclusion_check CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
  timeout_minutes bigint NOT NULL,
  instance_id text,
  launch_attempts bigint NOT NULL DEFAULT 0,
  launch_error text,
  next_launch_at bigint,
  heartbeat_at bigint,
  started_at bigint,
  completed_at bigint,
  steps jsonb NOT NULL DEFAULT '[]',
  log_key text,
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  template text,
  token_permissions jsonb NOT NULL,
  secret_refs jsonb NOT NULL,
  held text CONSTRAINT ci_jobs_held_check CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval', 'max_parallel', 'group',
    'capacity', 'fork_approval', 'fork_limit')),
  environment text,
  approval_attempt bigint,
  approval_decision text CONSTRAINT ci_jobs_approval_decision_check CHECK (approval_decision IN ('approved', 'rejected')),
  approval_by text,
  approval_at bigint,
  strategy_group text,
  max_parallel integer CONSTRAINT ci_jobs_max_parallel_check CHECK (max_parallel IS NULL OR max_parallel >= 1),
  capacity_attempts bigint NOT NULL DEFAULT 0,
  capacity_since bigint,
  needs jsonb NOT NULL,
  environment_url text,
  superseded boolean NOT NULL DEFAULT false,
  cache_scopes text,
  PRIMARY KEY (id, tier, run_created_at),
  UNIQUE (run_id, plan_id, tier, run_created_at),
  CONSTRAINT ci_jobs_max_parallel_group_check CHECK (max_parallel IS NULL OR strategy_group IS NOT NULL)
) PARTITION BY LIST (tier);

CREATE TABLE ci_jobs_t30 PARTITION OF ci_jobs_new FOR VALUES IN (30) PARTITION BY RANGE (run_created_at);
CREATE TABLE ci_jobs_t90 PARTITION OF ci_jobs_new FOR VALUES IN (90) PARTITION BY RANGE (run_created_at);

DO $$
DECLARE
  t int;
  first_ts timestamptz;
  last_ts timestamptz;
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  FOREACH t IN ARRAY ARRAY[30, 90] LOOP
    SELECT date_trunc('month', to_timestamp(MIN(r.created_at) / 1000.0)),
           date_trunc('month', to_timestamp(MAX(r.created_at) / 1000.0))
      INTO first_ts, last_ts
      -- Every run month, not only the months with jobs yet: a retried
      -- delivery adds the missing jobs of a run later, in its month.
      FROM ci_runs r WHERE r.tier = t;
    IF first_ts IS NULL THEN CONTINUE; END IF;
    cur := first_ts;
    WHILE cur <= last_ts LOOP
      lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
      hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
      EXECUTE format('CREATE TABLE %I PARTITION OF %I FOR VALUES FROM (%s) TO (%s)',
        'ci_jobs_t' || t || '_' || to_char(cur, 'YYYYMM'), 'ci_jobs_t' || t, lo, hi);
      cur := cur + INTERVAL '1 month';
    END LOOP;
  END LOOP;
END $$;

INSERT INTO ci_jobs_new
  (id, run_id, tier, run_created_at, plan_id, name, attempt, status, conclusion, timeout_minutes,
   instance_id, launch_attempts, launch_error, next_launch_at, heartbeat_at, started_at,
   completed_at, steps, log_key, created_at, updated_at, template, token_permissions,
   secret_refs, held, environment, approval_attempt, approval_decision, approval_by,
   approval_at, strategy_group, max_parallel, capacity_attempts, capacity_since, needs,
   environment_url, superseded, cache_scopes)
  SELECT j.id, j.run_id, r.tier, r.created_at, j.plan_id, j.name, j.attempt, j.status,
   j.conclusion, j.timeout_minutes, j.instance_id, j.launch_attempts, j.launch_error,
   j.next_launch_at, j.heartbeat_at, j.started_at, j.completed_at, j.steps, j.log_key,
   j.created_at, j.updated_at, j.template, j.token_permissions, j.secret_refs, j.held,
   j.environment, j.approval_attempt, j.approval_decision, j.approval_by, j.approval_at,
   j.strategy_group, j.max_parallel, j.capacity_attempts, j.capacity_since, j.needs,
   j.environment_url, j.superseded, j.cache_scopes
  FROM ci_jobs j JOIN ci_runs r ON r.id = j.run_id;

-- The ids the old sequence handed out stay used even when retention
-- deleted their rows: run and job ids name Durable Objects and R2
-- snapshots, so the new sequence starts past the old high-water mark.
CREATE TEMP TABLE ci_jobs_id_mark AS
  SELECT GREATEST(
    COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ci_jobs', 'id')::regclass), 0),
    COALESCE((SELECT MAX(id) FROM ci_jobs), 0)) AS hw;
DROP TABLE ci_jobs;
ALTER TABLE ci_jobs_new RENAME TO ci_jobs;
ALTER TABLE ci_jobs RENAME CONSTRAINT ci_jobs_new_pkey TO ci_jobs_pkey;
ALTER TABLE ci_jobs RENAME CONSTRAINT ci_jobs_new_run_id_plan_id_tier_run_created_at_key
  TO ci_jobs_run_id_plan_id_key;

CREATE SEQUENCE ci_jobs_id_seq;
SELECT setval('ci_jobs_id_seq', GREATEST((SELECT hw FROM ci_jobs_id_mark), 1),
  (SELECT hw FROM ci_jobs_id_mark) > 0);
DROP TABLE ci_jobs_id_mark;
ALTER TABLE ci_jobs ALTER COLUMN id SET DEFAULT nextval('ci_jobs_id_seq');

CREATE INDEX ci_jobs_open ON ci_jobs (status, updated_at) WHERE status <> 'completed';
CREATE INDEX ci_jobs_instance ON ci_jobs (instance_id) WHERE instance_id IS NOT NULL;
CREATE INDEX ci_jobs_started ON ci_jobs (started_at) WHERE started_at IS NOT NULL;
CREATE INDEX ci_jobs_strategy_group ON ci_jobs (run_id, strategy_group)
  WHERE strategy_group IS NOT NULL;

-- ci_usage, by month of ended_at. Every tenant keeps 90 days.
CREATE TABLE ci_usage_new (
  job_id bigint NOT NULL,
  attempt bigint NOT NULL,
  tenant_id text NOT NULL CONSTRAINT ci_usage_tenant_id_fkey REFERENCES tenants (id) ON DELETE CASCADE,
  template text NOT NULL,
  seconds bigint NOT NULL CONSTRAINT ci_usage_seconds_check CHECK (seconds >= 60),
  units bigint NOT NULL CONSTRAINT ci_usage_units_check CHECK (units >= 0),
  ended_at bigint NOT NULL,
  sent_at bigint,
  rejected_at bigint,
  reject_reason text,
  window_id bigint CONSTRAINT ci_usage_window_id_fkey REFERENCES ci_usage_windows (id) ON DELETE SET NULL,
  PRIMARY KEY (job_id, attempt, ended_at)
) PARTITION BY RANGE (ended_at);

DO $$
DECLARE
  first_ts timestamptz;
  last_ts timestamptz;
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  SELECT date_trunc('month', to_timestamp(MIN(ended_at) / 1000.0)),
         date_trunc('month', to_timestamp(MAX(ended_at) / 1000.0))
    INTO first_ts, last_ts
    FROM ci_usage;
  IF first_ts IS NULL THEN RETURN; END IF;
  cur := first_ts;
  WHILE cur <= last_ts LOOP
    lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
    hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
    EXECUTE format('CREATE TABLE %I PARTITION OF ci_usage_new FOR VALUES FROM (%s) TO (%s)',
      'ci_usage_' || to_char(cur, 'YYYYMM'), lo, hi);
    cur := cur + INTERVAL '1 month';
  END LOOP;
END $$;

INSERT INTO ci_usage_new
  (job_id, attempt, tenant_id, template, seconds, units, ended_at,
   sent_at, rejected_at, reject_reason, window_id)
  SELECT job_id, attempt, tenant_id, template, seconds, units, ended_at,
   sent_at, rejected_at, reject_reason, window_id
  FROM ci_usage;

DROP TABLE ci_usage;
ALTER TABLE ci_usage_new RENAME TO ci_usage;
ALTER TABLE ci_usage RENAME CONSTRAINT ci_usage_new_pkey TO ci_usage_pkey;

CREATE INDEX ci_usage_month ON ci_usage (tenant_id, ended_at);
CREATE INDEX ci_usage_unsent ON ci_usage (ended_at)
  WHERE sent_at IS NULL AND rejected_at IS NULL;
CREATE INDEX ci_usage_unwindowed ON ci_usage (tenant_id, ended_at)
  WHERE sent_at IS NULL AND rejected_at IS NULL AND window_id IS NULL;

-- ci_audit, by month of at. Audit rows keep 400 days whatever the
-- tenant's tier is.
CREATE TABLE ci_audit_new (
  id bigint NOT NULL,
  tenant_id text NOT NULL CONSTRAINT ci_audit_tenant_id_fkey REFERENCES tenants (id) ON DELETE CASCADE,
  at bigint NOT NULL,
  actor text NOT NULL,
  action text NOT NULL CONSTRAINT ci_audit_action_check CHECK (action IN
    ('secret.put', 'secret.delete', 'var.put', 'var.delete',
     'environment.put', 'environment.delete', 'repos.enabled', 'fork_policy.put')),
  scope text NOT NULL CONSTRAINT ci_audit_scope_check CHECK (scope IN ('org', 'repo', 'env')),
  repo text,
  target text NOT NULL,
  PRIMARY KEY (id, at)
) PARTITION BY RANGE (at);

DO $$
DECLARE
  first_ts timestamptz;
  last_ts timestamptz;
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  SELECT date_trunc('month', to_timestamp(MIN(at) / 1000.0)),
         date_trunc('month', to_timestamp(MAX(at) / 1000.0))
    INTO first_ts, last_ts
    FROM ci_audit;
  IF first_ts IS NULL THEN RETURN; END IF;
  cur := first_ts;
  WHILE cur <= last_ts LOOP
    lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
    hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
    EXECUTE format('CREATE TABLE %I PARTITION OF ci_audit_new FOR VALUES FROM (%s) TO (%s)',
      'ci_audit_' || to_char(cur, 'YYYYMM'), lo, hi);
    cur := cur + INTERVAL '1 month';
  END LOOP;
END $$;

INSERT INTO ci_audit_new
  (id, tenant_id, at, actor, action, scope, repo, target)
  SELECT id, tenant_id, at, actor, action, scope, repo, target
  FROM ci_audit;

-- The ids the old sequence handed out stay used even when retention
-- deleted their rows: run and job ids name Durable Objects and R2
-- snapshots, so the new sequence starts past the old high-water mark.
CREATE TEMP TABLE ci_audit_id_mark AS
  SELECT GREATEST(
    COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ci_audit', 'id')::regclass), 0),
    COALESCE((SELECT MAX(id) FROM ci_audit), 0)) AS hw;
DROP TABLE ci_audit;
ALTER TABLE ci_audit_new RENAME TO ci_audit;
ALTER TABLE ci_audit RENAME CONSTRAINT ci_audit_new_pkey TO ci_audit_pkey;

CREATE SEQUENCE ci_audit_id_seq;
SELECT setval('ci_audit_id_seq', GREATEST((SELECT hw FROM ci_audit_id_mark), 1),
  (SELECT hw FROM ci_audit_id_mark) > 0);
DROP TABLE ci_audit_id_mark;
ALTER TABLE ci_audit ALTER COLUMN id SET DEFAULT nextval('ci_audit_id_seq');

CREATE INDEX ci_audit_tenant ON ci_audit (tenant_id, id DESC);
CREATE INDEX ci_audit_age ON ci_audit (at);

-- The months from this one: the reconciler's upkeep keeps them made,
-- but a run recorded between this migration and its first pass needs
-- them already.
DO $$
DECLARE
  t int;
  m int;
  base timestamptz := date_trunc('month', now());
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  FOR m IN 0..2 LOOP
    cur := base + (m || ' months')::interval;
    lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
    hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
    EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF ci_usage FOR VALUES FROM (%s) TO (%s)',
      'ci_usage_' || to_char(cur, 'YYYYMM'), lo, hi);
    FOREACH t IN ARRAY ARRAY[30, 90] LOOP
      EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF %I FOR VALUES FROM (%s) TO (%s)',
        'ci_runs_t' || t || '_' || to_char(cur, 'YYYYMM'), 'ci_runs_t' || t, lo, hi);
      EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF %I FOR VALUES FROM (%s) TO (%s)',
        'ci_jobs_t' || t || '_' || to_char(cur, 'YYYYMM'), 'ci_jobs_t' || t, lo, hi);
    END LOOP;
    EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF ci_audit FOR VALUES FROM (%s) TO (%s)',
      'ci_audit_' || to_char(cur, 'YYYYMM'), lo, hi);
  END LOOP;
END $$;
