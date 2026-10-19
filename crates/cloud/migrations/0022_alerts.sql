-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Alerts on the reconciler and launches (crates/cloud/src/alerts.rs).

-- One row per alert key. state is what the last check found; sent is
-- what the last email said, NULL before the first. An email is due
-- while the two differ, so one that fails to send goes out on a later
-- pass. A resolved alert that never sent its firing email sends
-- nothing.
CREATE TABLE alerts (
  key TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK (state IN ('firing', 'resolved')),
  detail TEXT NOT NULL,
  fired_at INTEGER NOT NULL,
  resolved_at INTEGER,
  sent TEXT CHECK (sent IN ('firing', 'resolved')),
  updated_at INTEGER NOT NULL
) STRICT;

-- The last passes' errors and whether they stopped with work left, as
-- a JSON array, newest last; and when the daily digest of firing
-- alerts last went out.
ALTER TABLE reconciler ADD COLUMN history TEXT;
ALTER TABLE reconciler ADD COLUMN digest_at INTEGER;

-- Launches whose message ran out of retries and reached
-- hatchrun-launches-dlq, once per message. error and attempts are the
-- job's when the message arrived.
CREATE TABLE failed_launches (
  id INTEGER PRIMARY KEY,
  message_id TEXT NOT NULL UNIQUE,
  job_id INTEGER,
  installation_id INTEGER,
  error TEXT,
  attempts INTEGER,
  failed_at INTEGER NOT NULL
) STRICT;

CREATE INDEX failed_launches_age ON failed_launches (failed_at);

-- Installations whose GitHub answers were rate limited or had little of
-- the hour's requests left, the lowest count of the pass or launch that
-- saw it. Healthy answers write nothing.
CREATE TABLE rate_limits (
  installation_id INTEGER PRIMARY KEY,
  remaining INTEGER,
  limited INTEGER NOT NULL CHECK (limited IN (0, 1)),
  seen_at INTEGER NOT NULL
) STRICT;

CREATE INDEX rate_limits_seen ON rate_limits (seen_at);
