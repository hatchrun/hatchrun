// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Concurrency groups: who runs and who waits.
//!
//! A row of `ci_concurrency` stands for a run (`job_id` is `NULL`) or a
//! job that names a group. The run object records it when it queues the
//! first job ([`register`]); the launch consumer puts it in line
//! ([`enter`]) and looks at it again at every recheck. Per repository and
//! group at most one row is `running` and one `pending` (partial unique
//! indexes). Every change of a group's line runs in a transaction that
//! first locks the repository's row, as the tenant limit does for its
//! count, so two consumers cannot both take the place.

use hatchrun_cloud::ci_wake::Woken;
use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

/// Locks the repository row, so one group changes at a time.
const LOCK_REPO: &str = "SELECT id FROM ci_repos WHERE id = $1 FOR UPDATE";

/// Drops the rows of group `$2` of repository `$1` whose run or job
/// ended, or went on to another attempt.
const SWEEP: &str = "DELETE FROM ci_concurrency c WHERE c.repo_id = $1 AND c.group_key = $2 AND (\
       (c.job_id IS NULL AND EXISTS (SELECT 1 FROM ci_runs r WHERE r.id = c.run_id \
          AND (r.status = 'completed' OR r.run_attempt <> c.attempt))) \
       OR (c.job_id IS NOT NULL AND EXISTS (SELECT 1 FROM ci_jobs j WHERE j.id = c.job_id \
          AND (j.status = 'completed' OR j.attempt <> c.attempt))))";

/// Lets the oldest pending row of group `$2` of repository `$1` run when
/// nothing runs.
const PROMOTE: &str = "UPDATE ci_concurrency SET state = 'running' \
     WHERE id = (SELECT p.id FROM ci_concurrency p WHERE p.repo_id = $1 AND p.group_key = $2 \
                 AND p.state = 'pending' ORDER BY p.id LIMIT 1) \
     AND repo_id = $1 AND group_key = $2 AND state = 'pending' \
     AND NOT EXISTS (SELECT 1 FROM ci_concurrency r WHERE r.repo_id = $1 AND r.group_key = $2 \
                     AND r.state = 'running')";

/// The pending row of group `$2` of repository `$1` is replaced by row
/// `$3`, which comes after a running one.
const SUPERSEDE: &str = "UPDATE ci_concurrency SET state = 'superseded' \
     WHERE repo_id = $1 AND group_key = $2 AND state = 'pending' AND id <> $3 \
     AND EXISTS (SELECT 1 FROM ci_concurrency r WHERE r.repo_id = $1 AND r.group_key = $2 \
                 AND r.state = 'running')";

/// Marks the running row of group `$2` of repository `$1` for
/// cancellation when `$4`, the cancel-in-progress of arrival `$3`.
const MARK_CANCEL: &str = "UPDATE ci_concurrency SET cancel_me = true \
     WHERE repo_id = $1 AND group_key = $2 AND state = 'running' AND id <> $3 AND $4::boolean";

/// Puts row `$3`, which is new, in line: it runs when nothing does and
/// waits otherwise.
const ENTER: &str = "UPDATE ci_concurrency SET state = CASE WHEN EXISTS (\
       SELECT 1 FROM ci_concurrency r WHERE r.repo_id = $1 AND r.group_key = $2 \
       AND r.state = 'running') THEN 'pending' ELSE 'running' END \
     WHERE id = $3 AND repo_id = $1 AND state = 'new'";

/// Forgets what an earlier attempt, or an earlier look, left for the
/// run (`$1`) or job (`$2`) at attempt `$3`.
const CLEAR_OWN: &str = "DELETE FROM ci_concurrency WHERE run_id = $1 \
     AND job_id IS NOT DISTINCT FROM $2::bigint AND (attempt <> $3 OR state IN ('new', 'invalid'))";

