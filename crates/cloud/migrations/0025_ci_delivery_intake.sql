-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The intake of hatchrun-ci's webhook deliveries. The Worker stores
-- every verified delivery with its payload before it answers 202, and
-- plans it later from the hatchrun-ci-deliveries queue. `state` is
-- `received` (stored, not yet planned), `planning` (a consumer holds
-- the lease), `done` or `failed`. `lease_until` is the claim's lease
-- in Unix milliseconds, or for a `received` delivery the time its own
-- queue retry is due. `attempts` counts claims and resends and is
-- each claim's token, so it only grows; `attempts_before` is what it
-- was when a failed delivery last started over, so the cap counts the
-- tries since. `error` says why it failed and `failed_at` when, for the
-- failed delivery alert. Existing rows are done: they were planned inline by the old
-- Worker, which this default keeps working during the deploy.

ALTER TABLE ci_deliveries ADD COLUMN payload bytea,
  ADD COLUMN state text NOT NULL DEFAULT 'done',
  ADD COLUMN attempts bigint NOT NULL DEFAULT 0,
  ADD COLUMN attempts_before bigint NOT NULL DEFAULT 0,
  ADD COLUMN lease_until bigint,
  ADD COLUMN error text,
  ADD COLUMN failed_at bigint,
  ADD CONSTRAINT ci_deliveries_state_check
    CHECK (state IN ('received', 'planning', 'done', 'failed'));

CREATE INDEX ci_deliveries_stale ON ci_deliveries (state, received_at)
  WHERE state IN ('received', 'planning');

CREATE INDEX ci_deliveries_failed ON ci_deliveries (failed_at)
  WHERE state = 'failed';
