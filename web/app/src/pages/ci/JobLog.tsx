// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { ApiError, getCiLog, getCiLogIndex, type CiJob } from "@/api";
import { LoadError, Spinner } from "@/components/Status";
import { LogViewer } from "./LogViewer";
import { MemoryLog, StoredLog, type LogEvent, type LogSource } from "./logSource";

const MOST_RECONNECT_MS = 15_000;

type Message = { kind: "backlog"; text: string } | { kind: "events"; events: LogEvent[] } | { kind: "end" };

function parseMessage(data: unknown): Message | null {
  if (typeof data !== "string") {
    return null;
  }
  try {
    const message = JSON.parse(data) as { type?: string; text?: string; events?: LogEvent[] };
    if (message.type === "backlog" && typeof message.text === "string") {
      return { kind: "backlog", text: message.text };
    }
    if (message.type === "events" && Array.isArray(message.events)) {
      return { kind: "events", events: message.events };
    }
    if (message.type === "end") {
      return { kind: "end" };
    }
  } catch {
    // Not a message of ours.
  }
  return null;
}

/** How the steps of the job ended, by step index. */
function conclusions(job: CiJob) {
  const out = new Map<number, { conclusion: string | null; durationMs: number | null }>();
  job.steps.forEach((step, i) => {
    out.set(step.index ?? i, {
      conclusion: step.summary?.conclusion ?? step.conclusion ?? null,
      durationMs: step.summary?.duration_ms ?? step.duration_ms ?? null,
    });
  });
  return out;
}

type Feed = "loading" | "live" | "reconnecting" | "done" | { error: Error };

/**
 * A job's log. A job that ended reads its stored log by range, using
 * the index; one that runs opens the live socket (backlog first, then
 * `events`) and, on `end`, asks the page to read the job again, after
 * which the finished log shows.
 */
export function JobLog({ job, onCheck }: { job: CiJob; onCheck: () => void }) {
  const completed = job.status === "completed";
  const jobId = job.id;
  const [stored, setStored] = useState<LogSource | null>(null);
  const [memory] = useState(() => new MemoryLog());
  const source = completed ? stored : memory;
  const [feed, setFeed] = useState<Feed>("loading");
  const [downloading, setDownloading] = useState(false);
  const [round, setRound] = useState(0);
  // The latest job and callback, for effects that must not restart when they change.
  const latest = useRef({ job, onCheck });
  useLayoutEffect(() => {
    latest.current = { job, onCheck };
  });

  useEffect(() => {
    let stopped = false;
    let socket: WebSocket | null = null;
    let timer: number | undefined;

    const finished = async () => {
      setFeed("loading");
      try {
        const index = await getCiLogIndex(jobId);
        const marks = conclusions(latest.current.job);
        if (stopped) {
          return;
        }
        if (index !== null) {
          setStored(new StoredLog(jobId, index, marks));
        } else {
          // No index: read the whole log and find the steps by their headers.
          const whole = await getCiLog(jobId);
          if (stopped) {
            return;
          }
          setStored(MemoryLog.fromText(whole.text, marks));
        }
        setFeed("done");
      } catch (err) {
        if (!stopped) {
          setFeed({ error: err instanceof Error ? err : new Error(String(err)) });
        }
      }
    };

    if (completed) {
      void finished();
      return () => {
        stopped = true;
      };
    }

    let attempts = 0;
    const open = () => {
      const scheme = window.location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${scheme}://${window.location.host}/api/ci/jobs/${jobId}/live`);
      socket = ws;
      let ended = false;
      ws.onmessage = (event: MessageEvent) => {
        const message = parseMessage(event.data);
        if (message === null || stopped) {
          return;
        }
        attempts = 0;
        if (message.kind === "backlog") {
          memory.load(message.text);
          setFeed("live");
        } else if (message.kind === "events") {
          memory.apply(message.events);
        } else {
          ended = true;
          // The row may trail the object by a moment: ask a few times.
          const poll = (left: number) => {
            latest.current.onCheck();
            if (left > 0) {
              timer = window.setTimeout(() => poll(left - 1), 1500);
            }
          };
          poll(5);
        }
      };
      ws.onclose = () => {
        if (stopped || ended) {
          return;
        }
        // The socket answers 409 once the job ended, which closes it
        // before any message. Read the job again: if it ended, this
        // effect runs again for the finished log; if not, reconnect.
        setFeed("reconnecting");
        latest.current.onCheck();
        timer = window.setTimeout(open, Math.min(1000 * 2 ** attempts++, MOST_RECONNECT_MS));
      };
    };
    open();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      if (socket) {
        socket.onclose = null;
        socket.onmessage = null;
        socket.close();
      }
    };
  }, [jobId, completed, round, memory]);

  const download = async () => {
    if (source === null) {
      return;
    }
    setDownloading(true);
    try {
      const url = URL.createObjectURL(new Blob([await source.all()], { type: "text/plain" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = `${job.name.replace(/[^A-Za-z0-9._-]+/g, "-")}-${job.id}.log`;
      link.click();
      window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
    } catch {
      // The log stays on screen; the button works again.
    } finally {
      setDownloading(false);
    }
  };

  if (typeof feed === "object") {
    if (feed.error instanceof ApiError && feed.error.status === 404) {
      return <p className="muted">This job has no log.</p>;
    }
    return <LoadError error={feed.error} retry={() => setRound((n) => n + 1)} />;
  }
  if (source === null || (feed === "loading" && completed)) {
    return <Spinner label="Loading log" />;
  }
  if (!completed && feed === "loading" && source.lines === 0) {
    return <Spinner label="Waiting for the first line" />;
  }
  return (
    <LogViewer
      key={`${jobId}:${completed}`}
      source={source}
      name={job.name}
      live={!completed}
      feed={typeof feed === "string" ? feed : "done"}
      failed={job.conclusion === "failure" || job.conclusion === "timed_out"}
      onRetry={() => (source instanceof StoredLog ? source.retry() : setRound((n) => n + 1))}
      download={() => void download()}
      downloading={downloading}
    />
  );
}
