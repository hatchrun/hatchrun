// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Sends a running job's events to the control plane.
//!
//! [`HttpSink`] queues events where the executor writes them. A
//! [`Flusher`] task posts the queue about once a second in numbered
//! batches. A batch keeps its number and contents until the control
//! plane acknowledges it, so a retry never reorders or duplicates
//! anything. An empty batch goes out as a heartbeat when nothing was sent
//! for a while, and the control plane's answer can cancel the job.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hatchrun_workflow::agent::{
    AgentEvent, Batch, FLUSH_INTERVAL_MS, HEARTBEAT_INTERVAL_MS, MAX_BATCH_BYTES,
};
use hatchrun_workflow::{Annotation, StepSummary};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use super::client::{Client, Stopped};
use crate::exec::sink::Sink;

/// The most event bytes held while the control plane cannot be reached.
/// Past it, lines are dropped.
pub const MAX_PENDING_BYTES: usize = 8 << 20;
/// The longest line sent, in bytes. A longer one is cut.
const MAX_LINE_BYTES: usize = 64 << 10;
/// How long the final flush keeps trying.
const CLOSE_DEADLINE: Duration = Duration::from_secs(120);
/// The cancel count that stops the job for good.
const KILL: u32 = 2;

/// When the flusher sends.
#[derive(Clone, Copy)]
pub struct Timing {
    /// How often it posts what is queued.
    pub flush: Duration,
    /// The longest it stays silent.
    pub heartbeat: Duration,
    /// How long the final flush keeps trying.
    pub close: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            flush: Duration::from_millis(FLUSH_INTERVAL_MS),
            heartbeat: Duration::from_millis(HEARTBEAT_INTERVAL_MS),
            close: CLOSE_DEADLINE,
        }
    }
}

/// The queue between the executor and the flusher.
#[derive(Default)]
struct Buffer {
    /// Events not sent yet, with their serialized size plus a comma.
    queue: VecDeque<(AgentEvent, usize)>,
    /// Bytes of `queue` and of the batch in flight.
    bytes: usize,
    /// Lines dropped since the last batch was built.
    dropped: u64,
    /// The control plane stopped the job: nothing is sent any more.
    stopped: bool,
}

impl Buffer {
    fn push(&mut self, event: AgentEvent) {
        if self.stopped {
            return;
        }
        let size = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len()) + 1;
        if matches!(event, AgentEvent::Line { .. }) && self.bytes + size > MAX_PENDING_BYTES {
            self.dropped += 1;
            return;
        }
        self.bytes += size;
        self.queue.push_back((event, size));
    }

    /// Takes the next batch's events, at most [`MAX_BATCH_BYTES`] of them
    /// but always one if there is one, and the bytes they free once sent.
    /// Lines dropped so far lead the batch as a notice.
    fn take(&mut self) -> (Vec<AgentEvent>, usize) {
        let mut events = Vec::new();
        let mut size = 0;
        let mut freed = 0;
        if self.dropped > 0 {
            let notice = AgentEvent::Line {
                text: format!(
                    "[hatchrun] {} lines were dropped while the control plane was unreachable",
                    self.dropped
                ),
            };
            self.dropped = 0;
            size += serde_json::to_vec(&notice).map_or(0, |bytes| bytes.len()) + 1;
            events.push(notice);
        }
        while let Some((_, next)) = self.queue.front() {
            if !events.is_empty() && size + next > MAX_BATCH_BYTES {
                break;
            }
            let Some((event, next)) = self.queue.pop_front() else {
                break;
            };
            size += next;
            freed += next;
            events.push(event);
        }
        (events, freed)
    }

    fn acknowledge(&mut self, freed: usize) {
        self.bytes = self.bytes.saturating_sub(freed);
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.queue.clear();
        self.bytes = 0;
    }
}

/// A [`Sink`] that queues events for the control plane.
pub struct HttpSink {
    buffer: Arc<Mutex<Buffer>>,
}

