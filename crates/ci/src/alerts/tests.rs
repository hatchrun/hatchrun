// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use std::cell::{Cell, RefCell};

use hatchrun_cloud::db::params;
use hatchrun_cloud::testdb::Db;

use super::checks::{
    Database, Facts, FailedDelivery, FailedLaunch, LowRateLimit, PassRecord, check, failed_launches,
};
use super::health::history as parse_history;
use super::queries::{
    DATABASE, DELETE_FAILED, DELETE_RATE_LIMITS, FAILED_DELIVERIES_SINCE, FAILED_SINCE, LOW_SINCE,
    NOTE_RATE_LIMIT, RECORD_FAILED, RECORD_UNREADABLE,
};
use super::runtime::Kept;
use super::*;

const NOW: u64 = 1_800_000_000_000;
const HOUR: u64 = 60 * MINUTE;
const NONE: [&str; 0] = [];

/// Collects emails, or fails them while `down`. Keeps its own clock,
/// which a send moves on by `lag` and each reading of it by `step`.
/// While `stall` is set a send never completes and the time bound on it
/// runs out at once; while `hold` is set a send waits for the test.
/// `bound` is the last time bound a send was given.
#[derive(Default)]
struct Outbox {
    sent: RefCell<Vec<Email>>,
    down: Cell<bool>,
    stall: Cell<bool>,
    hold: Cell<bool>,
    entered: Cell<bool>,
    clock: Cell<u64>,
    lag: Cell<u64>,
    step: Cell<u64>,
    bound: Cell<Option<u64>>,
}

#[allow(clippy::unused_async_trait_impl)]
impl Mail for Outbox {
    async fn send(&self, email: &Email) -> Result<(), String> {
        self.clock.set(self.clock.get() + self.lag.get());
        if self.stall.get() {
            std::future::pending::<()>().await;
        }
        if self.hold.get() {
            self.entered.set(true);
            std::future::poll_fn(|cx| {
                if self.hold.get() {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .await;
        }
        if self.down.get() {
            return Err("Email Service unavailable".to_owned());
        }
        self.sent.borrow_mut().push(email.clone());
        Ok(())
    }

    fn now(&self) -> u64 {
        let now = self.clock.get();
        self.clock.set(now + self.step.get());
        now
    }

    async fn sleep(&self, ms: u64) {
        self.bound.set(Some(ms));
        if !self.stall.get() {
            std::future::pending::<()>().await;
        }
    }
}

impl Outbox {
    /// Sets the clock to `now`.
    fn at(&self, now: u64) -> &Self {
        self.clock.set(now);
        self
    }

    /// The subjects sent since the last call.
    fn take(&self) -> Vec<String> {
        self.sent
            .borrow_mut()
            .drain(..)
            .map(|email| email.subject)
            .collect()
    }
}

#[derive(serde::Deserialize)]
struct Claim {
    sending_until: Option<u64>,
}

fn claimed_until(db: &Db, key: Key) -> Option<u64> {
    db.first::<Claim>(
        "SELECT sending_until FROM ci_alerts WHERE key = $1",
        &params![key.as_str()],
    )
    .unwrap()
    .sending_until
}

/// A store that fails to save the row of one alert.
struct Unsaved<'a> {
    kept: Kept<'a>,
    key: Key,
    /// Digest undos that fail before one works.
    undo_fails: Cell<u32>,
}

impl Store for Unsaved<'_> {
    async fn alerts(&self) -> Result<Vec<Alert>, String> {
        self.kept.alerts().await
    }

    async fn save(&self, prev: Option<&Alert>, next: &Alert, now: u64) -> Result<bool, String> {
        if next.key == self.key.as_str() {
            return Err("database unavailable".to_owned());
        }
        self.kept.save(prev, next, now).await
    }

    async fn claim(
        &self,
        alert: &Alert,
        state: State,
        now: u64,
        until: u64,
    ) -> Result<bool, String> {
        self.kept.claim(alert, state, now, until).await
    }

    async fn sent(
        &self,
        alert: &Alert,
        state: State,
        until: u64,
        now: u64,
    ) -> Result<bool, String> {
        self.kept.sent(alert, state, until, now).await
    }

    async fn release(&self, key: &str, until: u64) -> Result<(), String> {
        self.kept.release(key, until).await
    }

    async fn digest_at(&self) -> Result<Option<u64>, String> {
        self.kept.digest_at().await
    }

    async fn digest_claim(&self, last: Option<u64>, at: u64) -> Result<bool, String> {
        self.kept.digest_claim(last, at).await
    }

    async fn digest_undo(&self, last: Option<u64>, at: u64) -> Result<(), String> {
        if self.undo_fails.get() > 0 {
            self.undo_fails.set(self.undo_fails.get() - 1);
            return Err("database unavailable".to_owned());
        }
        self.kept.digest_undo(last, at).await
    }
}

fn pass(db: &Db, outbox: &Outbox, now: u64, seen: &[(Key, Seen)]) -> Vec<String> {
    db.wait(settle(
        &Kept(db.db()),
        outbox.at(now),
        now,
        async || seen.to_vec(),
        true,
    ))
}

fn firing(detail: &str) -> Seen {
    Seen::Firing(detail.to_owned())
}

fn found(seen: &[(Key, Seen)], key: Key) -> &Seen {
    &seen.iter().find(|(at, _)| *at == key).unwrap().1
}

fn passes(errors: &[u32]) -> Vec<PassRecord> {
    errors
        .iter()
        .enumerate()
        .map(|(at, errors)| PassRecord {
            at: at as u64,
            errors: *errors,
        })
        .collect()
}

fn alert(db: &Db, key: Key) -> Alert {
    db.first(
        "SELECT key, state, detail, fired_at, resolved_at, sent FROM ci_alerts WHERE key = $1",
        &params![key.as_str()],
    )
    .unwrap()
}

#[test]
fn the_history_reads_as_the_reconciler_row_keeps_it() {
    let history = passes(&[0, 1, 0]);
    // As the reconciler row keeps it.
    let text = serde_json::to_string(&history).unwrap();
    assert_eq!(parse_history(Some(&text)), Some(history));
    assert_eq!(parse_history(Some("not json")), None);
    assert_eq!(parse_history(Some("")), Some(Vec::new()));
    assert_eq!(parse_history(None), Some(Vec::new()));
}

#[test]
fn checks_read_the_pass_history() {
    let facts = |history: Vec<PassRecord>| Facts {
        history: Some(history),
        ..Facts::default()
    };
    let seen = check(&facts(passes(&[0, 2, 1, 3, 1, 1])));
    assert!(matches!(
        found(&seen, Key::ReconcilerFailing),
        Seen::Firing(detail) if detail.contains("2, 1, 3, 1, 1")
    ));
    // One clean pass among the last five clears it.
    let seen = check(&facts(passes(&[1, 1, 1, 0, 1, 1])));
    assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);
    // Too few passes to tell.
    let seen = check(&facts(passes(&[1, 1, 1, 1])));
    assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);

    // Data that could not be read changes nothing.
    let seen = check(&Facts::default());
    assert!(seen.iter().all(|(_, seen)| *seen == Seen::Unknown));
    assert_eq!(seen.len(), 5);
}

