-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Where a deployment can be seen: the job's environment-url, evaluated by
-- the agent when the job ends. An environment name that is an expression
-- is written to ci_jobs.environment when the job is queued, so the row
-- stays NULL until then.
ALTER TABLE ci_jobs ADD COLUMN environment_url text;
