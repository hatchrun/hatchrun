-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- What the console needs beyond the first org-scale lists: the head
-- commit subject of a run, the dependencies of a job, when a secret or
-- var was last handed to a job, and an audit trail of settings changes.
-- Times are Unix milliseconds.

-- The subject line of the head commit of a push, or the title of a pull
-- request, cut to 200 characters. NULL for runs with no such text.
ALTER TABLE ci_runs ADD COLUMN message text;

-- The plan ids of the jobs this job waits on, a JSON array of text.
ALTER TABLE ci_jobs ADD COLUMN needs jsonb NOT NULL DEFAULT '[]'
  CHECK (jsonb_typeof(needs) = 'array');
ALTER TABLE ci_jobs ALTER COLUMN needs DROP DEFAULT;

-- The last time a job received the row. NULL until it does.
ALTER TABLE ci_secrets ADD COLUMN last_used_at bigint;
ALTER TABLE ci_vars ADD COLUMN last_used_at bigint;

-- Changes to secrets, vars, environments and the enabled repositories,
-- newest last. scope is org, repo or env; repo is the repository's name
-- when the scope has one; target is what changed. Rows are kept 400
-- days (see the reconciler's sweep).
CREATE TABLE ci_audit (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  at bigint NOT NULL,
  actor text NOT NULL,
  action text NOT NULL CHECK (action IN
    ('secret.put', 'secret.delete', 'var.put', 'var.delete',
     'environment.put', 'environment.delete', 'repos.enabled')),
  scope text NOT NULL CHECK (scope IN ('org', 'repo', 'env')),
  repo text,
  target text NOT NULL
);
CREATE INDEX ci_audit_tenant ON ci_audit (tenant_id, id DESC);
CREATE INDEX ci_audit_age ON ci_audit (at);

-- Activity charts read runs by time within a tenant.
CREATE INDEX ci_runs_tenant_created ON ci_runs (tenant_id, created_at);
CREATE INDEX ci_runs_tenant_completed ON ci_runs (tenant_id, completed_at)
  WHERE completed_at IS NOT NULL;
