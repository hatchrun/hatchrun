-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- A job a rerun left out of its matrix. The matrix of a job that reads it
-- when it is ready can make fewer jobs after a rerun of the job it reads
-- from. The row of a job that is no longer made ends as skipped for the new
-- attempt and is marked here, so the run's job list and counts leave it out;
-- its check run, which GitHub cannot delete, ends as skipped. A rerun that
-- makes the job again clears the mark.
ALTER TABLE ci_jobs ADD COLUMN superseded boolean NOT NULL DEFAULT false;
