-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- hatchrun.io's tables, as D1 held them after its last migration.
-- Times are Unix ms in bigint columns, as the Worker passes them; ids
-- are GitHub's.

-- One row per GitHub workflow job that asked for a hatchrun runner.
-- GitHub may deliver a webhook more than once; the job id keeps the
-- second delivery from launching a second runner.
CREATE TABLE jobs (
  id bigint PRIMARY KEY,
  installation_id bigint NOT NULL,
  repository text NOT NULL,
  run_id bigint NOT NULL,
  label text NOT NULL,
  status text NOT NULL CHECK (status IN ('queued', 'in_progress', 'completed')),
  queued_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  -- Where a job's runner stands. NULL until the Worker takes the job;
  -- 'launching' while it registers the runner and starts the instance,
  -- 'launched' once the instance exists and the broker holds its
  -- config, 'failed' with launch_error when it gave up. Only the Worker
  -- that moves a job from NULL to 'launching' launches it.
  launch_state text CHECK (launch_state IN ('launching', 'launched', 'failed')),
  runner_name text,
  instance_id text,
  launch_error text,
  -- The runner that took the job, from GitHub's in_progress and
  -- completed events. A runner may take another job with the same
  -- label than the one it was launched for.
  ran_on text,
  -- When the reconciler last asked GitHub about the job.
  checked_at bigint,
  -- Launches the Worker started for the job, counted when it claims it.
  launch_attempts bigint NOT NULL DEFAULT 0,
  -- With launch_state 'failed': 'backoff' when the reconciler launches
  -- the job again at next_launch_at, 'setup' when it waits for an
  -- owner to change the organization's setup. NULL when nothing
  -- launches it again.
  launch_retry text CHECK (launch_retry IN ('backoff', 'setup')),
  next_launch_at bigint,
  -- Where the job writes and reads cache entries: the write scope, and
  -- the read scopes as a JSON array (crates/cloud/src/cache.rs). For
  -- jobs whose run has no row.
  cache_write text,
  cache_reads text,
  -- The job's name as the run shows it, such as "Save (host)", and the
  -- run attempt it belongs to.
  name text NOT NULL DEFAULT '',
  run_attempt bigint NOT NULL DEFAULT 1,
  -- GitHub's conclusion once completed: success, failure, cancelled,
  -- skipped, timed_out, neutral, action_required.
  conclusion text,
  -- GitHub's times: when a runner took the job and when it finished.
  -- queued_at stays when the Worker heard of it.
  started_at bigint,
  completed_at bigint,
  -- When the reconciler deletes the row: queued_at plus the tenant's
  -- retention_days when the row was recorded.
  expires_at bigint
);
CREATE INDEX jobs_status ON jobs (status, queued_at);
CREATE INDEX jobs_recent ON jobs (installation_id, queued_at);
CREATE INDEX jobs_ran_on ON jobs (ran_on) WHERE ran_on IS NOT NULL;
-- The Failed tab pages back through failed launches only.
CREATE INDEX jobs_failed ON jobs (installation_id, queued_at)
  WHERE launch_state = 'failed';
-- The 24 hour strip groups the last day's jobs by updated_at.
CREATE INDEX jobs_updated ON jobs (installation_id, updated_at);
-- A run's jobs, and the newest attempt of each job name.
CREATE INDEX jobs_run ON jobs (run_id, name, run_attempt);
-- Open jobs, a small share of the table: launch's CLAIM counts a
-- tenant's runners and the reconciler reads a tenant's open jobs here.
CREATE INDEX jobs_open ON jobs (installation_id, launch_state)
  WHERE status <> 'completed';
-- Open jobs the reconciler asks GitHub about, quietest first.
CREATE INDEX jobs_unchecked ON jobs ((COALESCE(checked_at, updated_at)))
  WHERE status <> 'completed';
CREATE INDEX jobs_expiry ON jobs (expires_at);
-- Completed jobs a runner took, for the reconciler's sweep of jobs
-- whose minutes no webhook recorded.
CREATE INDEX jobs_completed ON jobs (completed_at) WHERE ran_on IS NOT NULL;

