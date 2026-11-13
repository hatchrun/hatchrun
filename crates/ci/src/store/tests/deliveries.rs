// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;

#[test]
fn concurrent_copies_of_one_delivery_store_once() {
    let db = setup();
    // Long enough that base64 would wrap it every 76 characters: the
    // payload must come back whole.
    let payload: Vec<u8> = (0..=u8::MAX).cycle().take(500).collect();
    // Both copies on one connection: exactly one stores. The overlap
    // of two connections is in
    // copies_on_two_connections_store_claim_and_restart_once.
    let (first, second) = db.wait(async {
        futures_util::join!(
            accept_delivery(db.db(), "g", "push", &payload, 1_000),
            accept_delivery(db.db(), "g", "push", &payload, 1_001),
        )
    });
    assert_ne!(first.unwrap(), second.unwrap());
    // Both consumers claim: exactly one plans.
    let (first, second) = db.wait(async {
        futures_util::join!(
            claim_delivery(db.db(), "g", 1_002),
            claim_delivery(db.db(), "g", 1_002),
        )
    });
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first.is_some(), second.is_some());
    let winner = first.or(second).unwrap();
    assert_eq!(winner.event, "push");
    assert_eq!(winner.payload, payload);
    assert_eq!(winner.attempts, 1);
}

#[test]
fn two_claims_of_one_delivery_one_wins() {
    let db = setup();
    db.wait(accept_delivery(db.db(), "g", "push", b"{}", 1_000))
        .unwrap();
    let first = db.wait(claim_delivery(db.db(), "g", 1_001)).unwrap();
    assert!(first.is_some());
    let token = first.unwrap().attempts;
    // Still planning with a live lease: nothing to claim.
    assert!(
        db.wait(claim_delivery(db.db(), "g", 1_002))
            .unwrap()
            .is_none()
    );
    // Done: nothing to claim either.
    assert!(
        db.wait(finish_delivery(db.db(), "g", token, "done", None, 1_002))
            .unwrap()
    );
    assert!(
        db.wait(claim_delivery(db.db(), "g", 1_003))
            .unwrap()
            .is_none()
    );
    // A done delivery drops its payload.
    let payload: Option<Option<String>> = db.column(
        "SELECT encode(payload, 'hex') AS p FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "p",
    );
    assert!(matches!(payload, Some(None)));
    let failed_at: Option<Option<i64>> = db.column(
        "SELECT failed_at AS t FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "t",
    );
    assert_eq!(failed_at, Some(None));
}

