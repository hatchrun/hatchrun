// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use crate::trust::Trust;
use hatchrun_cloud::testdb::Db as TestDb;

const MAIN: CacheOwner<'static> = CacheOwner {
    trust: Trust::Trusted,
    tenant_id: "t1",
    repo_id: 7,
    scope: "refs/heads/main",
};

fn on(scope: &'static str) -> CacheOwner<'static> {
    CacheOwner { scope, ..MAIN }
}

/// Starts an upload and finalizes it at `at`.
fn save(db: &TestDb, owner: CacheOwner<'_>, version: &str, key: &str, at: u64) -> CacheRow {
    let row = db
        .wait(start_cache_entry(
            db.db(),
            owner,
            version,
            key,
            &format!("cache/t1/7/{key}-{at}"),
            "mpu-1",
            at,
        ))
        .unwrap();
    let done = db
        .wait(finalize_cache_entry(db.db(), row.id, owner, 100, at + 1))
        .unwrap();
    assert_eq!(done, Finalized::Done);
    row
}

/// The entry named `key` in `owner`'s scope, or when not `exact` the
/// newest whose key starts with it.
fn find(db: &TestDb, owner: CacheOwner<'_>, key: &str, exact: bool) -> Option<CacheRow> {
    if exact {
        return db.wait(cache_entry(db.db(), owner, "v1", key)).unwrap();
    }
    restore(db, &[owner.scope], "no such key", &[key])
}

/// What a restore finds: `key`, then `restore_keys`, in `scopes`.
fn restore(db: &TestDb, scopes: &[&str], key: &str, restore_keys: &[&str]) -> Option<CacheRow> {
    let owned = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };
    db.wait(lookup_cache_entry(
        db.db(),
        "t1",
        7,
        "v1",
        &owned(scopes),
        key,
        &owned(restore_keys),
    ))
    .unwrap()
}

fn start(db: &TestDb, key: &str, at: u64) -> CacheRow {
    db.wait(start_cache_entry(
        db.db(),
        MAIN,
        "v1",
        key,
        &format!("cache/t1/7/{key}-{at}"),
        "mpu",
        at,
    ))
    .unwrap()
}