-- The runner image for each region and architecture. The images
-- pipeline writes a row when it publishes an image.
CREATE TABLE images (
  region text NOT NULL,
  arch text NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  image_id text NOT NULL,
  -- The image's root device, where a disk=N label puts its volume size.
  root_device text NOT NULL,
  updated_at bigint NOT NULL,
  -- The hatchrun/images release, YYYYMMDD.HHMM, so a late post of an
  -- older release cannot replace a newer one.
  version text,
  PRIMARY KEY (region, arch)
);

-- People who have signed in to app.hatchrun.io with GitHub, by GitHub
-- user id, which survives a rename.
CREATE TABLE users (
  id bigint PRIMARY KEY,
  login text NOT NULL,
  name text,
  avatar_url text NOT NULL,
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL
);

-- One row per signed-in browser, keyed by the SHA-256 of the session
-- id the browser holds, so the table alone cannot sign anyone in.
-- github_token is the user's GitHub token, sealed under a key derived
-- from the session id (crates/cloud/src/session.rs).
CREATE TABLE sessions (
  key text PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  github_token text NOT NULL,
  created_at bigint NOT NULL,
  expires_at bigint NOT NULL
);
CREATE INDEX sessions_expiry ON sessions (expires_at);
CREATE INDEX sessions_user ON sessions (user_id);

-- One row per GitHub organization set up in the app.
CREATE TABLE tenants (
  -- Random, [a-z0-9]{16}. Tokens name it as tenant:<id> and instances
  -- carry it in the hatchrun:tenant tag.
  id text PRIMARY KEY,
  -- The GitHub organization, by id, which survives renames, and login,
  -- refreshed whenever an owner opens the app.
  org_id bigint NOT NULL UNIQUE,
  org_login text NOT NULL,
  -- Set once an owner installs the App on the organization.
  installation_id bigint UNIQUE,
  -- The outputs of deploy/cloudformation/connect.yaml, set once the
  -- role is verified.
  role_arn text,
  region text,
  launch_template_id text,
  -- Comma separated, tried in order when a subnet has no capacity.
  subnet_ids text,
  -- Runners launching or running at once; jobs past it wait.
  max_runners bigint NOT NULL DEFAULT 10 CHECK (max_runners >= 0),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  -- In the invite-only beta, which launches without a subscription.
  beta boolean NOT NULL DEFAULT false,
  -- Regions runners may use, comma separated; NULL for every region.
  allowed_regions text,
  -- When the reconciler last went over the tenant.
  reconciled_at bigint,
  -- What the connect stack allows, from the tags on its launch
  -- template. NULL for a stack from before the tags.
  instance_types text,
  volume_size_limit bigint,
  root_volume_size bigint,
  -- The stack lets the role use EC2 Fleet (hatchrun:fleet tag).
  fleet boolean NOT NULL DEFAULT false,
  excluded_instance_types text,
  -- The connect.yaml version the stack runs, NULL for version 1.
  template_version bigint,
  -- When the reconciler last read the stack's launch template, and
  -- last deleted the launch template versions launches left behind.
  stack_checked_at bigint,
  versions_swept_at bigint,
  -- The job cache's bucket, from connect stack version 5 on.
  cache_bucket text,
  -- How long jobs and runs are kept: 30 days, up to 90.
  retention_days bigint NOT NULL DEFAULT 30 CHECK (retention_days BETWEEN 1 AND 90),
  -- When an unpaid tenant first used up its free minutes in a month.
  free_used_up_at bigint
);

-- Who may manage a tenant. Each is checked against GitHub when they
-- act; a row alone grants nothing to someone no longer an owner.
CREATE TABLE members (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at bigint NOT NULL,
  PRIMARY KEY (tenant_id, user_id)
);
CREATE INDEX members_user ON members (user_id);

-- Polar subscriptions as Polar's webhooks last reported them.
-- modified_at orders deliveries that arrive out of order.
CREATE TABLE subscriptions (
  id text PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  status text NOT NULL,
  customer_id text NOT NULL,
  product_id text NOT NULL,
  current_period_end text,
  cancel_at_period_end boolean NOT NULL,
  modified_at text NOT NULL,
  updated_at bigint NOT NULL
);
CREATE INDEX subscriptions_tenant ON subscriptions (tenant_id, status);