#[test]
fn a_failed_plan_retries_from_the_stored_payload() {
    let db = setup();
    let payload = br#"{"action":"opened"}"#;
    db.wait(accept_delivery(
        db.db(),
        "g",
        "pull_request",
        payload,
        1_000,
    ))
    .unwrap();
    let first = db
        .wait(claim_delivery(db.db(), "g", 1_001))
        .unwrap()
        .unwrap();
    assert_eq!(first.payload, payload);
    assert!(
        db.wait(release_delivery(db.db(), "g", first.attempts, 1_001))
            .unwrap()
    );
    let second = db
        .wait(claim_delivery(db.db(), "g", 1_002))
        .unwrap()
        .unwrap();
    assert_eq!(second.payload, payload);
    assert_eq!(second.payload, first.payload);
    assert_eq!(second.attempts, 2);
    // A failed delivery keeps its payload and its error.
    assert!(
        db.wait(finish_delivery(
            db.db(),
            "g",
            second.attempts,
            "failed",
            Some("boom"),
            1_003,
        ))
        .unwrap()
    );
    let row: Option<String> = db.column(
        "SELECT state AS s FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "s",
    );
    assert_eq!(row.as_deref(), Some("failed"));
    let error: Option<String> = db.column(
        "SELECT error AS e FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "e",
    );
    assert_eq!(error.as_deref(), Some("boom"));
    let failed_at: Option<i64> = db.column(
        "SELECT failed_at AS t FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "t",
    );
    assert_eq!(failed_at, Some(1_003));
    assert!(
        db.wait(claim_delivery(db.db(), "g", 1_003))
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_expired_lease_claims_again() {
    let db = setup();
    db.wait(accept_delivery(db.db(), "g", "push", b"{}", 1_000))
        .unwrap();
    assert!(
        db.wait(claim_delivery(db.db(), "g", 1_001))
            .unwrap()
            .is_some()
    );
    assert!(
        db.wait(claim_delivery(db.db(), "g", 1_002))
            .unwrap()
            .is_none()
    );
    // The lease ran out: the next claim plans it again.
    let later = 1_002 + LEASE_MS;
    let again = db
        .wait(claim_delivery(db.db(), "g", later))
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 2);
}

#[test]
fn stale_deliveries_cover_received_and_expired_leases_within_the_cap() {
    let db = setup();
    let now = 10_000_000u64;
    // Received long ago: stale.
    db.wait(accept_delivery(
        db.db(),
        "old",
        "push",
        b"{}",
        now - RECEIVED_STALE_MS - 1,
    ))
    .unwrap();
    // Received just now: the queue still holds it.
    db.wait(accept_delivery(db.db(), "fresh", "push", b"{}", now))
        .unwrap();
    // Planning with an expired lease: stale.
    db.wait(accept_delivery(
        db.db(),
        "expired",
        "push",
        b"{}",
        now - 1_000,
    ))
    .unwrap();
    db.wait(claim_delivery(db.db(), "expired", now - LEASE_MS - 1))
        .unwrap();
    // Planning with a live lease: a consumer holds it.
    db.wait(accept_delivery(db.db(), "live", "push", b"{}", now - 1_000))
        .unwrap();
    db.wait(claim_delivery(db.db(), "live", now)).unwrap();
    // Done and failed: never sent again.
    for guid in ["done", "failed"] {
        db.wait(accept_delivery(
            db.db(),
            guid,
            "push",
            b"{}",
            now - RECEIVED_STALE_MS - 1,
        ))
        .unwrap();
        let token = db
            .wait(claim_delivery(db.db(), guid, now))
            .unwrap()
            .unwrap()
            .attempts;
        let state = if guid == "done" { "done" } else { "failed" };
        assert!(
            db.wait(finish_delivery(db.db(), guid, token, state, None, now))
                .unwrap()
        );
    }
    // Claimed past the cap: left alone.
    db.wait(accept_delivery(
        db.db(),
        "capped",
        "push",
        b"{}",
        now - RECEIVED_STALE_MS - 1,
    ))
    .unwrap();
    db.run(
        "UPDATE ci_deliveries SET attempts = $1 WHERE guid = 'capped'",
        &[MAX_ATTEMPTS.into()],
    );
    let mut stale = db.wait(stale_deliveries(db.db(), now, 10)).unwrap();
    stale.sort();
    assert_eq!(stale, vec!["expired".to_owned(), "old".to_owned()]);
    // The limit binds the batch.
    let one = db.wait(stale_deliveries(db.db(), now, 1)).unwrap();
    assert_eq!(one.len(), 1);
}

#[test]
fn a_stale_consumer_changes_nothing_after_another_claim() {
    let db = setup();
    db.wait(accept_delivery(db.db(), "g", "push", b"{}", 1_000))
        .unwrap();
    let stale = db
        .wait(claim_delivery(db.db(), "g", 1_001))
        .unwrap()
        .unwrap();
    // The lease runs out and another consumer claims the delivery: a
    // slow plan, such as a 30-page installation resync.
    let live = db
        .wait(claim_delivery(db.db(), "g", 1_002 + LEASE_MS))
        .unwrap()
        .unwrap();
    assert_eq!((stale.attempts, live.attempts), (1, 2));
    // The stale token neither fails the other consumer's delivery nor
    // releases it back for a second plan.
    assert!(
        !db.wait(finish_delivery(
            db.db(),
            "g",
            stale.attempts,
            "failed",
            Some("stale"),
            1_003 + LEASE_MS,
        ))
        .unwrap()
    );
    assert!(
        !db.wait(release_delivery(
            db.db(),
            "g",
            stale.attempts,
            1_003 + LEASE_MS
        ))
        .unwrap()
    );
    let row: Option<String> = db.column(
        "SELECT state AS s FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "s",
    );
    assert_eq!(row.as_deref(), Some("planning"));
    let attempts: Option<i64> = db.column(
        "SELECT attempts AS n FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "n",
    );
    assert_eq!(attempts, Some(2));
    // The live token still ends it.
    assert!(
        db.wait(finish_delivery(
            db.db(),
            "g",
            live.attempts,
            "done",
            None,
            1_003 + LEASE_MS
        ))
        .unwrap()
    );
}

#[test]
fn a_released_delivery_waits_for_its_own_retry() {
    let db = setup();
    let now = 10_000_000u64;
    db.wait(accept_delivery(
        db.db(),
        "g",
        "push",
        b"{}",
        now - RECEIVED_STALE_MS - 1,
    ))
    .unwrap();
    let claim = db.wait(claim_delivery(db.db(), "g", now)).unwrap().unwrap();
    // The plan failed and the queue sends the message again in 5 min.
    let retry_at = now + 300_000;
    assert!(
        db.wait(release_delivery(db.db(), "g", claim.attempts, retry_at))
            .unwrap()
    );
    // Until that retry is past, the reconciler sends nothing: one
    // message is on its way already.
    for at in [now + 1, retry_at, retry_at + RECEIVED_STALE_MS - 1] {
        assert_eq!(
            db.wait(stale_deliveries(db.db(), at, 10)).unwrap(),
            Vec::<String>::new(),
            "{at}"
        );
        assert!(!db.wait(note_resend(db.db(), "g", at)).unwrap(), "{at}");
    }
    // The retry message still claims it.
    let retried = db
        .wait(claim_delivery(db.db(), "g", retry_at))
        .unwrap()
        .unwrap();
    assert_eq!(retried.attempts, 2);
    // Released again and its last retry never came: the message ran out
    // of retries. The reconciler sends it once the retry is past.
    assert!(
        db.wait(release_delivery(db.db(), "g", retried.attempts, retry_at))
            .unwrap()
    );
    let past = retry_at + RECEIVED_STALE_MS + 1;
    assert_eq!(
        db.wait(stale_deliveries(db.db(), past, 10)).unwrap(),
        vec!["g".to_owned()]
    );
    assert!(db.wait(note_resend(db.db(), "g", past)).unwrap());
}

#[test]
fn resends_count_toward_the_cap_and_capped_deliveries_fail() {
    let db = setup();
    let now = 10_000_000u64;
    db.wait(accept_delivery(
        db.db(),
        "old",
        "push",
        b"{}",
        now - RECEIVED_STALE_MS - 1,
    ))
    .unwrap();
    db.wait(accept_delivery(db.db(), "fresh", "push", b"{}", now))
        .unwrap();
    // A stale row's resend counts; a fresh one is not due, so the
    // queue's in-flight send is not counted twice.
    assert!(db.wait(note_resend(db.db(), "old", now)).unwrap());
    assert!(!db.wait(note_resend(db.db(), "fresh", now)).unwrap());
    let attempts: Option<i64> = db.column(
        "SELECT attempts AS n FROM ci_deliveries WHERE guid = 'old'",
        &[],
        "n",
    );
    assert_eq!(attempts, Some(1));
    // At the cap the row stops resending and fails with an error, so
    // the alert path sees it.
    db.run(
        "UPDATE ci_deliveries SET attempts = $1 WHERE guid = 'old'",
        &[MAX_ATTEMPTS.into()],
    );
    assert_eq!(
        db.wait(stale_deliveries(db.db(), now, 10)).unwrap(),
        Vec::<String>::new()
    );
    let capped = db.wait(fail_capped_deliveries(db.db(), now)).unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].0, "old");
    assert!(capped[0].1.contains("resent too often"), "{}", capped[0].1);
    assert!(capped[0].1.contains("old"), "{}", capped[0].1);
    let state: Option<String> = db.column(
        "SELECT state AS s FROM ci_deliveries WHERE guid = 'old'",
        &[],
        "s",
    );
    let error: Option<String> = db.column(
        "SELECT error AS e FROM ci_deliveries WHERE guid = 'old'",
        &[],
        "e",
    );
    assert_eq!(state.as_deref(), Some("failed"));
    let failed_at: Option<i64> = db.column(
        "SELECT failed_at AS t FROM ci_deliveries WHERE guid = 'old'",
        &[],
        "t",
    );
    assert_eq!(failed_at, Some(i64::try_from(now).unwrap()));
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("resent too often")),
        "{error:?}"
    );
    // At the cap, a delivery whose queue retry or lease is still live
    // fails only once it is past: a message may still plan it.
    for (guid, state) in [("retrying", "received"), ("leased", "planning")] {
        db.wait(accept_delivery(
            db.db(),
            guid,
            "push",
            b"{}",
            now - RECEIVED_STALE_MS - 1,
        ))
        .unwrap();
        db.run(
            "UPDATE ci_deliveries SET state = $2, attempts = $3, lease_until = $4 \
             WHERE guid = $1",
            &params![guid, state, MAX_ATTEMPTS, now + 60_000],
        );
    }
    assert_eq!(
        db.wait(fail_capped_deliveries(db.db(), now)).unwrap(),
        Vec::<(String, String)>::new()
    );
    let mut capped: Vec<String> = db
        .wait(fail_capped_deliveries(db.db(), now + 60_001))
        .unwrap()
        .into_iter()
        .map(|(guid, _)| guid)
        .collect();
    capped.sort();
    assert_eq!(capped, vec!["leased".to_owned(), "retrying".to_owned()]);
    // Failed rows resend and fail no more.
    assert!(!db.wait(note_resend(db.db(), "old", now)).unwrap());
    assert_eq!(
        db.wait(fail_capped_deliveries(db.db(), now)).unwrap(),
        Vec::<(String, String)>::new()
    );
}