/// Records the run or job as new, or invalid when `$5` is `invalid`.
const INSERT: &str = "INSERT INTO ci_concurrency \
       (repo_id, run_id, job_id, attempt, group_key, group_name, state, cancel_others, created_at) \
     VALUES ($1, $2, $3::bigint, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING";

/// The columns of a [`Slot`].
const SLOT_COLUMNS: &str = "id, repo_id, run_id, job_id, attempt, group_key, group_name, state, \
     cancel_others, cancel_me";

/// What a run or a job holds in a group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Slot {
    /// The row.
    pub id: u64,
    /// The repository the group is in.
    pub repo_id: u64,
    /// The run.
    pub run_id: u64,
    /// The job; `None` for a workflow's concurrency.
    pub job_id: Option<u64>,
    /// The attempt of the run or job it was recorded for.
    pub attempt: u32,
    /// The group in lower case.
    pub group_key: String,
    /// The group as evaluated; why it failed when the state is
    /// `invalid`.
    pub group_name: String,
    /// `new`, `running`, `pending`, `superseded` or `invalid`.
    pub state: String,
    /// The arrival cancels the one that runs.
    pub cancel_others: bool,
    /// A newer arrival asked to cancel this one.
    pub cancel_me: bool,
}

/// What to record when a run or job is about to be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSlot {
    /// The repository.
    pub repo_id: u64,
    /// The run.
    pub run_id: u64,
    /// The job, or `None` for the workflow's concurrency.
    pub job_id: Option<u64>,
    /// The run's or job's attempt.
    pub attempt: u32,
    /// The evaluated group and whether the arrival cancels the one that
    /// runs, or why it could not be evaluated.
    pub group: std::result::Result<(String, bool), String>,
    /// Unix milliseconds.
    pub now: u64,
}

/// Records the run or job as ready to take its place in a group. A row
/// already recorded for the attempt stays as it is.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn register(db: &Db, slot: &NewSlot) -> Result<()> {
    let (key, name, state, cancel) = match &slot.group {
        Ok((name, cancel)) => (name.to_lowercase(), name.clone(), "new", *cancel),
        Err(why) => (String::new(), why.clone(), "invalid", false),
    };
    db.batch(&[
        (CLEAR_OWN, params![slot.run_id, slot.job_id, slot.attempt]),
        (
            INSERT,
            params![
                slot.repo_id,
                slot.run_id,
                slot.job_id,
                slot.attempt,
                key,
                name,
                state,
                cancel,
                slot.now
            ],
        ),
    ])
    .await?;
    Ok(())
}

/// The rows of run `run_id` and of its job `job_id`, the run's first.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn slots(db: &Db, run_id: u64, job_id: u64) -> Result<Vec<Slot>> {
    db.all(
        &format!(
            "SELECT {SLOT_COLUMNS} FROM ci_concurrency \
             WHERE run_id = $1 AND (job_id IS NULL OR job_id = $2) ORDER BY job_id NULLS FIRST"
        ),
        &params![run_id, job_id],
    )
    .await
}

/// Puts a new `slot` in line: it runs when nothing in its group does,
/// and otherwise waits, taking the place of the one that waits (which is
/// superseded) and, with `cancel_others`, marking the one that runs for
/// cancellation. Returns the row as it is after. A row that is not new
/// is returned as it is, after the group is tidied.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn enter(db: &Db, slot: &Slot) -> Result<Option<Slot>> {
    let (repo, key, id) = (slot.repo_id, &slot.group_key, slot.id);
    db.batch(&[
        (LOCK_REPO, params![repo]),
        (SWEEP, params![repo, key]),
        (PROMOTE, params![repo, key]),
        (SUPERSEDE, params![repo, key, id]),
        (MARK_CANCEL, params![repo, key, id, slot.cancel_others]),
        (ENTER, params![repo, key, id]),
    ])
    .await?;
    db.first(
        &format!("SELECT {SLOT_COLUMNS} FROM ci_concurrency WHERE id = $1"),
        &params![id],
    )
    .await
}