#[test]
fn failed_deliveries_fire_their_own_alert() {
    let seen = check(&Facts {
        failed_deliveries: Some(vec![FailedDelivery {
            guid: "d-1".to_owned(),
            event: "push".to_owned(),
            error: Some("the delivery was resent too often: d-1".to_owned()),
            attempts: 50,
            failed_at: NOW,
        }]),
        ..Facts::default()
    });
    assert!(matches!(
        found(&seen, Key::DeliveryFailed),
        Seen::Firing(detail) if detail.contains("delivery d-1 (push), 50 attempts")
            && detail.contains("resent too often")
    ));
    // Launch failures are a separate alert.
    assert_eq!(*found(&seen, Key::LaunchFailed), Seen::Unknown);
    let clear = check(&Facts {
        failed_deliveries: Some(Vec::new()),
        ..Facts::default()
    });
    assert_eq!(*found(&clear, Key::DeliveryFailed), Seen::Clear);
    assert_eq!(Key::DeliveryFailed.as_str(), "ci_delivery_failed");
}

#[test]
fn checks_read_the_database_and_github() {
    let facts = Facts {
        database: Some(Database {
            connections: 41,
            max_connections: 50,
            bytes: 5_400_000_000,
        }),
        failed: Some(vec![FailedLaunch {
            job_id: Some(5),
            job_name: Some("build".to_owned()),
            error: Some("RunInstances: InsufficientInstanceCapacity".to_owned()),
            attempts: Some(4),
            failed_at: NOW,
        }]),
        rate_limits: Some(vec![
            LowRateLimit {
                installation_id: 42,
                remaining: Some(0),
                limited: true,
            },
            LowRateLimit {
                installation_id: 43,
                remaining: Some(1200),
                limited: false,
            },
        ]),
        ..Facts::default()
    };
    let seen = check(&facts);
    assert!(matches!(
        found(&seen, Key::DatabaseConnections),
        Seen::Firing(detail) if detail.contains("41 of 50 connections")
            && detail.contains("5.4 GB")
    ));
    assert!(matches!(
        found(&seen, Key::LaunchFailed),
        Seen::Firing(detail) if detail.contains("job 5 (build), 4 attempts")
            && detail.contains("InsufficientInstanceCapacity")
    ));
    assert!(matches!(
        found(&seen, Key::RateLimit),
        Seen::Firing(detail) if detail.contains("installation 42: rate limited")
            && detail.contains("installation 43: 1200 requests left")
    ));

    let at_the_line = check(&Facts {
        database: Some(Database {
            connections: 40,
            max_connections: 50,
            bytes: 0,
        }),
        ..Facts::default()
    });
    assert!(matches!(
        found(&at_the_line, Key::DatabaseConnections),
        Seen::Firing(_)
    ));

    let clear = check(&Facts {
        database: Some(Database {
            connections: 39,
            max_connections: 50,
            bytes: 0,
        }),
        failed: Some(Vec::new()),
        rate_limits: Some(Vec::new()),
        ..Facts::default()
    });
    for key in [Key::DatabaseConnections, Key::LaunchFailed, Key::RateLimit] {
        assert_eq!(*found(&clear, key), Seen::Clear, "{key:?}");
    }
}

#[test]
fn an_alert_emails_once_when_it_fires_and_once_when_it_resolves() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::ReconcilerFailing;
    assert_eq!(pass(&db, &outbox, NOW, &[(key, firing("errors"))]), NONE);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: CI reconciler failing"]);
    // Still firing: no email, the detail follows the last check.
    pass(&db, &outbox, NOW + MINUTE, &[(key, firing("more errors"))]);
    pass(&db, &outbox, NOW + 2 * MINUTE, &[(key, Seen::Unknown)]);
    assert_eq!(outbox.take(), NONE);
    let row = alert(&db, key);
    assert_eq!(row.detail, "more errors");
    assert_eq!(row.fired_at, NOW);

    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Resolved: CI reconciler failing"]
    );
    pass(&db, &outbox, NOW + HOUR + MINUTE, &[(key, Seen::Clear)]);
    assert_eq!(outbox.take(), NONE);
    let row = alert(&db, key);
    assert_eq!(row.state, State::Resolved);
    assert_eq!(row.resolved_at, Some(NOW + HOUR));
    assert_eq!(row.sent, Some(State::Resolved));

    // It fires again, from the new time.
    pass(&db, &outbox, NOW + 2 * HOUR, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: CI reconciler failing"]);
    assert_eq!(alert(&db, key).fired_at, NOW + 2 * HOUR);
}

#[test]
fn the_emails_say_what_and_since_when() {
    let fired = Alert {
        key: Key::ReconcilerFailing.as_str().to_owned(),
        state: State::Firing,
        detail: "acme: 3 jobs".to_owned(),
        fired_at: NOW,
        resolved_at: None,
        sent: None,
    };
    let email = notice(&fired, State::Firing, NOW);
    assert_eq!(email.subject, "[hatchrun] Firing: CI reconciler failing");
    assert!(email.text.contains("since 2027-01-15T08:00:00Z"));
    assert!(email.text.contains("acme: 3 jobs"));
    let resolved = Alert {
        state: State::Resolved,
        resolved_at: Some(NOW + 2 * HOUR + 5 * MINUTE),
        ..fired
    };
    let email = notice(&resolved, State::Resolved, NOW);
    assert!(email.text.contains("after 2 h 5 min"));
    assert_eq!(duration(3 * DAY + 4 * HOUR), "3 d 4 h");
    assert_eq!(duration(12 * MINUTE + 5000), "12 min");
}

