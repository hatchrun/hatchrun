-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Tenants now begin when an organization owner sets the organization
-- up in the app, before the App is installed or AWS connected, so the
-- installation and the stack's outputs become optional and the GitHub
-- organization becomes the key. 0002 created the table for tenants
-- made by hand and none were; the guard below stops this migration
-- rather than drop rows if any exist.
CREATE TABLE migration_guard (tenants INTEGER NOT NULL CHECK (tenants = 0)) STRICT;
INSERT INTO migration_guard SELECT COUNT(*) FROM tenants;
DROP TABLE migration_guard;
DROP TABLE tenants;

CREATE TABLE tenants (
  -- Random, [a-z0-9]{16}. Tokens name it as tenant:<id> and instances
  -- carry it in the hatchrun:tenant tag.
  id TEXT PRIMARY KEY,
  -- The GitHub organization, by id, which survives renames, and login,
  -- refreshed whenever an owner opens the app.
  org_id INTEGER NOT NULL UNIQUE,
  org_login TEXT NOT NULL,
  -- Set once an owner installs the App on the organization.
  installation_id INTEGER UNIQUE,
  -- The outputs of deploy/cloudformation/connect.yaml, set once the
  -- role is verified.
  role_arn TEXT,
  region TEXT,
  launch_template_id TEXT,
  -- Comma separated, tried in order when a subnet has no capacity.
  subnet_ids TEXT,
  -- Runners launching or running at once; jobs past it wait.
  max_runners INTEGER NOT NULL DEFAULT 10 CHECK (max_runners >= 0),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- Who may manage a tenant: the owner who set it up and every other
-- owner of the organization who has opened it since. Each is checked
-- against GitHub when they act; a row alone grants nothing to someone
-- who is no longer an owner.
CREATE TABLE members (
  tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id)
) STRICT;

CREATE INDEX members_user ON members (user_id);
