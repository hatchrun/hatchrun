-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- hatchrun CI environments, organization secret repository access, and
-- CI billed on the hatchrun.io subscription. Later tasks of the
-- environments plan append here.

-- CI is billed on the hatchrun.io product, so a subscription is a
-- subscription and the product column goes.
ALTER TABLE subscriptions DROP COLUMN product;