#[test]
fn a_failed_send_is_tried_on_the_next_pass() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    let errors = pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    assert!(errors[0].contains("unavailable"), "{errors:?}");
    let row = alert(&db, key);
    assert_eq!((row.state, row.sent), (State::Firing, None));

    outbox.down.set(false);
    assert_eq!(
        pass(&db, &outbox, NOW + MINUTE, &[(key, Seen::Unknown)]),
        NONE
    );
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    assert_eq!(alert(&db, key).fired_at, NOW);

    // A resolve that fails goes out later too.
    outbox.down.set(true);
    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    outbox.down.set(false);
    pass(&db, &outbox, NOW + HOUR + MINUTE, &[]);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Resolved: CI launch failed for good"]
    );
}

#[test]
fn a_racing_pass_cannot_send_an_email_another_pass_claimed() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);

    // The other pass claimed the email and is sending it.
    let row = alert(&db, key);
    let other = Kept(db.db());
    let at = NOW + MINUTE;
    assert!(
        db.wait(other.claim(&row, State::Firing, at, at + CLAIM_FOR))
            .unwrap()
    );
    assert_eq!(pass(&db, &outbox, at, &[(key, Seen::Unknown)]), NONE);
    assert_eq!(outbox.take(), NONE);
    assert!(
        !db.wait(other.claim(&row, State::Firing, at, at + CLAIM_FOR))
            .unwrap()
    );

    // It crashed: the claim ends and the next pass sends, once.
    let later = at + CLAIM_FOR;
    assert_eq!(pass(&db, &outbox, later, &[(key, Seen::Unknown)]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    assert_eq!(alert(&db, key).sent, Some(State::Firing));
    assert!(
        !db.wait(other.claim(&row, State::Firing, later, later + CLAIM_FOR))
            .unwrap()
    );
    assert_eq!(pass(&db, &outbox, later + MINUTE, &[]), NONE);
    assert_eq!(outbox.take(), NONE);
}

#[test]
fn a_failed_send_ends_the_claim() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);
    // The retry comes well inside the claim's time.
    assert_eq!(pass(&db, &outbox, NOW + 1, &[]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
}

#[test]
fn a_late_sent_or_release_leaves_another_passs_claim_alone() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);
    let row = alert(&db, key);
    let kept = Kept(db.db());

    // The first pass claims and stalls past its claim; a second pass
    // claims after it ran out.
    let first = NOW + MINUTE;
    let second = first + CLAIM_FOR;
    assert!(
        db.wait(kept.claim(&row, State::Firing, first, first + CLAIM_FOR))
            .unwrap()
    );
    assert!(
        db.wait(kept.claim(&row, State::Firing, second, second + CLAIM_FOR))
            .unwrap()
    );
    // The first pass wakes up and finishes. Neither call touches the
    // second pass's claim.
    db.wait(kept.sent(&row, State::Firing, first + CLAIM_FOR, second))
        .unwrap();
    assert_eq!(alert(&db, key).sent, None);
    db.wait(kept.release(key.as_str(), first + CLAIM_FOR))
        .unwrap();
    assert_eq!(pass(&db, &outbox, second + 1, &[]), NONE);
    assert_eq!(outbox.take(), NONE);

    // Once the second claim ends the email goes out, once.
    assert_eq!(pass(&db, &outbox, second + CLAIM_FOR, &[]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
}

#[test]
fn a_late_sent_for_an_old_incident_leaves_the_new_one_unannounced() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);
    let old = alert(&db, key);
    let kept = Kept(db.db());
    let at = NOW + MINUTE;
    let until = at + CLAIM_FOR;
    assert!(db.wait(kept.claim(&old, State::Firing, at, until)).unwrap());

    // The alert resolved and fired again while that pass was sending.
    let new = Alert {
        detail: "job 6".to_owned(),
        fired_at: NOW + 2 * MINUTE,
        ..old.clone()
    };
    assert!(db.wait(kept.save(Some(&old), &new, at)).unwrap());
    db.wait(kept.sent(&old, State::Firing, until, at)).unwrap();
    assert_eq!(alert(&db, key).sent, None);

    assert_eq!(pass(&db, &outbox, until, &[(key, Seen::Unknown)]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    assert_eq!(alert(&db, key).sent, Some(State::Firing));
}

#[test]
fn a_stale_snapshot_cannot_claim_a_newer_incident() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    let stale = alert(&db, key);
    let kept = Kept(db.db());
    let new = Alert {
        fired_at: NOW + MINUTE,
        ..stale.clone()
    };
    assert!(
        db.wait(kept.save(Some(&stale), &new, NOW + MINUTE))
            .unwrap()
    );
    let at = NOW + 2 * MINUTE;
    assert!(
        !db.wait(kept.claim(&stale, State::Firing, at, at + CLAIM_FOR))
            .unwrap()
    );
    assert!(
        db.wait(kept.claim(&new, State::Firing, at, at + CLAIM_FOR))
            .unwrap()
    );
}

#[test]
fn a_stale_detail_cannot_overwrite_a_newer_one() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    let stale = alert(&db, key);
    pass(&db, &outbox, NOW + MINUTE, &[(key, firing("job 6"))]);
    assert_eq!(alert(&db, key).detail, "job 6");

    let next = Alert {
        detail: "job 7".to_owned(),
        ..stale.clone()
    };
    let kept = Kept(db.db());
    assert!(
        !db.wait(kept.save(Some(&stale), &next, NOW + MINUTE))
            .unwrap()
    );
    assert_eq!(alert(&db, key).detail, "job 6");
}

#[test]
fn a_stale_snapshot_cannot_overwrite_a_newer_row() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    let stale = alert(&db, key);
    assert_eq!(stale.sent, Some(State::Firing));
    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    assert_eq!(alert(&db, key).sent, Some(State::Resolved));

    // A pass that read the row before it resolved has a new detail.
    let kept = Kept(db.db());
    let next = Alert {
        detail: "job 6".to_owned(),
        ..stale.clone()
    };
    assert!(!db.wait(kept.save(Some(&stale), &next, NOW + HOUR)).unwrap());
    // And one that found no row when another pass had inserted it.
    assert!(!db.wait(kept.save(None, &next, NOW + HOUR)).unwrap());
    let row = alert(&db, key);
    assert_eq!(
        (row.state, row.sent),
        (State::Resolved, Some(State::Resolved))
    );
    assert_eq!(row.detail, "job 5");
}