-- Beta invite codes, by SHA-256 of the code. A redeemed code puts the
-- tenant in the beta and uses up one of its uses.
CREATE TABLE invites (
  code_hash text PRIMARY KEY,
  note text NOT NULL DEFAULT '',
  uses_left bigint NOT NULL CHECK (uses_left >= 0),
  created_at bigint NOT NULL
);

-- Organizations let into the beta without a code, by GitHub login in
-- lower case.
CREATE TABLE beta_orgs (
  org_login text PRIMARY KEY CHECK (org_login = lower(org_login)),
  created_at bigint NOT NULL
);

-- Webhook deliveries the reconciler asked GitHub to deliver again, by
-- the event's guid, so no event is redelivered twice.
CREATE TABLE redeliveries (
  guid text PRIMARY KEY,
  delivery_id bigint NOT NULL,
  event text NOT NULL,
  redelivered_at bigint NOT NULL
);
CREATE INDEX redeliveries_age ON redeliveries (redelivered_at);

-- One row. lease_until keeps passes from overlapping; deliveries_until
-- is the newest delivery the last complete scan saw; summary is the
-- last pass's counts as JSON; history the last passes' errors as a
-- JSON array, newest last; digest_at when the daily digest of firing
-- alerts last went out.
CREATE TABLE reconciler (
  id bigint PRIMARY KEY CHECK (id = 1),
  lease_until bigint NOT NULL,
  deliveries_until bigint,
  started_at bigint,
  finished_at bigint,
  summary text,
  history text,
  digest_at bigint
);
INSERT INTO reconciler (id, lease_until) VALUES (1, 0);

-- An organization's own runner profiles (crates/cloud/src/profiles.rs).
CREATE TABLE runner_profiles (
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  name text NOT NULL,
  arch text NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  -- Instance families, preferred first, comma separated.
  families text NOT NULL,
  cpu_min bigint NOT NULL CHECK (cpu_min >= 1),
  cpu_max bigint CHECK (cpu_max >= cpu_min),
  -- GiB.
  memory_min bigint NOT NULL CHECK (memory_min >= 1),
  memory_max bigint CHECK (memory_max >= memory_min),
  -- Root volume in GiB; NULL for the connect stack's RootVolumeSize.
  disk_gib bigint,
  spot boolean NOT NULL,
  image text NOT NULL,
  extras text NOT NULL DEFAULT '',
  warm_pool bigint NOT NULL DEFAULT 0 CHECK (warm_pool >= 0),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  -- Instance categories, comma separated, empty for any.
  categories text NOT NULL DEFAULT '',
  -- The oldest generation, NULL for any.
  min_generation bigint CHECK (min_generation BETWEEN 1 AND 20),
  capacity text NOT NULL DEFAULT 'on-demand'
    CHECK (capacity IN ('on-demand', 'spot', 'spot-with-fallback')),
  PRIMARY KEY (tenant_id, name)
);

-- The instance type catalog (crates/cloud/src/instance_types.rs),
-- which the reconciler refreshes once a day.
CREATE TABLE instance_types (
  region text NOT NULL,
  -- As in m8g.xlarge: family m8g, category m, generation 8 (0 for a
  -- name without one).
  name text NOT NULL,
  family text NOT NULL,
  category text NOT NULL,
  generation bigint NOT NULL,
  arch text NOT NULL CHECK (arch IN ('amd64', 'arm64')),
  vcpus bigint NOT NULL,
  memory_mib bigint NOT NULL,
  gpus bigint NOT NULL,
  -- Inference accelerators, Neuron devices or FPGAs.
  accelerators boolean NOT NULL,
  bare_metal boolean NOT NULL,
  burstable boolean NOT NULL,
  local_storage boolean NOT NULL,
  spot boolean NOT NULL,
  -- Availability zone ids that offer it, comma separated.
  zones text NOT NULL,
  -- Linux prices per hour in millionths of a US dollar.
  on_demand_micros bigint,
  spot_micros bigint,
  PRIMARY KEY (region, name)
);

