-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- hatchrun CI environments, organization secret repository access, and
-- CI billed on the hatchrun.io subscription. Later tasks of the
-- environments plan append here.

-- CI is billed on the hatchrun.io product, so a subscription is a
-- subscription and the product column goes.
ALTER TABLE subscriptions DROP COLUMN product;

-- Environments of a repository. Names are unique per repository,
-- ignoring case; branches is a JSON array of glob patterns.
CREATE TABLE ci_environments (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL REFERENCES ci_repos (id) ON DELETE CASCADE,
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
  branches jsonb NOT NULL CHECK (jsonb_typeof(branches) = 'array'),
  approval boolean NOT NULL,
  updated_at bigint NOT NULL
);
CREATE UNIQUE INDEX ci_environments_name ON ci_environments (repo_id, lower(name));

-- Environment secrets and vars: repo_id is the environment's repository.
ALTER TABLE ci_secrets
  ADD COLUMN environment_id bigint REFERENCES ci_environments (id) ON DELETE CASCADE,
  ADD CONSTRAINT ci_secrets_environment_repo CHECK (environment_id IS NULL OR repo_id IS NOT NULL),
  DROP CONSTRAINT ci_secrets_tenant_id_repo_id_name_key,
  ADD CONSTRAINT ci_secrets_scope_name UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, environment_id, name);
ALTER TABLE ci_vars
  ADD COLUMN environment_id bigint REFERENCES ci_environments (id) ON DELETE CASCADE,
  ADD CONSTRAINT ci_vars_environment_repo CHECK (environment_id IS NULL OR repo_id IS NOT NULL),
  DROP CONSTRAINT ci_vars_tenant_id_repo_id_name_key,
  ADD CONSTRAINT ci_vars_scope_name UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, environment_id, name);

-- The environment a job names, and the approval of one attempt.
ALTER TABLE ci_jobs
  ADD COLUMN environment text,
  ADD COLUMN approval_attempt bigint,
  ADD COLUMN approval_decision text CHECK (approval_decision IN ('approved', 'rejected')),
  ADD COLUMN approval_by text,
  ADD COLUMN approval_at bigint;
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval'));

-- An organization secret or var reaches every repository (NULL) or the
-- ci_repos ids listed.
ALTER TABLE ci_secrets ADD COLUMN repositories bigint[]
  CHECK (repositories IS NULL OR (repo_id IS NULL AND cardinality(repositories) <= 1000));
ALTER TABLE ci_vars ADD COLUMN repositories bigint[]
  CHECK (repositories IS NULL OR (repo_id IS NULL AND cardinality(repositories) <= 1000));

-- Runner time is billed per second, at least 60 seconds a job, with no
-- free minutes and no grace period after them. Recorded minutes become
-- seconds.
ALTER TABLE usage_events RENAME COLUMN minutes TO seconds;
ALTER TABLE usage_events DROP CONSTRAINT usage_events_minutes_check;
UPDATE usage_events SET seconds = seconds * 60;
ALTER TABLE usage_events ADD CONSTRAINT usage_events_seconds_check CHECK (seconds >= 60);
ALTER TABLE tenants DROP COLUMN free_used_up_at;
