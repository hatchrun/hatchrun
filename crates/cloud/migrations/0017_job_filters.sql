-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- The Failed tab of the jobs table pages back through failed launches
-- only. On jobs_recent that scan would read every job of a tenant with
-- few failures; this index holds the failed ones alone.
CREATE INDEX jobs_failed ON jobs (installation_id, queued_at)
  WHERE launch_state = 'failed';

-- The 24 hour strip groups the last day's jobs by updated_at on every
-- refresh of the jobs page.
CREATE INDEX jobs_updated ON jobs (installation_id, updated_at);
