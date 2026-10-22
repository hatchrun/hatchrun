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
  units bigint NOT NULL CHECK (units >= 0), -- priced when the job completed
  ended_at bigint NOT NULL,
  sent_at bigint,                          -- when Polar took it
  rejected_at bigint,                      -- when Polar refused it for good
  reject_reason text,
  PRIMARY KEY (job_id, attempt)
);
CREATE INDEX ci_usage_month ON ci_usage (tenant_id, ended_at);
CREATE INDEX ci_usage_unsent ON ci_usage (ended_at) WHERE sent_at IS NULL AND rejected_at IS NULL;

-- The monthly spend cap an owner raised above the default.
CREATE TABLE ci_spend_caps (
  tenant_id text PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
  cap_micros bigint NOT NULL CHECK (cap_micros > 0),
  updated_at bigint NOT NULL
);

-- Why a queued job waits for a launch: no card, the spend cap, or the
-- concurrency cap. Cleared when the job is claimed or re-run.
ALTER TABLE ci_jobs ADD COLUMN held text CHECK (held IN ('card', 'spend_cap', 'concurrency'));

-- Secrets are sealed (AES-256-GCM) with a key that rotates: key_id names
-- the key that sealed the row, and the associated data binds the value
-- to its tenant, scope and name. repo_id NULL is an organization value.
CREATE TABLE ci_secrets (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint REFERENCES ci_repos (id) ON DELETE CASCADE,
  name text NOT NULL,
  key_id text NOT NULL,
  sealed text NOT NULL,                    -- base64 of nonce || ciphertext
  updated_at bigint NOT NULL,
  UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, name)
);
CREATE INDEX ci_secrets_key ON ci_secrets (key_id);

-- Vars are plain text, same scoping.
CREATE TABLE ci_vars (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint REFERENCES ci_repos (id) ON DELETE CASCADE,
  name text NOT NULL,
  value text NOT NULL,
  updated_at bigint NOT NULL,
  UNIQUE NULLS NOT DISTINCT (tenant_id, repo_id, name)
);

-- The warm pool: stopped instances that booted once and wait for a job,
-- one pool per runner template (the template's first label). Times are
-- Unix milliseconds. A job takes the oldest ready row; the reconciler
-- keeps the rest.
CREATE TABLE ci_pool (
  instance_id text PRIMARY KEY,
  template text NOT NULL,
  state text NOT NULL CHECK (state IN ('warming', 'stopping', 'ready')),
  launched_at bigint NOT NULL,                 -- Unix ms
  since bigint NOT NULL                        -- Unix ms
);
CREATE INDEX ci_pool_ready ON ci_pool (template, launched_at) WHERE state = 'ready';

-- The pool's size follows the claims of the last hour.
CREATE INDEX ci_jobs_started ON ci_jobs (started_at) WHERE started_at IS NOT NULL;