#[test]
fn an_alert_that_fires_again_after_a_failed_resolution_email_is_announced() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::ReconcilerFailing;
    pass(&db, &outbox, NOW, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: CI reconciler failing"]);
    outbox.down.set(true);
    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    let row = alert(&db, key);
    assert_eq!(
        (row.state, row.sent),
        (State::Resolved, Some(State::Firing))
    );

    outbox.down.set(false);
    pass(&db, &outbox, NOW + 2 * HOUR, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: CI reconciler failing"]);
    assert_eq!(alert(&db, key).sent, Some(State::Firing));
}

#[test]
fn an_alert_whose_row_fails_to_save_sends_nothing_that_pass() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);

    // Observed clear, but the stored row still says firing and unsent.
    let store = Unsaved {
        kept: Kept(db.db()),
        key,
        undo_fails: Cell::new(0),
    };
    let errors = db.wait(settle(
        &store,
        outbox.at(NOW + MINUTE),
        NOW + MINUTE,
        async || vec![(key, Seen::Clear)],
        true,
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("database unavailable"), "{errors:?}");
    assert_eq!(outbox.take(), NONE);
    assert_eq!(alert(&db, key).sent, None);
}

#[test]
fn a_failed_save_of_a_new_detail_still_announces_the_alert() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);

    // Still firing with a new detail; the row keeps its state, so the
    // unsent firing email goes out.
    let store = Unsaved {
        kept: Kept(db.db()),
        key,
        undo_fails: Cell::new(0),
    };
    db.wait(settle(
        &store,
        outbox.at(NOW + MINUTE),
        NOW + MINUTE,
        async || vec![(key, firing("job 6"))],
        true,
    ));
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
}

#[test]
fn an_alert_that_resolves_before_its_email_sends_nothing() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::RateLimit;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("installation 42"))]);
    pass(&db, &outbox, NOW + MINUTE, &[(key, Seen::Clear)]);
    outbox.down.set(false);
    assert_eq!(
        pass(&db, &outbox, NOW + 2 * MINUTE, &[(key, Seen::Clear)]),
        NONE
    );
    assert_eq!(outbox.take(), NONE);
}

#[test]
fn firing_alerts_go_out_in_one_digest_a_day() {
    let db = Db::new();
    let outbox = Outbox::default();
    // The first pass starts the day.
    pass(&db, &outbox, NOW, &[]);
    pass(
        &db,
        &outbox,
        NOW + HOUR,
        &[(Key::DatabaseConnections, firing("45 of 50"))],
    );
    pass(
        &db,
        &outbox,
        NOW + HOUR,
        &[(Key::ReconcilerFailing, firing("acme"))],
    );
    assert_eq!(outbox.take().len(), 2);
    pass(&db, &outbox, NOW + 2 * HOUR, &[]);
    assert_eq!(outbox.take(), NONE);

    // A day on, one digest with both; one that fires in the same
    // pass has its own email instead.
    outbox.down.set(true);
    let errors = pass(&db, &outbox, NOW + DAY, &[(Key::RateLimit, firing("42"))]);
    assert_eq!(errors.len(), 2, "{errors:?}");
    outbox.down.set(false);
    pass(&db, &outbox, NOW + DAY + MINUTE, &[]);
    let sent = outbox.sent.borrow_mut().drain(..).collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].subject, "[hatchrun] Firing: CI GitHub rate limit");
    assert_eq!(sent[1].subject, "[hatchrun] Still firing: 2 alerts");
    assert!(
        sent[1]
            .text
            .contains("Database connections near the limit, for 23 h 1 min")
    );
    assert!(sent[1].text.contains("CI reconciler failing"));
    pass(&db, &outbox, NOW + DAY + 2 * MINUTE, &[]);
    assert_eq!(outbox.take(), NONE);

    // The next day's has all three.
    pass(&db, &outbox, NOW + 2 * DAY + MINUTE, &[]);
    assert_eq!(outbox.take(), ["[hatchrun] Still firing: 3 alerts"]);

    // With nothing firing there is no digest.
    for key in [
        Key::DatabaseConnections,
        Key::RateLimit,
        Key::ReconcilerFailing,
    ] {
        pass(&db, &outbox, NOW + 2 * DAY + HOUR, &[(key, Seen::Clear)]);
    }
    assert_eq!(outbox.take().len(), 3);
    pass(&db, &outbox, NOW + 3 * DAY + MINUTE, &[]);
    assert_eq!(outbox.take(), NONE);
}

#[test]
fn healthz_says_when_the_reconciler_stopped_or_fails() {
    let db = Db::new();
    let read = |db: &Db| db.first::<HealthRow>(HEALTH, &[]).unwrap();
    // No pass yet.
    let (status, body) = health(&read(&db), NOW);
    assert_eq!(status, 503);
    assert_eq!(body["status"], "stale");

    let record = "UPDATE ci_reconciler SET finished_at = $1, history = $2 WHERE id = 1";
    let clean = serde_json::to_string(&passes(&[0, 3, 0])).unwrap();
    db.run(record, &params![NOW, clean.as_str()]);
    let (status, body) = health(&read(&db), NOW + 30_000);
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["ageSeconds"], 30);
    assert_eq!(body["lastPass"], "2027-01-15T08:00:00Z");

    let (status, body) = health(&read(&db), NOW + 6 * MINUTE);
    assert_eq!(status, 503);
    assert_eq!(body["status"], "stale");

    let failing = serde_json::to_string(&passes(&[1, 2, 1, 1, 4])).unwrap();
    db.run(record, &params![NOW, failing.as_str()]);
    // Failing passes email; only a stopped cron fails the uptime check.
    let (status, body) = health(&read(&db), NOW + 30_000);
    assert_eq!(status, 200);
    assert_eq!(body["status"], "failing");
    let (status, body) = health(&read(&db), NOW + 6 * MINUTE);
    assert_eq!(status, 503);
    assert_eq!(body["status"], "stale");

    // A history that does not read is not a clear one.
    db.run(record, &params![NOW, "not json"]);
    let (status, body) = health(&read(&db), NOW + 30_000);
    assert_eq!(status, 200);
    assert_eq!(body["status"], "unknown");
}

