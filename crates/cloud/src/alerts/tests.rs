// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use std::cell::{Cell, RefCell};

use super::checks::{Database, Facts, FailedLaunch, LowRateLimit, Waiting, check, failed_launches};
use super::queries::{NOTE_RATE_LIMIT, RECORD_FAILED};
use super::runtime::Kept;
use super::*;
use crate::db::params;
use crate::testdb::Db;

const NOW: u64 = 1_800_000_000_000;
const HOUR: u64 = 60 * MINUTE;
const NONE: [&str; 0] = [];

/// Collects emails, or fails them while `down`.
#[derive(Default)]
struct Outbox {
    sent: RefCell<Vec<Email>>,
    down: Cell<bool>,
}

#[allow(clippy::unused_async_trait_impl)]
impl Mail for Outbox {
    async fn send(&self, email: &Email) -> Result<(), String> {
        if self.down.get() {
            return Err("Email Service unavailable".to_owned());
        }
        self.sent.borrow_mut().push(email.clone());
        Ok(())
    }
}

impl Outbox {
    /// The subjects sent since the last call.
    fn take(&self) -> Vec<String> {
        self.sent
            .borrow_mut()
            .drain(..)
            .map(|email| email.subject)
            .collect()
    }
}

/// A store that fails to save the row of one alert.
struct Unsaved<'a> {
    kept: Kept<'a>,
    key: Key,
}

impl Store for Unsaved<'_> {
    async fn alerts(&self) -> Result<Vec<Alert>, String> {
        self.kept.alerts().await
    }

    async fn save(&self, alert: &Alert, now: u64) -> Result<(), String> {
        if alert.key == self.key.as_str() {
            return Err("database unavailable".to_owned());
        }
        self.kept.save(alert, now).await
    }

    async fn digest_at(&self) -> Result<Option<u64>, String> {
        self.kept.digest_at().await
    }

    async fn digested(&self, at: u64) -> Result<(), String> {
        self.kept.digested(at).await
    }
}

fn pass(db: &Db, outbox: &Outbox, now: u64, seen: &[(Key, Seen)]) -> Vec<String> {
    db.wait(settle(&Kept(db.db()), outbox, now, seen, true))
}

fn firing(detail: &str) -> Seen {
    Seen::Firing(detail.to_owned())
}

fn found(seen: &[(Key, Seen)], key: Key) -> &Seen {
    &seen.iter().find(|(at, _)| *at == key).unwrap().1
}

fn passes(errors: &[u32], more: bool) -> Vec<PassRecord> {
    errors
        .iter()
        .enumerate()
        .map(|(at, errors)| PassRecord {
            at: at as u64,
            errors: *errors,
            more,
        })
        .collect()
}

fn alert(db: &Db, key: Key) -> Alert {
    db.first(
        "SELECT key, state, detail, fired_at, resolved_at, sent FROM alerts WHERE key = $1",
        &params![key.as_str()],
    )
    .unwrap()
}

#[test]
fn the_history_keeps_the_last_passes() {
    let mut history = Vec::new();
    for at in 0..15 {
        history = remember(
            history,
            PassRecord {
                at,
                errors: 0,
                more: false,
            },
        );
    }
    assert_eq!(history.len(), HISTORY);
    assert_eq!(history[0].at, 5);
    assert_eq!(history[HISTORY - 1].at, 14);
    // As the reconciler row keeps it.
    let text = serde_json::to_string(&history).unwrap();
    assert_eq!(super::history(Some(&text)), history);
    assert_eq!(super::history(Some("not json")), []);
    assert_eq!(super::history(None), []);
}

