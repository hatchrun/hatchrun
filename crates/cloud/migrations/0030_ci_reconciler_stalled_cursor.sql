-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Where the last reconciler pass stopped in the stalled jobs, as the
-- (updated_at, id) of the last row it handled. The next pass starts
-- after it, so jobs whose run object keeps failing take their turn
-- instead of holding the head of the scan.
ALTER TABLE ci_reconciler
  ADD COLUMN stalled_after_at bigint NOT NULL DEFAULT 0,
  ADD COLUMN stalled_after_id bigint NOT NULL DEFAULT 0;
