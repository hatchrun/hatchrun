-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- The reconciler (crates/cloud/src/reconcile), run every minute.

-- The runner that took the job, from GitHub's in_progress and completed
-- events. A runner may take another job with the same label than the
-- one it was launched for; this says which instance ran which job.
ALTER TABLE jobs ADD COLUMN ran_on TEXT;
-- When the reconciler last asked GitHub about the job.
ALTER TABLE jobs ADD COLUMN checked_at INTEGER;

CREATE INDEX jobs_ran_on ON jobs (ran_on) WHERE ran_on IS NOT NULL;

-- When the reconciler last went over the tenant; the least recent go
-- first.
ALTER TABLE tenants ADD COLUMN reconciled_at INTEGER;

-- Webhook deliveries the reconciler asked GitHub to deliver again, by
-- the event's guid, so no event is redelivered twice.
CREATE TABLE redeliveries (
  guid TEXT PRIMARY KEY,
  delivery_id INTEGER NOT NULL,
  event TEXT NOT NULL,
  redelivered_at INTEGER NOT NULL
) STRICT;

CREATE INDEX redeliveries_age ON redeliveries (redelivered_at);

-- One row. lease_until keeps passes from overlapping; deliveries_until
-- is the newest delivery the last complete scan saw; summary is the
-- last pass's counts as JSON, which the operator and the dashboard read.
CREATE TABLE reconciler (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  lease_until INTEGER NOT NULL,
  deliveries_until INTEGER,
  started_at INTEGER,
  finished_at INTEGER,
  summary TEXT
) STRICT;

INSERT INTO reconciler (id, lease_until) VALUES (1, 0);