#[test]
fn checks_read_the_pass_history() {
    let facts = |history: Vec<PassRecord>| Facts {
        history: Some(history),
        ..Facts::default()
    };
    let seen = check(&facts(passes(&[0, 2, 1, 3, 1, 1], false)));
    assert!(matches!(
        found(&seen, Key::ReconcilerFailing),
        Seen::Firing(detail) if detail.contains("2, 1, 3, 1, 1")
    ));
    assert_eq!(*found(&seen, Key::ReconcilerBusy), Seen::Clear);
    // One clean pass among the last five clears it.
    let seen = check(&facts(passes(&[1, 1, 1, 0, 1, 1], false)));
    assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);
    // Too few passes to tell.
    let seen = check(&facts(passes(&[1, 1, 1, 1], false)));
    assert_eq!(*found(&seen, Key::ReconcilerFailing), Seen::Clear);

    let seen = check(&facts(passes(&[0; 10], true)));
    assert!(
        matches!(found(&seen, Key::ReconcilerBusy), Seen::Firing(detail) if detail.contains("#105"))
    );
    let seen = check(&facts(passes(&[0; 9], true)));
    assert_eq!(*found(&seen, Key::ReconcilerBusy), Seen::Clear);

    // Data that could not be read changes nothing.
    let seen = check(&Facts::default());
    assert!(seen.iter().all(|(_, seen)| *seen == Seen::Unknown));
    assert_eq!(seen.len(), 6);
}

