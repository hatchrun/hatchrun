-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- hatchrun is its own CI only: the self-hosted GitHub Actions runner
-- product is gone, and with it the tables and tenant columns that only
-- it read. What stays is what hatchrun-ci and the sign-in and billing
-- pages use: users, sessions, tenants, members, subscriptions,
-- beta_orgs, images, used_tokens, unavailable_offerings and the ci_*
-- tables.

-- The run list's trigger and functions go with the jobs table.
DROP TRIGGER jobs_end ON jobs;
DROP FUNCTION jobs_end();
DROP FUNCTION runs_end(bigint);

DROP TABLE jobs;
DROP TABLE runs;
DROP TABLE usage_events;
DROP TABLE runner_profiles;
DROP TABLE instance_types;
DROP TABLE instance_type_regions;
DROP TABLE cache_uploads;
DROP TABLE alerts;
DROP TABLE failed_launches;
DROP TABLE rate_limits;
DROP TABLE redeliveries;
DROP TABLE reconciler;
DROP TABLE invites;

-- The connect stack, the runner limits and the stack's bookkeeping.
ALTER TABLE tenants
  DROP COLUMN role_arn,
  DROP COLUMN region,
  DROP COLUMN launch_template_id,
  DROP COLUMN subnet_ids,
  DROP COLUMN max_runners,
  DROP COLUMN allowed_regions,
  DROP COLUMN reconciled_at,
  DROP COLUMN instance_types,
  DROP COLUMN volume_size_limit,
  DROP COLUMN root_volume_size,
  DROP COLUMN fleet,
  DROP COLUMN excluded_instance_types,
  DROP COLUMN template_version,
  DROP COLUMN stack_checked_at,
  DROP COLUMN versions_swept_at,
  DROP COLUMN cache_bucket;
