-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The run object keeps live job state in its own storage now: beats are
-- stored under `live:<id>` beside the job, never in Postgres. The
-- reconciler ends a wedged job from its start and timeout instead.
ALTER TABLE ci_jobs DROP COLUMN heartbeat_at;