CREATE TABLE instance_type_regions (
  region text PRIMARY KEY,
  refreshed_at bigint,
  attempted_at bigint NOT NULL,
  types bigint NOT NULL DEFAULT 0,
  -- Why the last attempt failed, NULL when it did not.
  error text
);

-- GitHub Actions tokens that posted an images release, by jti, so each
-- works once.
CREATE TABLE used_tokens (
  jti text PRIMARY KEY,
  expires_at bigint NOT NULL
);

-- Offerings EC2 refused for lack of capacity, skipped by launches until
-- they lapse (crates/cloud/src/offerings.rs). subnet_id is '' for the
-- launch template's network.
CREATE TABLE unavailable_offerings (
  tenant_id text NOT NULL,
  instance_type text NOT NULL,
  subnet_id text NOT NULL,
  spot boolean NOT NULL,
  code text NOT NULL,
  until bigint NOT NULL,
  PRIMARY KEY (tenant_id, instance_type, subnet_id, spot)
);
CREATE INDEX unavailable_offerings_until ON unavailable_offerings (until);

-- Multipart cache uploads in progress, so a job completes or aborts
-- only its own.
CREATE TABLE cache_uploads (
  id text PRIMARY KEY,
  job_id bigint NOT NULL,
  object_key text NOT NULL,
  upload_id text NOT NULL,
  created_at bigint NOT NULL
);
CREATE INDEX cache_uploads_created ON cache_uploads (created_at);

-- One row per GitHub workflow run with at least one hatchrun job.
CREATE TABLE runs (
  id bigint PRIMARY KEY,
  installation_id bigint NOT NULL,
  repository text NOT NULL,
  -- NULL where GitHub's workflow_job has none.
  workflow_name text,
  head_branch text,
  head_sha text NOT NULL,
  -- The newest attempt any job has reported, and when its first job
  -- was queued. The list orders and pages by queued_at.
  attempt bigint NOT NULL CHECK (attempt >= 1),
  queued_at bigint NOT NULL,
  -- From GET /repos/{repo}/actions/runs/{id}, NULL until details_at.
  run_number bigint,
  event text,
  display_title text,
  actor text,
  path text,
  details_at bigint,
  -- When the reconciler last asked for the details.
  details_tried_at bigint,
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  expires_at bigint,
  -- Where the run's jobs write and read cache entries.
  cache_write text,
  cache_reads text,
  -- When the run's last current job ended: the latest completed_at, or
  -- updated_at where GitHub sent none, never before queued_at. NULL
  -- while a current job has not completed. Kept by jobs_end below.
  ended_at bigint
);
CREATE INDEX runs_recent ON runs (installation_id, queued_at, id);
CREATE INDEX runs_age ON runs (queued_at);
CREATE INDEX runs_details ON runs ((COALESCE(details_tried_at, created_at)))
  WHERE details_at IS NULL;
CREATE INDEX runs_expiry ON runs (expires_at);
-- By repository, A to Z and Z to A, each repository newest first.
CREATE INDEX runs_repository ON runs
  (installation_id, lower(repository), queued_at, id);
CREATE INDEX runs_repository_az ON runs
  (installation_id, lower(repository), queued_at DESC, id DESC);
-- By how long runs took: live runs first, oldest first, then finished
-- runs shortest or longest first.
CREATE INDEX runs_took ON runs (installation_id, (ended_at IS NOT NULL),
  (COALESCE(ended_at - queued_at, queued_at)), id);
CREATE INDEX runs_took_desc ON runs (installation_id, (ended_at IS NOT NULL),
  (CASE WHEN ended_at IS NULL THEN queued_at ELSE queued_at - ended_at END), id);

