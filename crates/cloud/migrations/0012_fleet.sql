-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- Launches go through EC2 Fleet (crates/cloud/src/aws/fleet.rs).

-- 1 once the tenant's connect stack lets the role use EC2 Fleet, from
-- the hatchrun:fleet tag on its launch template, recorded when AWS is
-- connected. Tenants on an older stack keep launching with
-- RunInstances.
ALTER TABLE tenants ADD COLUMN fleet INTEGER NOT NULL DEFAULT 0 CHECK (fleet IN (0, 1));

-- Offerings EC2 refused for lack of capacity, skipped by launches until
-- they lapse (crates/cloud/src/offerings.rs). subnet_id is '' for the
-- launch template's network.
CREATE TABLE unavailable_offerings (
  tenant_id TEXT NOT NULL,
  instance_type TEXT NOT NULL,
  subnet_id TEXT NOT NULL,
  spot INTEGER NOT NULL CHECK (spot IN (0, 1)),
  code TEXT NOT NULL,
  until INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, instance_type, subnet_id, spot)
) STRICT;

CREATE INDEX unavailable_offerings_until ON unavailable_offerings (until);
