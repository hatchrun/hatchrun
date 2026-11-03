-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Artifacts of hatchrun CI runs, uploaded with actions/upload-artifact
-- and read by later jobs of the run. The bytes are one S3 object each,
-- under object_key, which names the tenant, repository and run; S3
-- lifecycle rules delete them with the run's retention tier, and the
-- rows go with the run.
--
-- A row starts as an upload (finalized_at is NULL, upload_id holds the
-- S3 multipart upload) and becomes an artifact when the upload is
-- finalized. Names are unique per run among finalized artifacts. A
-- re-run of a job replaces the artifact its earlier attempt uploaded
-- under the same name.
CREATE TABLE ci_artifacts (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  run_id bigint NOT NULL REFERENCES ci_runs (id) ON DELETE CASCADE,
  job_id bigint NOT NULL REFERENCES ci_jobs (id) ON DELETE CASCADE,
  attempt bigint NOT NULL,                     -- of the job, when it uploaded
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
  object_key text NOT NULL,
  upload_id text,                              -- NULL once finalized
  size bigint CHECK (size >= 0),
  digest text,                                 -- sha256:<hex> from the toolkit
  created_at bigint NOT NULL,
  finalized_at bigint,
  CHECK ((finalized_at IS NULL) = (size IS NULL))
);
CREATE UNIQUE INDEX ci_artifacts_name ON ci_artifacts (run_id, name) WHERE finalized_at IS NOT NULL;
CREATE INDEX ci_artifacts_run ON ci_artifacts (run_id, id);
