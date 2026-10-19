-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Billing is metered: every organization gets free job minutes each
-- calendar month (UTC), and a paid subscription pays per minute past
-- them. One row per job that ran on a hatchrun runner, written once
-- when the job completes, so a webhook GitHub delivers twice and the
-- reconciler's sync count it once. Rows outlive the jobs table's
-- retention, since a month is longer than 30 days.
CREATE TABLE usage_events (
  job_id INTEGER PRIMARY KEY,
  tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  -- GitHub's started_at to completed_at, rounded up to whole minutes.
  minutes INTEGER NOT NULL CHECK (minutes > 0),
  -- When the job completed, Unix ms.
  ended_at INTEGER NOT NULL,
  -- 1 when the tenant paid per minute as the job completed: those rows
  -- go to Polar's meter. Beta tenants and tenants on free minutes only
  -- count toward the month.
  billed INTEGER NOT NULL CHECK (billed IN (0, 1)),
  -- When Polar took the event, Unix ms.
  sent_at INTEGER
) STRICT;

CREATE INDEX usage_events_month ON usage_events (tenant_id, ended_at);
CREATE INDEX usage_events_unsent ON usage_events (ended_at)
  WHERE billed = 1 AND sent_at IS NULL;

-- When an unpaid tenant first used up its free minutes in a month,
-- Unix ms. Runners keep launching for a grace period after it, then
-- stop until a subscription starts or the next month does.
ALTER TABLE tenants ADD COLUMN free_used_up_at INTEGER;
