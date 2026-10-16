-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- A tenant launches runners while it holds a licence: a paid Polar
-- subscription, or a place in the invite-only beta.
ALTER TABLE tenants ADD COLUMN beta INTEGER NOT NULL DEFAULT 0 CHECK (beta IN (0, 1));

-- Polar subscriptions, one row each, as Polar's webhooks last reported
-- them. modified_at orders deliveries that arrive out of order.
CREATE TABLE subscriptions (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
  status TEXT NOT NULL,
  customer_id TEXT NOT NULL,
  product_id TEXT NOT NULL,
  current_period_end TEXT,
  cancel_at_period_end INTEGER NOT NULL CHECK (cancel_at_period_end IN (0, 1)),
  modified_at TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX subscriptions_tenant ON subscriptions (tenant_id, status);

-- Beta invite codes, by SHA-256 of the code. The operator adds them
-- with wrangler d1 execute (see README); a redeemed code puts the
-- tenant in the beta and uses up one of its uses.
CREATE TABLE invites (
  code_hash TEXT PRIMARY KEY,
  note TEXT NOT NULL DEFAULT '',
  uses_left INTEGER NOT NULL CHECK (uses_left >= 0),
  created_at INTEGER NOT NULL
) STRICT;

-- Organizations the operator lets into the beta without a code, by
-- GitHub login. Their tenants join the beta when an owner opens them.
CREATE TABLE beta_orgs (
  org_login TEXT PRIMARY KEY COLLATE NOCASE,
  created_at INTEGER NOT NULL
) STRICT;
