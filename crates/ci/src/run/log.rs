// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The log of a job: its storage keys, the pages read from it, the
//! sockets that follow it and the upload to R2.

use hatchrun_workflow::Conclusion;
use hatchrun_workflow::agent::Batch;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{HttpMetadata, Request, Response, Result, WebSocketPair};

use crate::checks::{StepView, TAIL_LINES, conclusion_word};
use crate::graph::State as NodeState;
use crate::logs::{BACKLOG_BYTES, Compressor, LogIndex, LogWriter, index_key, log_key, tail};

use super::decide::{plan_is_current, take_drain};
use super::object::{Cx, answer, fail};
use super::{
    BUCKET, DEADLINES, Deadlines, JobState, READ_CHUNKS, RunInfo, Sealed, TAIL_BYTES,
    TAIL_SCAN_BYTES, UPLOAD_RETRY_MS,
};

/// The prefix of a job's numbered chunks.
pub(super) fn log_prefix(job: u64) -> String {
    format!("log:{job}:")
}

pub(super) fn chunk_key(job: u64, number: u32) -> String {
    format!("log:{job}:{number:08}")
}

/// The job's [`LogWriter`] without its open chunk.
pub(super) fn writer_key(job: u64) -> String {
    format!("logw:{job}")
}

/// The open chunk of the job's writer, as bytes.
pub(super) fn open_key(job: u64) -> String {
    format!("logo:{job}")
}

/// What the object keeps of a job's log from the end of the job until
/// the upload is done: the [`Sealed`] value.
pub(super) fn sealed_key(job: u64) -> String {
    format!("logx:{job}")
}

/// The failed tail of a job's check run.
pub(super) fn tail_key(job: u64) -> String {
    format!("tail:{job}")
}

/// The keys of a job's log other than its chunks.
pub(super) fn log_keys(job: u64) -> Vec<String> {
    vec![
        writer_key(job),
        open_key(job),
        sealed_key(job),
        tail_key(job),
    ]
}

/// What the job's sockets get for an applied batch.
pub(crate) fn events_message(batch: &Batch) -> String {
    json!({ "type": "events", "events": batch.events }).to_string()
}

/// What the job's sockets get when the job ends.
pub(crate) fn end_message(conclusion: Conclusion) -> String {
    json!({ "type": "end", "conclusion": conclusion_word(conclusion) }).to_string()
}

/// What a new socket gets first.
pub(crate) fn backlog_message(text: &str) -> String {
    json!({ "type": "backlog", "text": text }).to_string()
}

