-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- One row per GitHub workflow job that asked for a hatchrun runner. GitHub
-- may deliver a webhook more than once; the job id keeps the second
-- delivery from launching a second runner.
CREATE TABLE jobs (
  id INTEGER PRIMARY KEY,
  installation_id INTEGER NOT NULL,
  repository TEXT NOT NULL,
  run_id INTEGER NOT NULL,
  label TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('queued', 'in_progress', 'completed')),
  queued_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX jobs_status ON jobs (status, queued_at);
