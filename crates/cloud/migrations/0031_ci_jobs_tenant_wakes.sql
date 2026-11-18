-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Records the tenant on each job, so waking and claiming a held job no
-- longer joins ci_runs, and held jobs wait without queue traffic until
-- an event wakes them: a wake marks the row (next_launch_at = now)
-- instead of requeueing it every 30 s. Rows held for anything but
-- capacity lose their recheck time here; capacity keeps its backoff.

ALTER TABLE ci_jobs ADD COLUMN tenant_id text;

UPDATE ci_jobs SET tenant_id = r.tenant_id FROM ci_runs r WHERE r.id = ci_jobs.run_id;

-- Jobs whose run is gone (the foreign key went with the partitions in
-- 0027) have no tenant to bill or wake them for.
DELETE FROM ci_jobs WHERE tenant_id IS NULL;

ALTER TABLE ci_jobs ALTER COLUMN tenant_id SET NOT NULL;

UPDATE ci_jobs SET next_launch_at = NULL
  WHERE status = 'queued' AND held IS NOT NULL AND held <> 'capacity';

CREATE INDEX ci_jobs_held ON ci_jobs (tenant_id, held, id)
  WHERE status = 'queued' AND held IS NOT NULL;

CREATE INDEX ci_jobs_tenant_live ON ci_jobs (tenant_id)
  WHERE status IN ('launching', 'launched', 'running');

CREATE INDEX ci_jobs_capacity ON ci_jobs (template, id)
  WHERE status = 'queued' AND held = 'capacity';

-- Where the held-job backstop stopped in the tenants with card or
-- spend-cap holds, so each pass checks the next ones.
ALTER TABLE ci_reconciler ADD COLUMN billing_after text NOT NULL DEFAULT '';