-- Works out a run's ended_at again from its current jobs, the newest
-- attempt of each job name.
CREATE FUNCTION runs_end(run bigint) RETURNS void LANGUAGE sql AS $$
  UPDATE runs SET ended_at = (
    SELECT CASE WHEN bool_or(j.status <> 'completed') THEN NULL
      ELSE GREATEST(runs.queued_at,
        COALESCE(MAX(COALESCE(j.completed_at, j.updated_at)), runs.queued_at))
    END
    FROM jobs AS j
    WHERE j.run_id = runs.id AND NOT EXISTS (SELECT 1 FROM jobs AS later
      WHERE later.run_id = j.run_id AND later.name = j.name
      AND later.run_attempt > j.run_attempt)
  ) WHERE runs.id = run;
$$;

-- Every write to a job that can change its run's end. Launch
-- bookkeeping on a job that has not completed leaves the run alone.
CREATE FUNCTION jobs_end() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    PERFORM runs_end(NEW.run_id);
  ELSIF TG_OP = 'DELETE' THEN
    PERFORM runs_end(OLD.run_id);
  ELSIF OLD.status IS DISTINCT FROM NEW.status
    OR OLD.run_id IS DISTINCT FROM NEW.run_id
    OR OLD.name IS DISTINCT FROM NEW.name
    OR OLD.run_attempt IS DISTINCT FROM NEW.run_attempt
    OR (NEW.status = 'completed' AND (OLD.completed_at IS DISTINCT FROM NEW.completed_at
      OR (NEW.completed_at IS NULL AND OLD.updated_at IS DISTINCT FROM NEW.updated_at))) THEN
    PERFORM runs_end(OLD.run_id);
    IF NEW.run_id <> OLD.run_id THEN
      PERFORM runs_end(NEW.run_id);
    END IF;
  END IF;
  RETURN NULL;
END;
$$;

CREATE TRIGGER jobs_end AFTER INSERT OR UPDATE OR DELETE ON jobs
  FOR EACH ROW EXECUTE FUNCTION jobs_end();

-- Billing is metered. One row per job that ran on a hatchrun runner,
-- written once when the job completes. Rows outlive the jobs table's
-- retention, since a month is longer than 30 days.
CREATE TABLE usage_events (
  job_id bigint PRIMARY KEY,
  tenant_id text NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  -- GitHub's started_at to completed_at, rounded up to whole minutes.
  minutes bigint NOT NULL CHECK (minutes > 0),
  -- When the job completed.
  ended_at bigint NOT NULL,
  -- The tenant paid per minute as the job completed: those rows go to
  -- Polar's meter. Beta tenants and tenants on free minutes only count
  -- toward the month.
  billed boolean NOT NULL,
  -- When Polar took the event.
  sent_at bigint
);
CREATE INDEX usage_events_month ON usage_events (tenant_id, ended_at);
CREATE INDEX usage_events_unsent ON usage_events (ended_at)
  WHERE billed AND sent_at IS NULL;
CREATE INDEX usage_events_done ON usage_events (ended_at)
  WHERE NOT billed OR sent_at IS NOT NULL;

-- Alerts (crates/cloud/src/alerts.rs). state is what the last check
-- found; sent is what the last email said, NULL before the first.
CREATE TABLE alerts (
  key text PRIMARY KEY,
  state text NOT NULL CHECK (state IN ('firing', 'resolved')),
  detail text NOT NULL,
  fired_at bigint NOT NULL,
  resolved_at bigint,
  sent text CHECK (sent IN ('firing', 'resolved')),
  updated_at bigint NOT NULL
);

-- Launches whose message ran out of retries and reached
-- hatchrun-launches-dlq, once per message.
CREATE TABLE failed_launches (
  id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
  message_id text NOT NULL UNIQUE,
  job_id bigint,
  installation_id bigint,
  error text,
  attempts bigint,
  failed_at bigint NOT NULL
);
CREATE INDEX failed_launches_age ON failed_launches (failed_at);

-- Installations whose GitHub answers were rate limited or had little of
-- the hour's requests left.
CREATE TABLE rate_limits (
  installation_id bigint PRIMARY KEY,
  remaining bigint,
  limited boolean NOT NULL,
  seen_at bigint NOT NULL
);
CREATE INDEX rate_limits_seen ON rate_limits (seen_at);
