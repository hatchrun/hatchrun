-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The job cache (#53). A tenant's bucket comes from its connect stack
-- settings, version 5 on.
ALTER TABLE tenants ADD COLUMN cache_bucket TEXT;

-- Where a job writes and reads cache entries, worked out from its
-- workflow run on its first cache call: the write scope, and the read
-- scopes as a JSON array (see crates/cloud/src/cache.rs).
ALTER TABLE jobs ADD COLUMN cache_write TEXT;
ALTER TABLE jobs ADD COLUMN cache_reads TEXT;

-- Multipart uploads in progress, so a job completes or aborts only its
-- own. S3's lifecycle rule removes the parts of one nobody finishes.
CREATE TABLE cache_uploads (
  id TEXT PRIMARY KEY,
  job_id INTEGER NOT NULL,
  object_key TEXT NOT NULL,
  upload_id TEXT NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX cache_uploads_created ON cache_uploads (created_at);
