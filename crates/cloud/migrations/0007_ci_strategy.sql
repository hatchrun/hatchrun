-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Matrix strategy of CI jobs. The jobs of one matrix share a
-- strategy_group within their run; max_parallel is the most of them
-- that launch, launch or run at once. A job over the limit stays
-- queued, held for max_parallel, until a job of its group ends.
ALTER TABLE ci_jobs
  ADD COLUMN strategy_group text,
  ADD COLUMN max_parallel integer CHECK (max_parallel IS NULL OR max_parallel >= 1),
  ADD CONSTRAINT ci_jobs_max_parallel_group CHECK (max_parallel IS NULL OR strategy_group IS NOT NULL);
ALTER TABLE ci_jobs DROP CONSTRAINT ci_jobs_held_check,
  ADD CONSTRAINT ci_jobs_held_check
    CHECK (held IN ('card', 'spend_cap', 'concurrency', 'approval', 'max_parallel'));

-- The claim counts the running jobs of a group, and the wake looks for
-- held jobs of one.
CREATE INDEX ci_jobs_strategy_group ON ci_jobs (run_id, strategy_group)
  WHERE strategy_group IS NOT NULL;
