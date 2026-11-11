// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! What a repository page shows: its pull requests, and its runs
//! grouped by workflow and by branch.
//!
//! Every list is keyed on the repository's id and its tenant, so runs
//! left from before a transfer stay hidden, as in the run lists.

use hatchrun_cloud::db::{Db, params};
use serde::{Deserialize, Serialize};
use worker::Result;

use super::runs::{After, BRANCH, like};

/// A pull request to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPull {
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The pull request number.
    pub number: u64,
    /// The title.
    pub title: String,
    /// The head branch, as a short name.
    pub head_ref: String,
    /// The base branch, as a short name.
    pub base_ref: String,
    /// The login of the user who opened it.
    pub author: String,
    /// GitHub's `author_association` of the user who opened it.
    pub author_association: String,
    /// The repository the head is in; `None` for a deleted fork.
    pub head_repo_id: Option<u64>,
    /// The head commit.
    pub head_sha: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    /// When GitHub last changed the pull request, in Unix milliseconds.
    /// A delivery with an older time than the row leaves it alone.
    pub updated_at: u64,
}

/// `INSERT ... ON CONFLICT DO UPDATE` of a pull request, with a `WHERE`
/// condition on the row it replaces. Parameters: repository `$1`, pull
/// request `$2`, title `$3`, head `$4`, base `$5`, author `$6`, state
/// `$7`, GitHub's time `$8`, the author's association `$9`, the head
/// repository `$10` and the head commit `$11`.
macro_rules! upsert_pull_sql {
    ($when:literal) => {
        concat!(
            "INSERT INTO ci_pulls \
             (repo_id, number, title, head_ref, base_ref, author, state, created_at, updated_at, \
              author_association, head_repo_id, head_sha) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10::bigint, $11) \
             ON CONFLICT (repo_id, number) DO UPDATE SET title = $3, head_ref = $4, base_ref = $5, \
             author = $6, state = $7, updated_at = $8, author_association = $9, \
             head_repo_id = $10::bigint, head_sha = $11 WHERE ",
            $when
        )
    };
}

/// Records pull request `$2` of repository `$1` as it stands (see
/// [`upsert_pull_sql`]). A delivery older than the row leaves it
/// alone, and so does one of the same second with another head commit:
/// GitHub's times have one second, so two pushes in a second tie, and
/// delivery order does not say which came last.
const UPSERT_PULL: &str = upsert_pull_sql!(
    "ci_pulls.updated_at < $8 OR (ci_pulls.updated_at = $8 AND ci_pulls.head_sha = $11)"
);

/// As [`UPSERT_PULL`] for a tie that GitHub's answer settled in favour
/// of the delivery: it applies only while the row still has the time
/// `$8` and the head `$12` it had at the tie, so a delivery that moved
/// the row since is not overwritten.
const UPSERT_PULL_TIED: &str =
    upsert_pull_sql!("ci_pulls.updated_at = $8 AND ci_pulls.head_sha = $12");

/// The time and head commit of pull request `$2` of repository `$1`.
const PULL_STAMP: &str =
    "SELECT updated_at, head_sha FROM ci_pulls WHERE repo_id = $1 AND number = $2";

/// The state and head commit of pull request `$2` of repository `$1`.
const PULL_HEAD: &str = "SELECT state, head_sha FROM ci_pulls WHERE repo_id = $1 AND number = $2";

/// The state and head commit a pull request was last recorded with.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullHead {
    /// `open`, `closed` or `merged`.
    pub state: String,
    /// The head commit.
    pub head_sha: String,
}

/// What is recorded of a pull request's state and head, `None` for a
/// pull request with no row.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pull_head(db: &Db, repo_id: u64, number: u64) -> Result<Option<PullHead>> {
    db.first(PULL_HEAD, &params![repo_id, number]).await
}

/// The time and head commit of a recorded pull request.
#[derive(Deserialize)]
struct Stamp {
    updated_at: u64,
    head_sha: String,
}

