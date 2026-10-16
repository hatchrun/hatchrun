-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The runs table sorts by repository, A to Z or Z to A, with each
-- repository's runs newest first, and pages by (repository, queued_at,
-- id). One index per direction, so neither sort reads every run of the
-- tenant into a temporary B-tree: Z to A walks runs_repository
-- backwards, A to Z walks runs_repository_az forwards.
CREATE INDEX runs_repository ON runs (installation_id, repository COLLATE NOCASE, queued_at, id);
CREATE INDEX runs_repository_az ON runs
  (installation_id, repository COLLATE NOCASE, queued_at DESC, id DESC);
