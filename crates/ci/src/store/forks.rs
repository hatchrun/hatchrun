// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The policy of a repository for pull requests from forks, and the
//! approval gate of the runs they start.
//!
//! A fork run that needs a maintainer's approval is recorded with
//! `gate_status` `pending` (or `deferred` when the repository already
//! has too many waiting). Every statement that decides it moves the row
//! out of `pending` or `deferred` only, so two clicks, an expiry and a
//! rejection that race settle on one outcome: whoever changes the row
//! first wins, and the others change nothing and see no row.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::runs::{ListedRun, RUN_COLUMNS, RunRow};
use super::with_columns;
use crate::policy::{MAX_PENDING, RepoPolicy, Waiting};

/// The policy of repository `$1`.
const POLICY: &str = "SELECT fork_runs, fork_approval, fork_max_running, fork_timeout_minutes, \
     pull_request_target_head_checkout FROM ci_repo_policy WHERE repo_id = $1";

/// Saves the policy of repository `$1`: fork runs `$2`, approval `$3`,
/// running cap `$4`, timeout `$5`, head checkout rule `$6`, at `$7`.
const PUT_POLICY: &str = "INSERT INTO ci_repo_policy (repo_id, fork_runs, fork_approval, \
     fork_max_running, fork_timeout_minutes, pull_request_target_head_checkout, updated_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7) \
     ON CONFLICT (repo_id) DO UPDATE SET fork_runs = $2, fork_approval = $3, \
     fork_max_running = $4, fork_timeout_minutes = $5, \
     pull_request_target_head_checkout = $6, updated_at = $7";

/// What waits in repository `$1`, and how many fork runs of author `$2`
/// are not over.
const WAITING: &str = "SELECT \
       count(*) FILTER (WHERE gate_status = 'pending')::bigint AS pending, \
       count(*) FILTER (WHERE gate_status = 'deferred')::bigint AS deferred, \
       count(*) FILTER (WHERE trust = 'fork' AND author_login = $2 AND status <> 'completed' \
         AND gate_status NOT IN ('expired', 'cancelled'))::bigint AS author_active \
     FROM ci_runs WHERE repo_id = $1 \
       AND (gate_status IN ('pending', 'deferred') \
            OR (trust = 'fork' AND author_login = $2 AND status <> 'completed'))";

/// Records `$2` as the check run that carries the approve button of the
/// runs delivery `$1` made and that still wait.
const SET_GATE_CHECK: &str = "UPDATE ci_runs SET gate_check_run_id = $2 \
     WHERE delivery = $1 AND gate_status IN ('pending', 'deferred')";

/// Approves run `$1` by `$2` at `$3`, if it waits (pending or deferred) and is not over.
const APPROVE_RUN: &str = "UPDATE ci_runs SET gate_status = 'approved', gate_by = $2, \
     gate_at = $3 WHERE id = $1 AND gate_status IN ('pending', 'deferred') AND status <> 'completed' \
     RETURNING {columns}";

/// Approves the waiting runs of repository `$1` whose approval is on
/// check run `$2`, by `$3` at `$4`.
const APPROVE_BY_CHECK: &str = "UPDATE ci_runs SET gate_status = 'approved', gate_by = $3, \
     gate_at = $4 WHERE repo_id = $1 AND gate_check_run_id = $2 AND gate_status = 'pending' \
     AND status <> 'completed' RETURNING {columns}";

/// Ends the wait of run `$1` with status `$2` by `$3` at `$4`.
const END_GATE: &str = "UPDATE ci_runs SET gate_status = $2, gate_by = $3, gate_at = $4 \
     WHERE id = $1 AND gate_status IN ('pending', 'deferred') RETURNING {columns}";

/// Cancels the waiting runs of pull request `$2` of repository `$1`,
/// except those at sha `$3` when it is set, by `$4` at `$5`.
const END_PULL: &str = "UPDATE ci_runs SET gate_status = 'cancelled', gate_by = $4, gate_at = $5 \
     WHERE repo_id = $1 AND pull_number = $2 AND gate_status IN ('pending', 'deferred') \
     AND ($3::text IS NULL OR sha <> $3::text) RETURNING {columns}";

/// Expires up to `$3` runs that waited since before `$1`, at `$2`.
const EXPIRE: &str = "UPDATE ci_runs SET gate_status = 'expired', gate_at = $2 WHERE id IN (\
       SELECT id FROM ci_runs WHERE gate_status IN ('pending', 'deferred') AND created_at < $1 \
       ORDER BY created_at, id LIMIT $3) AND gate_status IN ('pending', 'deferred') \
     RETURNING {columns}";

/// Asks for the approval of up to `$2` deferred runs, the oldest first,
/// while their repository has fewer than `$1` runs waiting.
const ADMIT: &str = "UPDATE ci_runs SET gate_status = 'pending' WHERE id IN (\
       SELECT id FROM (\
         SELECT d.id, row_number() OVER (PARTITION BY d.repo_id ORDER BY d.created_at, d.id) \
           AS place, d.created_at, \
           (SELECT count(*) FROM ci_runs w WHERE w.repo_id = d.repo_id \
              AND w.gate_status = 'pending') AS waiting \
         FROM ci_runs d WHERE d.gate_status = 'deferred' AND d.status <> 'completed') ranked \
       WHERE place <= $1 - waiting ORDER BY created_at LIMIT $2) \
     AND gate_status = 'deferred' RETURNING {columns}";

