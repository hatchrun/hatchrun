-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Which version of connect.yaml a tenant's stack runs, from the
-- hatchrun:template-version tag on its launch template; NULL for a
-- stack from before the tag (version 1). The app offers an update when
-- it is older than crates/cloud/src/connect.rs TEMPLATE_VERSION.
ALTER TABLE tenants ADD COLUMN template_version INTEGER;
-- When the reconciler last read the stack's launch template, so an
-- updated stack is picked up without verifying the role again.
ALTER TABLE tenants ADD COLUMN stack_checked_at INTEGER;
