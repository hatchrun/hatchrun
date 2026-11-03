// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use hatchrun_cloud::testdb::Db as TestDb;

/// Starts an upload of job `job` and finalizes it.
fn upload(db: &TestDb, run: u64, job: u64, attempt: u32, name: &str, size: u64) -> ArtifactRow {
    let row = start(db, run, job, attempt, name);
    let done = db
        .wait(finalize_artifact(
            db.db(),
            row.id,
            run,
            size,
            Some("sha256:ab"),
            50,
        ))
        .unwrap();
    assert_eq!(done, Finalized::Done);
    row
}

fn start(db: &TestDb, run: u64, job: u64, attempt: u32, name: &str) -> ArtifactRow {
    db.wait(start_artifact(
        db.db(),
        run,
        job,
        attempt,
        name,
        &format!("artifacts/30d/t1/7/{run}/{name}-{attempt}"),
        "mpu-1",
        10,
    ))
    .unwrap()
}

fn names(rows: &[ArtifactRow]) -> Vec<String> {
    rows.iter().map(|row| row.name.clone()).collect()
}

#[test]
fn an_upload_is_not_listed_until_it_is_finalized() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let pending = start(&db, run.id, job, 1, "dist");
    assert_eq!(pending.upload_id.as_deref(), Some("mpu-1"));
    assert_eq!(
        db.wait(artifacts(db.db(), run.id, None, None))
            .unwrap()
            .len(),
        0
    );
    assert!(
        db.wait(artifact_by_name(db.db(), run.id, "dist"))
            .unwrap()
            .is_none()
    );
    assert_eq!(db.wait(artifact_count(db.db(), run.id)).unwrap(), 1);

    let done = upload(&db, run.id, job, 1, "dist", 12);
    let found = db.wait(artifacts(db.db(), run.id, None, None)).unwrap();
    assert_eq!(names(&found), ["dist"]);
    assert_eq!(found[0].id, done.id);
    assert_eq!(found[0].size, Some(12));
    assert_eq!(found[0].digest.as_deref(), Some("sha256:ab"));
    assert_eq!(found[0].upload_id, None);
    assert!(found[0].finalized_at.is_some());
}

#[test]
fn lists_filter_by_name_and_id() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let a = upload(&db, run.id, job, 1, "a", 1);
    upload(&db, run.id, job, 1, "b", 2);
    let by = |name: Option<&str>, id: Option<u64>| {
        names(&db.wait(artifacts(db.db(), run.id, name, id)).unwrap())
    };
    assert_eq!(by(None, None), ["a", "b"]);
    assert_eq!(by(Some("b"), None), ["b"]);
    assert_eq!(by(None, Some(a.id)), ["a"]);
    assert_eq!(by(Some("b"), Some(a.id)).len(), 0);
    assert_eq!(by(Some("c"), None).len(), 0);
}

