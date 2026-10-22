// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `RunObject`: its routes, alarm and I/O on storage, GitHub, Postgres and the queue.

#![allow(missing_docs)]

use std::cell::{Cell, OnceCell};
use std::time::Duration;

use async_lock::Mutex;
use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::{Counted, Sender};
use hatchrun_cloud::http::Response as HttpResponse;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::agent::{Batch, BatchReply};
use hatchrun_workflow::{Conclusion, JobResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker::{
    DurableObject, Env, Method, Request, Response, Result, State, Storage, WebSocket,
    WebSocketIncomingMessage, durable_object,
};

use crate::checks::{JobView, Outbox, conclusion_word, details_url, render};
use crate::env::Settings;
use crate::github::{
    Capped, CheckRun, check_run_id, create_check_run, revoke_token, update_check_run,
};
use crate::graph::{Action, Graph, State as NodeState};
use crate::launches::{self, LaunchMessage};
use crate::store;

use super::decide::{
    Delivery, Failed, Flushed, Gate, apply_batch, apply_held, apply_result, arm_todo, attempts,
    bounded_event, delivery, due, file_annotations, gate, github_context, job_document,
    minimal_check, next_alarm, plan_is_current, record_flush, reschedule, reset_jobs, run_state,
    running_for, should_write_heartbeat, snapshot_ready, steps_json, take_drain,
};
use super::log::{events_message, log_keys, log_prefix, mark_upload_due, tail_key};
use super::{
    ALARM_FALLBACK_MS, BUCKET, DEADLINES, Deadlines, ESCALATE_AFTER_MS, EVENT, GRAPH, INFO,
    JobState, LAUNCHES, MAX_SEND_TRIES, OUTBOX, RESTORED_KEEP_MS, RunInfo, SNAPSHOT_RETRY_MS,
    Start, TODO, Todo, Work, is_run, job_key, restore_key, snapshot_key,
};

/// The run and tenant a request that can reach an empty object names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ident {
    run: u64,
    tenant: String,
    /// The retention tier the run's snapshot is under.
    days: u32,
}

impl Ident {
    fn from_query(run: Option<u64>, tenant: Option<String>, days: Option<u32>) -> Option<Self> {
        let tenant = tenant?;
        restore_key(run, Some(&tenant), days)?;
        Some(Self {
            run: run?,
            tenant,
            days: days?,
        })
    }
}

/// What an object holds when a request names a run.
enum Held {
    /// This run, or its snapshot loaded back.
    Run,
    /// Neither state nor snapshot.
    Missing,
    /// Another run.
    Other,
}

/// What an ended run leaves in R2 before the object deletes its
/// storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    info: RunInfo,
    graph: Graph,
    event: Value,
}

pub(super) fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

pub(super) fn fail(message: impl Into<String>) -> worker::Error {
    worker::Error::RustError(message.into())
}

pub(super) fn answer(status: u16) -> Result<Response> {
    Ok(Response::empty()?.with_status(status))
}

#[derive(Deserialize)]
struct ClaimBody {
    token: String,
    capped: Vec<Capped>,
}

#[derive(Deserialize)]
struct MessageBody {
    message: String,
}

/// What a handler works on: the pieces of state every handler reads,
/// and which of them it changed.
pub(super) struct Loaded {
    pub(super) info: RunInfo,
    pub(super) graph: Graph,
    pub(super) outbox: Outbox,
    pub(super) deadlines: Deadlines,
    pub(super) todo: Todo,
    pub(super) dirty: [bool; 5],
}

impl Loaded {
    fn info_mut(&mut self) -> &mut RunInfo {
        self.dirty[0] = true;
        &mut self.info
    }

    fn graph_mut(&mut self) -> &mut Graph {
        self.dirty[1] = true;
        &mut self.graph
    }

    fn outbox_mut(&mut self) -> &mut Outbox {
        self.dirty[2] = true;
        &mut self.outbox
    }

    pub(super) fn deadlines_mut(&mut self) -> &mut Deadlines {
        self.dirty[3] = true;
        &mut self.deadlines
    }

    fn todo_mut(&mut self) -> &mut Todo {
        self.dirty[4] = true;
        &mut self.todo
    }
}

/// One run.
#[durable_object]
pub struct RunObject {
    state: State,
    env: Env,
    /// Handlers take turns on the state in storage. The lock is
    /// held to read and change it and never across a call to
    /// GitHub, Postgres or the queue.
    turn: Mutex<()>,
    settings: OnceCell<Settings>,
    /// Whether a request or the alarm is running the todo list.
    draining: Cell<bool>,
    /// Whether a request or the alarm is uploading finished logs.
    uploading: Cell<bool>,
}

impl RunObject {
    fn cx(&self) -> Result<Cx<'_>> {
        if self.settings.get().is_none() {
            let settings = Settings::read(&self.env).map_err(fail)?;
            let _ = self.settings.set(settings);
        }
        let settings = self
            .settings
            .get()
            .ok_or_else(|| fail("settings are not read"))?;
        Ok(Cx::new(
            &self.state,
            &self.env,
            &self.turn,
            &self.draining,
            &self.uploading,
            settings,
        ))
    }
}

