-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The runs table's search matches `%text%` against a run's repository,
-- workflow and branch and its jobs' names, labels and instances. No
-- B-tree serves a pattern with a leading wildcard, so every search read
-- all of a tenant's jobs. Trigram indexes find the rows for a pattern
-- of three characters or more. pg_trgm is a trusted extension; it goes
-- in public, where every schema's search_path finds it.
CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public;

CREATE INDEX jobs_search ON jobs USING gin
  (name public.gin_trgm_ops, label public.gin_trgm_ops, instance_id public.gin_trgm_ops);
CREATE INDEX runs_search ON runs USING gin
  (repository public.gin_trgm_ops, workflow_name public.gin_trgm_ops,
   head_branch public.gin_trgm_ops);
