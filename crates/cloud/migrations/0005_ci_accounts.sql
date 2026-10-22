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

-- One row per metered attempt: minutes from claim to result. No FK to
-- ci_jobs, usage outlives the 30 day job rows. Polar takes unsent rows.
CREATE TABLE ci_usage (
  job_id bigint NOT NULL,
  attempt bigint NOT NULL,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  template text NOT NULL,
  minutes bigint NOT NULL CHECK (minutes > 0),
  ended_at bigint NOT NULL,
  sent_at bigint,                          -- when Polar took it
  PRIMARY KEY (job_id, attempt)
);
CREATE INDEX ci_usage_month ON ci_usage (tenant_id, ended_at);
CREATE INDEX ci_usage_unsent ON ci_usage (ended_at) WHERE sent_at IS NULL;

-- The monthly spend cap an owner raised above the default.
CREATE TABLE ci_spend_caps (
  tenant_id text PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
  cap_micros bigint NOT NULL CHECK (cap_micros > 0),
  updated_at bigint NOT NULL
);