impl DurableObject for RunObject {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            turn: Mutex::new(()),
            settings: OnceCell::new(),
            draining: Cell::new(false),
            uploading: Cell::new(false),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let url = req.url()?;
        let number = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.parse::<u64>().ok())
        };
        let (job, attempt) = (number("job"), number("attempt"));
        let days = number("days").and_then(|d| u32::try_from(d).ok());
        let tenant = url
            .query_pairs()
            .find(|(key, _)| key == "tenant")
            .map(|(_, value)| value.into_owned());
        self.cx()?
            .route(
                &mut req,
                url.path(),
                job,
                attempt.and_then(|a| u32::try_from(a).ok()),
                Ident::from_query(number("run"), tenant, days),
                number("offset"),
            )
            .await
    }

    async fn alarm(&self) -> Result<Response> {
        self.cx()?.alarm().await;
        Response::empty()
    }

    /// Viewers only listen; whatever they send is ignored.
    #[allow(clippy::unused_async_trait_impl)]
    async fn websocket_message(
        &self,
        _ws: WebSocket,
        _message: WebSocketIncomingMessage,
    ) -> Result<()> {
        Ok(())
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn websocket_close(
        &self,
        ws: WebSocket,
        code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        // Answers the close, so the connection ends cleanly. A
        // reserved code cannot be sent back.
        let code = u16::try_from(code)
            .ok()
            .filter(|code| (1000..=4999).contains(code) && ![1005, 1006, 1015].contains(code));
        ws.close(code, Some("closed"))
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn websocket_error(&self, _ws: WebSocket, _error: worker::Error) -> Result<()> {
        Ok(())
    }
}

/// One request or alarm: the clock, the settings and the connections.
pub(super) struct Cx<'a> {
    pub(super) state: &'a State,
    pub(super) storage: Storage,
    pub(super) env: Env,
    pub(super) turn: &'a Mutex<()>,
    pub(super) draining: &'a Cell<bool>,
    pub(super) uploading: &'a Cell<bool>,
    pub(super) settings: &'a Settings,
    pub(super) now: u64,
    pub(super) db: OnceCell<Db>,
    pub(super) github: Counted,
}

/// One check run request, decided under the lock and sent without it.
struct Planned {
    job: u64,
    generation: u64,
    check: CheckRun,
    /// The count of annotations with a file GitHub has once the whole
    /// request is sent.
    sent_count: usize,
    /// The count it has once a minimal request is sent.
    file_total: usize,
    check_run_id: Option<u64>,
    /// The job's attempt when the request was planned.
    node_attempt: u32,
    installation: u64,
    repo: String,
    attempt: u32,
}

impl<'a> Cx<'a> {
    fn new(
        state: &'a State,
        env: &Env,
        turn: &'a Mutex<()>,
        draining: &'a Cell<bool>,
        uploading: &'a Cell<bool>,
        settings: &'a Settings,
    ) -> Self {
        Self {
            state,
            storage: state.storage(),
            env: env.clone(),
            turn,
            draining,
            uploading,
            settings,
            now: now_ms(),
            db: OnceCell::new(),
            github: Counted::default(),
        }
    }

    async fn db(&self) -> Result<&Db> {
        if self.db.get().is_none() {
            let db = Db::connect(&self.env).await?;
            // Nothing else sets it: this request is the only user.
            let _ = self.db.set(db);
        }
        self.db.get().ok_or_else(|| fail("no database"))
    }

    async fn route(
        &self,
        req: &mut Request,
        path: &str,
        job: Option<u64>,
        attempt: Option<u32>,
        ident: Option<Ident>,
        offset: Option<u64>,
    ) -> Result<Response> {
        let method = req.method();
        match (&method, path) {
            (Method::Post, "/start") => {
                return match req.json::<Start>().await {
                    Ok(start) => self.start(start).await,
                    Err(_) => Response::error("bad start", 400),
                };
            }
            (Method::Post, "/purge") => return self.purge().await,
            (Method::Post, "/cancel") => return self.cancel().await,
            (Method::Get, "/live" | "/log") => {
                let Some(job) = job else {
                    return Response::error("a job is needed", 400);
                };
                return if path == "/live" {
                    self.live(req, job).await
                } else {
                    self.log(job, offset).await
                };
            }
            (Method::Get, "/state") => {
                let Some(ident) = ident else {
                    return Response::error("a run and a tenant are needed", 400);
                };
                return match self.restore_if_needed(&ident).await? {
                    Held::Run => self.state().await,
                    Held::Missing => answer(404),
                    Held::Other => answer(409),
                };
            }
            _ => {}
        }
        let Some(job) = job else {
            return Response::error("a job is needed", 400);
        };
        match (&method, path) {
            (Method::Post, "/held") => match req.json::<MessageBody>().await {
                Ok(body) => {
                    let Some(attempt) = attempt else {
                        return Response::error("an attempt is needed", 400);
                    };
                    self.held(job, attempt, &body.message).await
                }
                Err(_) => Response::error("bad hold", 400),
            },
            (Method::Post, "/failed") => match req.json::<MessageBody>().await {
                Ok(body) => {
                    let Some(attempt) = attempt else {
                        return Response::error("an attempt is needed", 400);
                    };
                    self.failed(job, attempt, body.message).await
                }
                Err(_) => Response::error("bad failure", 400),
            },
            (Method::Post, "/rerun") => {
                let Some(ident) = ident else {
                    return Response::error("a run and a tenant are needed", 400);
                };
                match self.restore_if_needed(&ident).await? {
                    Held::Run => self.rerun(job).await,
                    Held::Missing => answer(404),
                    Held::Other => answer(409),
                }
            }
            (Method::Post, "/claim" | "/events" | "/result") => {
                let Some(attempt) = attempt else {
                    return Response::error("an attempt is needed", 400);
                };
                match path {
                    "/claim" => match req.json::<ClaimBody>().await {
                        Ok(body) => self.claim(job, attempt, body.token, body.capped).await,
                        Err(_) => Response::error("bad claim", 400),
                    },
                    "/events" => match req.json::<Batch>().await {
                        Ok(batch) => self.events(job, attempt, &batch).await,
                        Err(_) => Response::error("bad batch", 400),
                    },
                    _ => match req.json::<JobResult>().await {
                        Ok(result) => self.result(job, attempt, result).await,
                        Err(_) => Response::error("bad result", 400),
                    },
                }
            }
            _ => Response::error("not found", 404),
        }
    }

