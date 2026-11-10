-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- pull_request_target runs a workflow from the base branch with the
-- base repository's secrets and a write token, on a pull request that
-- anyone can open. A workflow that checks out the pull request head and
-- runs it hands those secrets to the author. The planner finds such a
-- workflow, and this setting says what to do with it: 'warn' runs it
-- with a warning in the run's check runs, 'block' fails the run before
-- it starts. Repositories that saved a fork policy before this keep
-- warning, which is what ran until now.
ALTER TABLE ci_repo_policy ADD COLUMN pull_request_target_head_checkout text NOT NULL
  DEFAULT 'warn' CHECK (pull_request_target_head_checkout IN ('warn', 'block'));
-- The default stays: between this migration and the deploy of the
-- Worker that names the value, the previous Worker still saves policies
-- without it, and 'warn' is the value they mean.

-- 0022 kept defaults on its columns for the Worker that ran between that
-- migration and its own deploy. That Worker is gone: every insert names
-- the trust of the run, the author and the gate, so a row without them
-- is a bug and fails instead of passing as a trusted, ungated run.
ALTER TABLE ci_runs ALTER COLUMN trust DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN author_login DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN author_association DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN gate DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN gate_status DROP DEFAULT;
ALTER TABLE ci_pulls ALTER COLUMN author_association DROP DEFAULT;
ALTER TABLE ci_pulls ALTER COLUMN head_sha DROP DEFAULT;
ALTER TABLE ci_cache_entries ALTER COLUMN trust DROP DEFAULT;