/// Job 5 of a run in `acme/app`, whose launch failed for good.
fn failed_job(db: &Db) {
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, created_at, updated_at) \
         VALUES ('t1', 1, 'acme', 99, 0, 0)",
        &[],
    );
    db.run(
        "INSERT INTO ci_repos (id, tenant_id, installation_id, full_name, default_branch, \
         private, updated_at) VALUES (7, 't1', 99, 'acme/app', 'main', false, 0)",
        &[],
    );
    db.run(
        "INSERT INTO ci_runs (tenant_id, repo_id, workflow, run_number, delivery, event, ref, \
         sha, actor, status, created_at, updated_at, expires_at, trust, author_login, \
         author_association, gate, gate_status) \
         VALUES ('t1', 7, 'ci.yml', 1, 'd1', 'push', 'refs/heads/main', 'abc', 'me', \
         'completed', 0, 0, 0, 'trusted', 'me', '', 'none', 'none')",
        &[],
    );
    db.run(
        "INSERT INTO ci_jobs (id, run_id, plan_id, name, status, conclusion, timeout_minutes, \
         launch_attempts, launch_error, steps, created_at, updated_at, token_permissions, \
         secret_refs, needs) \
         OVERRIDING SYSTEM VALUE \
         SELECT 5, id, 'build', 'build', 'completed', 'failure', 30, 4, \
         'RunInstances: InsufficientInstanceCapacity', '[]', 0, 0, \
         '{\"metadata\":\"read\"}', '{\"all\":false,\"names\":[]}', '[]' FROM ci_runs",
        &[],
    );
}

#[test]
fn the_failed_delivery_alert_reads_failed_deliveries() {
    let db = Db::new();
    db.run(
        "INSERT INTO ci_deliveries (guid, event, received_at, state, attempts, error, failed_at) \
         VALUES ('old', 'push', 0, 'failed', 3, 'boom', $1), \
         ('new', 'push', 0, 'failed', 50, 'resent too often', $2), \
         ('done', 'push', 0, 'done', 1, NULL, NULL), \
         ('waiting', 'push', 0, 'received', 1, NULL, NULL)",
        &params![NOW - 2 * HOUR, NOW],
    );
    let failed = db.all::<FailedDelivery>(FAILED_DELIVERIES_SINCE, &params![NOW - HOUR, LISTED]);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].guid, "new");
    assert_eq!(failed[0].attempts, 50);
    assert_eq!(failed[0].error.as_deref(), Some("resent too often"));
}

#[test]
fn the_dead_letter_queue_records_each_launch_once() {
    let db = Db::new();
    failed_job(&db);
    let record = |message: &str, job: Option<u64>, at: u64| {
        db.run(RECORD_FAILED, &params![message, job, at])
    };
    assert_eq!(record("m1", Some(5), NOW), 1);
    // The queue may deliver it again.
    assert_eq!(record("m1", Some(5), NOW + 1), 0);
    // A job that is gone.
    assert_eq!(record("m2", Some(6), NOW + 2), 1);

    let failed = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW - HOUR, LISTED]);
    assert_eq!(failed.len(), 2);
    assert_eq!(
        failed[1].error.as_deref(),
        Some("RunInstances: InsufficientInstanceCapacity")
    );
    assert_eq!(failed[1].attempts, Some(4));
    assert_eq!(failed[1].job_name.as_deref(), Some("build"));
    assert_eq!(failed[0].job_name, None);
    let seen = failed_launches(&failed);
    assert!(matches!(&seen, Seen::Firing(detail) if detail.contains("job 6 (?), ? attempts")));

    let outbox = Outbox::default();
    let fire = |now| {
        db.wait(settle(
            &Kept(db.db()),
            outbox.at(now),
            now,
            async || vec![(Key::LaunchFailed, seen.clone())],
            false,
        ))
    };
    assert_eq!(fire(NOW), NONE);
    assert_eq!(fire(NOW + 5), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    // The consumer sends no digest.
    assert_eq!(db.wait(Kept(db.db()).digest_at()), Ok(None));

    // An hour on, the reconciler finds none and resolves it.
    let recent = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW + 2, LISTED]);
    assert_eq!(recent.len(), 1);
    let none = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW + HOUR, LISTED]);
    assert_eq!(failed_launches(&none), Seen::Clear);

    assert_eq!(db.run(DELETE_FAILED, &params![NOW + 1, 1000u32]), 1);
}

#[test]
fn low_rate_limits_are_kept_per_installation() {
    let db = Db::new();
    let note = |installation: u64, remaining: Option<u32>, limited: bool, at: u64| {
        db.run(
            NOTE_RATE_LIMIT,
            &params![installation, remaining, limited, at],
        )
    };
    note(42, Some(1500), false, NOW - 20 * MINUTE);
    note(42, Some(0), true, NOW - MINUTE);
    note(43, Some(1900), false, NOW - 20 * MINUTE);
    let low = db.all::<LowRateLimit>(LOW_SINCE, &params![NOW - RATE_LIMITS_FOR, LISTED]);
    assert_eq!(
        low,
        [LowRateLimit {
            installation_id: 42,
            remaining: Some(0),
            limited: true,
        }]
    );
    assert_eq!(db.run(DELETE_RATE_LIMITS, &params![NOW - 10 * MINUTE]), 1);
}

#[test]
fn a_late_rate_limit_note_keeps_the_newer_row() {
    let db = Db::new();
    let note = |remaining: Option<u32>, limited: bool, at: u64| {
        db.run(NOTE_RATE_LIMIT, &params![42u64, remaining, limited, at])
    };
    note(Some(0), true, NOW);
    // A pass that read the answer earlier writes after the newer one.
    note(Some(1500), false, NOW - MINUTE);
    let low = db.all::<LowRateLimit>(LOW_SINCE, &params![NOW - RATE_LIMITS_FOR, LISTED]);
    assert_eq!(
        low,
        [LowRateLimit {
            installation_id: 42,
            remaining: Some(0),
            limited: true,
        }]
    );
    // A later one replaces it.
    note(Some(1900), false, NOW + MINUTE);
    let low = db.all::<LowRateLimit>(LOW_SINCE, &params![NOW - RATE_LIMITS_FOR, LISTED]);
    assert_eq!(low[0].remaining, Some(1900));
    assert!(!low[0].limited);
}

