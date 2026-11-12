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
    // Both copies arrive at once: exactly one stores.
    let (first, second) = db.wait(async {
        futures_util::join!(
            accept_delivery(db.db(), "g", "push", &payload, 1_000),
            accept_delivery(db.db(), "g", "push", &payload, 1_001),
        )
    });
    assert_ne!(first.unwrap(), second.unwrap());
    // Both consumers claim at once: exactly one plans.
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
        db.wait(finish_delivery(db.db(), "g", token, "done", None))
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
        db.wait(release_delivery(db.db(), "g", first.attempts))
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
            Some("boom")
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
            db.wait(finish_delivery(db.db(), guid, token, state, None))
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
            Some("stale")
        ))
        .unwrap()
    );
    assert!(
        !db.wait(release_delivery(db.db(), "g", stale.attempts))
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
        db.wait(finish_delivery(db.db(), "g", live.attempts, "done", None))
            .unwrap()
    );
}

#[test]
fn the_dead_letter_only_finishes_deliveries_that_wait() {
    let db = setup();
    for guid in ["waiting", "planning", "done", "failed"] {
        db.wait(accept_delivery(db.db(), guid, "push", b"{}", 1_000))
            .unwrap();
    }
    let planning = db
        .wait(claim_delivery(db.db(), "planning", 1_001))
        .unwrap()
        .unwrap();
    let done = db
        .wait(claim_delivery(db.db(), "done", 1_001))
        .unwrap()
        .unwrap();
    assert!(
        db.wait(finish_delivery(
            db.db(),
            "done",
            done.attempts,
            "done",
            None
        ))
        .unwrap()
    );
    assert!(
        db.wait(finish_delivery(
            db.db(),
            "failed",
            db.wait(claim_delivery(db.db(), "failed", 1_001))
                .unwrap()
                .unwrap()
                .attempts,
            "failed",
            Some("boom"),
        ))
        .unwrap()
    );
    // Still waiting: finished as failed, keeping the payload.
    assert!(
        db.wait(finish_dead_letter(db.db(), "waiting", "late: waiting"))
            .unwrap()
    );
    assert!(
        db.wait(finish_dead_letter(db.db(), "planning", "late: planning"))
            .unwrap()
    );
    // Already decided: nothing changes and no alert goes out for them.
    assert!(
        !db.wait(finish_dead_letter(db.db(), "done", "late"))
            .unwrap()
    );
    assert!(
        !db.wait(finish_dead_letter(db.db(), "failed", "late"))
            .unwrap()
    );
    assert!(
        !db.wait(finish_dead_letter(db.db(), "missing", "late"))
            .unwrap()
    );
    let rows: Vec<(String, String)> = ["done", "failed", "planning", "waiting"]
        .into_iter()
        .map(|guid| {
            let state: Option<String> = db.column(
                "SELECT state AS s FROM ci_deliveries WHERE guid = $1",
                &params![guid],
                "s",
            );
            (guid.to_owned(), state.unwrap())
        })
        .collect();
    assert_eq!(planning.attempts, 1);
    assert_eq!(
        rows,
        vec![
            ("done".to_owned(), "done".to_owned()),
            ("failed".to_owned(), "failed".to_owned()),
            ("planning".to_owned(), "failed".to_owned()),
            ("waiting".to_owned(), "failed".to_owned()),
        ]
    );
    // The done delivery still drops its payload; the failed ones keep
    // theirs, and the done one's error stays empty.
    let payloads: i64 = db
        .column(
            "SELECT COUNT(*) AS n FROM ci_deliveries WHERE payload IS NOT NULL",
            &[],
            "n",
        )
        .unwrap();
    assert_eq!(payloads, 3);
    let error: Option<String> = db.column(
        "SELECT error AS e FROM ci_deliveries WHERE guid = 'failed'",
        &[],
        "e",
    );
    assert_eq!(error.as_deref(), Some("boom"));
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
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("resent too often")),
        "{error:?}"
    );
    // Failed rows resend and fail no more.
    assert!(!db.wait(note_resend(db.db(), "old", now)).unwrap());
    assert_eq!(
        db.wait(fail_capped_deliveries(db.db(), now)).unwrap(),
        Vec::<(String, String)>::new()
    );
}
