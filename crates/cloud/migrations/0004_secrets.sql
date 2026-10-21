-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Secrets, variables and environments that owners set in the app for
-- their organization's jobs (crates/cloud/src/secrets.rs). Names are
-- case-insensitive: each table is unique on lower(name) within its
-- scope. Repositories are named without their owner, in lower case,
-- since the tenant is the owner. NULLS NOT DISTINCT makes rows with no
-- repository or no environment collide like any other value.

-- Named groups of secrets and variables with their own branch rules
-- and approval. repo NULL means every repository of the organization.
-- branches is a JSON array of branch patterns, empty for every branch;
-- reviewers a JSON array of GitHub logins who may approve.
CREATE TABLE environments (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo text,
  name text NOT NULL,
  branches text NOT NULL DEFAULT '[]',
  require_approval boolean NOT NULL DEFAULT false,
  reviewers text NOT NULL DEFAULT '[]',
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL
);
CREATE UNIQUE INDEX environments_name ON environments (tenant_id, repo, lower(name))
  NULLS NOT DISTINCT;

-- Encrypted values. scope says which of repo and environment_id is
-- set: neither for the organization, repo for a repository,
-- environment_id for an environment. ciphertext is AES-256-GCM output
-- with its tag and nonce its 96-bit nonce, both base64; key_id names
-- the key that sealed it, so keys can rotate. The additional data
-- binds a value to its tenant, scope and name: a row copied anywhere
-- else does not decrypt. updated_by is the owner's GitHub login.
CREATE TABLE secrets (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  scope text NOT NULL CHECK (scope IN ('org', 'repo', 'env')),
  repo text,
  environment_id bigint REFERENCES environments (id) ON DELETE CASCADE,
  name text NOT NULL,
  ciphertext text NOT NULL,
  nonce text NOT NULL,
  key_id text NOT NULL,
  updated_at bigint NOT NULL,
  updated_by text NOT NULL,
  CHECK (
    (scope = 'org' AND repo IS NULL AND environment_id IS NULL)
    OR (scope = 'repo' AND repo IS NOT NULL AND environment_id IS NULL)
    OR (scope = 'env' AND repo IS NULL AND environment_id IS NOT NULL)
  )
);
CREATE UNIQUE INDEX secrets_name ON secrets
  (tenant_id, scope, repo, environment_id, lower(name)) NULLS NOT DISTINCT;
CREATE INDEX secrets_environment ON secrets (environment_id);

-- Plain values, readable in the app and in job logs.
CREATE TABLE variables (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  scope text NOT NULL CHECK (scope IN ('org', 'repo', 'env')),
  repo text,
  environment_id bigint REFERENCES environments (id) ON DELETE CASCADE,
  name text NOT NULL,
  value text NOT NULL,
  updated_at bigint NOT NULL,
  updated_by text NOT NULL,
  CHECK (
    (scope = 'org' AND repo IS NULL AND environment_id IS NULL)
    OR (scope = 'repo' AND repo IS NOT NULL AND environment_id IS NULL)
    OR (scope = 'env' AND repo IS NULL AND environment_id IS NOT NULL)
  )
);
CREATE UNIQUE INDEX variables_name ON variables
  (tenant_id, scope, repo, environment_id, lower(name)) NULLS NOT DISTINCT;
CREATE INDEX variables_environment ON variables (environment_id);

-- Every change to the tables above, for the app's activity list,
-- newest first by id. Never a value. environment is the environment's
-- name, kept here since the environment may be gone; for an
-- environment's own events scope is 'org' when it covers every
-- repository and 'repo' otherwise.
CREATE TABLE secret_events (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  actor text NOT NULL,
  action text NOT NULL CHECK (action IN ('set', 'delete')),
  kind text NOT NULL CHECK (kind IN ('secret', 'variable', 'environment')),
  scope text NOT NULL CHECK (scope IN ('org', 'repo', 'env')),
  repo text,
  environment text,
  name text NOT NULL,
  at bigint NOT NULL
);
CREATE INDEX secret_events_tenant ON secret_events (tenant_id, id DESC);