/// What recording a delivery of a pull request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullWrite {
    /// The row now says what the delivery says.
    Applied,
    /// The delivery is older than the row.
    Stale,
    /// The delivery has the row's time and another head commit: which is
    /// newer is not known from the two, and GitHub's answer decides.
    /// Holds the head commit the row has.
    Tied(String),
}

fn pull_params(pull: &NewPull) -> Vec<hatchrun_cloud::db::Param> {
    params![
        pull.repo_id,
        pull.number,
        &pull.title,
        &pull.head_ref,
        &pull.base_ref,
        &pull.author,
        &pull.state,
        pull.updated_at,
        &pull.author_association,
        pull.head_repo_id,
        &pull.head_sha
    ]
}

/// Records the pull request for a tie that GitHub's answer settled in
/// favour of `pull`, only while the row still has the time of `pull`
/// and the head commit `held` it had at the tie. `false` when the row
/// moved since: another delivery wrote it, and the row is left alone.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn upsert_pull_if_tied(db: &Db, pull: &NewPull, held: &str) -> Result<bool> {
    let mut params = pull_params(pull);
    params.extend(params![held]);
    Ok(db.run(UPSERT_PULL_TIED, &params).await? == 1)
}

/// Records the state of a pull request. A delivery older than what is
/// recorded is [`PullWrite::Stale`] and says nothing about the pull
/// request as it stands now; one of the same second with another head
/// commit is [`PullWrite::Tied`].
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn upsert_pull(db: &Db, pull: &NewPull) -> Result<PullWrite> {
    let changed = db
        .run(
            UPSERT_PULL,
            &params![
                pull.repo_id,
                pull.number,
                &pull.title,
                &pull.head_ref,
                &pull.base_ref,
                &pull.author,
                &pull.state,
                pull.updated_at,
                &pull.author_association,
                pull.head_repo_id,
                &pull.head_sha
            ],
        )
        .await?;
    if changed == 1 {
        return Ok(PullWrite::Applied);
    }
    let held: Option<Stamp> = db
        .first(PULL_STAMP, &params![pull.repo_id, pull.number])
        .await?;
    Ok(match held {
        Some(held) if held.updated_at == pull.updated_at => PullWrite::Tied(held.head_sha),
        _ => PullWrite::Stale,
    })
}

/// What a repository page counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCounts {
    /// Workflows that ran.
    pub workflows: u64,
    /// Branches that ran.
    pub branches: u64,
    /// Open pull requests.
    pub open_pulls: u64,
}

/// The counts of repository `$1` of tenant `$2`.
fn counts_sql() -> String {
    format!(
        "SELECT \
           (SELECT count(DISTINCT r.workflow) FROM ci_runs r \
            WHERE r.repo_id = $1 AND r.tenant_id = $2) AS workflows, \
           (SELECT count(DISTINCT {BRANCH}) FROM ci_runs r \
            WHERE r.repo_id = $1 AND r.tenant_id = $2) AS branches, \
           (SELECT count(*) FROM ci_pulls WHERE repo_id = $1 AND state = 'open') AS open_pulls"
    )
}

/// What repository `repo_id` of `tenant` counts.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn repo_counts(db: &Db, repo_id: u64, tenant: &str) -> Result<RepoCounts> {
    Ok(db
        .first(&counts_sql(), &params![repo_id, tenant])
        .await?
        .unwrap_or_default())
}

/// A workflow that ran, in the repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkflowRow {
    /// The workflow's file name.
    pub name: String,
    /// Its newest run.
    pub last_run_id: u64,
    /// When the newest run was recorded.
    pub last_at: u64,
    /// Runs recorded in the window.
    pub runs_7d: u64,
    /// Runs recorded in the window that passed.
    pub passed_7d: u64,
    /// Runs recorded in the window that failed.
    pub failed_7d: u64,
}

