-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- JIT compiles a query whose estimated cost passes jit_above_cost. The
-- Worker's queries are short, and the dashboard's counts, estimated
-- high for their per-row subplans, spent 220 ms compiling to run 12 ms
-- of work. Hyperdrive resets session settings between transactions,
-- so the setting goes on the database, where every session starts
-- from it.
DO $$
BEGIN
  EXECUTE format('ALTER DATABASE %I SET jit = off', current_database());
END
$$;
