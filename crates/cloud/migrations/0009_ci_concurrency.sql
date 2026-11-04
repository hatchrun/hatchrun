-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- hatchrun CI concurrency groups: within a repository, at most one run
-- (or job) of a group runs and one waits.

-- A job waits for a group.
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval', 'max_parallel', 'group'));

-- One row for each run (job_id NULL, the workflow's concurrency) or job
-- (the job's concurrency) that names a group, from the moment its first
-- job is queued until it ends.
--
-- state: new (recorded, not yet in line), running (holds the group),
-- pending (the one that waits), superseded (a newer arrival took its
-- place and it is being cancelled), invalid (group_name holds why the
-- group could not be evaluated).
-- cancel_others: the arrival asked to cancel the one that runs.
-- cancel_me: a newer arrival with cancel-in-progress asked to cancel
-- this one.
-- group_key is group_name in lower case, as GitHub compares groups.
CREATE TABLE ci_concurrency (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  run_id bigint NOT NULL REFERENCES ci_runs (id) ON DELETE CASCADE,
  job_id bigint REFERENCES ci_jobs (id) ON DELETE CASCADE,
  attempt bigint NOT NULL,
  group_key text NOT NULL,
  group_name text NOT NULL,
  state text NOT NULL CHECK (state IN ('new', 'running', 'pending', 'superseded', 'invalid')),
  cancel_others boolean NOT NULL DEFAULT false,
  cancel_me boolean NOT NULL DEFAULT false,
  created_at bigint NOT NULL
);
CREATE UNIQUE INDEX ci_concurrency_run ON ci_concurrency (run_id) WHERE job_id IS NULL;
CREATE UNIQUE INDEX ci_concurrency_job ON ci_concurrency (job_id) WHERE job_id IS NOT NULL;
CREATE UNIQUE INDEX ci_concurrency_running ON ci_concurrency (repo_id, group_key) WHERE state = 'running';
CREATE UNIQUE INDEX ci_concurrency_pending ON ci_concurrency (repo_id, group_key) WHERE state = 'pending';
CREATE INDEX ci_concurrency_group ON ci_concurrency (repo_id, group_key);
