-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The intake of hatchrun-ci's webhook deliveries. The Worker stores
-- every verified delivery with its payload before it answers 202, and
-- plans it later from the hatchrun-ci-deliveries queue. `state` is
-- `received` (stored, not yet planned), `planning` (a consumer holds
-- the lease), `done` or `failed`. `lease_until` is the claim's lease
-- in Unix milliseconds, or for a `received` delivery the time the
-- reconciler may send it again. `attempts` counts claims and resends
-- and is each claim's token, so it only grows; `attempts_before` is
-- what it was when a failed delivery last started over, so the cap
-- counts the tries since. `error` says why it failed and `failed_at`
-- when, for the failed delivery alert. A delivery drops its payload
-- once it is `done`.
--
-- `state` has no default: the intake names the state of every row it
-- stores. The rows already here were planned inline before the intake
-- existed, so they load as `done` with no payload.
--
-- Payloads make the table large, so it is partitioned by RANGE on the
-- month of received_at, like the tables of 0027, and the reconciler
-- drops a month once all of it is older than 30 days instead of
-- deleting rows. The primary key has to include received_at, so it no
-- longer stops a second row for one guid: a copy can arrive a moment
-- later, or in the next month. The intake locks the guid's row in
-- ci_delivery_locks, a fixed set of 1024 slots picked by a hash of the
-- guid, and stores the delivery only when no partition holds the guid
-- yet. Hyperdrive does not pass advisory locks through, so the lock is
-- on a row.

-- Month boundaries are UTC, as the reconciler's upkeep makes them, so
-- the partitions made here never overlap the ones it adds later.
SET LOCAL TIME ZONE 'UTC';

CREATE TABLE ci_deliveries_new (
  guid text NOT NULL,
  event text NOT NULL,
  received_at bigint NOT NULL,
  payload bytea,
  state text NOT NULL
    CONSTRAINT ci_deliveries_state_check CHECK (state IN ('received', 'planning', 'done', 'failed')),
  attempts bigint NOT NULL DEFAULT 0,
  attempts_before bigint NOT NULL DEFAULT 0,
  lease_until bigint,
  error text,
  failed_at bigint,
  PRIMARY KEY (guid, received_at)
) PARTITION BY RANGE (received_at);

DO $$
DECLARE
  first_ts timestamptz;
  last_ts timestamptz;
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  SELECT date_trunc('month', to_timestamp(MIN(received_at) / 1000.0)),
         date_trunc('month', to_timestamp(MAX(received_at) / 1000.0))
    INTO first_ts, last_ts
    FROM ci_deliveries;
  IF first_ts IS NULL THEN RETURN; END IF;
  cur := first_ts;
  WHILE cur <= last_ts LOOP
    lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
    hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
    EXECUTE format('CREATE TABLE %I PARTITION OF ci_deliveries_new FOR VALUES FROM (%s) TO (%s)',
      'ci_deliveries_' || to_char(cur, 'YYYYMM'), lo, hi);
    cur := cur + INTERVAL '1 month';
  END LOOP;
END $$;

INSERT INTO ci_deliveries_new (guid, event, received_at, state)
  SELECT guid, event, received_at, 'done' FROM ci_deliveries;

DROP TABLE ci_deliveries;
ALTER TABLE ci_deliveries_new RENAME TO ci_deliveries;
ALTER TABLE ci_deliveries RENAME CONSTRAINT ci_deliveries_new_pkey TO ci_deliveries_pkey;

CREATE INDEX ci_deliveries_stale ON ci_deliveries (state, received_at)
  WHERE state IN ('received', 'planning');

CREATE INDEX ci_deliveries_failed ON ci_deliveries (failed_at)
  WHERE state = 'failed';

-- The months from this one: the reconciler's upkeep keeps them made,
-- but a delivery stored between this migration and its first pass
-- needs them already.
DO $$
DECLARE
  m int;
  base timestamptz := date_trunc('month', now());
  cur timestamptz;
  lo bigint;
  hi bigint;
BEGIN
  FOR m IN 0..2 LOOP
    cur := base + (m || ' months')::interval;
    lo := (EXTRACT(EPOCH FROM cur) * 1000)::bigint;
    hi := (EXTRACT(EPOCH FROM (cur + INTERVAL '1 month')) * 1000)::bigint;
    EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF ci_deliveries FOR VALUES FROM (%s) TO (%s)',
      'ci_deliveries_' || to_char(cur, 'YYYYMM'), lo, hi);
  END LOOP;
END $$;

-- The lock a delivery's guid takes while the intake decides whether
-- it is new: slot hashtext(guid) & 1023.
CREATE TABLE ci_delivery_locks (
  slot integer PRIMARY KEY
);
INSERT INTO ci_delivery_locks (slot) SELECT generate_series(0, 1023);
