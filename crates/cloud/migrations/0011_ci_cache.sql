-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Cache entries of hatchrun CI jobs, saved and restored with
-- actions/cache. The bytes are one S3 object each, under object_key,
-- which names the tenant and repository and a random token. S3 does not
-- know when an entry was last restored, so the reconciler deletes the
-- entries nobody restored for 7 days: the row goes first, so a lookup
-- never finds a row whose object is gone, and the object after.
--
-- scope is the git ref the saving run had. A run writes to its own
-- scope and reads its own, then (for a pull request) its base branch's,
-- then the default branch's. There are no
-- foreign keys: a repository or tenant that goes away must not drop the
-- rows that name objects still in the bucket; the sweep removes both.
--
-- A row starts as an upload (finalized_at is NULL, upload_id holds the
-- S3 multipart upload) and becomes an entry when the upload is
-- finalized. A scope holds one finalized entry per version and key.
CREATE TABLE ci_cache_entries (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL,
  repo_id bigint NOT NULL,
  scope text NOT NULL,                         -- git ref, such as refs/heads/main
  version text NOT NULL,                       -- the toolkit's hash of paths and compression
  key text NOT NULL CHECK (char_length(key) BETWEEN 1 AND 512),
  object_key text NOT NULL,
  upload_id text,                              -- NULL once finalized
  size bigint CHECK (size >= 0),
  created_at bigint NOT NULL,
  last_accessed_at bigint NOT NULL,            -- saved or restored
  finalized_at bigint,
  CHECK ((finalized_at IS NULL) = (size IS NULL))
);
CREATE UNIQUE INDEX ci_cache_entries_key
  ON ci_cache_entries (tenant_id, repo_id, scope, version, key) WHERE finalized_at IS NOT NULL;
CREATE INDEX ci_cache_entries_idle ON ci_cache_entries (last_accessed_at);
