-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Polar ingest goes from one event per job to one per tenant and
-- 15 minute window. Each window keeps the summed units, seconds and
-- job count of the usage rows closed into it, and the rows point at
-- it. A window's totals never change once written: a row that arrives
-- late for a written window joins a later one. Per-job rows stay in
-- ci_usage, which the spend cap and the billing page read.

CREATE TABLE ci_usage_windows (
  id bigserial PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  window_start bigint NOT NULL,
  window_end bigint NOT NULL,
  units bigint NOT NULL CHECK (units >= 0),
  seconds bigint NOT NULL CHECK (seconds >= 0),
  jobs integer NOT NULL CHECK (jobs > 0),
  sent_at bigint,                          -- when Polar took it
  rejected_at bigint,                      -- when Polar refused it for good
  reject_reason text,
  created_at bigint NOT NULL,
  UNIQUE (tenant_id, window_start)
);
CREATE INDEX ci_usage_windows_unsent ON ci_usage_windows (window_start)
  WHERE sent_at IS NULL AND rejected_at IS NULL;

ALTER TABLE ci_usage ADD COLUMN window_id bigint
  REFERENCES ci_usage_windows (id) ON DELETE SET NULL;
CREATE INDEX ci_usage_unwindowed ON ci_usage (tenant_id, ended_at)
  WHERE sent_at IS NULL AND rejected_at IS NULL AND window_id IS NULL;
