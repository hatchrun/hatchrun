-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Runner templates, job token permissions and secret references of
-- hatchrun CI (crates/ci). Later tasks of the accounts plan append here.

ALTER TABLE ci_jobs ADD COLUMN template text;   -- first runs-on label, NULL for approval jobs
ALTER TABLE ci_jobs ADD COLUMN token_permissions jsonb NOT NULL DEFAULT '{"metadata":"read"}';
ALTER TABLE ci_jobs ALTER COLUMN token_permissions DROP DEFAULT;
ALTER TABLE ci_jobs ADD COLUMN secret_refs jsonb NOT NULL DEFAULT '{"all":false,"names":[]}';
ALTER TABLE ci_jobs ALTER COLUMN secret_refs DROP DEFAULT;

-- A card is the gate: an active or trialing subscription to the CI
-- product. Tenants no longer carry a switch.
ALTER TABLE tenants DROP COLUMN ci_enabled;
-- A CI subscription never licenses hatchrun.io runners, and the reverse.
ALTER TABLE subscriptions ADD COLUMN product text NOT NULL DEFAULT 'runners'
  CHECK (product IN ('runners', 'ci'));
ALTER TABLE subscriptions ALTER COLUMN product DROP DEFAULT;
