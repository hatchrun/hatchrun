-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- pull_request_target runs a workflow from the base branch with the
-- base repository's secrets and a token that can write, on a pull
-- request that anyone can open. A workflow that gets the pull request
-- head onto the runner and runs it hands those secrets to the author, and
-- no reading of the workflow finds every way to do that. The setting says
-- what a repository does about it for pull requests from forks: 'warn'
-- runs them at once, with the planner's findings in the run's check
-- runs, 'approve' makes each run wait for a maintainer's approval, the
-- way a fork run of the pull_request event does, whoever the author is.
-- Repositories that saved a fork policy before this keep warning, which
-- is what ran until now.
ALTER TABLE ci_repo_policy ADD COLUMN pull_request_target_head_checkout text NOT NULL
  DEFAULT 'warn' CHECK (pull_request_target_head_checkout IN ('warn', 'approve'));
-- The default stays: between this migration and the deploy of the
-- Worker that names the value, the previous Worker still saves policies
-- without it, and 'warn' is the value they mean.

-- The pull request head a pull_request_target run is for. The run's own
-- sha is the base branch's, so the approval of a run, its card and the
-- newer commit that supersedes it are all by the head. pr_sha is the
-- commit an approval is about: the head for a pull_request_target run,
-- the run's sha for the others.
ALTER TABLE ci_runs ADD COLUMN head_sha text;
ALTER TABLE ci_runs ADD COLUMN pr_sha text GENERATED ALWAYS AS (COALESCE(head_sha, sha)) STORED;
-- What the planner found in the run's workflow, one finding to a line,
-- for the approval card.
ALTER TABLE ci_runs ADD COLUMN lints text;

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
