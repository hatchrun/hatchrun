-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Browsing a repository by workflow, branch and pull request. A run of a
-- pull request keeps the pull request's number and head branch, and
-- belongs to that branch; the title is read from ci_pulls. Runs
-- recorded before this have neither, and stay without a pull request.
ALTER TABLE ci_runs ADD COLUMN pull_number bigint;
ALTER TABLE ci_runs ADD COLUMN head_ref text;

-- The pull requests of a repository, kept from the pull_request webhook, every action. Pull requests from forks never run and are not kept.
-- state is open, closed, or merged; times are Unix milliseconds, updated_at
-- GitHub's own, which orders the deliveries.
CREATE TABLE ci_pulls (
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  number bigint NOT NULL,
  title text NOT NULL,
  head_ref text NOT NULL,
  base_ref text NOT NULL,
  author text NOT NULL,
  state text NOT NULL CHECK (state IN ('open', 'closed', 'merged')),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX ci_pulls_updated ON ci_pulls (repo_id, updated_at DESC, number DESC);

-- A pull request's runs, newest first.
CREATE INDEX ci_runs_pull ON ci_runs (repo_id, pull_number, created_at DESC)
  WHERE pull_number IS NOT NULL;

-- Jobs of runs created before 0017 (2026-11-06 09:21:59 UTC) never had
-- their plan ids recorded; 0017 filled them with an empty array, which
-- the console cannot tell from a job that waits on nothing. NULL marks
-- them unknown.
ALTER TABLE ci_jobs ALTER COLUMN needs DROP NOT NULL;
UPDATE ci_jobs SET needs = NULL
  WHERE run_id IN (SELECT id FROM ci_runs WHERE created_at < 1793956919000);
