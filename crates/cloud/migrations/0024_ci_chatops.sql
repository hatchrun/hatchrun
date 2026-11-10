-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- issues and issue_comment runs, and the chat commands of
-- on.command. Every column here is nullable and has no default: a
-- run that does not come from an issue or a comment has none of them,
-- so there is nothing to backfill, and the runs recorded before this
-- migration are exactly those.

-- The issue or pull request a run of the issues or issue_comment event
-- is about. A comment run on a pull request carries the pull request's
-- number here, since GitHub numbers both in one sequence. It is what
-- the cap of 5 active runs per issue or pull request counts by.
ALTER TABLE ci_runs ADD COLUMN issue_number bigint;

-- The comment a chat command was read from. The run's end puts its
-- result on this comment as a reaction (+1 or -1, or rocket or
-- confused), as the App.
ALTER TABLE ci_runs ADD COLUMN comment_id bigint;

-- The command's name, without the slash, for the app and the audit
-- trail.
ALTER TABLE ci_runs ADD COLUMN command_name text;

-- The reactions the command leaves when the run ends: 'default' for +1
-- and -1, 'rocket' for rocket and confused. It is the command's
-- `reaction` option, as the workflow said it when the run started.
ALTER TABLE ci_runs ADD COLUMN command_reaction text;

-- Both constraints are added NOT VALID and validated after. Adding a
-- CHECK validated scans the whole table under an ACCESS EXCLUSIVE lock,
-- which blocks reads and writes of ci_runs for the scan. NOT VALID takes
-- the lock only for the catalog change and checks new and updated rows
-- from then on; VALIDATE CONSTRAINT then scans under a SHARE UPDATE
-- EXCLUSIVE lock, which lets reads and writes go on.
ALTER TABLE ci_runs ADD CONSTRAINT ci_runs_command_reaction_known
  CHECK (command_reaction IN ('default', 'rocket')) NOT VALID;

-- The three describe one command and are set together.
ALTER TABLE ci_runs ADD CONSTRAINT ci_runs_command_whole CHECK (
  (comment_id IS NULL) = (command_name IS NULL)
  AND (comment_id IS NULL) = (command_reaction IS NULL)) NOT VALID;

ALTER TABLE ci_runs VALIDATE CONSTRAINT ci_runs_command_reaction_known;
ALTER TABLE ci_runs VALIDATE CONSTRAINT ci_runs_command_whole;

-- The runs of a comment event that are not over, by issue, for the cap.
-- The migration runner applies a pass in one transaction, which rules
-- out CREATE INDEX CONCURRENTLY; the index is partial, so building it
-- reads the table once under a lock that blocks writes to it for that
-- scan.
CREATE INDEX ci_runs_issue_active ON ci_runs (repo_id, issue_number)
  WHERE issue_number IS NOT NULL AND status <> 'completed';