#[test]
fn an_upload_is_not_an_entry_until_it_is_finalized() {
    let db = setup();
    let pending = start(&db, "deps", 10);
    assert_eq!(pending.upload_id.as_deref(), Some("mpu"));
    assert_eq!(pending.size, None);
    assert!(find(&db, MAIN, "deps", true).is_none());
    let found = db
        .wait(pending_cache_entry(db.db(), pending.id, MAIN))
        .unwrap();
    assert_eq!(found.map(|row| row.id), Some(pending.id));

    db.wait(finalize_cache_entry(db.db(), pending.id, MAIN, 42, 20))
        .unwrap();
    let entry = find(&db, MAIN, "deps", true).unwrap();
    assert_eq!(
        (entry.size, entry.upload_id, entry.finalized_at),
        (Some(42), None, Some(20))
    );
    assert!(
        db.wait(pending_cache_entry(db.db(), pending.id, MAIN))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_scope_holds_one_entry_per_version_and_key() {
    let db = setup();
    let first = save(&db, MAIN, "v1", "deps", 10);
    let second = start(&db, "deps", 30);
    let done = db
        .wait(finalize_cache_entry(db.db(), second.id, MAIN, 5, 31))
        .unwrap();
    assert_eq!(done, Finalized::Refused);
    assert_eq!(find(&db, MAIN, "deps", true).unwrap().id, first.id);
    db.wait(drop_pending_cache_entry(db.db(), second.id, MAIN))
        .unwrap();
    assert!(
        db.wait(pending_cache_entry(db.db(), second.id, MAIN))
            .unwrap()
            .is_none()
    );
    // Another version, scope or key is a different entry.
    save(&db, MAIN, "v2", "deps", 40);
    save(&db, on("refs/heads/topic"), "v1", "deps", 40);
    save(&db, MAIN, "v1", "deps2", 40);
}

#[test]
fn calls_reach_only_their_own_tenant_repository_and_scope() {
    let db = setup();
    let entry = save(&db, MAIN, "v1", "deps", 10);
    let other_repo = CacheOwner { repo_id: 8, ..MAIN };
    let other_tenant = CacheOwner {
        tenant_id: "t2",
        ..MAIN
    };
    for stranger in [on("refs/heads/topic"), other_repo, other_tenant] {
        assert!(find(&db, stranger, "deps", true).is_none());
    }

    let pending = start(&db, "more", 50);
    for stranger in [on("refs/heads/topic"), other_repo, other_tenant] {
        assert!(
            db.wait(pending_cache_entry(db.db(), pending.id, stranger))
                .unwrap()
                .is_none()
        );
        let done = db
            .wait(finalize_cache_entry(db.db(), pending.id, stranger, 1, 60))
            .unwrap();
        assert_eq!(done, Finalized::Refused);
        db.wait(drop_pending_cache_entry(db.db(), pending.id, stranger))
            .unwrap();
    }
    assert!(
        db.wait(pending_cache_entry(db.db(), pending.id, MAIN))
            .unwrap()
            .is_some()
    );
    assert_eq!(find(&db, MAIN, "deps", true).unwrap().id, entry.id);
}

#[test]
fn a_restore_key_matches_a_prefix_and_takes_the_newest() {
    let db = setup();
    save(&db, MAIN, "v1", "linux-cargo-aaa", 10);
    let newest = save(&db, MAIN, "v1", "linux-cargo-bbb", 30);
    save(&db, MAIN, "v1", "linux-cargo-ccc", 20);
    save(&db, MAIN, "v1", "macos-cargo-ddd", 40);
    assert_eq!(
        find(&db, MAIN, "linux-cargo-", false).unwrap().id,
        newest.id
    );
    assert!(find(&db, MAIN, "windows-", false).is_none());
    // An exact lookup does not match a prefix.
    assert!(find(&db, MAIN, "linux-cargo-", true).is_none());
    // A prefix is text, not a pattern.
    assert!(find(&db, MAIN, "linux%", false).is_none());
    assert!(find(&db, MAIN, "linux_cargo", false).is_none());
}

#[test]
fn a_restore_touches_an_entry_at_most_once_an_hour() {
    let db = setup();
    let entry = save(&db, MAIN, "v1", "deps", 10);
    let accessed = |db: &TestDb| find(db, MAIN, "deps", true).unwrap().last_accessed_at;
    assert_eq!(accessed(&db), 11);

    db.wait(touch_cache_entry(db.db(), entry.id, "t1", 1_000))
        .unwrap();
    assert_eq!(accessed(&db), 11);
    db.wait(touch_cache_entry(
        db.db(),
        entry.id,
        "t1",
        TOUCH_EVERY_MS + 20,
    ))
    .unwrap();
    assert_eq!(accessed(&db), TOUCH_EVERY_MS + 20);
    // Another tenant cannot touch it.
    db.wait(touch_cache_entry(
        db.db(),
        entry.id,
        "t2",
        10 * TOUCH_EVERY_MS,
    ))
    .unwrap();
    assert_eq!(accessed(&db), TOUCH_EVERY_MS + 20);
}

fn evict_idle(db: &TestDb, now: u64, limit: u32) -> Vec<CacheRow> {
    db.wait(evict_idle_cache_entries(db.db(), now, 7 * DAY, DAY, limit))
        .unwrap()
}

#[test]
fn entries_idle_for_a_week_and_stale_uploads_are_evicted() {
    let db = setup();
    let now = 10 * DAY;
    let idle = save(&db, MAIN, "v1", "idle", 1);
    let used = save(&db, MAIN, "v1", "used", 1);
    let fresh = save(&db, MAIN, "v1", "fresh", now - DAY);
    // Saved nine days ago, restored two days ago.
    db.wait(touch_cache_entry(db.db(), used.id, "t1", now - 2 * DAY))
        .unwrap();
    let stale = start(&db, "stale", now - 2 * DAY);
    let uploading = start(&db, "going", now - 100);

    // The rows come back so the caller can delete their objects, oldest
    // first, at most `limit`.
    let first = evict_idle(&db, now, 1);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, idle.id);
    assert_eq!(first[0].object_key, idle.object_key);
    let rest = evict_idle(&db, now, 10);
    assert_eq!(
        rest.iter().map(|row| row.id).collect::<Vec<_>>(),
        [stale.id]
    );

    assert!(find(&db, MAIN, "idle", true).is_none());
    assert!(find(&db, MAIN, "used", true).is_some());
    assert!(find(&db, MAIN, "fresh", true).is_some());
    assert!(
        db.wait(pending_cache_entry(db.db(), stale.id, MAIN))
            .unwrap()
            .is_none()
    );
    assert!(
        db.wait(pending_cache_entry(db.db(), uploading.id, MAIN))
            .unwrap()
            .is_some()
    );
    assert_eq!(evict_idle(&db, now, 10), Vec::<CacheRow>::new());
    assert_ne!(used.id, fresh.id);
}

#[test]
fn an_evicted_entry_does_not_block_a_save_of_its_key() {
    let db = setup();
    let now = 10 * DAY;
    save(&db, MAIN, "v1", "deps", 1);
    assert_eq!(evict_idle(&db, now, 10).len(), 1);
    // Its row is gone with it, so nothing says the entry exists.
    assert!(find(&db, MAIN, "deps", true).is_none());
    let again = save(&db, MAIN, "v1", "deps", now);
    assert!(find(&db, MAIN, "deps", true).is_some_and(|row| row.id == again.id));
}

#[test]
fn an_entry_restored_before_the_sweep_stays() {
    let db = setup();
    let now = 10 * DAY;
    let entry = save(&db, MAIN, "v1", "deps", 1);
    // The sweep deletes with the idleness in the statement, so a restore
    // that lands first leaves the row, and its object, alone.
    db.wait(touch_cache_entry(db.db(), entry.id, "t1", now - 10))
        .unwrap();
    assert_eq!(evict_idle(&db, now, 10), Vec::<CacheRow>::new());
    assert!(find(&db, MAIN, "deps", true).is_some());
}

#[test]
fn a_restore_finds_the_first_scope_then_the_key_then_the_newest_prefix() {
    let db = setup();
    let topic = "refs/heads/topic";
    let main = "refs/heads/main";
    save(&db, MAIN, "v1", "linux-1", 10);
    let own = save(&db, on(topic), "v1", "linux-2", 20);
    let exact_main = save(&db, MAIN, "v1", "linux-exact", 30);
    save(&db, MAIN, "v2", "linux-exact", 40);
    save(&db, on("refs/heads/other"), "v1", "linux-exact", 50);

    // The first scope with a match wins, even over an exact key in the next.
    let found = restore(&db, &[topic, main], "linux-exact", &["linux-"]).unwrap();
    assert_eq!(found.id, own.id);
    // Within a scope the exact key beats a newer prefix match.
    let found = restore(&db, &[main], "linux-exact", &["linux-"]).unwrap();
    assert_eq!(found.id, exact_main.id);
    // Restore keys are tried in order.
    let found = restore(&db, &[main], "none", &["win-", "linux-e", "linux-"]).unwrap();
    assert_eq!(found.key, "linux-exact");
    let found = restore(&db, &[main], "none", &["linux-", "linux-e"]).unwrap();
    assert_eq!(found.key, "linux-exact");
    // The scopes asked for are the only ones read.
    assert!(restore(&db, &[topic], "linux-exact", &[]).is_none());
    assert!(restore(&db, &["refs/heads/none"], "linux-exact", &["linux-"]).is_none());
    assert!(restore(&db, &[], "linux-exact", &[]).is_none());
    // Another repository has no entry.
    let none = db
        .wait(lookup_cache_entry(
            db.db(),
            "t1",
            8,
            "v1",
            &[main.to_owned()],
            "linux-exact",
            &[],
        ))
        .unwrap();
    assert!(none.is_none());
}

/// Starts and finalizes an entry of `size` bytes, restored last at `at`.
fn save_sized(db: &TestDb, owner: CacheOwner<'_>, key: &str, size: u64, at: u64) -> CacheRow {
    let row = db
        .wait(start_cache_entry(
            db.db(),
            owner,
            "v1",
            key,
            &format!("cache/t1/7/{key}"),
            "mpu",
            at,
        ))
        .unwrap();
    db.wait(finalize_cache_entry(db.db(), row.id, owner, size, at))
        .unwrap();
    row
}

/// The keys of the entries the quota evicts at `cap` bytes, which it
/// deletes.
fn over(db: &TestDb, cap: u64, limit: u32) -> Vec<String> {
    db.wait(evict_over_quota_cache_entries(db.db(), "t1", 7, cap, limit))
        .unwrap()
        .into_iter()
        .map(|row| row.key)
        .collect()
}

#[test]
fn a_repository_over_its_cap_loses_the_least_recently_used_entries() {
    let db = setup();
    save_sized(&db, MAIN, "oldest", 40, 10);
    save_sized(&db, on("refs/heads/topic"), "old", 40, 20);
    save_sized(&db, MAIN, "new", 40, 30);
    // Other repositories do not count.
    save_sized(&db, CacheOwner { repo_id: 8, ..MAIN }, "elsewhere", 1000, 5);
    // Neither do uploads in flight.
    start(&db, "going", 40);

    assert_eq!(over(&db, 120, 10), Vec::<String>::new());
    assert_eq!(over(&db, 79, 1), ["oldest"]);
    assert_eq!(over(&db, 79, 10), ["old"]);
    // An entry that does not fit alone goes too.
    assert_eq!(over(&db, 30, 10), ["new"]);
    assert_eq!(over(&db, 30, 10), Vec::<String>::new());
    assert!(find(&db, MAIN, "new", true).is_none());
    // The other repository keeps its entry.
    let elsewhere = CacheOwner { repo_id: 8, ..MAIN };
    assert!(find_in(&db, elsewhere, "elsewhere").is_some());
}

fn find_in(db: &TestDb, owner: CacheOwner<'_>, key: &str) -> Option<CacheRow> {
    db.wait(cache_entry(db.db(), owner, "v1", key)).unwrap()
}

#[test]
fn a_restore_moves_an_entry_out_of_the_way_of_the_quota() {
    let db = setup();
    let oldest = save_sized(&db, MAIN, "oldest", 40, 10);
    save_sized(&db, MAIN, "old", 40, 20);
    save_sized(&db, MAIN, "new", 40, 30);
    db.wait(touch_cache_entry(
        db.db(),
        oldest.id,
        "t1",
        2 * TOUCH_EVERY_MS,
    ))
    .unwrap();
    assert_eq!(over(&db, 80, 10), ["old"]);
    assert!(find(&db, MAIN, "oldest", true).is_some());
}

#[test]
fn a_pull_request_run_records_its_base_branch() {
    let db = setup();
    let run = db
        .wait(insert_run(
            db.db(),
            &NewRun {
                base_ref: Some("release".into()),
                message: None,
                ..new_run("pr", "ci", 1)
            },
        ))
        .unwrap()
        .unwrap();
    assert_eq!(run.base_ref.as_deref(), Some("release"));
    let read = db.wait(super::run(db.db(), run.id)).unwrap().unwrap();
    assert_eq!(read.base_ref.as_deref(), Some("release"));
    let push = db
        .wait(insert_run(db.db(), &new_run("push", "ci", 2)))
        .unwrap()
        .unwrap();
    assert_eq!(push.base_ref, None);
}

#[test]
fn a_fork_save_counts_against_the_fork_share_and_evicts_only_fork_entries() {
    let db = TestDb::new();
    let fork = |scope| CacheOwner {
        trust: Trust::Fork,
        ..on(scope)
    };
    // The base branch holds a big, old entry that a fork's saves must
    // not push out.
    save_sized(&db, MAIN, "base", 5_000, 1);
    save_sized(&db, fork("refs/pull/3/merge"), "a", 100, 10);
    save_sized(&db, fork("refs/pull/3/merge"), "b", 100, 20);
    save_sized(&db, fork("refs/pull/4/merge"), "c", 100, 30);
    assert_eq!(db.wait(fork_cache_bytes(db.db(), "t1", 7)).unwrap(), 300);
    let gone = db
        .wait(evict_over_fork_quota_cache_entries(
            db.db(),
            "t1",
            7,
            250,
            10,
        ))
        .unwrap();
    let keys: Vec<&str> = gone.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(keys, ["a"], "the least recently used fork entry goes");
    assert!(find(&db, MAIN, "base", true).is_some(), "base entries stay");
    assert_eq!(db.wait(fork_cache_bytes(db.db(), "t1", 7)).unwrap(), 200);
    // Under the general quota rule the fork entries would not protect
    // the base entry: the fork rule never reaches it, whatever the cap.
    let none = db
        .wait(evict_over_fork_quota_cache_entries(
            db.db(),
            "t1",
            7,
            1_000,
            10,
        ))
        .unwrap();
    assert_eq!(none.len(), 0);
    let all = db
        .wait(evict_over_fork_quota_cache_entries(db.db(), "t1", 7, 0, 10))
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(find(&db, MAIN, "base", true).is_some());
}

#[test]
fn an_entry_records_the_trust_of_the_run_that_saved_it() {
    let db = TestDb::new();
    let owner = CacheOwner {
        trust: Trust::Fork,
        ..on("refs/pull/3/merge")
    };
    let row = save(&db, owner, "v", "k", 1);
    let trust: String = db
        .column(
            "SELECT trust AS t FROM ci_cache_entries WHERE id = $1",
            &params![row.id],
            "t",
        )
        .unwrap();
    assert_eq!(trust, "fork");
    let base = save(&db, MAIN, "v", "k", 2);
    let trust: String = db
        .column(
            "SELECT trust AS t FROM ci_cache_entries WHERE id = $1",
            &params![base.id],
            "t",
        )
        .unwrap();
    assert_eq!(trust, "trusted");
}
