-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Shards the CI reconciler: every schedule and job carries the bucket
-- its repository hashes into, and ci_shards keeps one row per shard
-- with its leases, stalled cursor, last schedule minute, and when its
-- last pass and schedule pass finished with how many errors. The cron pass fans one message per shard out to the
-- hatchrun-ci-reconcile queue, and each shard fixes its own buckets'
-- stalled jobs and fires its own buckets' schedules. Everything deploys
-- together, so the old columns on ci_reconciler go with no backward
-- compatible reads.

CREATE FUNCTION ci_shard_bucket(repo_id bigint) RETURNS smallint
  LANGUAGE sql IMMUTABLE PARALLEL SAFE
  RETURN div(mod(repo_id::numeric * 2654435761, 4294967296), 1048576)::smallint;

ALTER TABLE ci_schedules
  ADD COLUMN bucket smallint GENERATED ALWAYS AS (ci_shard_bucket(repo_id)) STORED;

CREATE INDEX ci_schedules_bucket ON ci_schedules (bucket, repo_id, workflow, cron);

ALTER TABLE ci_jobs ADD COLUMN bucket smallint;

UPDATE ci_jobs SET bucket = ci_shard_bucket(r.repo_id)
  FROM ci_runs r WHERE r.id = ci_jobs.run_id;

-- Jobs whose run is gone (the foreign key went with the partitions in
-- 0027) have no repository to hash them by.
DELETE FROM ci_jobs WHERE bucket IS NULL;

ALTER TABLE ci_jobs ALTER COLUMN bucket SET NOT NULL;

CREATE INDEX ci_jobs_open_bucket ON ci_jobs (bucket, updated_at, id)
  WHERE status <> 'completed';

CREATE TABLE ci_shards (
  first_bucket smallint PRIMARY KEY,
  end_bucket smallint NOT NULL,
  lease_until bigint NOT NULL DEFAULT 0,
  stalled_after_at bigint NOT NULL DEFAULT 0,
  stalled_after_id bigint NOT NULL DEFAULT 0,
  schedule_lease_until bigint NOT NULL DEFAULT 0,
  last_minute bigint,
  finished_at bigint,
  errors integer NOT NULL DEFAULT 0,
  schedule_finished_at bigint,
  schedule_errors integer NOT NULL DEFAULT 0
);

INSERT INTO ci_shards (first_bucket, end_bucket, last_minute)
  SELECT s::smallint, (s + 256)::smallint, (SELECT min(last_minute) FROM ci_reconciler)
  FROM generate_series(0, 3840, 256) AS s;

ALTER TABLE ci_reconciler
  DROP COLUMN last_minute,
  DROP COLUMN stalled_after_at,
  DROP COLUMN stalled_after_id;