fn lock(buffer: &Mutex<Buffer>) -> MutexGuard<'_, Buffer> {
    buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HttpSink {
    /// Starts the flusher. `cancel` is raised to the control plane's
    /// cancel count, and to 2 when the control plane stops answering for
    /// good, because the job ended or was reset.
    pub fn start(client: Client, cancel: watch::Sender<u32>) -> (Self, Flusher) {
        Self::start_with(client, cancel, Timing::default())
    }

    /// [`HttpSink::start`] with other timing.
    pub fn start_with(
        client: Client,
        cancel: watch::Sender<u32>,
        timing: Timing,
    ) -> (Self, Flusher) {
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let (close, closing) = oneshot::channel();
        let task = Task {
            client,
            cancel,
            buffer: Arc::clone(&buffer),
            timing,
            seq: 0,
            inflight: None,
            last_sent: Instant::now(),
        };
        let task = tokio::spawn(task.run(closing));
        (
            Self { buffer },
            Flusher {
                close: Some(close),
                task,
            },
        )
    }

    fn push(&self, event: AgentEvent) {
        lock(&self.buffer).push(event);
    }
}

impl Sink for HttpSink {
    fn step_started(&mut self, index: usize, name: &str) {
        self.push(AgentEvent::StepStarted {
            index,
            name: name.to_owned(),
        });
    }

    fn line(&mut self, text: &str) {
        let mut end = text.len().min(MAX_LINE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = if end < text.len() {
            format!("{} [line cut]", &text[..end])
        } else {
            text.to_owned()
        };
        self.push(AgentEvent::Line { text });
    }

    fn annotation(&mut self, annotation: &Annotation) {
        self.push(AgentEvent::Annotation {
            annotation: annotation.clone(),
        });
    }

    fn step_finished(&mut self, index: usize, summary: &StepSummary) {
        self.push(AgentEvent::StepFinished {
            index,
            summary: summary.clone(),
        });
    }
}

/// The task that posts the queue.
pub struct Flusher {
    close: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl Flusher {
    /// Sends what is left, retrying for up to two minutes.
    ///
    /// # Errors
    ///
    /// When the control plane stayed unreachable. A job it stopped
    /// answering for is not an error: there is nothing left to send.
    pub async fn close(mut self) -> Result<()> {
        if let Some(close) = self.close.take() {
            // The task also closes when the sender is gone.
            let _ = close.send(());
        }
        self.task.await.context("the event flusher failed")?
    }
}

struct Task {
    client: Client,
    cancel: watch::Sender<u32>,
    buffer: Arc<Mutex<Buffer>>,
    timing: Timing,
    seq: u64,
    /// The batch last posted and not acknowledged, with the bytes it
    /// frees.
    inflight: Option<(Batch, usize)>,
    last_sent: Instant,
}

enum Flush {
    Sent,
    Idle,
    Failed(anyhow::Error),
    Stopped,
}

impl Task {
    async fn run(mut self, mut close: oneshot::Receiver<()>) -> Result<()> {
        let mut closing_since: Option<Instant> = None;
        let mut failures = 0u32;
        loop {
            if closing_since.is_none() {
                tokio::select! {
                    () = tokio::time::sleep(self.timing.flush) => {}
                    _ = &mut close => closing_since = Some(Instant::now()),
                }
            } else if failures > 0 {
                tokio::time::sleep(self.timing.flush).await;
            }
            let closing = closing_since.is_some();
            match self.flush(closing).await {
                Flush::Sent => failures = 0,
                Flush::Idle if closing => return Ok(()),
                Flush::Idle => {}
                Flush::Stopped => return Ok(()),
                Flush::Failed(err) => {
                    failures += 1;
                    if failures == 1 {
                        warn!("send events: {err:#}");
                    } else {
                        debug!("send events: {err:#}");
                    }
                    if let Some(since) = closing_since
                        && since.elapsed() >= self.timing.close
                    {
                        bail!("the last events were not delivered: {err:#}");
                    }
                }
            }
        }
    }

