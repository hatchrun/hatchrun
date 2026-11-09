-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Pull requests from forks. A run has a trust level, set at intake from
-- the delivery and never derived again: `trusted` for every run that
-- ran before this, `fork` for a pull request whose head repository is
-- not the base repository. The control plane reads it to withhold
-- secrets, the write token, OIDC tokens and environments from a fork
-- run, and to keep its cache writes out of the base scopes.
--
-- A fork run may wait for a maintainer's approval first. gate is
-- 'fork_approval' for such a run and 'none' otherwise. gate_status is
-- 'pending' while it waits, 'deferred' while the repository has too many
-- approvals waiting to ask for one, then 'approved', 'expired' (nobody
-- decided in 7 days) or 'cancelled' (rejected, superseded by a newer
-- commit, or the pull request closed). Runs that never waited are
-- 'none'. The approver's login and time are kept, and the id of the
-- check run that carries the approve button.
--
-- Rows recorded before this are trusted and were never gated, so the
-- columns are added with a default that backfills them, and the default
-- is then dropped: the code names every value.
ALTER TABLE ci_runs ADD COLUMN trust text NOT NULL DEFAULT 'trusted'
  CHECK (trust IN ('trusted', 'fork'));
ALTER TABLE ci_runs ADD COLUMN head_repo_id bigint;
ALTER TABLE ci_runs ADD COLUMN head_repo_name text;
ALTER TABLE ci_runs ADD COLUMN author_login text NOT NULL DEFAULT '';
ALTER TABLE ci_runs ADD COLUMN author_association text NOT NULL DEFAULT '';
ALTER TABLE ci_runs ADD COLUMN gate text NOT NULL DEFAULT 'none'
  CHECK (gate IN ('none', 'fork_approval'));
ALTER TABLE ci_runs ADD COLUMN gate_status text NOT NULL DEFAULT 'none'
  CHECK (gate_status IN ('none', 'pending', 'deferred', 'approved', 'expired', 'cancelled'));
ALTER TABLE ci_runs ADD COLUMN gate_by text;
ALTER TABLE ci_runs ADD COLUMN gate_at bigint;
ALTER TABLE ci_runs ADD COLUMN gate_check_run_id bigint;
UPDATE ci_runs SET author_login = actor;
ALTER TABLE ci_runs ALTER COLUMN trust DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN author_login DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN author_association DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN gate DROP DEFAULT;
ALTER TABLE ci_runs ALTER COLUMN gate_status DROP DEFAULT;

-- The runs that wait for a decision, per repository, oldest first: the
-- approval caps, the expiry and the list the app shows read them.
CREATE INDEX ci_runs_gate ON ci_runs (repo_id, created_at, id)
  WHERE gate_status IN ('pending', 'deferred');
-- The fork runs of an author that are not over, for the per-author cap.
CREATE INDEX ci_runs_fork_author ON ci_runs (repo_id, author_login)
  WHERE trust = 'fork' AND status <> 'completed';

-- A pull request keeps its author's association and the repository its
-- head is in (null for a deleted fork), as the last delivery said.
ALTER TABLE ci_pulls ADD COLUMN author_association text NOT NULL DEFAULT '';
ALTER TABLE ci_pulls ADD COLUMN head_repo_id bigint;
ALTER TABLE ci_pulls ALTER COLUMN author_association DROP DEFAULT;

-- What a repository does with pull requests from forks. A row is made
-- by the first write; a repository without one has the defaults
-- (fork_runs on for a public repository and off for a private one,
-- first_time_contributors, 2, 30 and warn).
CREATE TABLE ci_repo_policy (
  repo_id bigint PRIMARY KEY REFERENCES ci_repos (id) ON DELETE CASCADE,
  fork_runs text NOT NULL CHECK (fork_runs IN ('off', 'on')),
  fork_approval text NOT NULL
    CHECK (fork_approval IN ('first_timers', 'first_time_contributors', 'all_outside')),
  fork_max_running integer NOT NULL CHECK (fork_max_running BETWEEN 1 AND 10),
  fork_timeout_minutes integer NOT NULL CHECK (fork_timeout_minutes BETWEEN 5 AND 60),
  pull_request_target_head_checkout text NOT NULL
    CHECK (pull_request_target_head_checkout IN ('warn', 'block')),
  updated_at bigint NOT NULL
);

-- The trust of the run that saved a cache entry. A fork run's saves
-- count against the repository's fork share of the cache, and only
-- entries saved by fork runs are evicted for it. Entries saved before
-- this were saved by trusted runs.
ALTER TABLE ci_cache_entries ADD COLUMN trust text NOT NULL DEFAULT 'trusted'
  CHECK (trust IN ('trusted', 'fork'));
ALTER TABLE ci_cache_entries ALTER COLUMN trust DROP DEFAULT;

-- Why a job of a fork run waits: its run waits for an approval
-- (fork_approval), or its repository already runs the most fork jobs its
-- policy allows (fork_limit).
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval', 'max_parallel', 'group',
                    'capacity', 'fork_approval', 'fork_limit'));