#[test]
fn artifacts_stay_with_their_run() {
    let db = setup();
    let (run, job) = run_with_job_named(&db, "one", "running");
    let (other, other_job) = run_with_job_named(&db, "two", "running");
    let mine = upload(&db, run.id, job, 1, "dist", 1);
    upload(&db, other.id, other_job, 1, "dist", 2);

    assert_eq!(
        db.wait(artifact_by_name(db.db(), run.id, "dist"))
            .unwrap()
            .unwrap()
            .id,
        mine.id
    );
    assert_eq!(
        db.wait(artifacts(db.db(), other.id, None, Some(mine.id)))
            .unwrap()
            .len(),
        0
    );
    // Neither the upload nor a delete reaches across runs.
    let pending = start(&db, run.id, job, 1, "later");
    assert!(
        db.wait(pending_artifact(db.db(), pending.id, other.id))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.wait(finalize_artifact(
            db.db(),
            pending.id,
            other.id,
            1,
            None,
            60
        ))
        .unwrap(),
        Finalized::Refused
    );
    assert!(
        db.wait(delete_artifact(db.db(), other.id, "later"))
            .unwrap()
            .is_none()
    );
    let theirs = db
        .wait(delete_artifact(db.db(), other.id, "dist"))
        .unwrap()
        .unwrap();
    assert_eq!(theirs.run_id, other.id);
    assert!(
        db.wait(artifact_by_name(db.db(), run.id, "dist"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_name_is_taken_once_per_run() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let first = upload(&db, run.id, job, 1, "dist", 1);
    let again = start(&db, run.id, job, 1, "dist");
    assert_eq!(
        db.wait(finalize_artifact(db.db(), again.id, run.id, 2, None, 60))
            .unwrap(),
        Finalized::Refused
    );
    let kept = db
        .wait(artifact_by_name(db.db(), run.id, "dist"))
        .unwrap()
        .unwrap();
    assert_eq!((kept.id, kept.size), (first.id, Some(1)));
    // The refused upload is still an upload, which the agent drops.
    db.wait(drop_pending_artifact(db.db(), again.id, run.id))
        .unwrap();
    assert!(
        db.wait(pending_artifact(db.db(), again.id, run.id))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_later_attempt_of_the_job_replaces_its_artifact() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let first = upload(&db, run.id, job, 1, "dist", 1);
    let second = upload(&db, run.id, job, 2, "dist", 2);
    let found = db.wait(artifacts(db.db(), run.id, None, None)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].id, found[0].size), (second.id, Some(2)));
    assert_ne!(first.id, second.id);
}

#[test]
fn another_job_cannot_replace_an_artifact() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let ids = db
        .wait(insert_jobs(db.db(), run.id, &[new_job("test")], 1_000))
        .unwrap();
    let other = ids[0].1;
    let first = upload(&db, run.id, job, 1, "dist", 1);
    let theirs = start(&db, run.id, other, 5, "dist");
    assert_eq!(
        db.wait(finalize_artifact(db.db(), theirs.id, run.id, 2, None, 60))
            .unwrap(),
        Finalized::Refused
    );
    let found = db.wait(artifacts(db.db(), run.id, None, None)).unwrap();
    assert_eq!(
        found.iter().map(|row| row.id).collect::<Vec<_>>(),
        [first.id]
    );
}

#[test]
fn finalizing_twice_does_nothing_the_second_time() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let row = upload(&db, run.id, job, 1, "dist", 1);
    assert_eq!(
        db.wait(finalize_artifact(db.db(), row.id, run.id, 9, None, 99))
            .unwrap(),
        Finalized::Refused
    );
    let kept = db
        .wait(artifact_by_name(db.db(), run.id, "dist"))
        .unwrap()
        .unwrap();
    assert_eq!((kept.size, kept.finalized_at), (Some(1), Some(50)));
}

#[test]
fn a_delete_takes_the_row_and_frees_the_name() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    let row = upload(&db, run.id, job, 1, "dist", 1);
    let gone = db
        .wait(delete_artifact(db.db(), run.id, "dist"))
        .unwrap()
        .unwrap();
    assert_eq!(gone.object_key, row.object_key);
    assert!(
        db.wait(delete_artifact(db.db(), run.id, "dist"))
            .unwrap()
            .is_none()
    );
    upload(&db, run.id, job, 1, "dist", 3);
    assert_eq!(db.wait(artifact_count(db.db(), run.id)).unwrap(), 1);
}

#[test]
fn a_run_takes_its_artifacts_with_it() {
    let db = setup();
    let (run, job) = run_with_job(&db, "running");
    upload(&db, run.id, job, 1, "dist", 1);
    db.run("DELETE FROM ci_runs WHERE id = $1", &params![run.id]);
    let left: Option<u64> = db.column("SELECT count(*) AS n FROM ci_artifacts", &[], "n");
    assert_eq!(left, Some(0));
}
