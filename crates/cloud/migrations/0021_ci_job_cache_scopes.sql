-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The cache scopes of a claimed job, a JSON array of refs: the one it
-- saves to, then the ones it restores from after it. They are fixed when
-- the job is claimed, because its runtime token names them and BuildKit
-- builds its index keys from the token; the cache service reads the same
-- list, so a default branch renamed while the job runs cannot make the
-- two disagree. Null before the claim.
ALTER TABLE ci_jobs ADD COLUMN cache_scopes text;
