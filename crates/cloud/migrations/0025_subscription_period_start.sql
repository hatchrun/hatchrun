-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Polar bills from the subscription anniversary, so the spend cap
-- counts usage over the subscription's current period instead of the
-- UTC calendar month. Every row here names that period's start, as
-- ISO 8601 text in UTC like the period end next to it.

ALTER TABLE subscriptions ADD COLUMN current_period_start text;

-- Backfill from the period end minus one month, the billing interval
-- of every product.
UPDATE subscriptions
  SET current_period_start =
    to_char((current_period_end::timestamptz - interval '1 month') AT TIME ZONE 'UTC',
      'YYYY-MM-DD"T"HH24:MI:SS"Z"')
  WHERE current_period_start IS NULL AND current_period_end IS NOT NULL;

-- Rows without a period end keep their last change as the start.
UPDATE subscriptions
  SET current_period_start = modified_at
  WHERE current_period_start IS NULL;

ALTER TABLE subscriptions ALTER COLUMN current_period_start SET NOT NULL;