/// The workflows of repository `$1` of tenant `$2` with runs, each
/// with its newest run and its runs since `$3`.
const WORKFLOWS: &str = "SELECT r.workflow AS name, \
       (array_agg(r.id ORDER BY r.created_at DESC, r.id DESC))[1] AS last_run_id, \
       max(r.created_at) AS last_at, \
       count(*) FILTER (WHERE r.created_at >= $3) AS runs_7d, \
       count(*) FILTER (WHERE r.created_at >= $3 AND r.conclusion = 'success') AS passed_7d, \
       count(*) FILTER (WHERE r.created_at >= $3 AND r.conclusion = 'failure') AS failed_7d \
     FROM ci_runs r WHERE r.repo_id = $1 AND r.tenant_id = $2 GROUP BY r.workflow";

/// A page of the workflows that ran in repository `repo_id` of
/// `tenant`, the newest activity first, those after `after` when it is
/// set, at most `limit`; and how many there are in all. `since` starts
/// the window of the counts.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn workflows_page(
    db: &Db,
    repo_id: u64,
    tenant: &str,
    since: u64,
    after: Option<After>,
    limit: u32,
) -> Result<(Vec<WorkflowRow>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let total = db
        .first::<Total>(
            "SELECT count(DISTINCT workflow) AS total FROM ci_runs \
             WHERE repo_id = $1 AND tenant_id = $2",
            &params![repo_id, tenant],
        )
        .await?
        .map_or(0, |row| row.total);
    let sql = format!(
        "SELECT * FROM ({WORKFLOWS}) w \
         WHERE ($4::bigint IS NULL OR (w.last_at, w.last_run_id) < ($4::bigint, $5::bigint)) \
         ORDER BY w.last_at DESC, w.last_run_id DESC LIMIT $6"
    );
    let rows = db
        .all(
            &sql,
            &params![
                repo_id,
                tenant,
                since,
                after.map(|a| a.created_at),
                after.map(|a| a.id),
                limit
            ],
        )
        .await?;
    Ok((rows, total))
}

/// A branch that ran, in the repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BranchRow {
    /// The branch.
    pub name: String,
    /// Whether it is the repository's default branch.
    pub default: bool,
    /// Its newest run.
    pub last_run_id: u64,
    /// When the newest run was recorded.
    pub last_at: u64,
    /// Runs on the branch.
    pub runs: u64,
}

/// The place after which a page of branches starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchAfter {
    /// Whether the last branch was not the default one: 0 or 1.
    pub rank: u64,
    /// Where its newest run is.
    pub run: After,
}

/// The branches of repository `$1` of tenant `$2` that ran, whose name
/// holds `$3` when it is set, and where the default branch `$4` is.
fn branches_sql() -> String {
    format!(
        "SELECT * FROM (SELECT {BRANCH} AS name, count(*) AS runs, \
           (array_agg(r.id ORDER BY r.created_at DESC, r.id DESC))[1] AS last_run_id, \
           max(r.created_at) AS last_at FROM ci_runs r \
           WHERE r.repo_id = $1 AND r.tenant_id = $2 GROUP BY 1) b \
         WHERE b.name IS NOT NULL AND ($3::text IS NULL OR b.name ILIKE $3::text)"
    )
}