    /// Posts the batch in flight, or builds and posts the next one.
    async fn flush(&mut self, closing: bool) -> Flush {
        if self.inflight.is_none() {
            let (events, freed) = lock(&self.buffer).take();
            if events.is_empty() && (closing || self.last_sent.elapsed() < self.timing.heartbeat) {
                return Flush::Idle;
            }
            self.seq += 1;
            self.inflight = Some((
                Batch {
                    seq: self.seq,
                    events,
                },
                freed,
            ));
        }
        // Set above when it was empty.
        let Some((batch, freed)) = self.inflight.as_ref() else {
            return Flush::Idle;
        };
        match self.client.events(batch).await {
            Ok(reply) => {
                let freed = *freed;
                self.inflight = None;
                self.last_sent = Instant::now();
                lock(&self.buffer).acknowledge(freed);
                if reply.cancel > 0 {
                    self.raise(reply.cancel);
                }
                Flush::Sent
            }
            Err(err) if err.downcast_ref::<Stopped>().is_some() => {
                error!("{err:#}: the job ended or was reset, stopping it");
                lock(&self.buffer).stop();
                self.inflight = None;
                self.raise(KILL);
                Flush::Stopped
            }
            Err(err) => Flush::Failed(err),
        }
    }

    fn raise(&self, count: u32) {
        self.cancel.send_if_modified(|current| {
            if *current < count {
                *current = count;
                true
            } else {
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hatchrun_workflow::agent::EVENTS_PATH;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    const FAST: Timing = Timing {
        flush: Duration::from_millis(20),
        heartbeat: Duration::from_millis(300),
        close: Duration::from_millis(300),
    };

    fn summary() -> StepSummary {
        StepSummary {
            name: "build".into(),
            outcome: hatchrun_workflow::Conclusion::Success,
            conclusion: hatchrun_workflow::Conclusion::Success,
            duration_ms: 5,
        }
    }

    fn client(server: &MockServer) -> Client {
        Client::new(&server.uri(), "tok".into()).unwrap()
    }

    async fn ok(server: &MockServer, cancel: u32) {
        Mock::given(method("POST"))
            .and(path(EVENTS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "cancel": cancel })))
            .mount(server)
            .await;
    }

    async fn bodies(server: &MockServer, at_least: usize) -> Vec<Batch> {
        for _ in 0..400 {
            let requests = server.received_requests().await.unwrap();
            if requests.len() >= at_least {
                return requests
                    .iter()
                    .map(|request| serde_json::from_slice(&request.body).unwrap())
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fewer than {at_least} requests arrived");
    }

    fn lines(batch: &Batch) -> Vec<&str> {
        batch
            .events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Line { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn batches_lines_once_a_second() {
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (cancel, _rx) = watch::channel(0);
        let started = std::time::Instant::now();
        let (mut sink, _flusher) = HttpSink::start(client(&server), cancel);
        sink.line("a");
        sink.line("b");
        sink.line("c");
        let batches = bodies(&server, 1).await;
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert_eq!(batches[0].seq, 1);
        assert_eq!(lines(&batches[0]), ["a", "b", "c"]);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// 500 for the first `fail` requests, then 200.
    struct Flaky(AtomicUsize, usize);

    impl Respond for Flaky {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            if self.0.fetch_add(1, Ordering::SeqCst) < self.1 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({ "cancel": 0 }))
            }
        }
    }

    #[tokio::test]
    async fn retries_with_the_same_seq() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(Flaky(AtomicUsize::new(0), 1))
            .mount(&server)
            .await;
        let (cancel, _rx) = watch::channel(0);
        let (mut sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        sink.step_started(0, "build");
        sink.line("one");
        let first = bodies(&server, 2).await;
        assert_eq!(first[0], first[1]);
        assert_eq!(first[0].seq, 1);
        // A line written during the retries waits for the next batch.
        sink.line("two");
        let all = bodies(&server, 3).await;
        assert_eq!(all[2].seq, 2);
        assert_eq!(lines(&all[2]), ["two"]);
    }

    #[tokio::test]
    async fn cancel_from_reply_reaches_the_watch() {
        let server = MockServer::start().await;
        ok(&server, 1).await;
        let (cancel, mut rx) = watch::channel(0);
        let (mut sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        sink.line("x");
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*rx.borrow(), 1);
    }

    #[tokio::test]
    async fn heartbeat_when_idle() {
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (cancel, _rx) = watch::channel(0);
        let started = std::time::Instant::now();
        let (_sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        let batches = bodies(&server, 2).await;
        assert!(started.elapsed() >= FAST.heartbeat);
        assert_eq!(
            batches[0],
            Batch {
                seq: 1,
                events: Vec::new()
            }
        );
        assert_eq!(batches[1].seq, 2);
    }

    #[tokio::test]
    async fn unauthorized_stops_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let (cancel, mut rx) = watch::channel(0);
        let (mut sink, flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        sink.line("x");
        tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|n| *n == 2))
            .await
            .unwrap()
            .unwrap();
        sink.line("after");
        tokio::time::sleep(FAST.heartbeat * 2).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        flusher.close().await.unwrap();
    }

    #[tokio::test]
    async fn ended_job_stops_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let (cancel, mut rx) = watch::channel(0);
        let (_sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|n| *n == 2))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn drops_lines_past_the_cap_and_says_so() {
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (cancel, _rx) = watch::channel(0);
        let (mut sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        let text = "x".repeat(1000);
        let size = serde_json::to_vec(&AgentEvent::Line { text: text.clone() })
            .unwrap()
            .len()
            + 1;
        let step = serde_json::to_vec(&AgentEvent::StepStarted {
            index: 0,
            name: "build".into(),
        })
        .unwrap()
        .len()
            + 1;
        let kept = (MAX_PENDING_BYTES - step) / size;
        let total = kept + 50;
        sink.step_started(0, "build");
        for _ in 0..total {
            sink.line(&text);
        }
        sink.step_finished(0, &summary());
        // The flusher has not run yet: the cap decided everything.
        let batches = bodies(&server, 1).await;
        let notice = format!(
            "[hatchrun] {} lines were dropped while the control plane was unreachable",
            total - kept
        );
        assert_eq!(lines(&batches[0])[0], notice);
        assert!(matches!(
            batches[0].events.last().unwrap(),
            AgentEvent::Line { .. }
        ));
    }

    #[tokio::test]
    async fn steps_are_kept_when_lines_are_dropped() {
        let mut buffer = Buffer::default();
        let text = "x".repeat(MAX_LINE_BYTES);
        for _ in 0..(MAX_PENDING_BYTES / MAX_LINE_BYTES + 2) {
            buffer.push(AgentEvent::Line { text: text.clone() });
        }
        assert!(buffer.dropped > 0);
        buffer.push(AgentEvent::StepFinished {
            index: 3,
            summary: summary(),
        });
        let last = buffer.queue.back().unwrap();
        assert!(matches!(last.0, AgentEvent::StepFinished { index: 3, .. }));
    }

    #[tokio::test]
    async fn batches_stay_under_the_cap() {
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (cancel, _rx) = watch::channel(0);
        let (mut sink, flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        let text = "y".repeat(1000);
        for _ in 0..1000 {
            sink.line(&text);
        }
        flusher.close().await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(requests.len() >= 4);
        let mut sent = 0;
        for (n, request) in requests.iter().enumerate() {
            assert!(request.body.len() <= MAX_BATCH_BYTES + 64);
            let batch: Batch = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(batch.seq, n as u64 + 1);
            sent += batch.events.len();
        }
        assert_eq!(sent, 1000);
    }

    #[tokio::test]
    async fn long_lines_are_cut() {
        let (cancel, _rx) = watch::channel(0);
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (mut sink, _flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        sink.line(&"é".repeat(MAX_LINE_BYTES));
        let batches = bodies(&server, 1).await;
        let text = lines(&batches[0])[0];
        assert!(text.len() <= MAX_LINE_BYTES + 16);
        assert!(text.ends_with("[line cut]"));
    }

    #[tokio::test]
    async fn close_sends_what_is_left() {
        let server = MockServer::start().await;
        ok(&server, 0).await;
        let (cancel, _rx) = watch::channel(0);
        let (mut sink, flusher) = HttpSink::start(client(&server), cancel);
        sink.line("last");
        flusher.close().await.unwrap();
        let batches = bodies(&server, 1).await;
        assert_eq!(lines(&batches[0]), ["last"]);
    }

    #[tokio::test]
    async fn close_gives_up_when_the_control_plane_stays_down() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (cancel, _rx) = watch::channel(0);
        let (mut sink, flusher) = HttpSink::start_with(client(&server), cancel, FAST);
        sink.line("x");
        assert!(flusher.close().await.is_err());
    }
}