#[test]
fn answers_through_counted_reach_the_rate_limit_rows() {
    use hatchrun_cloud::fetch::Counted;
    use hatchrun_cloud::http::Response;

    let db = Db::new();
    let answer = |status: u16, body: &str| Response {
        status,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    };
    let calls = Counted::default();
    // 1999 is low, 2000 is not; a refusal is low whatever it left.
    calls.note(1, &answer(200, "{}"), Some("1999"));
    calls.note(2, &answer(200, "{}"), Some("2000"));
    calls.note(3, &answer(403, "API rate limit exceeded"), None);
    // A later, higher answer does not raise the lowest one seen.
    calls.note(1, &answer(200, "{}"), Some("2500"));
    db.wait(note_rate_limits(db.db(), &calls, NOW));
    let mut low = db.all::<LowRateLimit>(LOW_SINCE, &params![NOW - RATE_LIMITS_FOR, LISTED]);
    low.sort_by_key(|limit| limit.installation_id);
    assert_eq!(
        low,
        [
            LowRateLimit {
                installation_id: 1,
                remaining: Some(1999),
                limited: false,
            },
            LowRateLimit {
                installation_id: 3,
                remaining: None,
                limited: true,
            },
        ]
    );
}

#[test]
fn alert_queries_read_through_indexes() {
    let db = Db::new();
    for (name, sql, index) in [
        ("FAILED_SINCE", FAILED_SINCE, "ci_failed_launches_age"),
        ("DELETE_FAILED", DELETE_FAILED, "ci_failed_launches_age"),
        ("LOW_SINCE", LOW_SINCE, "ci_rate_limits_seen"),
    ] {
        let params = if sql.contains("$2") {
            params![NOW, LISTED]
        } else {
            params![NOW]
        };
        let plan = db.plan(sql, &params);
        assert!(plan.contains(index), "{name} without {index}\n{plan}");
    }
}

#[test]
fn the_database_reads_its_connections_and_size() {
    let db = Db::new();
    let database: Database = db.first(DATABASE, &[]).unwrap();
    assert!(database.connections >= 1, "{database:?}");
    assert!(
        database.max_connections > database.connections,
        "{database:?}"
    );
    assert!(database.bytes > 0, "{database:?}");
}

#[test]
fn a_claim_starts_when_it_is_made() {
    let db = Db::new();
    let outbox = Outbox::default();
    let (first, second) = (Key::LaunchFailed, Key::ReconcilerFailing);
    // Each send takes three minutes and never finishes, so both claims
    // stay on their rows.
    outbox.lag.set(3 * MINUTE);
    outbox.stall.set(true);
    let errors = pass(
        &db,
        &outbox,
        NOW,
        &[(first, firing("job 5")), (second, firing("job 6"))],
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    let mut until = [claimed_until(&db, first), claimed_until(&db, second)];
    until.sort();
    assert_eq!(
        until,
        [Some(NOW + CLAIM_FOR), Some(NOW + 3 * MINUTE + CLAIM_FOR)]
    );
}

#[test]
fn a_firing_email_in_flight_when_the_alert_resolves_is_followed_by_the_resolution() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.down.set(true);
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    outbox.down.set(false);
    let row = alert(&db, key);
    let kept = Kept(db.db());
    let at = NOW + MINUTE;

    // The firing email waits in flight while the check saves a clear
    // observation, then goes out.
    outbox.hold.set(true);
    let resolve = async {
        let start = std::time::Instant::now();
        while !outbox.entered.get() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(30),
                "the firing send never started"
            );
            tokio::task::yield_now().await;
        }
        let resolved = Alert {
            state: State::Resolved,
            resolved_at: Some(at),
            ..row.clone()
        };
        assert!(kept.save(Some(&row), &resolved, at).await.unwrap());
        outbox.hold.set(false);
    };
    let (errors, ()) = db.wait(futures_util::future::join(
        settle(&kept, outbox.at(at), at, async || Vec::new(), true),
        resolve,
    ));
    assert_eq!(errors, NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    let stored = alert(&db, key);
    assert_eq!(
        (stored.state, stored.sent),
        (State::Resolved, Some(State::Firing))
    );

    assert_eq!(pass(&db, &outbox, at + 1, &[]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Resolved: CI launch failed for good"]
    );
}

#[test]
fn a_claim_too_close_to_its_end_is_given_back_unsent() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    // Reading the clock after the claim finds it just too late to send
    // and record the email inside the claim.
    outbox.step.set(CLAIM_FOR - SEND_FOR - RECORD_FOR + 1);
    let errors = pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("too close"), "{errors:?}");
    assert_eq!(outbox.take(), NONE);
    assert_eq!(claimed_until(&db, key), None);

    outbox.step.set(0);
    assert_eq!(pass(&db, &outbox, NOW + MINUTE, &[]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
}

#[test]
fn the_digest_keeps_its_own_clock_for_an_alert_that_just_fired() {
    let db = Db::new();
    let outbox = Outbox::default();
    pass(&db, &outbox, NOW, &[]);
    pass(
        &db,
        &outbox,
        NOW + DAY - MINUTE,
        &[(Key::ReconcilerFailing, firing("acme"))],
    );
    assert_eq!(outbox.take().len(), 1);
    // The day ends a minute later: the digest lists it.
    pass(&db, &outbox, NOW + DAY, &[]);
    assert_eq!(outbox.take(), ["[hatchrun] Still firing: 1 alerts"]);
}

#[test]
fn the_failed_launch_detail_says_the_latest_only_when_there_are_more() {
    let launch = |id: u64| FailedLaunch {
        job_id: Some(id),
        job_name: None,
        error: None,
        attempts: None,
        failed_at: NOW,
    };
    let detail = |count: u64| match failed_launches(&(0..count).map(launch).collect::<Vec<_>>()) {
        Seen::Firing(detail) => detail,
        other => panic!("{other:?}"),
    };
    let listed = u64::from(LISTED);
    let exact = detail(listed);
    assert!(!exact.contains("the latest of them"));
    assert_eq!(exact.matches("\njob ").count(), LISTED as usize);
    let more = detail(listed + 1);
    assert!(more.contains("the latest of them"));
    assert_eq!(more.matches("\njob ").count(), LISTED as usize);
}

#[test]
fn two_passes_send_one_digest() {
    let db = Db::new();
    let outbox = Outbox::default();
    pass(&db, &outbox, NOW, &[]);
    pass(
        &db,
        &outbox,
        NOW + HOUR,
        &[(Key::ReconcilerFailing, firing("acme"))],
    );
    assert_eq!(outbox.take().len(), 1);

    // Both passes read the same last digest before either claims.
    let kept = Kept(db.db());
    let firing = [alert(&db, Key::ReconcilerFailing)];
    let at = NOW + DAY;
    let results = db.wait(futures_util::future::join(
        send_digest(&kept, outbox.at(at), at, &firing),
        send_digest(&kept, outbox.at(at), at, &firing),
    ));
    assert_eq!(results, (Ok(()), Ok(())));
    assert_eq!(outbox.take(), ["[hatchrun] Still firing: 1 alerts"]);
}

#[test]
fn a_new_incident_never_reuses_the_time_the_last_one_fired() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    let old = alert(&db, key);
    pass(&db, &outbox, NOW, &[(key, Seen::Clear)]);
    pass(&db, &outbox, NOW, &[(key, firing("job 6"))]);
    let new = alert(&db, key);
    assert_eq!((old.fired_at, new.fired_at), (NOW, NOW + 1));
    outbox.take();

    // A claim on the old incident's snapshot finds the new incident.
    let kept = Kept(db.db());
    let at = NOW + MINUTE;
    assert!(
        !db.wait(kept.claim(&old, State::Firing, at, at + CLAIM_FOR))
            .unwrap()
    );
}

