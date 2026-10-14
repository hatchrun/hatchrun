-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- Regions an organization lets its runners use, comma separated; NULL
-- for every region hatchrun runs in. The app connects AWS only in one
-- of them and the Worker launches only there.
ALTER TABLE tenants ADD COLUMN allowed_regions TEXT;

-- The dashboard lists a tenant's jobs newest first.
CREATE INDEX jobs_recent ON jobs (installation_id, queued_at);
