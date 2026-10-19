-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The runs table sorts by how long a run took. A run's time rolls up
-- from its current jobs, the newest attempt of each job name, so it is
-- kept on the run where an index can order it.

-- When the run's last current job ended, as the app shows it: the
-- latest completed_at, or updated_at where GitHub sent none, and never
-- before the run was queued. NULL while a current job has not
-- completed. A run with no jobs ended when it was queued.
ALTER TABLE runs ADD COLUMN ended_at INTEGER;

-- Every write to a job that can change its run's end works the end out
-- again from the run's jobs, through jobs_run. The webhook moves a run
-- to its new attempt (RUN_ATTEMPT) before it records the attempt's
-- first job, so queued_at is the new one by then. Launch bookkeeping
-- on a job that has not completed leaves the run alone.
CREATE TRIGGER jobs_end_insert AFTER INSERT ON jobs
BEGIN
  UPDATE runs SET ended_at = (
    SELECT CASE WHEN MAX(j.status <> 'completed') THEN NULL
      ELSE MAX(runs.queued_at, COALESCE(MAX(COALESCE(j.completed_at, j.updated_at)), runs.queued_at))
    END
    FROM jobs AS j
    WHERE j.run_id = runs.id AND NOT EXISTS (SELECT 1 FROM jobs AS later
      WHERE later.run_id = j.run_id AND later.name = j.name
      AND later.run_attempt > j.run_attempt)
  ) WHERE id = NEW.run_id;
END;

CREATE TRIGGER jobs_end_update AFTER UPDATE ON jobs
WHEN OLD.status IS NOT NEW.status
  OR OLD.run_id IS NOT NEW.run_id
  OR OLD.name IS NOT NEW.name
  OR OLD.run_attempt IS NOT NEW.run_attempt
  OR (NEW.status = 'completed' AND (OLD.completed_at IS NOT NEW.completed_at
    OR (NEW.completed_at IS NULL AND OLD.updated_at IS NOT NEW.updated_at)))
BEGIN
  UPDATE runs SET ended_at = (
    SELECT CASE WHEN MAX(j.status <> 'completed') THEN NULL
      ELSE MAX(runs.queued_at, COALESCE(MAX(COALESCE(j.completed_at, j.updated_at)), runs.queued_at))
    END
    FROM jobs AS j
    WHERE j.run_id = runs.id AND NOT EXISTS (SELECT 1 FROM jobs AS later
      WHERE later.run_id = j.run_id AND later.name = j.name
      AND later.run_attempt > j.run_attempt)
  ) WHERE id IN (OLD.run_id, NEW.run_id);
END;

-- Retention deletes a re-run's older jobs before the run itself.
CREATE TRIGGER jobs_end_delete AFTER DELETE ON jobs
BEGIN
  UPDATE runs SET ended_at = (
    SELECT CASE WHEN MAX(j.status <> 'completed') THEN NULL
      ELSE MAX(runs.queued_at, COALESCE(MAX(COALESCE(j.completed_at, j.updated_at)), runs.queued_at))
    END
    FROM jobs AS j
    WHERE j.run_id = runs.id AND NOT EXISTS (SELECT 1 FROM jobs AS later
      WHERE later.run_id = j.run_id AND later.name = j.name
      AND later.run_attempt > j.run_attempt)
  ) WHERE id = OLD.run_id;
END;

UPDATE runs SET ended_at = (
  SELECT CASE WHEN MAX(j.status <> 'completed') THEN NULL
    ELSE MAX(runs.queued_at, COALESCE(MAX(COALESCE(j.completed_at, j.updated_at)), runs.queued_at))
  END
  FROM jobs AS j
  WHERE j.run_id = runs.id AND NOT EXISTS (SELECT 1 FROM jobs AS later
    WHERE later.run_id = j.run_id AND later.name = j.name
    AND later.run_attempt > j.run_attempt)
);

-- Shortest first, then the runs still going, oldest first; and longest
-- first, then the same. A finished run's key is how long it took, a
-- live run's when it was queued. One index per direction, as the live
-- runs keep their order either way.
CREATE INDEX runs_took ON runs (installation_id, ended_at IS NULL,
  COALESCE(ended_at - queued_at, queued_at), id);
CREATE INDEX runs_took_desc ON runs (installation_id, ended_at IS NULL,
  CASE WHEN ended_at IS NULL THEN queued_at ELSE queued_at - ended_at END, id);
