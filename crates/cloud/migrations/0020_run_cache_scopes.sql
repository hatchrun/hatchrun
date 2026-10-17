-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Where a run's jobs write and read cache entries. Every job of a run
-- has the same scopes, so the first cache call of the run works them
-- out from GitHub and the rest read them here (crates/cloud/src/cache/
-- serve.rs). jobs.cache_write and jobs.cache_reads stay for jobs whose
-- run has no row.
ALTER TABLE runs ADD COLUMN cache_write TEXT;
ALTER TABLE runs ADD COLUMN cache_reads TEXT;
