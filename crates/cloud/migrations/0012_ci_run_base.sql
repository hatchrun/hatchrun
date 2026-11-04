-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The base branch of a pull request run, as a short name. The job cache
-- restores from it after the run's own ref. NULL for runs of other
-- events, and for runs recorded before this column existed.
ALTER TABLE ci_runs ADD COLUMN base_ref text;