/// The runs of the repositories `$2` of tenant `$1` that wait for an
/// approval, the oldest first, at most `$3`.
const WAITING_RUNS: &str = "SELECT {columns} FROM ci_runs r, ci_repos p \
     WHERE p.id = r.repo_id AND p.tenant_id = r.tenant_id AND p.tenant_id = $1 \
     AND p.id = ANY (string_to_array($2, ',')::bigint[]) AND r.gate_status = 'pending' \
     AND r.status <> 'completed' ORDER BY r.created_at, r.id LIMIT $3";

/// The policy of repository `repo_id`: the row when one was saved,
/// otherwise the defaults for a repository that is `private` or not.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo_policy(db: &Db, repo_id: u64, private: bool) -> Result<RepoPolicy> {
    Ok(db
        .first(POLICY, &params![repo_id])
        .await?
        .unwrap_or(RepoPolicy::default_for(private)))
}

/// Saves the policy of repository `repo_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn put_repo_policy(db: &Db, repo_id: u64, policy: &RepoPolicy, now: u64) -> Result<()> {
    db.run(
        PUT_POLICY,
        &params![
            repo_id,
            policy.fork_runs.as_str(),
            policy.fork_approval.as_str(),
            policy.fork_max_running,
            policy.fork_timeout_minutes,
            policy.pull_request_target_head_checkout.as_str(),
            now
        ],
    )
    .await?;
    Ok(())
}

/// What waits in repository `repo_id`, and how many fork runs `author`
/// has that are not over.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn fork_waiting(db: &Db, repo_id: u64, author: &str) -> Result<Waiting> {
    #[derive(Deserialize, Default)]
    struct Counts {
        pending: u64,
        deferred: u64,
        author_active: u64,
    }
    let counts: Counts = db
        .first(WAITING, &params![repo_id, author])
        .await?
        .unwrap_or_default();
    Ok(Waiting {
        pending: counts.pending,
        deferred: counts.deferred,
        author_active: counts.author_active,
    })
}

/// Records check run `check_run_id` as the carrier of the approve
/// button of the waiting runs `delivery` made.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_gate_check(db: &Db, delivery: &str, check_run_id: u64) -> Result<()> {
    db.run(SET_GATE_CHECK, &params![delivery, check_run_id])
        .await?;
    Ok(())
}

/// Approves run `run_id` for `by`, whether it is pending or deferred (a
/// maintainer may admit and approve a deferred run at once). `None`
/// when the run does not wait for a decision any more: someone decided
/// first, or it expired.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn approve_run(db: &Db, run_id: u64, by: &str, now: u64) -> Result<Option<RunRow>> {
    db.first(
        &with_columns(APPROVE_RUN, RUN_COLUMNS),
        &params![run_id, by, now],
    )
    .await
}

/// Approves the runs of repository `repo_id` that wait on check run
/// `check_run_id`, for `by`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn approve_by_check(
    db: &Db,
    repo_id: u64,
    check_run_id: u64,
    by: &str,
    now: u64,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(APPROVE_BY_CHECK, RUN_COLUMNS),
        &params![repo_id, check_run_id, by, now],
    )
    .await
}

/// Ends the wait of run `run_id` without approving it: `status` is
/// `cancelled` or `expired`, `by` who rejected it. `None` when it did
/// not wait.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn end_gate(
    db: &Db,
    run_id: u64,
    status: &str,
    by: Option<&str>,
    now: u64,
) -> Result<Option<RunRow>> {
    db.first(
        &with_columns(END_GATE, RUN_COLUMNS),
        &params![run_id, status, by, now],
    )
    .await
}

/// Cancels the runs of pull request `number` that wait, except those at
/// `keep_sha`: a newer commit supersedes them, or the pull request
/// closed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn end_pull_gates(
    db: &Db,
    repo_id: u64,
    number: u64,
    keep_sha: Option<&str>,
    now: u64,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(END_PULL, RUN_COLUMNS),
        &params![repo_id, number, keep_sha, "hatchrun", now],
    )
    .await
}

/// Expires up to `limit` runs that have waited since before `before`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn expire_gates(db: &Db, before: u64, now: u64, limit: u32) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(EXPIRE, RUN_COLUMNS),
        &params![before, now, limit],
    )
    .await
}

/// Moves up to `limit` deferred runs to pending, the oldest first,
/// while their repository has fewer than [`MAX_PENDING`] waiting.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn admit_deferred(db: &Db, limit: u32) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(ADMIT, RUN_COLUMNS),
        &params![MAX_PENDING, limit],
    )
    .await
}

/// The runs of `repo_ids` of `tenant` that wait for an approval, the
/// longest waiting first, at most `limit`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pending_gate_runs(
    db: &Db,
    tenant: &str,
    repo_ids: &[u64],
    limit: u32,
) -> Result<Vec<ListedRun>> {
    let ids: Vec<String> = repo_ids.iter().map(u64::to_string).collect();
    db.all(
        &with_columns(WAITING_RUNS, super::runs::LISTED_COLUMNS),
        &params![tenant, ids.join(","), limit],
    )
    .await
}