    /// Runs each due part of the alarm in its own scope, then sets the
    /// alarm for what is left. A part that fails is tried again after a
    /// backoff; planning the next alarm falls back to a fixed wait when
    /// even that fails.
    async fn alarm(&self) {
        if let Err(err) = self.run_alarm().await {
            worker::console_error!("alarm: {err}");
            if let Err(err) = self
                .storage
                .set_alarm(Duration::from_millis(ALARM_FALLBACK_MS))
                .await
            {
                worker::console_error!("alarm: fallback: {err}");
            }
        }
    }

    async fn run_alarm(&self) -> Result<()> {
        let due = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return self.storage.delete_alarm().await;
            };
            due(&run.deadlines, &run.outbox, self.now)
        };
        let mut failed = Failed::default();
        if due.purge {
            match self.purge_due().await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => {
                    worker::console_error!("alarm: purge: {err}");
                    failed.purge = true;
                }
            }
        }
        if due.flush
            && let Err(err) = self.flush().await
        {
            worker::console_error!("alarm: check runs: {err}");
            failed.flush = true;
        }
        if due.escalate {
            let done = self
                .update(|run| {
                    run.deadlines_mut().escalate_at = None;
                    // The reply to the agent's next batch carries the
                    // new count.
                    run.graph_mut().escalate();
                })
                .await;
            if let Err(err) = done {
                worker::console_error!("alarm: escalate: {err}");
                failed.escalate = true;
            }
        }
        if due.todo
            && let Err(err) = self.drain().await
        {
            worker::console_error!("alarm: side effects: {err}");
            failed.todo = true;
        }
        if due.snapshot
            && let Err(err) = self.snapshot_now().await
        {
            worker::console_error!("alarm: snapshot: {err}");
            failed.snapshot = true;
        }
        if due.upload
            && let Err(err) = self.upload_logs(false).await
        {
            worker::console_error!("alarm: uploads: {err}");
            failed.upload = true;
        }
        let now = self.now;
        self.update(|run| {
            let Loaded {
                deadlines, outbox, ..
            } = run;
            reschedule(deadlines, outbox, failed, now);
            run.dirty[2] = true;
            run.dirty[3] = true;
        })
        .await
    }

    /// Reads the state, lets `change` change it and writes it, with the
    /// lock held throughout.
    async fn update(&self, change: impl FnOnce(&mut Loaded)) -> Result<()> {
        let _turn = self.turn.lock().await;
        let Some(mut run) = self.loaded().await? else {
            return Ok(());
        };
        change(&mut run);
        self.commit(&mut run).await
    }

    /// Reads the state. The caller holds the lock.
    pub(super) async fn loaded(&self) -> Result<Option<Loaded>> {
        let storage = &self.storage;
        let (Some(info), Some(graph)) = (
            storage.get::<RunInfo>(INFO).await?,
            storage.get::<Graph>(GRAPH).await?,
        ) else {
            return Ok(None);
        };
        Ok(Some(Loaded {
            info,
            graph,
            outbox: storage.get(OUTBOX).await?.unwrap_or_default(),
            deadlines: storage.get(DEADLINES).await?.unwrap_or_default(),
            todo: storage.get(TODO).await?.unwrap_or_default(),
            dirty: [false; 5],
        }))
    }

    /// Reads the snapshot of an ended run back into an object that was
    /// purged, unless it holds the run. `ident` comes with the request;
    /// the snapshot is read from R2 without the lock, and what the
    /// object holds, or the snapshot, must be that run.
    async fn restore_if_needed(&self, ident: &Ident) -> Result<Held> {
        {
            let _turn = self.turn.lock().await;
            if let Some(info) = self.storage.get::<RunInfo>(INFO).await?
                && self.storage.get::<Graph>(GRAPH).await?.is_some()
            {
                return Ok(if is_run(&info, ident.run, &ident.tenant) {
                    Held::Run
                } else {
                    Held::Other
                });
            }
        }
        let Some(snapshot) = self.fetch_snapshot(ident).await? else {
            return Ok(Held::Missing);
        };
        if !is_run(&snapshot.info, ident.run, &ident.tenant) {
            return Ok(Held::Other);
        }
        let _turn = self.turn.lock().await;
        if self.storage.get::<Graph>(GRAPH).await?.is_some() {
            return Ok(Held::Run);
        }
        self.storage.put(EVENT, &snapshot.event).await?;
        let mut run = Loaded {
            info: snapshot.info,
            graph: snapshot.graph,
            outbox: Outbox::default(),
            deadlines: Deadlines {
                purge_at: Some(self.now + RESTORED_KEEP_MS),
                ..Deadlines::default()
            },
            todo: Todo::default(),
            dirty: [true; 5],
        };
        self.commit(&mut run).await?;
        Ok(Held::Run)
    }

    async fn fetch_snapshot(&self, ident: &Ident) -> Result<Option<Snapshot>> {
        let Some(key) = restore_key(Some(ident.run), Some(&ident.tenant), Some(ident.days)) else {
            return Ok(None);
        };
        let Some(object) = self.env.bucket(BUCKET)?.get(key).execute().await? else {
            return Ok(None);
        };
        let Some(body) = object.body() else {
            return Ok(None);
        };
        let snapshot = serde_json::from_slice(&body.bytes().await?)
            .map_err(|err| fail(format!("run {}: snapshot: {err}", ident.run)))?;
        Ok(Some(snapshot))
    }

    /// Writes what the handler changed and sets the alarm, which also
    /// wakes the object to snapshot an ended run. The caller holds the lock.
    pub(super) async fn commit(&self, run: &mut Loaded) -> Result<()> {
        arm_todo(&mut run.deadlines, &run.todo, self.now);
        if run.deadlines.snapshot_at.is_none()
            && snapshot_ready(&run.graph, &run.outbox, &run.deadlines, &run.todo)
        {
            run.deadlines.snapshot_at = Some(self.now);
        }
        let storage = &self.storage;
        // One call, so the keys land together: `start` treats a stored
        // graph as a completed start.
        storage
            .put_multiple(Commit {
                info: run.dirty[0].then_some(&run.info),
                graph: run.dirty[1].then_some(&run.graph),
                outbox: run.dirty[2].then_some(&run.outbox),
                todo: run.dirty[4].then_some(&run.todo),
                // Not conditional: the code above changes the deadlines.
                deadlines: &run.deadlines,
            })
            .await?;
        run.dirty = [false; 5];
        match next_alarm(&run.deadlines, &run.outbox, self.now) {
            Some(at) => {
                let wait = Duration::from_millis(at.saturating_sub(self.now));
                storage.set_alarm(wait).await
            }
            None => storage.delete_alarm().await,
        }
    }

    /// Writes the state of an ended run to R2 and sets the purge. The
    /// state is read under the lock, written without it and recorded
    /// under it again. A write that fails is tried again after a minute.
    async fn snapshot_now(&self) -> Result<()> {
        let (key, body, attempt) = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return Ok(());
            };
            if run.deadlines.snapshot_at.is_none()
                || !snapshot_ready(&run.graph, &run.outbox, &run.deadlines, &run.todo)
            {
                return Ok(());
            }
            let snapshot = Snapshot {
                info: run.info,
                graph: run.graph,
                event: self.event().await?,
            };
            let body = serde_json::to_vec(&snapshot).map_err(|err| fail(err.to_string()))?;
            (
                snapshot_key(
                    snapshot.info.retention_days,
                    &snapshot.info.tenant_id,
                    snapshot.info.run_id,
                ),
                body,
                snapshot.info.run_attempt,
            )
        };
        let written = async { self.env.bucket(BUCKET)?.put(key, body).execute().await }.await;
        let now = self.now;
        match written {
            Ok(_) => {
                self.update(|run| {
                    // A rerun that came meanwhile, even one that ended
                    // again, made this snapshot stale.
                    if run.graph.conclusion().is_some() && run.info.run_attempt == attempt {
                        let deadlines = run.deadlines_mut();
                        deadlines.snapshot_at = None;
                        deadlines.purge_at = Some(now);
                    }
                })
                .await
            }
            Err(err) => {
                worker::console_error!("snapshot: {err}");
                self.update(|run| run.deadlines_mut().snapshot_at = Some(now + SNAPSHOT_RETRY_MS))
                    .await
            }
        }
    }

    async fn purge(&self) -> Result<Response> {
        let _turn = self.turn.lock().await;
        self.storage.delete_alarm().await?;
        self.storage.delete_all().await?;
        answer(204)
    }

    /// Purges when the deadline is still there. Answers whether it did.
    async fn purge_due(&self) -> Result<bool> {
        let _turn = self.turn.lock().await;
        let deadlines: Deadlines = self.storage.get(DEADLINES).await?.unwrap_or_default();
        if deadlines.purge_at.is_none_or(|at| at > self.now) {
            return Ok(false);
        }
        self.storage.delete_alarm().await?;
        self.storage.delete_all().await?;
        Ok(true)
    }

    async fn event(&self) -> Result<Value> {
        Ok(self.storage.get(EVENT).await?.unwrap_or(Value::Null))
    }

    async fn github_ctx(&self, info: &RunInfo) -> Result<Value> {
        Ok(github_context(
            info,
            &self.event().await?,
            &self.settings.github_api,
        ))
    }

    pub(super) async fn job_state(&self, job: u64) -> Result<JobState> {
        Ok(self.storage.get(&job_key(job)).await?.unwrap_or_default())
    }

    pub(super) async fn put_job(&self, job: u64, state: &JobState) -> Result<()> {
        self.storage.put(&job_key(job), state).await
    }

    async fn state(&self) -> Result<Response> {
        let _turn = self.turn.lock().await;
        let Some(run) = self.loaded().await? else {
            return answer(404);
        };
        let mut states = Vec::new();
        for node in &run.graph.jobs {
            states.push((node.id, self.job_state(node.id).await?));
        }
        Response::from_json(&run_state(&run.info, &run.graph, &states))
    }

    /// Does what the todo list says and drops each item once done. The
    /// lock is taken to read and to drop an item, not while one runs.
    ///
    /// One drain runs at a time. A caller that finds one running does
    /// nothing: the running one loops until the list is empty, and the
    /// alarm comes back to anything it missed.
    async fn drain(&self) -> Result<()> {
        if !take_drain(self.draining) {
            return Ok(());
        }
        let done = self.drain_all().await;
        self.draining.set(false);
        done
    }

    async fn drain_all(&self) -> Result<()> {
        loop {
            let (info, pending) = {
                let _turn = self.turn.lock().await;
                let todo: Todo = self.storage.get(TODO).await?.unwrap_or_default();
                let Some(first) = todo.items.first().cloned() else {
                    // Under the lock, so an item added after this check
                    // finds the flag clear.
                    self.draining.set(false);
                    return Ok(());
                };
                let info: RunInfo = self
                    .storage
                    .get(INFO)
                    .await?
                    .ok_or_else(|| fail("side effects without a run"))?;
                (info, first)
            };
            self.perform(&info, &pending.work).await?;
            self.update(|run| run.todo_mut().remove(pending.id)).await?;
        }
    }

    /// Does one side effect. Each can be repeated.
    async fn perform(&self, info: &RunInfo, work: &Work) -> Result<()> {
        let db = self.db().await?;
        match work {
            Work::StartRun => store::run_started(db, info.run_id, self.now).await,
            Work::QueueLaunch { job, attempt } => {
                // A job that is launching or launched is not queued
                // again, and no second launch is sent for it.
                if store::queue_job(db, *job, *attempt, self.now).await? {
                    self.env
                        .queue(LAUNCHES)?
                        .send(LaunchMessage {
                            job_id: *job,
                            attempt: *attempt,
                        })
                        .await?;
                }
                Ok(())
            }
            Work::FinishJob {
                job,
                attempt,
                conclusion,
                steps,
                log_key,
            } => {
                let ended = store::finish_job(
                    db,
                    *job,
                    *attempt,
                    conclusion_word(*conclusion),
                    steps,
                    log_key.as_deref(),
                    self.now,
                )
                .await?;
                if ended {
                    // The freed place goes to the oldest held job.
                    launches::wake_oldest(&self.env, db, &info.tenant_id).await;
                }
                Ok(())
            }
            Work::FinishRun {
                attempt,
                conclusion,
            } => {
                store::finish_run(
                    db,
                    info.run_id,
                    *attempt,
                    conclusion_word(*conclusion),
                    self.now,
                )
                .await
            }
            Work::ResetJob { job, attempt } => store::reset_job(db, *job, *attempt, self.now).await,
            Work::ReopenRun { attempt } => {
                store::reopen_run_at(db, info.run_id, *attempt, self.now).await
            }
        }
    }

    /// Turns what the graph decided into storage changes now and side
    /// effects on the todo list, and returns the GitHub tokens to
    /// revoke once the lock is released. The caller holds the lock.
    async fn apply_actions(&self, run: &mut Loaded, actions: Vec<Action>) -> Result<Vec<String>> {
        let mut revoke = Vec::new();
        for action in actions {
            match action {
                Action::Launch(job) => {
                    let attempt = run.graph.node(job).map_or(1, |node| node.attempt);
                    run.todo_mut().push(Work::QueueLaunch { job, attempt });
                }
                Action::Finished { job, conclusion } => {
                    let mut state = self.job_state(job).await?;
                    state.completed_at = Some(self.now);
                    let steps = steps_json(&state.steps);
                    revoke.extend(state.github_token.take());
                    let attempt = run.graph.node(job).map_or(1, |node| node.attempt);
                    let log_key = self
                        .seal_log(&run.info, job, attempt, conclusion, &state)
                        .await?;
                    if log_key.is_some() {
                        state.upload_pending = true;
                        mark_upload_due(run.deadlines_mut(), self.now);
                    }
                    self.end_sockets(job, conclusion);
                    self.put_job(job, &state).await?;
                    run.outbox_mut().mark(job);
                    run.todo_mut().push(Work::FinishJob {
                        job,
                        attempt,
                        conclusion,
                        steps,
                        log_key,
                    });
                }
                // The agent learns of a cancel from the reply to its
                // next batch.
                Action::Cancel { .. } => {}
                Action::RunDone(conclusion) => {
                    let attempt = run.info.run_attempt;
                    run.todo_mut().push(Work::FinishRun {
                        attempt,
                        conclusion,
                    });
                }
            }
        }
        Ok(revoke)
    }

    /// Carries out what a handler decided, after it released the lock:
    /// the side effects, then the revokes. A failure is logged; the
    /// todo list keeps what is left and the alarm comes back to it.
    async fn after(&self, revoke: Vec<String>) {
        if let Err(err) = self.drain().await {
            worker::console_error!("side effects: {err}");
        }
        if let Err(err) = self.upload_logs(false).await {
            worker::console_error!("uploads: {err}");
        }
        if let Err(err) = self.snapshot_now().await {
            worker::console_error!("snapshot: {err}");
        }
        for token in revoke {
            self.revoke(&token).await;
        }
    }

    async fn start(&self, start: Start) -> Result<Response> {
        let Start {
            run: info,
            jobs,
            event,
        } = start;
        let revoke = {
            let _turn = self.turn.lock().await;
            // The graph is written last, so a start that failed halfway
            // runs again.
            if self.storage.get::<Graph>(GRAPH).await?.is_some() {
                let held = self.storage.get::<RunInfo>(INFO).await?;
                return answer(
                    if held.is_some_and(|held| is_run(&held, info.run_id, &info.tenant_id)) {
                        204
                    } else {
                        409
                    },
                );
            }
            let mut run = Loaded {
                graph: Graph::new(jobs),
                outbox: Outbox::default(),
                deadlines: Deadlines::default(),
                todo: Todo::default(),
                info,
                dirty: [true; 5],
            };
            let ids: Vec<u64> = run.graph.jobs.iter().map(|n| n.id).collect();
            for id in ids {
                run.outbox.mark(id);
            }
            self.storage.put(EVENT, &bounded_event(event)).await?;
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph.advance(&ctx);
            run.todo.push(Work::StartRun);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    async fn claim(
        &self,
        job: u64,
        attempt: u32,
        token: String,
        capped: Vec<Capped>,
    ) -> Result<Response> {
        let first = {
            let _turn = self.turn.lock().await;
            let Some(run) = self.loaded().await? else {
                return answer(409);
            };
            gate(run.graph.node(job), attempt)
        };
        match first {
            Gate::Missing => return Response::error("no such job", 404),
            Gate::Stale | Gate::Ended => return answer(409),
            Gate::Queued => {
                // The row moves first; a claim that comes again finds it
                // running for this attempt.
                let db = self.db().await?;
                if !store::start_job(db, job, self.now).await? {
                    let running = store::job(db, job)
                        .await?
                        .is_some_and(|row| running_for(&row.status, row.attempt, attempt));
                    if !running {
                        return answer(409);
                    }
                }
            }
            Gate::Running => {}
        }
        let (document, old) = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            let was = run.graph.node(job).map(|node| node.state);
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                Gate::Stale | Gate::Ended => return answer(409),
            }
            run.graph_mut().running(job);
            let mut state = self.job_state(job).await?;
            if state.held.take().is_some() || (!capped.is_empty() && state.capped != capped) {
                run.outbox_mut().mark(job);
            }
            state.capped = capped;
            let old = state
                .github_token
                .replace(token.clone())
                .filter(|old| *old != token);
            if was == Some(NodeState::Queued) || state.started_at.is_none() {
                state.started_at = Some(self.now);
                state.heartbeat_written_at = self.now;
            }
            self.put_job(job, &state).await?;
            let document = job_document(
                &run.info,
                &self.event().await?,
                &run.graph,
                job,
                &token,
                &self.settings.github_api,
            )
            .ok_or_else(|| fail("job vanished"))?;
            self.commit(&mut run).await?;
            (document, old)
        };
        if let Some(old) = old {
            self.revoke(&old).await;
        }
        Response::from_json(&document)
    }

    async fn events(&self, job: u64, attempt: u32, batch: &Batch) -> Result<Response> {
        let (reply, heartbeat) = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                _ => return answer(409),
            }
            let cancel = run.graph.node(job).map_or(0, |node| node.cancel);
            let mut state = self.job_state(job).await?;
            let mut writer = self.load_writer(job).await?.unwrap_or_default();
            let (applied, wrote) = apply_batch(&mut state, &mut writer, batch);
            let mut changed = applied.fresh;
            if applied.steps_finished > 0 {
                run.outbox_mut().mark(job);
            }
            if wrote {
                // The log first: a crash before the state is written
                // repeats lines, which beats losing them.
                let chunks = writer.take_full_chunks();
                self.save_log(job, &writer, &chunks).await?;
            }
            let heartbeat = should_write_heartbeat(state.heartbeat_written_at, self.now);
            if heartbeat {
                state.heartbeat_written_at = self.now;
                changed = true;
            }
            if changed {
                self.put_job(job, &state).await?;
            }
            if applied.fresh {
                self.send_sockets(job, || events_message(batch));
            }
            if run.dirty.iter().any(|dirty| *dirty) {
                self.commit(&mut run).await?;
            }
            (BatchReply { cancel }, heartbeat)
        };
        if heartbeat {
            // The next one is 30 s away; a miss is not worth failing
            // the batch.
            let written = async { store::heartbeat(self.db().await?, job, self.now).await }.await;
            if let Err(err) = written {
                worker::console_warn!("job {job}: heartbeat: {err}");
            }
        }
        Response::from_json(&reply)
    }

    async fn result(&self, job: u64, attempt: u32, result: JobResult) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(409);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                _ => return answer(409),
            }
            let mut state = self.job_state(job).await?;
            apply_result(&mut state, &result);
            self.put_job(job, &state).await?;
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().finished(job, result, &ctx);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    async fn failed(&self, job: u64, attempt: u32, message: String) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(204);
            };
            match gate(run.graph.node(job), attempt) {
                Gate::Queued | Gate::Running => {}
                Gate::Missing => return Response::error("no such job", 404),
                Gate::Stale | Gate::Ended => return answer(409),
            }
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().failed(job, message, &ctx);
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    /// Stores why the queued job waits for a launch. The check run is
    /// updated only when the message differs from the one stored: the
    /// launch consumer posts it again at every recheck.
    async fn held(&self, job: u64, attempt: u32, message: &str) -> Result<Response> {
        let _turn = self.turn.lock().await;
        let Some(mut run) = self.loaded().await? else {
            return answer(409);
        };
        match gate(run.graph.node(job), attempt) {
            Gate::Queued => {}
            Gate::Missing => return Response::error("no such job", 404),
            Gate::Stale | Gate::Ended | Gate::Running => return answer(409),
        }
        let mut state = self.job_state(job).await?;
        if apply_held(&mut state, message) {
            self.put_job(job, &state).await?;
            run.outbox_mut().mark(job);
            self.commit(&mut run).await?;
        }
        answer(204)
    }

    async fn cancel(&self) -> Result<Response> {
        let revoke = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(204);
            };
            let ctx = self.github_ctx(&run.info).await?;
            let actions = run.graph_mut().cancel(&ctx);
            if run.graph.conclusion().is_none() {
                let at = self.now + ESCALATE_AFTER_MS;
                run.deadlines_mut().escalate_at.get_or_insert(at);
            }
            let revoke = self.apply_actions(&mut run, actions).await?;
            self.commit(&mut run).await?;
            revoke
        };
        self.after(revoke).await;
        answer(204)
    }

    /// A rerun starts every reset job's attempt from nothing: a new
    /// state (so its batches count from 1 again), its old token revoked,
    /// its log chunks dropped and its row set up for the new attempt.
    async fn rerun(&self, job: u64) -> Result<Response> {
        // A log that is not in R2 yet is uploaded first: the reset
        // deletes its chunks and its job row names its key.
        if let Err(err) = self.upload_logs(true).await {
            worker::console_error!("rerun: uploads: {err}");
        }
        let mut revoke = Vec::new();
        {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return answer(404);
            };
            let ctx = self.github_ctx(&run.info).await?;
            let before = attempts(&run.graph);
            let actions = run.graph_mut().rerun(job, &ctx);
            let reset = reset_jobs(&before, &run.graph);
            if reset.is_empty() {
                return answer(204);
            }
            for (id, _) in &reset {
                if self.job_state(*id).await?.upload_pending {
                    // Nothing is written yet; the caller asks again.
                    return answer(409);
                }
            }
            for (id, attempt) in reset {
                revoke.extend(self.job_state(id).await?.github_token);
                self.put_job(id, &JobState::default()).await?;
                self.delete_prefix(&log_prefix(id)).await?;
                self.storage.delete_multiple(log_keys(id)).await?;
                run.todo_mut().push(Work::ResetJob { job: id, attempt });
                run.outbox_mut().mark(id);
            }
            let attempt = run.info.run_attempt + 1;
            run.todo_mut().push(Work::ReopenRun { attempt });
            run.info_mut().run_attempt = attempt;
            let deadlines = run.deadlines_mut();
            deadlines.purge_at = None;
            deadlines.escalate_at = None;
            revoke.extend(self.apply_actions(&mut run, actions).await?);
            self.commit(&mut run).await?;
        }
        self.after(revoke).await;
        answer(204)
    }

    /// Revokes a job's GitHub token. A token that stays valid expires
    /// within the hour, so a failure is logged and not retried.
    async fn revoke(&self, token: &str) {
        let request = revoke_token(&self.settings.github_api, token);
        match self.github.send(&request).await {
            Ok(response) if response.ok() || response.status == 401 => {}
            Ok(response) => {
                worker::console_warn!("revoke token: GitHub answered {}", response.status);
            }
            Err(err) => worker::console_warn!("revoke token: {err}"),
        }
    }

    /// Sends the check run update of each job that is due, one request
    /// per job. The requests are planned under the lock, sent without
    /// it, and each answer is recorded under it again. Stops when GitHub
    /// throttles.
    async fn flush(&self) -> Result<()> {
        let plans = {
            let _turn = self.turn.lock().await;
            let Some(mut run) = self.loaded().await? else {
                return Ok(());
            };
            let plans = self.plan_flush(&mut run).await?;
            if run.dirty.iter().any(|dirty| *dirty) {
                self.commit(&mut run).await?;
            }
            plans
        };
        let settings = self.settings;
        let tokens = Tokens::new(
            Kv::new(&self.env),
            App::new(
                settings.github_api.clone(),
                settings.app_id.clone(),
                settings.app_key.clone(),
            ),
            &self.github,
            self.now,
        );
        for plan in plans {
            let flushed = self.send(&tokens, &plan).await;
            let go_on = {
                let _turn = self.turn.lock().await;
                let Some(mut run) = self.loaded().await? else {
                    return Ok(());
                };
                // A rerun that came while the request was out has a
                // new state and its own mark; this answer is not its.
                if !plan_is_current(run.graph.node(plan.job), plan.node_attempt) {
                    continue;
                }
                let mut state = self.job_state(plan.job).await?;
                let terminal = matches!(
                    run.graph.node(plan.job).map(|node| &node.state),
                    Some(NodeState::Done(_))
                );
                if matches!(flushed, Flushed::Failed)
                    && !terminal
                    && state.send_failures + 1 >= MAX_SEND_TRIES
                {
                    worker::console_error!(
                        "run {}: check run of job {}: update dropped after {MAX_SEND_TRIES} tries",
                        run.info.run_id,
                        plan.job
                    );
                }
                let now = self.now;
                let go_on = record_flush(
                    &mut state,
                    run.outbox_mut(),
                    plan.job,
                    plan.generation,
                    &flushed,
                    terminal,
                    now,
                );
                self.put_job(plan.job, &state).await?;
                self.commit(&mut run).await?;
                go_on
            };
            if !go_on {
                break;
            }
        }
        Ok(())
    }

    /// The request of each job that is due. A job the graph does not
    /// know has nothing to send and its mark goes.
    async fn plan_flush(&self, run: &mut Loaded) -> Result<Vec<Planned>> {
        let mut plans = Vec::new();
        for job in run.outbox.due(self.now) {
            let Some(generation) = run.outbox.generation(job) else {
                continue;
            };
            let state = self.job_state(job).await?;
            let Some(node) = run.graph.node(job) else {
                run.outbox_mut().sent(job, generation);
                continue;
            };
            let tail = if node.state == NodeState::Done(Conclusion::Failure) {
                self.storage.get::<Vec<String>>(&tail_key(job)).await?
            } else {
                None
            };
            let view = JobView {
                workflow: &run.info.workflow,
                node,
                head_sha: &run.info.sha,
                details_url: details_url(
                    &self.settings.app_url,
                    &run.info.repo.full_name,
                    run.info.run_id,
                    job,
                ),
                steps: &state.steps,
                annotations: &state.annotations,
                file_annotations_sent: state.file_annotations_sent,
                failed_tail: tail.as_deref(),
                held: state.held.as_deref(),
                capped: &state.capped,
                started_at: state.started_at,
                completed_at: state.completed_at,
            };
            let (check, sent_count) = render(&view);
            plans.push(Planned {
                job,
                generation,
                check,
                sent_count,
                file_total: file_annotations(&state),
                check_run_id: state.check_run_id,
                node_attempt: node.attempt,
                installation: run.info.installation_id,
                repo: run.info.repo.full_name.clone(),
                attempt: run.outbox.attempt(),
            });
        }
        Ok(plans)
    }

    /// Sends the request of `plan`. GitHub refusing it is answered with
    /// a minimal request once.
    async fn send(&self, tokens: &Tokens<'_, Kv, App, Counted>, plan: &Planned) -> Flushed {
        let Some(response) = self.request(tokens, plan, &plan.check).await else {
            return Flushed::Failed;
        };
        match delivery(&response, self.now, plan.attempt) {
            Delivery::Done => Self::sent(plan, &response, plan.sent_count),
            Delivery::Wait(seconds) => Flushed::Throttled(seconds),
            Delivery::Failed => Flushed::Failed,
            Delivery::Rejected => {
                worker::console_warn!(
                    "check run of job {}: GitHub answered {}: {}",
                    plan.job,
                    response.status,
                    response.text()
                );
                let minimal = minimal_check(&plan.check);
                let Some(response) = self.request(tokens, plan, &minimal).await else {
                    return Flushed::Failed;
                };
                match delivery(&response, self.now, plan.attempt) {
                    Delivery::Done => Self::sent(plan, &response, plan.file_total),
                    Delivery::Wait(seconds) => Flushed::Throttled(seconds),
                    Delivery::Failed | Delivery::Rejected => Flushed::Failed,
                }
            }
        }
    }

    /// A request that went through: the id of a check run just made
    /// must be readable, or the next request would make another.
    fn sent(plan: &Planned, response: &HttpResponse, annotations_sent: usize) -> Flushed {
        if plan.check_run_id.is_some() {
            return Flushed::Sent {
                new_check_run: None,
                annotations_sent,
            };
        }
        match check_run_id(response) {
            Ok(id) => Flushed::Sent {
                new_check_run: Some(id),
                annotations_sent,
            },
            Err(err) => {
                worker::console_error!("check run of job {}: {err}", plan.job);
                Flushed::Failed
            }
        }
    }

    /// Creates or updates the job's check run. `None` when no answer
    /// came.
    async fn request(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        plan: &Planned,
        check: &CheckRun,
    ) -> Option<HttpResponse> {
        let api = &self.settings.github_api;
        let repo = &plan.repo;
        let response = match plan.check_run_id {
            None => {
                tokens
                    .send(plan.installation, |t| create_check_run(api, repo, t, check))
                    .await
            }
            Some(id) => {
                tokens
                    .send(plan.installation, |t| {
                        update_check_run(api, repo, id, t, check)
                    })
                    .await
            }
        };
        match response {
            Ok(response) => Some(response),
            Err(err) => {
                worker::console_warn!("check run of job {}: {err}", plan.job);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restore_key_is_built_from_the_run_and_tenant_of_the_request() {
        assert_eq!(
            restore_key(Some(12), Some("abc"), Some(90)).as_deref(),
            Some("state/90d/abc/12.json")
        );
        assert_eq!(restore_key(None, Some("abc"), Some(30)), None);
        assert_eq!(restore_key(Some(12), None, Some(30)), None);
        assert_eq!(restore_key(Some(12), Some("abc"), Some(60)), None);
        assert_eq!(restore_key(Some(12), Some("abc"), None), None);
        assert_eq!(restore_key(Some(12), Some(""), Some(30)), None);
        assert_eq!(restore_key(Some(12), Some("a/../b"), Some(30)), None);
        assert_eq!(
            Ident::from_query(Some(1), Some("t".into()), Some(30))
                .unwrap()
                .run,
            1
        );
        assert_eq!(
            Ident::from_query(Some(1), Some("x/y".into()), Some(30)),
            None
        );
        assert_eq!(Ident::from_query(None, Some("t".into()), Some(30)), None);
    }
}

/// The keys one [`Loaded::commit`] writes, as the fields of a struct for
/// `put_multiple`; the field names are the storage keys. Keys that did
/// not change are left out.
#[derive(serde::Serialize)]
struct Commit<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<&'a RunInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph: Option<&'a Graph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outbox: Option<&'a Outbox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    todo: Option<&'a Todo>,
    deadlines: &'a Deadlines,
}
