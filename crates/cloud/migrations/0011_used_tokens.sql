-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- The GitHub Actions tokens that posted an images release, by jti, so
-- each works once. Rows go once the token has expired.
CREATE TABLE used_tokens (
  jti TEXT PRIMARY KEY,
  expires_at INTEGER NOT NULL
) STRICT;
