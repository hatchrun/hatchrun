-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The instance type catalog (crates/cloud/src/instance_types.rs): the
-- current generation amd64 and arm64 types each region offers, which
-- the reconciler refreshes once a day.
CREATE TABLE instance_types (
  region TEXT NOT NULL,
  -- As in m8g.xlarge: family m8g, category m, generation 8 (0 for a
  -- name without one).
  name TEXT NOT NULL,
  family TEXT NOT NULL,
  category TEXT NOT NULL,
  generation INTEGER NOT NULL,
  arch TEXT NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  vcpus INTEGER NOT NULL,
  memory_mib INTEGER NOT NULL,
  gpus INTEGER NOT NULL,
  -- 1 for inference accelerators, Neuron devices or FPGAs.
  accelerators INTEGER NOT NULL CHECK (accelerators IN (0, 1)),
  bare_metal INTEGER NOT NULL CHECK (bare_metal IN (0, 1)),
  burstable INTEGER NOT NULL CHECK (burstable IN (0, 1)),
  local_storage INTEGER NOT NULL CHECK (local_storage IN (0, 1)),
  spot INTEGER NOT NULL CHECK (spot IN (0, 1)),
  -- Availability zone ids that offer it, comma separated.
  zones TEXT NOT NULL,
  -- Linux prices per hour in millionths of a US dollar: on-demand from
  -- AWS's public price list, the lowest Spot price across its zones.
  on_demand_micros INTEGER,
  spot_micros INTEGER,
  PRIMARY KEY (region, name)
) STRICT;

-- One row per region the reconciler refreshed or tried to.
CREATE TABLE instance_type_regions (
  region TEXT PRIMARY KEY,
  refreshed_at INTEGER,
  attempted_at INTEGER NOT NULL,
  types INTEGER NOT NULL DEFAULT 0,
  -- Why the last attempt failed, NULL when it did not.
  error TEXT
) STRICT;
