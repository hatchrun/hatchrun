-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The app lists jobs by workflow run.

-- One row per GitHub workflow run with at least one hatchrun job. The
-- run's first job creates it from what workflow_job carries; the rest
-- comes from the run on GitHub, fetched once (crates/cloud/src/runs.rs).
CREATE TABLE runs (
  id INTEGER PRIMARY KEY,
  installation_id INTEGER NOT NULL,
  repository TEXT NOT NULL,
  -- NULL where GitHub's workflow_job has none.
  workflow_name TEXT,
  head_branch TEXT,
  head_sha TEXT NOT NULL,
  -- The newest attempt any job has reported, and when its first job
  -- was queued. The list orders and pages by queued_at, so a re-run
  -- moves the run back to the top.
  attempt INTEGER NOT NULL CHECK (attempt >= 1),
  queued_at INTEGER NOT NULL,
  -- From GET /repos/{repo}/actions/runs/{id}, NULL until details_at.
  -- A run GitHub would not show keeps them NULL.
  run_number INTEGER,
  event TEXT,
  display_title TEXT,
  actor TEXT,
  path TEXT,
  details_at INTEGER,
  -- When the reconciler last asked, so runs it cannot fetch take turns
  -- with the rest.
  details_tried_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX runs_recent ON runs (installation_id, queued_at, id);
CREATE INDEX runs_age ON runs (queued_at);
-- Runs the reconciler fetches the details of, least recently tried
-- first.
CREATE INDEX runs_details ON runs (COALESCE(details_tried_at, created_at))
  WHERE details_at IS NULL;

-- The job's name as the run shows it, such as "Save (host)", and the
-- run attempt it belongs to. The defaults only let SQLite add the
-- columns; the Worker writes both.
ALTER TABLE jobs ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN run_attempt INTEGER NOT NULL DEFAULT 1;
-- GitHub's conclusion once completed: success, failure, cancelled,
-- skipped, timed_out, neutral, action_required.
ALTER TABLE jobs ADD COLUMN conclusion TEXT;
-- GitHub's times, Unix ms: when a runner took the job and when it
-- finished. queued_at stays when the Worker heard of it.
ALTER TABLE jobs ADD COLUMN started_at INTEGER;
ALTER TABLE jobs ADD COLUMN completed_at INTEGER;

-- A run's jobs, and the newest attempt of each job name.
CREATE INDEX jobs_run ON jobs (run_id, name, run_attempt);
