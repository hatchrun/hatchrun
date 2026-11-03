// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Artifacts: the rows behind `actions/upload-artifact` and
//! `download-artifact`. Every statement names the run, so a job reaches
//! the artifacts of its own run and no others.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

use super::with_columns;

/// An artifact, or an upload that is not finalized yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ArtifactRow {
    /// The artifact's identity.
    pub id: u64,
    /// The run it belongs to.
    pub run_id: u64,
    /// The job that uploaded it.
    pub job_id: u64,
    /// The attempt of that job.
    pub attempt: u32,
    /// The name the workflow gave it.
    pub name: String,
    /// The S3 key of its bytes.
    pub object_key: String,
    /// The S3 multipart upload, until the artifact is finalized.
    pub upload_id: Option<String>,
    /// Bytes, once finalized.
    pub size: Option<u64>,
    /// `sha256:<hex>`, when the toolkit sent one.
    pub digest: Option<String>,
    /// When the upload started.
    pub created_at: u64,
    /// When the artifact was finalized.
    pub finalized_at: Option<u64>,
}

/// The columns of an [`ArtifactRow`].
const ARTIFACT_COLUMNS: &str = "id, run_id, job_id, attempt, name, object_key, upload_id, size, \
     digest, created_at, finalized_at";

/// Records an upload in run `$1` by job `$2`'s attempt `$3`, named `$4`,
/// to S3 key `$5` as multipart upload `$6`, at `$7`.
const START: &str = "INSERT INTO ci_artifacts (run_id, job_id, attempt, name, object_key, \
     upload_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {columns}";

/// The finalized artifact of run `$1` named `$2`.
const BY_NAME: &str = "SELECT {columns} FROM ci_artifacts \
     WHERE run_id = $1 AND name = $2 AND finalized_at IS NOT NULL";

/// The upload `$1` of run `$2`, which is not finalized.
const PENDING: &str =
    "SELECT {columns} FROM ci_artifacts WHERE id = $1 AND run_id = $2 AND finalized_at IS NULL";

/// The finalized artifacts of run `$1`, those named `$2` and those with
/// id `$3` when they are set.
const LIST: &str = "SELECT {columns} FROM ci_artifacts \
     WHERE run_id = $1 AND finalized_at IS NOT NULL \
     AND ($2::text IS NULL OR name = $2::text) \
     AND ($3::bigint IS NULL OR id = $3::bigint) ORDER BY id";

/// How many artifacts and uploads run `$1` has.
const COUNT: &str = "SELECT count(*) AS n FROM ci_artifacts WHERE run_id = $1";

/// Removes the artifact an earlier attempt of the same job uploaded
/// under the name of upload `$1` in run `$2`, so this attempt's artifact
/// can take the name.
const REPLACE: &str = "DELETE FROM ci_artifacts AS old USING ci_artifacts AS new \
     WHERE new.id = $1 AND new.run_id = $2 AND new.finalized_at IS NULL \
     AND old.run_id = new.run_id AND old.name = new.name AND old.id <> new.id \
     AND old.finalized_at IS NOT NULL AND old.job_id = new.job_id AND old.attempt < new.attempt";

/// Finalizes upload `$1` of run `$2` at `$3`, with `$4` bytes and digest
/// `$5`, unless another artifact of the run holds its name.
const FINALIZE: &str = "UPDATE ci_artifacts SET finalized_at = $3, size = $4, digest = $5, \
     upload_id = NULL WHERE id = $1 AND run_id = $2 AND finalized_at IS NULL \
     AND NOT EXISTS (SELECT 1 FROM ci_artifacts AS held WHERE held.run_id = $2 \
       AND held.name = ci_artifacts.name AND held.finalized_at IS NOT NULL)";

/// Forgets upload `$1` of run `$2`, which is not finalized.
const DROP_PENDING: &str =
    "DELETE FROM ci_artifacts WHERE id = $1 AND run_id = $2 AND finalized_at IS NULL";

/// Deletes the finalized artifact of run `$1` named `$2`.
const DELETE: &str = "DELETE FROM ci_artifacts WHERE run_id = $1 AND name = $2 \
     AND finalized_at IS NOT NULL RETURNING {columns}";

#[derive(Deserialize)]
struct Count {
    n: u64,
}

/// What the call that finalized an upload found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finalized {
    /// The upload is an artifact now.
    Done,
    /// The upload is gone or already finalized, or another artifact of
    /// the run holds its name.
    Refused,
}

/// Records the start of an upload.
///
/// # Errors
///
/// Postgres refused the statement.
#[allow(clippy::too_many_arguments)]
pub async fn start_artifact(
    db: &Db,
    run_id: u64,
    job_id: u64,
    attempt: u32,
    name: &str,
    object_key: &str,
    upload_id: &str,
    now: u64,
) -> Result<ArtifactRow> {
    let row = db
        .first(
            &with_columns(START, ARTIFACT_COLUMNS),
            &params![run_id, job_id, attempt, name, object_key, upload_id, now],
        )
        .await?;
    row.ok_or_else(|| worker::Error::RustError("artifact insert returned no row".to_owned()))
}

/// The finalized artifact of `run_id` named `name`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn artifact_by_name(db: &Db, run_id: u64, name: &str) -> Result<Option<ArtifactRow>> {
    db.first(
        &with_columns(BY_NAME, ARTIFACT_COLUMNS),
        &params![run_id, name],
    )
    .await
}

/// Upload `id` of `run_id`, while it is not finalized.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn pending_artifact(db: &Db, id: u64, run_id: u64) -> Result<Option<ArtifactRow>> {
    db.first(
        &with_columns(PENDING, ARTIFACT_COLUMNS),
        &params![id, run_id],
    )
    .await
}

/// The finalized artifacts of `run_id`, oldest first: only those named
/// `name` and only the one with id `id`, when those are set.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn artifacts(
    db: &Db,
    run_id: u64,
    name: Option<&str>,
    id: Option<u64>,
) -> Result<Vec<ArtifactRow>> {
    db.all(
        &with_columns(LIST, ARTIFACT_COLUMNS),
        &params![run_id, name, id],
    )
    .await
}

/// How many artifacts and unfinished uploads `run_id` has.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn artifact_count(db: &Db, run_id: u64) -> Result<u64> {
    let count: Option<Count> = db.first(COUNT, &params![run_id]).await?;
    Ok(count.map_or(0, |count| count.n))
}

/// Finalizes upload `id` of `run_id`. The artifact an earlier attempt of
/// the same job uploaded under the same name goes in the same
/// transaction.
///
/// # Errors
///
/// Postgres refused a statement.
pub async fn finalize_artifact(
    db: &Db,
    id: u64,
    run_id: u64,
    size: u64,
    digest: Option<&str>,
    now: u64,
) -> Result<Finalized> {
    let changes = db
        .batch(&[
            (REPLACE, params![id, run_id]),
            (FINALIZE, params![id, run_id, now, size, digest]),
        ])
        .await?;
    Ok(if changes.get(1) == Some(&1) {
        Finalized::Done
    } else {
        Finalized::Refused
    })
}

/// Forgets upload `id` of `run_id`.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn drop_pending_artifact(db: &Db, id: u64, run_id: u64) -> Result<()> {
    db.run(DROP_PENDING, &params![id, run_id]).await?;
    Ok(())
}

/// Deletes the finalized artifact of `run_id` named `name` and returns
/// its row.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delete_artifact(db: &Db, run_id: u64, name: &str) -> Result<Option<ArtifactRow>> {
    db.first(
        &with_columns(DELETE, ARTIFACT_COLUMNS),
        &params![run_id, name],
    )
    .await
}
