// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The webhook deliveries the intake stored. The Worker answers 202
//! once a delivery is stored, and the `hatchrun-ci-deliveries`
//! consumer plans it from the stored payload, so a delivery is planned
//! exactly once: storing and claiming are each one statement.
//!
//! A delivery is `received` when stored, `planning` while a consumer
//! holds its lease, and `done` or `failed` after. `attempts` counts
//! the claims and the reconciler's resends, `lease_until` ends the
//! claim in Unix milliseconds, and `error` says why a delivery failed.

use hatchrun_cloud::db::{Db, params};
use serde::Deserialize;
use worker::Result;

/// How long a claim holds a delivery before the reconciler sends it
/// again, in milliseconds.
pub const LEASE_MS: u64 = 5 * 60_000;

/// A `received` delivery older than this is sent again, in
/// milliseconds: the queue lost it on the way to a consumer.
pub const RECEIVED_STALE_MS: u64 = 2 * 60_000;

/// Claims past this are left alone by the queue and fail instead:
/// something about the delivery keeps failing. See
/// [`fail_capped_deliveries`].
pub const MAX_ATTEMPTS: u64 = 50;

/// Stores delivery `guid` of `event` with `payload`, as `received`.
/// Returns whether it is new: a duplicate stores nothing. The payload
/// goes in hex, since the statements bind text.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn accept_delivery(
    db: &Db,
    guid: &str,
    event: &str,
    payload: &[u8],
    now: u64,
) -> Result<bool> {
    Ok(db
        .run(
            ACCEPT_DELIVERY,
            &params![guid, event, hex::encode(payload), now],
        )
        .await?
        == 1)
}

/// A claimed delivery: what the consumer plans from. `attempts` is
/// the claim's token: [`finish_delivery`] and [`release_delivery`]
/// only touch the row while it still holds this claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryClaim {
    /// The `X-GitHub-Delivery` id.
    pub guid: String,
    /// The `X-GitHub-Event` name.
    pub event: String,
    /// The delivery body, as it arrived.
    pub payload: Vec<u8>,
    /// How often it was claimed, this claim included: the token that
    /// ends or releases this claim.
    pub attempts: u64,
}

/// Claims delivery `guid` for planning: it becomes `planning` with a
/// lease until `now + LEASE_MS`, and `attempts` grows by one. Only a
/// `received` delivery, or a `planning` one whose lease ran out before
/// `now`, can be claimed, so two consumers never plan one delivery.
/// Returns `None` when there is nothing to claim. The payload comes
/// back base64 as stored.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn claim_delivery(db: &Db, guid: &str, now: u64) -> Result<Option<DeliveryClaim>> {
    #[derive(Deserialize)]
    struct Row {
        event: String,
        payload: Option<String>,
        attempts: u64,
    }
    let row: Option<Row> = db
        .first(CLAIM_DELIVERY, &params![guid, now + LEASE_MS, now])
        .await?;
    row.map(|row| {
        hex::decode(row.payload.unwrap_or_default())
            .map(|payload| DeliveryClaim {
                guid: guid.to_owned(),
                event: row.event,
                payload,
                attempts: row.attempts,
            })
            .map_err(|err| worker::Error::RustError(format!("delivery {guid}: {err}")))
    })
    .transpose()
}

/// Ends delivery `guid` as `state` (`done` or `failed`) with `error`,
/// but only while it still holds the claim of `token`: a consumer
/// whose lease ran out cannot end what another consumer claimed since.
/// A `done` delivery drops its payload; a failed one keeps it, so the
/// dead-letter consumer can say what it was. Returns whether the row
/// was ended: false means another consumer owns the delivery now, and
/// the caller acks and does nothing else.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn finish_delivery(
    db: &Db,
    guid: &str,
    token: u64,
    state: &str,
    error: Option<&str>,
) -> Result<bool> {
    debug_assert!(state == "done" || state == "failed");
    Ok(db
        .run(FINISH_DELIVERY, &params![guid, state, error, token])
        .await?
        == 1)
}

/// Gives delivery `guid` back: it becomes `received` again with its
/// lease cleared, but only while it still holds the claim of `token`.
/// `attempts` stays, counting the tries. Returns whether the row was
/// released: false means another consumer owns the delivery now, and
/// the caller acks and does nothing else.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn release_delivery(db: &Db, guid: &str, token: u64) -> Result<bool> {
    Ok(db.run(RELEASE_DELIVERY, &params![guid, token]).await? == 1)
}

/// Ends delivery `guid` as failed with `error`, but only while it still
/// waits: `received` or `planning`. A delivery that is already `done`
/// or `failed` is left alone. Returns whether the row was ended.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn finish_dead_letter(db: &Db, guid: &str, error: &str) -> Result<bool> {
    Ok(db.run(FINISH_DEAD_LETTER, &params![guid, error]).await? == 1)
}

/// The state of delivery `guid`, if it is stored.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn delivery_state(db: &Db, guid: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        state: String,
    }
    Ok(db
        .first::<Row>(DELIVERY_STATE, &params![guid])
        .await?
        .map(|row| row.state))
}

/// Counts a reconciler resend of delivery `guid` toward
/// [`MAX_ATTEMPTS`]: `attempts` grows by one, but only while the row
/// still waits to be sent, `received` and older than
/// [`RECEIVED_STALE_MS`] or `planning` with its lease out. Returns
/// whether it was counted: false means a consumer claimed or ended the
/// delivery since, and the caller sends nothing.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn note_resend(db: &Db, guid: &str, now: u64) -> Result<bool> {
    Ok(db
        .run(
            NOTE_RESEND,
            &params![
                guid,
                now.saturating_sub(RECEIVED_STALE_MS),
                now,
                MAX_ATTEMPTS
            ],
        )
        .await?
        == 1)
}