/// `text` without its first lines until at most `max` bytes are left,
/// and how many bytes that cut. The cut falls on a line start.
pub(crate) fn trim_front(text: &str, max: usize) -> (usize, &str) {
    if text.len() <= max {
        return (0, text);
    }
    let cut = text.len() - max;
    // The line that holds byte `cut` is partly gone, so its start is
    // not in the text any more: begin after its end.
    let start = text.as_bytes()[cut - 1..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(text.len(), |at| cut + at);
    (start, &text[start..])
}

/// Makes the upload wake the alarm now, unless it wakes sooner.
pub(crate) fn mark_upload_due(deadlines: &mut Deadlines, now_ms: u64) {
    deadlines.upload_retry_at = Some(
        deadlines
            .upload_retry_at
            .map_or(now_ms, |at| at.min(now_ms)),
    );
}

/// Whether the uploads may start: their time came, or `force`.
pub(crate) fn upload_claimable(deadlines: &Deadlines, now_ms: u64, force: bool) -> bool {
    deadlines
        .upload_retry_at
        .is_some_and(|at| force || at <= now_ms)
}

/// Where the data of byte offset `from` is among chunks of `sizes`
/// bytes: the first chunk that ends after it (the count of chunks when
/// `from` lies in the open chunk) and the offset where that chunk
/// starts.
pub(crate) fn chunks_from(sizes: &[u32], from: u64) -> (u32, u64) {
    let mut start = 0_u64;
    for (number, size) in sizes.iter().enumerate() {
        let end = start + u64::from(*size);
        if end > from {
            return (u32::try_from(number).unwrap_or(u32::MAX), start);
        }
        start = end;
    }
    (u32::try_from(sizes.len()).unwrap_or(u32::MAX), start)
}

/// `text` (which starts at byte offset `start`) from byte offset
/// `from`, at most `max` bytes: the offset the answer starts at and the
/// text. It starts at a character boundary and ends at a line end when
/// the cut is inside a line.
pub(crate) fn page_of(text: &str, start: u64, from: u64, max: usize) -> (u64, &str) {
    let mut skip = usize::try_from(from.saturating_sub(start))
        .unwrap_or(usize::MAX)
        .min(text.len());
    while !text.is_char_boundary(skip) {
        skip += 1;
    }
    let rest = &text[skip..];
    let mut end = max.min(rest.len());
    if end < rest.len() {
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        // A cut inside a line moves back to the last line end.
        if let Some(at) = rest[..end].rfind('\n') {
            end = at + 1;
        }
    }
    (start + skip as u64, &rest[..end])
}

/// When uploads that are still pending are tried again.
pub(crate) fn upload_deadline(pending: bool, now_ms: u64) -> Option<u64> {
    pending.then_some(now_ms + UPLOAD_RETRY_MS)
}

/// Where the lines for a failed job's check run lie in its log: the
/// start (`None` when no step is marked failed) and the end. The span is
/// the first failed step, or the whole log.
pub(crate) fn tail_span(index: &LogIndex, steps: &[StepView]) -> (Option<u64>, u64) {
    let failed = steps
        .iter()
        .find(|step| {
            step.summary
                .as_ref()
                .is_some_and(|s| s.conclusion == Conclusion::Failure)
        })
        .and_then(|step| index.steps.iter().position(|mark| mark.index == step.index));
    match failed {
        Some(at) => (
            Some(index.steps[at].offset),
            index.steps.get(at + 1).map_or(index.bytes, |m| m.offset),
        ),
        None => (None, index.bytes),
    }
}

/// The lines of a failed job's log for its check run. `text` is the log
/// from byte `base` on, up to at least the end of the span of
/// [`tail_span`]. At most [`TAIL_LINES`] lines and [`TAIL_BYTES`]
/// bytes.
pub(crate) fn failed_tail(
    index: &LogIndex,
    steps: &[StepView],
    base: u64,
    text: &str,
) -> Vec<String> {
    let (from, to) = tail_span(index, steps);
    let relative = |at: u64| {
        usize::try_from(at.saturating_sub(base)).map_or(text.len(), |at| at.min(text.len()))
    };
    let within = text.get(..relative(to)).unwrap_or(text);
    let mut lines = tail(within, relative(from.unwrap_or(base)) as u64, TAIL_LINES);
    let mut size: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut cut = 0;
    while size > TAIL_BYTES && cut < lines.len() {
        size -= lines[cut].len() + 1;
        cut += 1;
    }
    lines.drain(..cut);
    lines
}

impl Cx<'_> {
    pub(super) async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        loop {
            let options = worker::durable::ListOptions::new()
                .prefix(prefix)
                .limit(128);
            let found = self.storage.list_with_options(options).await?;
            let keys: Vec<String> = found
                .keys()
                .into_iter()
                .filter_map(|k| k.ok()?.as_string())
                .collect();
            if keys.is_empty() {
                return Ok(());
            }
            self.storage.delete_multiple(keys).await?;
        }
    }

    pub(super) fn raw_set(object: &js_sys::Object, key: &str, bytes: &[u8]) -> Result<()> {
        let value = js_sys::Uint8Array::from(bytes);
        js_sys::Reflect::set(object, &JsValue::from_str(key), &value)
            .map(|_| ())
            .map_err(|err| fail(format!("{key}: {err:?}")))
    }

    /// Reads values stored as bytes, in the order of `keys`.
    pub(super) async fn raw_get(&self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
        let found = self
            .storage
            .get_multiple(keys.iter().map(String::as_str).collect::<Vec<_>>())
            .await?;
        Ok(keys
            .iter()
            .map(|key| {
                let value = found.get(&JsValue::from_str(key));
                (!value.is_undefined()).then(|| js_sys::Uint8Array::new(&value).to_vec())
            })
            .collect())
    }

    /// The job's log writer. Log text is stored as bytes: a text value
    /// takes two bytes per character once one character is outside
    /// Latin-1, which would break the 128 KiB limit of a value.
    pub(super) async fn load_writer(&self, job: u64) -> Result<Option<LogWriter>> {
        let mut found = self
            .raw_get(&[writer_key(job), open_key(job)])
            .await?
            .into_iter();
        let (Some(Some(meta)), open) = (found.next(), found.next().flatten()) else {
            return Ok(None);
        };
        let writer: LogWriter = serde_json::from_slice(&meta)
            .map_err(|err| fail(format!("job {job}: log writer: {err}")))?;
        let open = open.map_or_else(String::new, |bytes| {
            String::from_utf8_lossy(&bytes).into_owned()
        });
        Ok(Some(writer.with_open(open)))
    }

    /// Writes the full chunks the writer gave up and what it still
    /// holds, in one write.
    pub(super) async fn save_log(
        &self,
        job: u64,
        writer: &LogWriter,
        chunks: &[(u32, String)],
    ) -> Result<()> {
        let object = js_sys::Object::new();
        for (number, text) in chunks {
            Self::raw_set(&object, &chunk_key(job, *number), text.as_bytes())?;
        }
        let meta = serde_json::to_vec(writer).map_err(|err| fail(err.to_string()))?;
        Self::raw_set(&object, &writer_key(job), &meta)?;
        Self::raw_set(&object, &open_key(job), writer.open_text().as_bytes())?;
        self.storage.put_multiple_raw(object).await
    }

    pub(super) async fn sealed(&self, job: u64) -> Result<Option<Sealed>> {
        let Some(Some(bytes)) = self.raw_get(&[sealed_key(job)]).await?.pop() else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| fail(format!("job {job}: sealed log: {err}")))
    }

    /// The text of the log between `from_needed` and `to`, read from the
    /// newest chunk back, and the byte offset it starts at. The chunks
    /// hold `chunk_bytes` bytes together; a chunk that starts at or
    /// after `to` is read past and not kept. Stops at a chunk that is
    /// gone.
    pub(super) async fn read_window(
        &self,
        job: u64,
        chunks: u32,
        chunk_bytes: u64,
        to: u64,
        from_needed: u64,
    ) -> Result<(u64, String)> {
        let mut base = chunk_bytes;
        let mut parts = Vec::new();
        let mut next = chunks;
        'read: while next > 0 && base > from_needed {
            let first = next.saturating_sub(4);
            let keys: Vec<String> = (first..next).map(|n| chunk_key(job, n)).collect();
            for bytes in self.raw_get(&keys).await?.into_iter().rev() {
                let Some(bytes) = bytes else {
                    break 'read;
                };
                base = base.saturating_sub(bytes.len() as u64);
                if base >= to {
                    continue;
                }
                parts.push(String::from_utf8_lossy(&bytes).into_owned());
                if base <= from_needed {
                    break 'read;
                }
            }
            next = first;
        }
        parts.reverse();
        Ok((base, parts.concat()))
    }

    /// Up to [`BACKLOG_BYTES`] of the newest text of a job's log and the
    /// offset it starts at, `None` when the job has no log in storage.
    pub(super) async fn newest(&self, job: u64) -> Result<Option<(u64, String)>> {
        let (total, chunks, open) = if let Some(writer) = self.load_writer(job).await? {
            (
                writer.bytes(),
                writer.chunk_count(),
                writer.open_text().to_owned(),
            )
        } else if let Some(sealed) = self.sealed(job).await? {
            (sealed.index.bytes, sealed.chunks, String::new())
        } else {
            return Ok(None);
        };
        let in_chunks = total.saturating_sub(open.len() as u64);
        let (base, mut text) = self
            .read_window(
                job,
                chunks,
                in_chunks,
                u64::MAX,
                total.saturating_sub(BACKLOG_BYTES),
            )
            .await?;
        text.push_str(&open);
        let (skipped, kept) =
            trim_front(&text, usize::try_from(BACKLOG_BYTES).unwrap_or(usize::MAX));
        Ok(Some((base + skipped as u64, kept.to_owned())))
    }

    /// Up to [`BACKLOG_BYTES`] of a job's stored log from byte offset
    /// `from` on and the offset it starts at (the next page starts at
    /// that offset plus the text's length). `None` when the job has no
    /// log in storage.
    pub(super) async fn page(&self, job: u64, from: u64) -> Result<Option<(u64, String)>> {
        let (sizes, open) = if let Some(writer) = self.load_writer(job).await? {
            (writer.chunk_sizes().to_vec(), writer.open_text().to_owned())
        } else if let Some(sealed) = self.sealed(job).await? {
            (sealed.sizes, String::new())
        } else {
            return Ok(None);
        };
        let max = usize::try_from(BACKLOG_BYTES).unwrap_or(usize::MAX);
        let (first, start) = chunks_from(&sizes, from);
        let mut text = String::new();
        let mut next = first;
        let count = u32::try_from(sizes.len()).unwrap_or(u32::MAX);
        'read: while next < count && text.len() < max {
            let last = (next + 4).min(count);
            let keys: Vec<String> = (next..last).map(|n| chunk_key(job, n)).collect();
            for bytes in self.raw_get(&keys).await? {
                let Some(bytes) = bytes else {
                    break 'read;
                };
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
            next = last;
        }
        if next >= count {
            text.push_str(&open);
        }
        let (at, page) = page_of(&text, start, from, max);
        Ok(Some((at, page.to_owned())))
    }

    /// `GET /live`: a WebSocket that gets the newest stored text, then
    /// every applied batch. The backlog is read and the socket accepted
    /// under one lock, so no batch falls between them.
    pub(super) async fn live(&self, req: &Request, job: u64) -> Result<Response> {
        let upgrade = req.headers().get("Upgrade")?;
        if !upgrade.is_some_and(|value| value.eq_ignore_ascii_case("websocket")) {
            return Response::error("a WebSocket upgrade is needed", 426);
        }
        let _turn = self.turn.lock().await;
        let Some(run) = self.loaded().await? else {
            return answer(404);
        };
        match run.graph.node(job) {
            None => return answer(404),
            Some(node) if matches!(node.state, NodeState::Done(_)) => return answer(409),
            Some(_) => {}
        }
        let (_, text) = self.newest(job).await?.unwrap_or_default();
        let pair = WebSocketPair::new()?;
        self.state
            .accept_websocket_with_tags(&pair.server, &[&job.to_string()]);
        pair.server.send_with_str(backlog_message(&text))?;
        Response::from_websocket(pair.client)
    }

    /// `GET /log`: up to 2 MiB of the stored text of a job's log, from
    /// `?offset=` when given and else the newest. `x-log-offset` is
    /// where the text starts and `x-log-next` where the next page
    /// starts, so a viewer can page through the whole stored text.
    pub(super) async fn log(&self, job: u64, offset: Option<u64>) -> Result<Response> {
        let _turn = self.turn.lock().await;
        let Some(run) = self.loaded().await? else {
            return answer(404);
        };
        let Some(node) = run.graph.node(job) else {
            return answer(404);
        };
        let found = match offset {
            Some(from) => self.page(job, from).await?,
            None => self.newest(job).await?,
        };
        let (offset, text) = match found {
            Some(found) => found,
            None if matches!(node.state, NodeState::Done(_)) => return answer(404),
            None => (0, String::new()),
        };
        let text_len = text.len() as u64;
        let mut response = Response::ok(text)?;
        response
            .headers_mut()
            .set("content-type", "text/plain; charset=utf-8")?;
        let next = offset + text_len;
        response
            .headers_mut()
            .set("x-log-offset", &offset.to_string())?;
        response
            .headers_mut()
            .set("x-log-next", &next.to_string())?;
        Ok(response)
    }

    /// Sends `message()` to the job's sockets, when it has any.
    pub(super) fn send_sockets(&self, job: u64, message: impl FnOnce() -> String) {
        let sockets = self.state.get_websockets_with_tag(&job.to_string());
        if sockets.is_empty() {
            return;
        }
        let message = message();
        for socket in sockets {
            if let Err(err) = socket.send_with_str(&message) {
                worker::console_warn!("job {job}: socket: {err}");
            }
        }
    }

    /// Tells the job's sockets how it ended and closes them.
    pub(super) fn end_sockets(&self, job: u64, conclusion: Conclusion) {
        self.send_sockets(job, || end_message(conclusion));
        for socket in self.state.get_websockets_with_tag(&job.to_string()) {
            if let Err(err) = socket.close(Some(1000), Some("job ended")) {
                worker::console_warn!("job {job}: close: {err}");
            }
        }
    }

    /// Ends the job's log: the last chunk and the index are stored with
    /// the rest of the log kept for the upload, and for a failed job
    /// the tail of its check run is read. Answers the key the log will
    /// have in R2, `None` when the job wrote none. Sealing twice gives
    /// the same answer. The caller holds the lock.
    pub(super) async fn seal_log(
        &self,
        info: &RunInfo,
        job: u64,
        attempt: u32,
        conclusion: Conclusion,
        state: &JobState,
    ) -> Result<Option<String>> {
        let sealed = if let Some(mut writer) = self.load_writer(job).await? {
            let (last, index) = writer.finish();
            let sealed = Sealed {
                index,
                chunks: writer.chunk_count(),
                sizes: writer.chunk_sizes().to_vec(),
                attempt,
            };
            let object = js_sys::Object::new();
            if let Some((number, text)) = &last {
                Self::raw_set(&object, &chunk_key(job, *number), text.as_bytes())?;
            }
            let bytes = serde_json::to_vec(&sealed).map_err(|err| fail(err.to_string()))?;
            Self::raw_set(&object, &sealed_key(job), &bytes)?;
            self.storage.put_multiple_raw(object).await?;
            self.storage
                .delete_multiple(vec![writer_key(job), open_key(job)])
                .await?;
            sealed
        } else if let Some(sealed) = self.sealed(job).await? {
            sealed
        } else {
            return Ok(None);
        };
        if conclusion == Conclusion::Failure {
            let (from, to) = tail_span(&sealed.index, &state.steps);
            let needed = to.saturating_sub(TAIL_SCAN_BYTES).max(from.unwrap_or(0));
            let (base, text) = self
                .read_window(job, sealed.chunks, sealed.index.bytes, to, needed)
                .await?;
            let lines = failed_tail(&sealed.index, &state.steps, base, &text);
            self.storage.put(&tail_key(job), &lines).await?;
        }
        Ok(Some(log_key(
            info.retention_days,
            &info.tenant_id,
            info.repo.id,
            info.run_id,
            job,
            attempt,
        )))
    }

    /// Uploads every finished log that is not in R2 yet, when the
    /// retry time has come (`force` skips that check). One caller at a
    /// time: a second one finds the work claimed and returns. Each
    /// upload is planned under the lock, done without it and recorded
    /// under it again. A log that fails stays pending and is tried again
    /// in 30 s; an error is only a storage failure.
    pub(super) async fn upload_logs(&self, force: bool) -> Result<()> {
        if !take_drain(self.uploading) {
            return Ok(());
        }
        let done = self.upload_pending(force).await;
        self.uploading.set(false);
        done
    }

    pub(super) async fn upload_pending(&self, force: bool) -> Result<()> {
        let (info, jobs) = {
            let _turn = self.turn.lock().await;
            let deadlines: Deadlines = self.storage.get(DEADLINES).await?.unwrap_or_default();
            if !upload_claimable(&deadlines, self.now, force) {
                return Ok(());
            }
            let Some(mut run) = self.loaded().await? else {
                return Ok(());
            };
            let mut jobs = Vec::new();
            for node in &run.graph.jobs {
                if self.job_state(node.id).await?.upload_pending {
                    jobs.push((node.id, node.attempt, self.sealed(node.id).await?));
                }
            }
            // Claimed: the next caller finds the time in the future.
            run.deadlines_mut().upload_retry_at = upload_deadline(!jobs.is_empty(), self.now);
            self.commit(&mut run).await?;
            if jobs.is_empty() {
                return Ok(());
            }
            (run.info, jobs)
        };
        for (job, attempt, sealed) in jobs {
            let uploaded = match &sealed {
                Some(sealed) => self.upload_one(&info, job, sealed).await,
                None => Ok(()),
            };
            match uploaded {
                Ok(()) => self.uploaded(job, attempt, sealed.as_ref()).await?,
                Err(err) => worker::console_error!("job {job}: log upload: {err}"),
            }
        }
        let now = self.now;
        let _turn = self.turn.lock().await;
        let Some(mut run) = self.loaded().await? else {
            return Ok(());
        };
        let mut pending = false;
        for node in &run.graph.jobs {
            pending |= self.job_state(node.id).await?.upload_pending;
        }
        run.deadlines_mut().upload_retry_at = upload_deadline(pending, now);
        self.commit(&mut run).await
    }

    /// Records a finished upload: the chunks and what goes with them
    /// are deleted, unless a rerun made them stale meanwhile.
    pub(super) async fn uploaded(
        &self,
        job: u64,
        attempt: u32,
        sealed: Option<&Sealed>,
    ) -> Result<()> {
        let _turn = self.turn.lock().await;
        let Some(run) = self.loaded().await? else {
            return Ok(());
        };
        if !plan_is_current(run.graph.node(job), attempt) {
            return Ok(());
        }
        let mut state = self.job_state(job).await?;
        if !state.upload_pending {
            return Ok(());
        }
        state.upload_pending = false;
        self.put_job(job, &state).await?;
        if let Some(sealed) = sealed {
            for first in (0..sealed.chunks).step_by(READ_CHUNKS as usize) {
                let last = (first + READ_CHUNKS).min(sealed.chunks);
                let keys: Vec<String> = (first..last).map(|n| chunk_key(job, n)).collect();
                self.storage.delete_multiple(keys).await?;
            }
        }
        self.storage.delete(&sealed_key(job)).await?;
        Ok(())
    }

    /// Writes one job's gzipped log and its index to R2. The chunks are
    /// read a few at a time, so the text is never held whole.
    pub(super) async fn upload_one(&self, info: &RunInfo, job: u64, sealed: &Sealed) -> Result<()> {
        let mut gzip = Compressor::new();
        for first in (0..sealed.chunks).step_by(READ_CHUNKS as usize) {
            let last = (first + READ_CHUNKS).min(sealed.chunks);
            let keys: Vec<String> = (first..last).map(|n| chunk_key(job, n)).collect();
            for (key, bytes) in keys.iter().zip(self.raw_get(&keys).await?) {
                let bytes = bytes.ok_or_else(|| fail(format!("chunk {key} is gone")))?;
                gzip.push(&String::from_utf8_lossy(&bytes));
            }
        }
        let bucket = self.env.bucket(BUCKET)?;
        bucket
            .put(
                log_key(
                    info.retention_days,
                    &info.tenant_id,
                    info.repo.id,
                    info.run_id,
                    job,
                    sealed.attempt,
                ),
                gzip.finish(),
            )
            .http_metadata(HttpMetadata {
                content_type: Some("text/plain; charset=utf-8".to_owned()),
                content_encoding: Some("gzip".to_owned()),
                ..HttpMetadata::default()
            })
            .execute()
            .await?;
        let index = serde_json::to_vec(&sealed.index).map_err(|err| fail(err.to_string()))?;
        bucket
            .put(
                index_key(
                    info.retention_days,
                    &info.tenant_id,
                    info.repo.id,
                    info.run_id,
                    job,
                    sealed.attempt,
                ),
                index,
            )
            .http_metadata(HttpMetadata {
                content_type: Some("application/json".to_owned()),
                ..HttpMetadata::default()
            })
            .execute()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::Value;

    use super::super::Todo;
    use super::super::decide::{
        snapshot_ready,
        tests::{batch, graph, line, summary},
    };
    use super::*;
    use crate::checks::Outbox;

    #[test]
    fn sockets_get_batches_and_the_end_as_json() {
        let message: Value =
            serde_json::from_str(&events_message(&batch(1, vec![line("hi")]))).unwrap();
        assert_eq!(message["type"], "events");
        assert_eq!(message["events"][0]["type"], "line");
        assert_eq!(message["events"][0]["text"], "hi");
        let end: Value = serde_json::from_str(&end_message(Conclusion::Failure)).unwrap();
        assert_eq!(end, json!({ "type": "end", "conclusion": "failure" }));
        let backlog: Value = serde_json::from_str(&backlog_message("a\nb")).unwrap();
        assert_eq!(backlog, json!({ "type": "backlog", "text": "a\nb" }));
    }

    #[test]
    fn trim_front_cuts_on_a_line_start() {
        assert_eq!(trim_front("a\nb\n", 10), (0, "a\nb\n"));
        // Exactly the last line is left when it fits.
        assert_eq!(trim_front("aaa\nbb\n", 3), (4, "bb\n"));
        // A cut inside a line drops that line.
        assert_eq!(trim_front("aaa\nbbbb\ncc\n", 7), (9, "cc\n"));
        // A cut right at a line start keeps the line.
        assert_eq!(trim_front("aaa\nbbb\n", 4), (4, "bbb\n"));
        assert_eq!(trim_front("aaaaaa", 3), (6, ""));
    }

    #[test]
    fn a_log_that_ends_arms_the_upload_and_a_pending_one_retries() {
        let mut deadlines = Deadlines::default();
        mark_upload_due(&mut deadlines, 500);
        assert_eq!(deadlines.upload_retry_at, Some(500));
        mark_upload_due(&mut deadlines, 900);
        assert_eq!(deadlines.upload_retry_at, Some(500));
        assert_eq!(upload_deadline(true, 1_000), Some(31_000));
        assert_eq!(upload_deadline(false, 1_000), None);
        let mut ended = graph();
        ended.cancel(&json!({}));
        let ready = |deadlines: &Deadlines| {
            snapshot_ready(&ended, &Outbox::default(), deadlines, &Todo::default())
        };
        assert!(ready(&Deadlines::default()));
        assert!(!ready(&deadlines));
    }

    fn log_of_two_steps() -> (LogIndex, String) {
        let mut writer = LogWriter::default();
        writer.step_started(0, "setup");
        writer.line("ok");
        writer.step_started(1, "test");
        writer.line("a");
        writer.line("error: boom");
        writer.step_started(2, "cleanup");
        writer.line("done");
        let (last, index) = writer.finish();
        (index, last.unwrap().1)
    }

    fn step_view(index: usize, conclusion: Conclusion) -> StepView {
        let mut step = summary("x");
        step.conclusion = conclusion;
        StepView {
            index,
            name: "x".to_owned(),
            summary: Some(step),
        }
    }

    #[test]
    fn the_failed_tail_is_the_end_of_the_failed_step() {
        let (index, text) = log_of_two_steps();
        let steps = [
            step_view(0, Conclusion::Success),
            step_view(1, Conclusion::Failure),
            step_view(2, Conclusion::Success),
        ];
        assert_eq!(
            failed_tail(&index, &steps, 0, &text),
            vec!["== test", "a", "error: boom"]
        );
        // The same from a window that starts inside the step.
        let base = text.find("a\nerror").unwrap();
        assert_eq!(
            failed_tail(&index, &steps, base as u64, &text[base..]),
            vec!["a", "error: boom"]
        );
    }

    #[test]
    fn without_a_failed_step_the_tail_is_the_end_of_the_log() {
        let (index, text) = log_of_two_steps();
        let steps = [step_view(0, Conclusion::Success)];
        assert_eq!(tail_span(&index, &steps), (None, index.bytes));
        assert_eq!(
            failed_tail(&index, &steps, 0, &text),
            vec![
                "== setup",
                "ok",
                "== test",
                "a",
                "error: boom",
                "== cleanup",
                "done"
            ]
        );
    }

    #[test]
    fn the_failed_tail_is_bounded() {
        let mut writer = LogWriter::default();
        writer.step_started(0, "t");
        for i in 0..100 {
            writer.line(&format!("{i:0>4000}"));
        }
        let (last, index) = writer.finish();
        let text = last.unwrap().1;
        let lines = failed_tail(&index, &[step_view(0, Conclusion::Failure)], 0, &text);
        assert!(lines.len() <= TAIL_LINES);
        assert!(lines.iter().map(|l| l.len() + 1).sum::<usize>() <= TAIL_BYTES);
        assert_eq!(lines.last().unwrap(), &format!("{:0>4000}", 99));
    }

    #[test]
    fn log_keys_do_not_overlap_the_chunk_prefix() {
        assert_eq!(chunk_key(4, 7), "log:4:00000007");
        assert!(chunk_key(4, 7).starts_with(&log_prefix(4)));
        for key in log_keys(4) {
            assert!(!key.starts_with(&log_prefix(4)), "{key}");
        }
        assert!(!chunk_key(14, 0).starts_with(&log_prefix(4)));
    }

    #[test]
    fn uploads_start_when_their_time_came_or_when_forced() {
        let at = |at| Deadlines {
            upload_retry_at: at,
            ..Deadlines::default()
        };
        assert!(!upload_claimable(&at(None), 100, false));
        assert!(!upload_claimable(&at(None), 100, true));
        assert!(upload_claimable(&at(Some(100)), 100, false));
        assert!(!upload_claimable(&at(Some(101)), 100, false));
        assert!(upload_claimable(&at(Some(101)), 100, true));
    }

    #[test]
    fn one_upload_runs_at_a_time() {
        let flag = Cell::new(false);
        assert!(take_drain(&flag));
        assert!(!take_drain(&flag));
        flag.set(false);
        assert!(take_drain(&flag));
    }

    #[test]
    fn a_claimed_upload_waits_for_the_retry_time() {
        let mut deadlines = Deadlines::default();
        mark_upload_due(&mut deadlines, 1_000);
        assert!(upload_claimable(&deadlines, 1_000, false));
        deadlines.upload_retry_at = upload_deadline(true, 1_000);
        assert!(!upload_claimable(&deadlines, 1_000, false));
        assert!(!upload_claimable(&deadlines, 30_999, false));
        assert!(upload_claimable(&deadlines, 31_000, false));
    }

    #[test]
    fn chunks_from_finds_the_chunk_of_an_offset() {
        let sizes = [10, 20, 5];
        assert_eq!(chunks_from(&sizes, 0), (0, 0));
        assert_eq!(chunks_from(&sizes, 9), (0, 0));
        assert_eq!(chunks_from(&sizes, 10), (1, 10));
        assert_eq!(chunks_from(&sizes, 34), (2, 30));
        // In the open chunk, and past the end.
        assert_eq!(chunks_from(&sizes, 35), (3, 35));
        assert_eq!(chunks_from(&sizes, 99), (3, 35));
        assert_eq!(chunks_from(&[], 4), (0, 0));
    }

    #[test]
    fn pages_walk_the_whole_text() {
        let text = "aaaa\nbbbb\ncccc\ndd\n";
        let mut from = 0;
        let mut pages = Vec::new();
        loop {
            let (at, page) = page_of(text, 0, from, 7);
            assert_eq!(at, from);
            if page.is_empty() {
                break;
            }
            pages.push(page);
            from = at + page.len() as u64;
        }
        assert_eq!(pages, ["aaaa\n", "bbbb\n", "cccc\n", "dd\n"]);
        assert_eq!(pages.concat(), text);
    }

    #[test]
    fn a_page_starts_inside_a_chunk_and_on_a_character() {
        // The text starts at byte 100; offset 103 is inside "\u{e9}".
        let text = "x\u{e9}y\nz\n";
        let (at, page) = page_of(text, 100, 102, 100);
        assert_eq!(at, 103);
        assert_eq!(page, "y\nz\n");
        // One line longer than the page is cut at a character.
        let (at, page) = page_of("\u{e9}\u{e9}\u{e9}", 0, 0, 3);
        assert_eq!((at, page), (0, "\u{e9}"));
        assert_eq!(page_of("ab", 0, 50, 10), (2, ""));
    }
}
