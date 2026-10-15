-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- When the reconciler last deleted the launch template versions that
-- launches left behind, so it lists them once a day.
ALTER TABLE tenants ADD COLUMN versions_swept_at INTEGER;
