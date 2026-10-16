// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Durable Object that holds one ticket's config.

use std::time::Duration;

use worker::{DurableObject, Env, Request, Response, Result, State, durable_object};

use crate::protocol::{ClaimResponse, Record, StoreRequest};
use crate::{error, now_ms};

/// Storage key of the record.
const RECORD: &str = "record";

/// One ticket.
#[durable_object]
pub struct Ticket {
    state: State,
}

impl DurableObject for Ticket {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let storage = self.state.storage();
        let now = now_ms();
        let record: Option<Record> = storage.get(RECORD).await?;
        let record = match record {
            Some(record) if record.expires_at <= now => {
                storage.delete_all().await?;
                None
            }
            record => record,
        };

        match req.path().as_str() {
            "/store" => {
                if record.is_some() {
                    return error("ticket already stored", 409);
                }
                let record = match StoreRequest::parse(&req.bytes().await?, now) {
                    Ok(record) => record,
                    Err(message) => return error(&message, 400),
                };
                let ttl = Duration::from_millis(record.expires_at.saturating_sub(now));
                storage.put(RECORD, &record).await?;
                storage.set_alarm(ttl).await?;
                Ok(Response::empty()?.with_status(204))
            }
            "/claim" => {
                let Some(record) = record else {
                    return error("not found", 404);
                };
                storage.delete_all().await?;
                storage.delete_alarm().await?;
                Response::from_json(&ClaimResponse {
                    config: record.config,
                })
            }
            _ => error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        self.state.storage().delete_all().await?;
        Response::empty()
    }
}