#[test]
fn a_send_that_outlasts_its_time_keeps_the_claim() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    outbox.stall.set(true);
    let errors = pass(&db, &outbox, NOW, &[(key, firing("job 5"))]);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("no answer"), "{errors:?}");
    assert_eq!(outbox.bound.get(), Some(SEND_FOR));
    assert_eq!(claimed_until(&db, key), Some(NOW + CLAIM_FOR));
    assert_eq!(alert(&db, key).sent, None);

    // Another pass inside the claim sends nothing, the stalled one
    // included.
    outbox.stall.set(false);
    assert_eq!(pass(&db, &outbox, NOW + MINUTE, &[]), NONE);
    assert_eq!(outbox.take(), NONE);

    // After the claim ran out the email goes out.
    assert_eq!(pass(&db, &outbox, NOW + CLAIM_FOR, &[]), NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
    assert_eq!(alert(&db, key).sent, Some(State::Firing));
}

#[test]
fn a_failed_digest_send_reports_the_mail_error_and_undoes_its_claim() {
    let db = Db::new();
    let outbox = Outbox::default();
    pass(&db, &outbox, NOW, &[]);
    pass(
        &db,
        &outbox,
        NOW + HOUR,
        &[(Key::ReconcilerFailing, firing("acme"))],
    );
    outbox.take();
    let firing = [alert(&db, Key::ReconcilerFailing)];
    let store = Unsaved {
        kept: Kept(db.db()),
        key: Key::RateLimit,
        undo_fails: Cell::new(1),
    };
    let digest_at = || db.wait(Kept(db.db()).digest_at()).unwrap();

    // The undo works on its second try.
    outbox.down.set(true);
    let at = NOW + DAY;
    let err = db
        .wait(send_digest(&store, outbox.at(at), at, &firing))
        .unwrap_err();
    assert_eq!(err, "Email Service unavailable");
    assert_eq!(digest_at(), Some(NOW));

    // Both fail: the claim stays and the error says so.
    store.undo_fails.set(2);
    let at = NOW + 2 * DAY;
    let err = db
        .wait(send_digest(&store, outbox.at(at), at, &firing))
        .unwrap_err();
    assert!(err.starts_with("Email Service unavailable;"), "{err}");
    assert!(err.contains("database unavailable"), "{err}");
    assert_eq!(digest_at(), Some(at));
}