/// Fails the deliveries that reached [`MAX_ATTEMPTS`] claims and
/// resends and still wait to be sent, so they stop resending every
/// minute and the alert path sees them. Returns each failed delivery
/// with its error.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn fail_capped_deliveries(db: &Db, now: u64) -> Result<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Row {
        guid: String,
        error: String,
    }
    Ok(db
        .all::<Row>(
            FAIL_CAPPED_DELIVERIES,
            &params![now.saturating_sub(RECEIVED_STALE_MS), now, MAX_ATTEMPTS],
        )
        .await?
        .into_iter()
        .map(|row| (row.guid, row.error))
        .collect())
}

/// The guids to send to the queue again, oldest first, at most
/// `limit`: `received` deliveries older than
/// [`RECEIVED_STALE_MS`], and `planning` ones whose lease ran out,
/// both tried fewer than [`MAX_ATTEMPTS`] times.
///
/// # Errors
///
/// Postgres refused the statement.
pub async fn stale_deliveries(db: &Db, now: u64, limit: u32) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Row {
        guid: String,
    }
    let rows: Vec<Row> = db
        .all(
            STALE_DELIVERIES,
            &params![
                now.saturating_sub(RECEIVED_STALE_MS),
                now,
                MAX_ATTEMPTS,
                limit
            ],
        )
        .await?;
    Ok(rows.into_iter().map(|row| row.guid).collect())
}

/// Stores delivery `$1` of event `$2` with payload `decode($3,
/// 'hex')`, received at `$4`, unless its guid is stored already: one
/// row written means it is new. Hex, not base64: `encode` wraps base64
/// every 76 characters.
const ACCEPT_DELIVERY: &str = "INSERT INTO ci_deliveries \
     (guid, event, payload, state, received_at) \
     VALUES ($1, $2, decode($3, 'hex'), 'received', $4) \
     ON CONFLICT (guid) DO NOTHING";

/// Claims delivery `$1` until `$2` when it is `received`, or
/// `planning` with its lease out before `$3`.
const CLAIM_DELIVERY: &str = "UPDATE ci_deliveries SET state = 'planning', lease_until = $2, \
     attempts = attempts + 1 WHERE guid = $1 \
     AND (state = 'received' OR (state = 'planning' AND lease_until < $3)) \
     RETURNING event, encode(payload, 'hex') AS payload, attempts";

/// Ends delivery `$1` as `$2` with error `$3`, dropping the payload
/// of a `done` one, but only while it still holds the claim of `$4`:
/// `planning` with that claim count.
const FINISH_DELIVERY: &str = "UPDATE ci_deliveries SET state = $2, error = $3, \
     payload = CASE WHEN $2 = 'done' THEN NULL ELSE payload END, lease_until = NULL \
     WHERE guid = $1 AND state = 'planning' AND attempts = $4";

/// Gives delivery `$1` back to `received`, clearing its lease, but
/// only while it still holds the claim of `$2`.
const RELEASE_DELIVERY: &str = "UPDATE ci_deliveries SET state = 'received', lease_until = NULL \
     WHERE guid = $1 AND state = 'planning' AND attempts = $2";

/// Ends delivery `$1` as failed with error `$2`, but only while it
/// still waits: `received` or `planning`.
const FINISH_DEAD_LETTER: &str = "UPDATE ci_deliveries SET state = 'failed', error = $2, \
     lease_until = NULL WHERE guid = $1 AND state IN ('received', 'planning')";

/// The state of delivery `$1`, if it is stored.
const DELIVERY_STATE: &str = "SELECT state FROM ci_deliveries WHERE guid = $1";

/// Counts a reconciler resend of delivery `$1`: `attempts` grows by
/// one, but only while it still waits to be sent and is claimed fewer
/// than `$4` times: `received` from before `$2`, or `planning` with
/// its lease out before `$3`.
const NOTE_RESEND: &str = "UPDATE ci_deliveries SET attempts = attempts + 1 WHERE guid = $1 \
     AND attempts < $4 \
     AND ((state = 'received' AND received_at < $2) \
     OR (state = 'planning' AND lease_until IS NOT NULL AND lease_until < $3))";

/// Fails the deliveries that still wait to be sent past `$3` claims
/// and resends: `received` ones from before `$1`, and `planning` ones
/// whose lease ran out before `$2`. Each is failed with an error naming
/// it, so the alert path sees it.
const FAIL_CAPPED_DELIVERIES: &str = "UPDATE ci_deliveries \
     SET state = 'failed', error = 'the delivery was resent too often: ' || guid, \
     lease_until = NULL \
     WHERE attempts >= $3 \
     AND ((state = 'received' AND received_at < $1) \
     OR (state = 'planning' AND lease_until IS NOT NULL AND lease_until < $2)) \
     RETURNING guid, error";

/// Deliveries to plan again: `received` ones from before `$1`, and
/// `planning` ones whose lease ran out before `$2`, claimed fewer than
/// `$3` times, at most `$4`, oldest first.
const STALE_DELIVERIES: &str = "SELECT guid FROM ci_deliveries \
     WHERE attempts < $3 \
     AND ((state = 'received' AND received_at < $1) \
     OR (state = 'planning' AND lease_until IS NOT NULL AND lease_until < $2)) \
     ORDER BY received_at, guid LIMIT $4";
