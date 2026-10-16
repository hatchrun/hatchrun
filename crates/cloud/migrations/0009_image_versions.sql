-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- The hatchrun/images release each image row came from, YYYYMMDD.HHMM,
-- so a late post of an older release cannot replace a newer one. NULL
-- for rows written before releases were posted.
ALTER TABLE images ADD COLUMN version TEXT;
