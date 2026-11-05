-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The repositories of a tenant's GitHub App installation, which the
-- console lists and lets an admin enable for CI. ci_repos only holds
-- repositories that delivered an event; this holds all of them. It is
-- kept in sync from the installation_repositories webhook, and read
-- again from GitHub when it is empty or old.
--
-- A tenant with ci_all_repos runs every repository. Existing tenants
-- start there, so nothing stops running. Without it, only repositories
-- with enabled set run.
ALTER TABLE tenants ADD COLUMN ci_all_repos boolean NOT NULL DEFAULT true;
ALTER TABLE tenants ADD COLUMN ci_repos_synced_at bigint;

CREATE TABLE ci_installation_repos (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  repo_id bigint NOT NULL,                     -- GitHub repository id
  full_name text NOT NULL,
  default_branch text NOT NULL,
  private boolean NOT NULL,
  enabled boolean NOT NULL,
  updated_at bigint NOT NULL,
  PRIMARY KEY (tenant_id, repo_id)
);
CREATE INDEX ci_installation_repos_name ON ci_installation_repos (tenant_id, lower(full_name));