/// A run or job ended: gives up its place and lets the one that waits
/// run. Returns the group, `None` when the run or job held none.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn leave(db: &Db, run_id: u64, job_id: Option<u64>) -> Result<Option<(u64, String)>> {
    #[derive(Deserialize)]
    struct Group {
        repo_id: u64,
        group_key: String,
    }
    let found: Option<Group> = db
        .first(
            "SELECT repo_id, group_key FROM ci_concurrency \
             WHERE run_id = $1 AND job_id IS NOT DISTINCT FROM $2::bigint",
            &params![run_id, job_id],
        )
        .await?;
    let Some(Group { repo_id, group_key }) = found else {
        return Ok(None);
    };
    db.batch(&[
        (LOCK_REPO, params![repo_id]),
        (
            "DELETE FROM ci_concurrency WHERE run_id = $1 AND job_id IS NOT DISTINCT FROM $2::bigint",
            params![run_id, job_id],
        ),
        (SWEEP, params![repo_id, &group_key]),
        (PROMOTE, params![repo_id, &group_key]),
    ])
    .await?;
    Ok(Some((repo_id, group_key)))
}

/// A row to cancel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Victim {
    /// The run.
    pub run_id: u64,
    /// The job; `None` for a whole run.
    pub job_id: Option<u64>,
    /// The attempt of the job; of the run when there is no job.
    pub attempt: u32,
    /// `superseded` (never ran) or `running`.
    pub state: String,
    /// The group as evaluated.
    pub group_name: String,
    /// The run's tenant.
    pub tenant_id: String,
    /// The run's retention in days.
    pub retention_days: u32,
}

/// The rows of a group that must be cancelled: the superseded ones and
/// the running one a newer arrival marked.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn victims(db: &Db, repo_id: u64, group_key: &str) -> Result<Vec<Victim>> {
    db.all(
        "SELECT c.run_id, c.job_id, c.attempt, c.state, c.group_name, r.tenant_id, \
           (r.expires_at - r.created_at) / 86400000 AS retention_days \
         FROM ci_concurrency c JOIN ci_runs r ON r.id = c.run_id \
         WHERE c.repo_id = $1 AND c.group_key = $2 \
           AND (c.state = 'superseded' OR (c.state = 'running' AND c.cancel_me)) \
         ORDER BY c.id",
        &params![repo_id, group_key],
    )
    .await
}

/// Who runs in a group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Blocker {
    /// The run's number.
    pub run_number: u64,
    /// The run's workflow.
    pub workflow: String,
    /// The job's name, for a job that holds the group.
    pub job_name: Option<String>,
}

/// The run or job that holds group `group_key` of repository `repo_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn blocker(db: &Db, repo_id: u64, group_key: &str) -> Result<Option<Blocker>> {
    db.first(
        "SELECT r.run_number, r.workflow, j.name AS job_name \
         FROM ci_concurrency c JOIN ci_runs r ON r.id = c.run_id \
         LEFT JOIN ci_jobs j ON j.id = c.job_id \
         WHERE c.repo_id = $1 AND c.group_key = $2 AND c.state = 'running'",
        &params![repo_id, group_key],
    )
    .await
}

/// The queued jobs held for the group whose holder runs now, to send to
/// the launch queue after a place was freed.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn wake_targets(db: &Db, repo_id: u64, group_key: &str) -> Result<Vec<Woken>> {
    db.all(
        "SELECT j.id AS job_id, j.attempt FROM ci_concurrency c \
         JOIN ci_jobs j ON (j.id = c.job_id OR (c.job_id IS NULL AND j.run_id = c.run_id)) \
         WHERE c.repo_id = $1 AND c.group_key = $2 AND c.state = 'running' \
           AND j.status = 'queued' AND j.held = 'group' ORDER BY j.id",
        &params![repo_id, group_key],
    )
    .await
}
