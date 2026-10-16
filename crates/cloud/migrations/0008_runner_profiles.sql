-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- An organization's own runner profiles (crates/cloud/src/profiles.rs).
-- Workflows name one in runs-on; a row named like a built-in profile
-- replaces it for this tenant, and deleting the row brings it back.
CREATE TABLE runner_profiles (
  tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  arch TEXT NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  -- Instance families, preferred first, comma separated.
  families TEXT NOT NULL,
  cpu_min INTEGER NOT NULL CHECK (cpu_min >= 1),
  cpu_max INTEGER CHECK (cpu_max >= cpu_min),
  -- GiB.
  memory_min INTEGER NOT NULL CHECK (memory_min >= 1),
  memory_max INTEGER CHECK (memory_max >= memory_min),
  -- Root volume in GiB; NULL for the connect stack's RootVolumeSize.
  disk_gib INTEGER,
  -- 1 for Spot, which falls back to on-demand without Spot capacity.
  spot INTEGER NOT NULL CHECK (spot IN (0, 1)),
  image TEXT NOT NULL,
  -- Reserved: add-ons such as a run cache, comma separated, and warm
  -- instances to keep ready. Empty and 0 until those exist.
  extras TEXT NOT NULL DEFAULT '',
  warm_pool INTEGER NOT NULL DEFAULT 0 CHECK (warm_pool >= 0),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, name)
) STRICT;

-- What the tenant's connect stack allows, from the tags on its launch
-- template, recorded when AWS is connected. NULL for a tenant connected
-- before the stack carried them; the Worker then assumes the old
-- template defaults.
ALTER TABLE tenants ADD COLUMN instance_types TEXT;
ALTER TABLE tenants ADD COLUMN volume_size_limit INTEGER;
ALTER TABLE tenants ADD COLUMN root_volume_size INTEGER;
