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

use super::runs::{GateStatus, ListedRun, RUN_COLUMNS, RunRow};
use super::with_columns;
use crate::policy::{RepoPolicy, Waiting};

/// The policy of repository `$1`.
const POLICY: &str = "SELECT fork_runs, fork_approval, fork_max_running, fork_timeout_minutes \
     FROM ci_repo_policy WHERE repo_id = $1";

/// Saves the policy of repository `$1`: fork runs `$2`, approval `$3`,
/// running cap `$4`, timeout `$5`, at `$6`.
const PUT_POLICY: &str = "INSERT INTO ci_repo_policy (repo_id, fork_runs, fork_approval, \
     fork_max_running, fork_timeout_minutes, updated_at) \
     VALUES ($1, $2, $3, $4, $5, $6) \
     ON CONFLICT (repo_id) DO UPDATE SET fork_runs = $2, fork_approval = $3, \
     fork_max_running = $4, fork_timeout_minutes = $5, updated_at = $6";

/// What waits in repository `$1`, and how many pull requests of author
/// `$2` have a fork run that is not over, not counting pull request
/// `$3` when it is set (a new commit on a pull request that is active
/// already is not another pull request).
const WAITING: &str = "SELECT \
       count(*) FILTER (WHERE gate_status = 'pending')::bigint AS pending, \
       count(*) FILTER (WHERE gate_status = 'deferred')::bigint AS deferred, \
       count(DISTINCT pull_number) FILTER (WHERE trust = 'fork' AND author_login = $2 \
         AND status <> 'completed' AND pull_number IS NOT NULL \
         AND gate_status NOT IN ('deferred', 'expired', 'cancelled') \
         AND ($3::bigint IS NULL OR pull_number <> $3::bigint))::bigint AS author_active \
     FROM ci_runs WHERE repo_id = $1 \
       AND (gate_status IN ('pending', 'deferred') \
            OR (trust = 'fork' AND author_login = $2 AND status <> 'completed'))";

/// Records `$4` as the check run that carries the approve button of the
/// runs of pull request `$2` of repository `$1` at `$3` that still wait.
/// One check run covers all of them: what the card lists is what a click
/// approves.
const SET_GATE_CHECK: &str = "UPDATE ci_runs SET gate_check_run_id = $4 \
     WHERE repo_id = $1 AND pull_number = $2 AND sha = $3 \
     AND gate_status IN ('pending', 'deferred')";

/// The runs of pull request `$2` of repository `$1` at `$3` that wait.
const GATE_GROUP: &str = "SELECT {columns} FROM ci_runs WHERE repo_id = $1 AND pull_number = $2 \
     AND sha = $3 AND gate_status IN ('pending', 'deferred') AND status <> 'completed' \
     ORDER BY id";

/// Approves run `$1` by `$2` at `$3`, if it waits (pending or deferred) and is not over.
const APPROVE_RUN: &str = "UPDATE ci_runs SET gate_status = 'approved', gate_by = $2, \
     gate_at = $3 WHERE id = $1 AND gate_status IN ('pending', 'deferred') AND status <> 'completed' \
     RETURNING {columns}";

/// Approves the waiting runs of repository `$1` of tenant `$5` whose
/// approval is on check run `$2`, by `$3` at `$4`: exactly the runs the
/// card lists.
const APPROVE_BY_CHECK: &str = "UPDATE ci_runs SET gate_status = 'approved', gate_by = $3, \
     gate_at = $4 WHERE repo_id = $1 AND tenant_id = $5 AND gate_check_run_id = $2 \
     AND gate_status IN ('pending', 'deferred') AND status <> 'completed' \
     RETURNING {columns}";

/// Approves the waiting runs of the pull request and commit of run `$1`,
/// by `$2` at `$3`: what the card of that run lists.
const APPROVE_GROUP: &str = "UPDATE ci_runs SET gate_status = 'approved', gate_by = $2, \
     gate_at = $3 WHERE (repo_id, pull_number, sha) = (\
       SELECT repo_id, pull_number, sha FROM ci_runs WHERE id = $1) \
     AND gate_status IN ('pending', 'deferred') AND status <> 'completed' \
     RETURNING {columns}";

/// Ends the wait of the waiting runs of the pull request and commit of
/// run `$1` with status `$2` by `$3` at `$4`.
const END_GROUP: &str = "UPDATE ci_runs SET gate_status = $2, gate_by = $3, gate_at = $4 \
     WHERE (repo_id, pull_number, sha) = (\
       SELECT repo_id, pull_number, sha FROM ci_runs WHERE id = $1) \
     AND gate_status IN ('pending', 'deferred') RETURNING {columns}";

/// Asks for approval again for run `$1`, which expired or was cancelled
/// before it ran: a rerun of it must not start on an approval that
/// ended.
const REOPEN: &str = "UPDATE ci_runs SET gate_status = 'pending', gate = 'fork_approval', \
     gate_by = NULL, gate_at = NULL, gate_check_run_id = NULL \
     WHERE id = $1 AND trust = 'fork' AND gate_status IN ('expired', 'cancelled') \
     RETURNING {columns}";

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

