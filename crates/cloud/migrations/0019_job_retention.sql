-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Jobs and runs expire per tenant, and the reconciler's job queries
-- read through indexes instead of every job.

-- Open jobs are a small share of the table. launch's CLAIM counts a
-- tenant's runners and the reconciler reads a tenant's open jobs on
-- every pass; both read this index alone. jobs_launch held every job
-- a runner ever ran, as 'launched', so CLAIM's count read the
-- tenant's whole history through it. Nothing else needs it: the
-- setup recheck reads jobs_failed.
CREATE INDEX jobs_open ON jobs (installation_id, launch_state)
  WHERE status <> 'completed';
DROP INDEX jobs_launch;

-- Open jobs the reconciler asks GitHub about, quietest first.
CREATE INDEX jobs_unchecked ON jobs (COALESCE(checked_at, updated_at))
  WHERE status <> 'completed';

-- How long a tenant's jobs and runs are kept: 30 days, up to 90 for
-- enterprise tenants.
ALTER TABLE tenants ADD COLUMN retention_days INTEGER NOT NULL DEFAULT 30
  CHECK (retention_days BETWEEN 1 AND 90);

-- When the reconciler deletes the row: queued_at plus the tenant's
-- retention_days when the row was recorded, 30 days for an
-- installation without a tenant. A change to retention_days applies
-- to rows recorded after it.
ALTER TABLE jobs ADD COLUMN expires_at INTEGER;
ALTER TABLE runs ADD COLUMN expires_at INTEGER;

-- Every tenant has the default so far.
UPDATE jobs SET expires_at = queued_at + 30 * 24 * 60 * 60 * 1000;
UPDATE runs SET expires_at = queued_at + 30 * 24 * 60 * 60 * 1000;

CREATE INDEX jobs_expiry ON jobs (expires_at);
CREATE INDEX runs_expiry ON runs (expires_at);