#[test]
fn checks_read_the_database_and_github() {
    let facts = Facts {
        database: Some(Database {
            connections: 41,
            max_connections: 50,
            bytes: 5_400_000_000,
        }),
        waiting: Some(vec![Waiting {
            org: "acme".to_owned(),
            jobs: 3,
            oldest: NOW - 20 * MINUTE,
        }]),
        failed: Some(vec![FailedLaunch {
            job_id: Some(5),
            installation_id: Some(42),
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
    assert!(
        matches!(found(&seen, Key::JobsWaiting), Seen::Firing(detail) if detail.contains("acme: 3 jobs"))
    );
    assert!(matches!(
        found(&seen, Key::LaunchFailed),
        Seen::Firing(detail) if detail.contains("job 5 of installation 42, 4 attempts")
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
        waiting: Some(Vec::new()),
        failed: Some(Vec::new()),
        rate_limits: Some(Vec::new()),
        ..Facts::default()
    });
    for key in [
        Key::DatabaseConnections,
        Key::JobsWaiting,
        Key::LaunchFailed,
        Key::RateLimit,
    ] {
        assert_eq!(*found(&clear, key), Seen::Clear, "{key:?}");
    }
}

#[test]
fn an_alert_emails_once_when_it_fires_and_once_when_it_resolves() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::ReconcilerFailing;
    assert_eq!(pass(&db, &outbox, NOW, &[(key, firing("errors"))]), NONE);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
    // Still firing: no email, the detail follows the last check.
    pass(&db, &outbox, NOW + MINUTE, &[(key, firing("more errors"))]);
    pass(&db, &outbox, NOW + 2 * MINUTE, &[(key, Seen::Unknown)]);
    assert_eq!(outbox.take(), NONE);
    let row = alert(&db, key);
    assert_eq!(row.detail, "more errors");
    assert_eq!(row.fired_at, NOW);

    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    assert_eq!(outbox.take(), ["[hatchrun] Resolved: Reconciler failing"]);
    pass(&db, &outbox, NOW + HOUR + MINUTE, &[(key, Seen::Clear)]);
    assert_eq!(outbox.take(), NONE);
    let row = alert(&db, key);
    assert_eq!(row.state, State::Resolved);
    assert_eq!(row.resolved_at, Some(NOW + HOUR));
    assert_eq!(row.sent, Some(State::Resolved));

    // It fires again, from the new time.
    pass(&db, &outbox, NOW + 2 * HOUR, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
    assert_eq!(alert(&db, key).fired_at, NOW + 2 * HOUR);
}

#[test]
fn the_emails_say_what_and_since_when() {
    let fired = Alert {
        key: Key::JobsWaiting.as_str().to_owned(),
        state: State::Firing,
        detail: "acme: 3 jobs".to_owned(),
        fired_at: NOW,
        resolved_at: None,
        sent: None,
    };
    let email = notice(&fired, State::Firing, NOW);
    assert_eq!(
        email.subject,
        "[hatchrun] Firing: Jobs waiting for a runner"
    );
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
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Launch failed for good"]);
    assert_eq!(alert(&db, key).fired_at, NOW);

    // A resolve that fails goes out later too.
    outbox.down.set(true);
    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    outbox.down.set(false);
    pass(&db, &outbox, NOW + HOUR + MINUTE, &[]);
    assert_eq!(
        outbox.take(),
        ["[hatchrun] Resolved: Launch failed for good"]
    );
}

#[test]
fn an_alert_that_fires_again_after_a_failed_resolution_email_is_announced() {
    let db = Db::new();
    let outbox = Outbox::default();
    let key = Key::ReconcilerFailing;
    pass(&db, &outbox, NOW, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
    outbox.down.set(true);
    pass(&db, &outbox, NOW + HOUR, &[(key, Seen::Clear)]);
    let row = alert(&db, key);
    assert_eq!(
        (row.state, row.sent),
        (State::Resolved, Some(State::Firing))
    );

    outbox.down.set(false);
    pass(&db, &outbox, NOW + 2 * HOUR, &[(key, firing("errors"))]);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Reconciler failing"]);
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
    };
    let errors = db.wait(settle(
        &store,
        &outbox,
        NOW + MINUTE,
        &[(key, Seen::Clear)],
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
    };
    db.wait(settle(
        &store,
        &outbox,
        NOW + MINUTE,
        &[(key, firing("job 6"))],
        true,
    ));
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Launch failed for good"]);
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
        &[(Key::JobsWaiting, firing("acme"))],
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
    assert_eq!(sent[0].subject, "[hatchrun] Firing: GitHub rate limit");
    assert_eq!(sent[1].subject, "[hatchrun] Still firing: 2 alerts");
    assert!(
        sent[1]
            .text
            .contains("Database connections near the limit, for 23 h 1 min")
    );
    assert!(sent[1].text.contains("Jobs waiting for a runner"));
    pass(&db, &outbox, NOW + DAY + 2 * MINUTE, &[]);
    assert_eq!(outbox.take(), NONE);

    // The next day's has all three.
    pass(&db, &outbox, NOW + 2 * DAY + MINUTE, &[]);
    assert_eq!(outbox.take(), ["[hatchrun] Still firing: 3 alerts"]);

    // With nothing firing there is no digest.
    for key in [Key::DatabaseConnections, Key::RateLimit, Key::JobsWaiting] {
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

    let record = "UPDATE reconciler SET finished_at = $1, history = $2 WHERE id = 1";
    let clean = serde_json::to_string(&passes(&[0, 3, 0], false)).unwrap();
    db.run(record, &params![NOW, clean.as_str()]);
    let (status, body) = health(&read(&db), NOW + 30_000);
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["ageSeconds"], 30);
    assert_eq!(body["lastPass"], "2027-01-15T08:00:00Z");

    let (status, body) = health(&read(&db), NOW + 6 * MINUTE);
    assert_eq!(status, 503);
    assert_eq!(body["status"], "stale");

    let failing = serde_json::to_string(&passes(&[1, 2, 1, 1, 4], false)).unwrap();
    db.run(record, &params![NOW, failing.as_str()]);
    let (status, body) = health(&read(&db), NOW + 30_000);
    assert_eq!(status, 503);
    assert_eq!(body["status"], "failing");
}

fn job(db: &Db, id: u64, installation_id: u64, queued_at: u64, launch: Option<&str>) {
    db.run(
        "INSERT INTO jobs (id, installation_id, repository, run_id, label, status, \
         queued_at, updated_at, launch_state) \
         VALUES ($1, $2, 'acme/app', 7, 'hatchrun', 'queued', $3, $3, $4)",
        &params![id, installation_id, queued_at, launch],
    );
}

fn tenant(db: &Db, org: &str, installation_id: u64, verified: bool) {
    db.run(
        "INSERT INTO tenants (id, org_id, org_login, installation_id, role_arn, region, \
         launch_template_id, subnet_ids, max_runners, created_at, updated_at) \
         VALUES ($1, $2, $1, $2, $3, 'eu-west-1', 'lt-1', 'subnet-1', 2, $4, $4)",
        &params![
            org,
            installation_id,
            verified.then_some("arn:aws:iam::1:role/r"),
            NOW
        ],
    );
}

#[test]
fn jobs_wait_when_their_tenant_has_room_and_nothing_launches() {
    let db = Db::new();
    let old = NOW - 10 * MINUTE;
    tenant(&db, "acme", 42, true);
    job(&db, 1, 42, old, None);
    job(&db, 2, 42, old + 1000, None);
    job(&db, 3, 42, NOW - MINUTE, None);
    job(&db, 4, 42, old, Some("launching"));
    // A backoff counts; a job waiting for its owner does not.
    job(&db, 5, 42, old, Some("failed"));
    job(&db, 6, 42, old, Some("failed"));
    db.run("UPDATE jobs SET launch_retry = 'backoff' WHERE id = 5", &[]);
    db.run("UPDATE jobs SET launch_retry = 'setup' WHERE id = 6", &[]);
    tenant(&db, "unverified", 43, false);
    job(&db, 7, 43, old, None);

    let waiting = |db: &Db| db.all::<Waiting>(&waiting_query(), &params![NOW - WAITING, LISTED]);
    assert_eq!(
        waiting(&db),
        [Waiting {
            org: "acme".to_owned(),
            jobs: 3,
            oldest: old,
        }]
    );
    // At max_runners, jobs wait for a runner to finish.
    job(&db, 8, 42, old, Some("launched"));
    assert_eq!(waiting(&db), []);

    let plan = db.plan(&waiting_query(), &params![NOW - WAITING, LISTED]);
    assert!(!plan.contains("Seq Scan on jobs"), "{plan}");
    assert!(plan.contains("jobs_open"), "{plan}");
}

#[test]
fn the_dead_letter_queue_records_each_launch_once() {
    let db = Db::new();
    job(&db, 5, 42, NOW - HOUR, Some("failed"));
    db.run(
        "UPDATE jobs SET launch_error = 'RunInstances: InsufficientInstanceCapacity', \
         launch_attempts = 4 WHERE id = 5",
        &[],
    );
    let record = |message: &str, job: Option<u64>, installation: Option<u64>, at: u64| {
        db.run(RECORD_FAILED, &params![message, job, installation, at])
    };
    assert_eq!(record("m1", Some(5), Some(42), NOW), 1);
    // The queue may deliver it again.
    assert_eq!(record("m1", Some(5), Some(42), NOW + 1), 0);
    // A body that did not read.
    assert_eq!(record("m2", None, None, NOW + 2), 1);

    let failed = db.all::<FailedLaunch>(FAILED_SINCE, &params![NOW - HOUR, LISTED]);
    assert_eq!(failed.len(), 2);
    assert_eq!(
        failed[1].error.as_deref(),
        Some("RunInstances: InsufficientInstanceCapacity")
    );
    assert_eq!(failed[1].attempts, Some(4));
    assert_eq!(failed[0].job_id, None);
    let seen = failed_launches(&failed);
    assert!(matches!(&seen, Seen::Firing(detail) if detail.contains("job ? of installation ?")));

    let outbox = Outbox::default();
    let fire = |now| {
        db.wait(settle(
            &Kept(db.db()),
            &outbox,
            now,
            &[(Key::LaunchFailed, seen.clone())],
            false,
        ))
    };
    assert_eq!(fire(NOW), NONE);
    assert_eq!(fire(NOW + 5), NONE);
    assert_eq!(outbox.take(), ["[hatchrun] Firing: Launch failed for good"]);
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
fn alert_queries_read_through_indexes() {
    let db = Db::new();
    for (name, sql, index) in [
        ("FAILED_SINCE", FAILED_SINCE, "failed_launches_age"),
        ("DELETE_FAILED", DELETE_FAILED, "failed_launches_age"),
        ("LOW_SINCE", LOW_SINCE, "rate_limits_seen"),
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
