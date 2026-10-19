-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- By how long runs took, the runs still going came last, so a search
-- for a busy repository opened on finished runs and hid the ones that
-- are taking time now. They come first instead, oldest first, so the
-- longest running leads; then the finished runs, shortest or longest
-- first. A finished run's key is how long it took, a live run's when it
-- was queued.
DROP INDEX runs_took;
DROP INDEX runs_took_desc;
CREATE INDEX runs_took ON runs (installation_id, ended_at IS NOT NULL,
  COALESCE(ended_at - queued_at, queued_at), id);
CREATE INDEX runs_took_desc ON runs (installation_id, ended_at IS NOT NULL,
  CASE WHEN ended_at IS NULL THEN queued_at ELSE queued_at - ended_at END, id);
