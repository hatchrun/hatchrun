-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Runner profiles hold requirements in Karpenter's vocabulary
-- (crates/cloud/src/profiles.rs); families become an optional pin.

-- Instance categories, comma separated, empty for any. Profiles saved
-- before this pinned their families, which decide on their own.
ALTER TABLE runner_profiles ADD COLUMN categories TEXT NOT NULL DEFAULT '';
-- The oldest generation, NULL for any.
ALTER TABLE runner_profiles ADD COLUMN min_generation INTEGER
  CHECK (min_generation BETWEEN 1 AND 20);
-- Replaces spot, which stays in step for older Workers.
ALTER TABLE runner_profiles ADD COLUMN capacity TEXT NOT NULL DEFAULT 'on-demand'
  CHECK (capacity IN ('on-demand', 'spot', 'spot-with-fallback'));
UPDATE runner_profiles SET capacity = 'spot-with-fallback' WHERE spot = 1;

-- The connect stack's ExcludedInstanceTypes, from the
-- hatchrun:excluded-instance-types tag on its launch template; NULL for
-- a stack from before it, which excludes nothing.
ALTER TABLE tenants ADD COLUMN excluded_instance_types TEXT;
