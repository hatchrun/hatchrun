-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Failed launches are tried again (crates/cloud/src/reconcile/retry.rs).

-- Launches the Worker started for the job, counted when it claims it.
ALTER TABLE jobs ADD COLUMN launch_attempts INTEGER NOT NULL DEFAULT 0;
-- With launch_state 'failed': 'backoff' when the reconciler launches
-- the job again at next_launch_at, 'setup' when it waits for an owner
-- to change the organization's setup. NULL when nothing launches it
-- again, as for a runner that waited too long for its job.
ALTER TABLE jobs ADD COLUMN launch_retry TEXT
  CHECK (launch_retry IN ('backoff', 'setup'));
ALTER TABLE jobs ADD COLUMN next_launch_at INTEGER;