/// A page of the branches that ran in repository `repo_id` of
/// `tenant`, the default branch first and then the newest activity,
/// those after `after` when it is set, at most `limit`; and how many
/// there are in all. `q` narrows the names.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn branches_page(
    db: &Db,
    repo_id: u64,
    tenant: &str,
    default_branch: &str,
    q: Option<&str>,
    after: Option<BranchAfter>,
    limit: u32,
) -> Result<(Vec<BranchRow>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let pattern = q.map(like);
    let total = db
        .first::<Total>(
            &format!("SELECT count(*) AS total FROM ({}) n", branches_sql()),
            &params![repo_id, tenant, &pattern],
        )
        .await?
        .map_or(0, |row| row.total);
    // Sorted by (rank, -time, -id), so one row comparison finds the
    // place after the cursor.
    let sql = format!(
        "SELECT name, (name = $4::text) AS \"default\", last_run_id, last_at, runs FROM ({}) n \
         WHERE ($5::bigint IS NULL OR \
           ((CASE WHEN name = $4::text THEN 0 ELSE 1 END)::bigint, -last_at, -last_run_id) \
             > ($5::bigint, -$6::bigint, -$7::bigint)) \
         ORDER BY (CASE WHEN name = $4::text THEN 0 ELSE 1 END), last_at DESC, last_run_id DESC \
         LIMIT $8",
        branches_sql()
    );
    let rows = db
        .all(
            &sql,
            &params![
                repo_id,
                tenant,
                &pattern,
                default_branch,
                after.map(|a| a.rank),
                after.map(|a| a.run.created_at),
                after.map(|a| a.run.id),
                limit
            ],
        )
        .await?;
    Ok((rows, total))
}

/// A pull request of the repository, with its newest run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRow {
    /// The number.
    pub number: u64,
    /// The title.
    pub title: String,
    /// The head branch.
    pub head_ref: String,
    /// The base branch.
    pub base_ref: String,
    /// The login of who opened it.
    pub author: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    /// When the pull request last changed.
    pub updated_at: u64,
    /// Its newest run.
    pub last_run_id: Option<u64>,
}

/// The place after which a page of pull requests starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullAfter {
    /// When the last pull request changed.
    pub updated_at: u64,
    /// Its number.
    pub number: u64,
}

/// The pull requests of repository `$1`, in state `$3` when it is
/// set, whose title, branch or author holds `$4` or whose number is
/// `$5`.
const PULLS_WHERE: &str = "n.repo_id = $1 \
     AND ($3::text IS NULL OR n.state = $3::text) \
     AND ($4::text IS NULL OR n.title ILIKE $4::text OR n.head_ref ILIKE $4::text \
       OR n.base_ref ILIKE $4::text OR n.author ILIKE $4::text OR n.number::text = $5::text)";

/// A page of the pull requests of repository `repo_id` of `tenant`
/// that are in `state` (all when `None`) and match `q`, the newest
/// change first, those after `after` when it is set, at most `limit`;
/// and how many there are in all.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pulls_page(
    db: &Db,
    repo_id: u64,
    tenant: &str,
    state: Option<&str>,
    q: Option<&str>,
    after: Option<PullAfter>,
    limit: u32,
) -> Result<(Vec<PullRow>, u64)> {
    #[derive(Deserialize)]
    struct Total {
        total: u64,
    }
    let pattern = q.map(like);
    let total = db
        .first::<Total>(
            &format!("SELECT count(*) AS total FROM ci_pulls n WHERE {PULLS_WHERE}"),
            &params![repo_id, tenant, state, &pattern, q],
        )
        .await?
        .map_or(0, |row| row.total);
    let sql = format!(
        "SELECT n.number, n.title, n.head_ref, n.base_ref, n.author, n.state, n.updated_at, \
           (SELECT r.id FROM ci_runs r WHERE r.repo_id = n.repo_id AND r.tenant_id = $2 \
            AND r.pull_number = n.number ORDER BY r.created_at DESC, r.id DESC LIMIT 1) \
           AS last_run_id \
         FROM ci_pulls n WHERE {PULLS_WHERE} \
         AND ($6::bigint IS NULL OR (n.updated_at, n.number) < ($6::bigint, $7::bigint)) \
         ORDER BY n.updated_at DESC, n.number DESC LIMIT $8"
    );
    let rows = db
        .all(
            &sql,
            &params![
                repo_id,
                tenant,
                state,
                &pattern,
                q,
                after.map(|a| a.updated_at),
                after.map(|a| a.number),
                limit
            ],
        )
        .await?;
    Ok((rows, total))
}
