-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- One row per GitHub App installation that has connected an AWS account
-- with deploy/cloudformation/connect.yaml. Onboarding fills it from the
-- stack's outputs.
CREATE TABLE tenants (
  installation_id INTEGER PRIMARY KEY,
  -- The TenantId the stack was created with; tokens name it as
  -- tenant:<id> and instances carry it in the hatchrun:tenant tag.
  id TEXT NOT NULL UNIQUE,
  role_arn TEXT NOT NULL,
  region TEXT NOT NULL,
  launch_template_id TEXT NOT NULL,
  -- Comma separated, tried in order when a subnet has no capacity.
  subnet_ids TEXT NOT NULL,
  -- Runners launching or running at once; jobs past it wait.
  max_runners INTEGER NOT NULL CHECK (max_runners >= 0)
) STRICT;

-- The runner image for each region and architecture. The images
-- pipeline writes a row when it publishes an image.
CREATE TABLE images (
  region TEXT NOT NULL,
  arch TEXT NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  image_id TEXT NOT NULL,
  -- The image's root device, where a disk=N label puts its volume size.
  root_device TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (region, arch)
) STRICT;

-- Where a job's runner stands. NULL until the Worker takes the job;
-- 'launching' while it registers the runner and starts the instance,
-- 'launched' once the instance exists and the broker holds its config,
-- 'failed' with launch_error when it gave up. Only the Worker that
-- moves a job from NULL to 'launching' launches it.
ALTER TABLE jobs ADD COLUMN launch_state TEXT
  CHECK (launch_state IN ('launching', 'launched', 'failed'));
ALTER TABLE jobs ADD COLUMN runner_name TEXT;
ALTER TABLE jobs ADD COLUMN instance_id TEXT;
ALTER TABLE jobs ADD COLUMN launch_error TEXT;

CREATE INDEX jobs_launch ON jobs (installation_id, launch_state, status);