/// The deferred runs that are not over: the oldest `$2` of each
/// repository, so one repository's stuck runs cannot keep another's
/// from being looked at, the oldest first, at most `$1`.
const DEFERRED: &str = "SELECT {columns} FROM (\
       SELECT ci_runs.*, row_number() OVER (PARTITION BY repo_id ORDER BY created_at, id) AS rn \
       FROM ci_runs WHERE gate_status = 'deferred' AND status <> 'completed') ci_runs \
     WHERE rn <= $2 ORDER BY created_at, id LIMIT $1";

/// Takes deferred run `$1` out of the queue: gate status `$2`, by `$3`
/// at `$4`.
const ADMIT: &str = "UPDATE ci_runs SET gate_status = $2, gate_by = $3, gate_at = $4 \
     WHERE id = $1 AND gate_status = 'deferred' RETURNING {columns}";

/// The waiting runs created before `$1` that have no check run with the
/// approve button: the oldest `$3` of each repository, the oldest first,
/// at most `$2`.
const WITHOUT_CARD: &str = "SELECT {columns} FROM (\
       SELECT ci_runs.*, row_number() OVER (PARTITION BY repo_id ORDER BY created_at, id) AS rn \
       FROM ci_runs WHERE gate_status IN ('pending', 'deferred') \
       AND gate_check_run_id IS NULL AND status <> 'completed' AND created_at < $1) ci_runs \
     WHERE rn <= $3 ORDER BY created_at, id LIMIT $2";

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
            now
        ],
    )
    .await?;
    Ok(())
}

/// What waits in repository `repo_id`, and how many pull requests
/// `author` has with a fork run that is not over, not counting pull
/// request `except_pull`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn fork_waiting(
    db: &Db,
    repo_id: u64,
    author: &str,
    except_pull: Option<u64>,
) -> Result<Waiting> {
    #[derive(Deserialize, Default)]
    struct Counts {
        pending: u64,
        deferred: u64,
        author_active: u64,
    }
    let counts: Counts = db
        .first(WAITING, &params![repo_id, author, except_pull])
        .await?
        .unwrap_or_default();
    Ok(Waiting {
        pending: counts.pending,
        deferred: counts.deferred,
        author_active: counts.author_active,
    })
}

/// Records check run `check_run_id` as the carrier of the approve
/// button of the waiting runs of pull request `number` at `sha`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn set_gate_check(
    db: &Db,
    repo_id: u64,
    number: u64,
    sha: &str,
    check_run_id: u64,
) -> Result<()> {
    db.run(SET_GATE_CHECK, &params![repo_id, number, sha, check_run_id])
        .await?;
    Ok(())
}

/// The runs of pull request `number` of repository `repo_id` at `sha`
/// that wait for a decision: what one approval check run lists and one
/// click decides.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn gate_group(db: &Db, repo_id: u64, number: u64, sha: &str) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(GATE_GROUP, RUN_COLUMNS),
        &params![repo_id, number, sha],
    )
    .await
}

/// Approves every waiting run that shares pull request and commit with
/// run `run_id`, for `by`: the runs its card lists. Empty when none
/// waits any more.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn approve_group(db: &Db, run_id: u64, by: &str, now: u64) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(APPROVE_GROUP, RUN_COLUMNS),
        &params![run_id, by, now],
    )
    .await
}

/// Ends the wait of every waiting run that shares pull request and
/// commit with run `run_id` without approving it (`cancelled`, by
/// whoever rejected it).
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn end_group(
    db: &Db,
    run_id: u64,
    status: &str,
    by: Option<&str>,
    now: u64,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(END_GROUP, RUN_COLUMNS),
        &params![run_id, status, by, now],
    )
    .await
}

/// Asks for approval again for run `run_id` when it expired or was
/// cancelled: it is pending with no card, which a rerun of it needs
/// before any job starts. `None` for a run that is not a fork run in
/// one of those states.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn reopen_gate(db: &Db, run_id: u64) -> Result<Option<RunRow>> {
    db.first(&with_columns(REOPEN, RUN_COLUMNS), &params![run_id])
        .await
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
    tenant_id: &str,
    repo_id: u64,
    check_run_id: u64,
    by: &str,
    now: u64,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(APPROVE_BY_CHECK, RUN_COLUMNS),
        &params![repo_id, check_run_id, by, now, tenant_id],
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

/// The deferred runs, the oldest first, at most `limit`: what the
/// reconciler admits when a slot frees or the author's runs end.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn deferred_runs(db: &Db, limit: u32, per_repo: u32) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(DEFERRED, RUN_COLUMNS),
        &params![limit, per_repo],
    )
    .await
}

/// Takes deferred run `run_id` out of the queue with `status`
/// (`pending` to ask for approval, `approved` when none is needed), by
/// `by` when it is approved. `None` when it was decided meanwhile.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn admit_run(
    db: &Db,
    run_id: u64,
    status: GateStatus,
    by: Option<&str>,
    now: u64,
) -> Result<Option<RunRow>> {
    db.first(
        &with_columns(ADMIT, RUN_COLUMNS),
        &params![run_id, status.as_str(), by, now],
    )
    .await
}

/// The waiting runs created before `before` that have no approval check
/// run recorded, at most `limit`: a delivery that stopped between
/// making the runs and the check run.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn runs_without_card(
    db: &Db,
    before: u64,
    limit: u32,
    per_repo: u32,
) -> Result<Vec<RunRow>> {
    db.all(
        &with_columns(WITHOUT_CARD, RUN_COLUMNS),
        &params![before, limit, per_repo],
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
