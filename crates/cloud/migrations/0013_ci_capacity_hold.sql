-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- A launch that EC2 refused for a limit or capacity that clears on its
-- own (a vCPU quota used up by the previous run's instances, no spot
-- capacity, throttling) keeps the job queued instead of failing it.
-- capacity_attempts counts those launches so they do not use up the
-- three launch attempts a job gets; capacity_since is when the job
-- first waited, for the ceiling after which it fails.
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval', 'max_parallel', 'group',
            'capacity'));
ALTER TABLE ci_jobs ADD COLUMN capacity_attempts bigint NOT NULL DEFAULT 0;
ALTER TABLE ci_jobs ADD COLUMN capacity_since bigint;