/// Runs `second` on `other` while `db` holds an open transaction, which
/// commits after 200 ms. Returns what `second` gave and how long it
/// took: at least the 200 ms when it waited on the transaction's row.
fn overlapped<T>(
    db: &TestDb,
    second: impl std::future::Future<Output = T>,
) -> (T, std::time::Duration) {
    db.wait(async {
        let started = std::time::Instant::now();
        let (out, ()) = futures_util::join!(
            async {
                let out = second.await;
                (out, started.elapsed())
            },
            async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                db.db().run("COMMIT", &[]).await.unwrap();
            }
        );
        out
    })
}

#[test]
fn copies_on_two_connections_store_claim_and_restart_once() {
    let db = setup();
    let config: tokio_postgres::Config = hatchrun_cloud::testdb::url().parse().unwrap();
    let other = db.connect(&config);
    let waited = std::time::Duration::from_millis(150);

    // Two copies arrive: the second waits for the first's insert and
    // stores nothing.
    db.run("BEGIN", &[]);
    assert!(
        db.wait(accept_delivery(db.db(), "g", "push", b"{}", 1_000))
            .unwrap()
    );
    let (copy, took) = overlapped(&db, accept_delivery(&other, "g", "push", b"{}", 1_001));
    assert!(!copy.unwrap());
    assert!(took >= waited, "{took:?}");

    // Two consumers claim it: the second waits for the first's claim
    // and gets nothing.
    db.run("BEGIN", &[]);
    let claim = db
        .wait(claim_delivery(db.db(), "g", 1_002))
        .unwrap()
        .unwrap();
    let (second, took) = overlapped(&db, claim_delivery(&other, "g", 1_002));
    assert!(second.unwrap().is_none());
    assert!(took >= waited, "{took:?}");

    // It fails, and two redeliveries arrive: it starts over once.
    assert!(
        db.wait(finish_delivery(
            db.db(),
            "g",
            claim.attempts,
            "failed",
            Some("boom"),
            1_003
        ))
        .unwrap()
    );
    db.run("BEGIN", &[]);
    assert!(
        db.wait(accept_delivery(db.db(), "g", "push", b"{}", 1_004))
            .unwrap()
    );
    let (copy, took) = overlapped(&db, accept_delivery(&other, "g", "push", b"{}", 1_004));
    assert!(!copy.unwrap());
    assert!(took >= waited, "{took:?}");
    let state: Option<String> = db.column(
        "SELECT state AS s FROM ci_deliveries WHERE guid = 'g'",
        &[],
        "s",
    );
    assert_eq!(state.as_deref(), Some("received"));
}
