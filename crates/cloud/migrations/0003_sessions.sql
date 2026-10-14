-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: Apache-2.0

-- People who have signed in to app.hatchrun.io with GitHub, by GitHub
-- user id, which survives a rename.
CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  login TEXT NOT NULL,
  name TEXT,
  avatar_url TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- One row per signed-in browser. The browser holds the session id; the
-- row is keyed by its SHA-256, so the table alone cannot sign anyone in.
-- github_token is the user's GitHub token, sealed under a key derived
-- from the session id (crates/cloud/src/session.rs).
CREATE TABLE sessions (
  key TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  github_token TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
) STRICT;

CREATE INDEX sessions_expiry ON sessions (expires_at);
CREATE INDEX sessions_user ON sessions (user_id);