#[test]
fn a_digest_waits_for_the_pass_that_reads_every_firing_alert() {
    let db = Db::new();
    let outbox = Outbox::default();
    let (stale, other) = (Key::LaunchFailed, Key::ReconcilerFailing);
    pass(&db, &outbox, NOW, &[]);
    pass(
        &db,
        &outbox,
        NOW + HOUR,
        &[(stale, firing("job 5")), (other, firing("acme"))],
    );
    assert_eq!(outbox.take().len(), 2);

    // One alert's row is stale after a lost save, so the digest is held
    // back and the day stays unclaimed.
    let store = Unsaved {
        kept: Kept(db.db()),
        key: stale,
        undo_fails: Cell::new(0),
    };
    let at = NOW + DAY;
    let errors = db.wait(settle(
        &store,
        outbox.at(at),
        at,
        async || vec![(stale, Seen::Clear)],
        true,
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(outbox.take(), NONE);
    assert_eq!(db.wait(Kept(db.db()).digest_at()), Ok(Some(NOW)));

    // The next pass sends it with the stored rows.
    assert_eq!(
        pass(&db, &outbox, at + MINUTE, &[(stale, Seen::Clear)]),
        NONE
    );
    assert_eq!(
        outbox.take(),
        [
            "[hatchrun] Resolved: CI launch failed for good",
            "[hatchrun] Still firing: 1 alerts"
        ]
    );
}

#[test]
fn a_check_read_after_another_pass_fired_does_not_undo_it() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::LaunchFailed;
    let kept = Kept(db.db());
    // Another pass fires the alert while this one's check runs. The
    // check read its data before that, so it finds nothing wrong.
    let fire = async || {
        let fired = Alert {
            key: key.as_str().to_owned(),
            state: State::Firing,
            detail: "job 5".to_owned(),
            fired_at: NOW,
            resolved_at: None,
            sent: None,
        };
        assert!(kept.save(None, &fired, NOW).await.unwrap());
        vec![(key, Seen::Clear)]
    };
    let errors = db.wait(settle(&kept, outbox.at(NOW), NOW, fire, false));
    assert_eq!(errors, NONE);
    assert_eq!(alert(&db, key).state, State::Firing);
}

#[test]
fn overlapping_passes_keep_each_others_records() {
    let db = Db::new();
    let at = |n: u64| NOW + n * MINUTE;
    // Both passes finish without having read the other's record.
    let (a, b) = db.wait(futures_util::future::join(
        runtime::record_pass(db.db(), at(0), at(0) + 5000, 0),
        runtime::record_pass(db.db(), at(1), at(1) + 5000, 1),
    ));
    a.unwrap();
    b.unwrap();
    let row = db.first::<HealthRow>(HEALTH, &[]).unwrap();
    assert_eq!(parse_history(row.history.as_deref()).unwrap().len(), 2);
    // The history stays at its limit, newest last.
    for n in 2..15 {
        db.wait(runtime::record_pass(db.db(), at(n), at(n) + 5000, 0))
            .unwrap();
    }
    let row = db.first::<HealthRow>(HEALTH, &[]).unwrap();
    let kept = parse_history(row.history.as_deref()).unwrap();
    assert_eq!(kept.len(), HISTORY);
    assert_eq!(kept.last().map(|pass| pass.at), Some(at(14)));
}

#[test]
fn a_pass_is_added_to_the_history_and_stamps_its_end() {
    let db = Db::new();
    let first = db
        .wait(runtime::record_pass(db.db(), NOW, NOW + 5000, 2))
        .unwrap();
    assert_eq!(first, passes_at(&[(NOW, 2)]));
    let second = db
        .wait(runtime::record_pass(
            db.db(),
            NOW + MINUTE,
            NOW + MINUTE + 5000,
            0,
        ))
        .unwrap();
    assert_eq!(second, passes_at(&[(NOW, 2), (NOW + MINUTE, 0)]));

    let row = db.first::<HealthRow>(HEALTH, &[]).unwrap();
    assert_eq!(row.finished_at, Some(NOW + MINUTE + 5000));
    assert_eq!(parse_history(row.history.as_deref()), Some(second));
}

fn passes_at(passes: &[(u64, u32)]) -> Vec<PassRecord> {
    passes
        .iter()
        .map(|&(at, errors)| PassRecord { at, errors })
        .collect()
}

#[test]
fn old_failed_launches_and_rate_limits_are_pruned() {
    let db = Db::new();
    db.run(RECORD_FAILED, &params!["old", Some(1u64), NOW - 8 * DAY]);
    db.run(RECORD_FAILED, &params!["new", Some(2u64), NOW - DAY]);
    db.run(
        NOTE_RATE_LIMIT,
        &params![42u64, Some(0u32), true, NOW - 2 * DAY],
    );
    db.run(
        NOTE_RATE_LIMIT,
        &params![43u64, Some(0u32), true, NOW - MINUTE],
    );
    db.wait(runtime::prune(db.db(), NOW)).unwrap();
    let failed = db.all::<FailedLaunch>(FAILED_SINCE, &params![0u64, LISTED]);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].job_id, Some(2));
    let low = db.all::<LowRateLimit>(LOW_SINCE, &params![0u64, LISTED]);
    assert_eq!(low.len(), 1);
    assert_eq!(low[0].installation_id, 43);
}

#[test]
fn an_unreadable_dead_letter_is_recorded_and_fires_the_alert() {
    let db = Db::new();
    let long = "x".repeat(2 * BODY_KEPT);
    let error = unreadable("missing field `job_id`", &long);
    assert!(error.starts_with("unreadable message (missing field `job_id`): xxx"));
    assert!(error.ends_with("..."));
    assert_eq!(
        error.len(),
        "unreadable message (missing field `job_id`): ".len() + BODY_KEPT + 3
    );
    assert_eq!(unreadable("bad", "{}"), "unreadable message (bad): {}");

    let record =
        |message: &str, at: u64| db.run(RECORD_UNREADABLE, &params![message, error.as_str(), at]);
    assert_eq!(record("m1", NOW), 1);
    assert_eq!(record("m1", NOW + 1), 0);

    let failed = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW - HOUR, LISTED]);
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].job_id, failed[0].attempts), (None, None));
    let seen = failed_launches(&failed);
    assert!(matches!(&seen, Seen::Firing(detail) if detail.contains("unreadable message")));

    let outbox = Outbox::default();
    let errors = db.wait(settle(
        &Kept(db.db()),
        outbox.at(NOW),
        NOW,
        async || vec![(Key::LaunchFailed, seen)],
        false,
    ));
    assert_eq!(errors, NONE);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Firing: CI launch failed for good"]
    );
}

/// A store whose stored alerts cannot be read.
struct Unreadable;

#[allow(clippy::unused_async_trait_impl)]
impl Store for Unreadable {
    async fn alerts(&self) -> Result<Vec<Alert>, String> {
        Err("database unavailable".to_owned())
    }

    async fn save(&self, _: Option<&Alert>, _: &Alert, _: u64) -> Result<bool, String> {
        unreachable!()
    }

    async fn claim(&self, _: &Alert, _: State, _: u64, _: u64) -> Result<bool, String> {
        unreachable!()
    }

    async fn sent(&self, _: &Alert, _: State, _: u64, _: u64) -> Result<bool, String> {
        unreachable!()
    }

    async fn release(&self, _: &str, _: u64) -> Result<(), String> {
        unreachable!()
    }

    async fn digest_at(&self) -> Result<Option<u64>, String> {
        unreachable!()
    }

    async fn digest_claim(&self, _: Option<u64>, _: u64) -> Result<bool, String> {
        unreachable!()
    }

    async fn digest_undo(&self, _: Option<u64>, _: u64) -> Result<(), String> {
        unreachable!()
    }
}

#[test]
fn a_pass_is_recorded_once_whether_or_not_the_checks_run() {
    let db = Db::new();
    let outbox = Outbox::default();
    let order = RefCell::new(Vec::new());
    let record = async || order.borrow_mut().push("record");
    let read = async |()| {
        order.borrow_mut().push("read");
        Vec::new()
    };

    // The checks run after the pass is recorded.
    let errors = db.wait(settle_pass(
        &Kept(db.db()),
        outbox.at(NOW),
        NOW,
        record,
        read,
    ));
    assert_eq!(errors, NONE);
    assert_eq!(*order.borrow(), ["record", "read"]);

    // The stored alerts cannot be read, so the checks are skipped and the
    // pass is still recorded.
    order.borrow_mut().clear();
    let errors = db.wait(settle_pass(&Unreadable, outbox.at(NOW), NOW, record, read));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(*order.borrow(), ["record"]);
}
