// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { memo, useEffect, useLayoutEffect, useReducer, useRef, useState } from "react";

import { ApiError, getCiLog, getCiLogIndex, type CiJob } from "@/api";
import { LoadError, Spinner } from "@/components/Status";
import { cn } from "@/lib/utils";
import { segments } from "./ansi";
import { BLOCK_LINES, EMPTY, reduce, type LogEvent, type Section } from "./logModel";

/** Blocks of a step shown until "Show earlier lines" is pressed. */
const TAIL_BLOCKS = 6;
/** Total lines up to which every step starts open. */
const OPEN_ALL_UNDER = 400;
const MOST_RECONNECT_MS = 15_000;

type Socket = { kind: "backlog"; text: string } | { kind: "events"; events: LogEvent[] } | { kind: "end" };

function parseMessage(data: unknown): Socket | null {
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

/** How the steps of a finished job ended, by position. */
function conclusions(job: CiJob): Map<number, string> {
  const out = new Map<number, string>();
  job.steps.forEach((step, i) => {
    const conclusion = step.summary?.conclusion ?? step.conclusion;
    if (conclusion) {
      out.set(step.index ?? i, conclusion);
    }
  });
  return out;
}

type Feed = "loading" | "live" | "reconnecting" | "done" | { error: Error };

/**
 * A job's log, one collapsible section per step. A job that ended shows
 * its finished log with the step index. A running job opens the live
 * socket: the backlog first, then every `events` message; on `end` it
 * asks the page to read the job again and then shows the finished log.
 */
export function JobLog({ job, onCheck }: { job: CiJob; onCheck: () => void }) {
  const [log, dispatch] = useReducer(reduce, EMPTY);
  const [feed, setFeed] = useState<Feed>("loading");
  const [follow, setFollow] = useState(true);
  const completed = job.status === "completed";
  const jobId = job.id;
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
        const [text, index] = await Promise.all([getCiLog(jobId), getCiLogIndex(jobId)]);
        if (!stopped) {
          dispatch({ type: "load", text: text.text, index, conclusions: conclusions(latest.current.job) });
          setFeed("done");
        }
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
          dispatch({ type: "load", text: message.text, index: null, conclusions: new Map() });
          setFeed("live");
        } else if (message.kind === "events") {
          dispatch({ type: "events", events: message.events });
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
  }, [jobId, completed]);

  const sentinel = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (follow && !completed) {
      sentinel.current?.scrollIntoView({ block: "end" });
    }
  }, [log, follow, completed]);
  useEffect(() => {
    // Scrolling up ends following; the checkbox turns it back on.
    const stop = (event: WheelEvent) => event.deltaY < 0 && setFollow(false);
    window.addEventListener("wheel", stop, { passive: true });
    return () => window.removeEventListener("wheel", stop);
  }, []);

  if (typeof feed === "object") {
    if (feed.error instanceof ApiError && feed.error.status === 404) {
      return <p className="muted">This job has no log.</p>;
    }
    return <LoadError error={feed.error} />;
  }
  const total = log.sections.reduce((sum, section) => sum + section.lines, 0);
  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center justify-between font-mono text-xs text-fog">
        <span aria-live="polite">
          {feed === "live" && "Live"}
          {feed === "reconnecting" && "Reconnecting"}
          {feed === "done" && `${total} ${total === 1 ? "line" : "lines"}`}
        </span>
        {!completed && (
          <label className="flex items-center gap-1.5">
            <input type="checkbox" checked={follow} onChange={(event) => setFollow(event.target.checked)} />
            Follow output
          </label>
        )}
      </div>
      {feed === "loading" && <Spinner label="Loading log" />}
      {log.sections.map((section, i) => (
        <StepSection
          key={section.key}
          section={section}
          last={i === log.sections.length - 1}
          live={!completed}
          startOpen={failed(section) || (i === log.sections.length - 1 && !completed) || total <= OPEN_ALL_UNDER}
        />
      ))}
      <div ref={sentinel} />
    </div>
  );
}

const failed = (section: Section) => section.conclusion === "failure" || section.conclusion === "timed_out";

const StepSection = memo(function StepSection({
  section,
  last,
  live,
  startOpen,
}: {
  section: Section;
  last: boolean;
  live: boolean;
  startOpen: boolean;
}) {
  // What the viewer chose wins; until then a step is open when `startOpen` says so.
  const [chosen, setChosen] = useState<boolean | null>(null);
  const [all, setAll] = useState(false);
  const open = chosen ?? startOpen;
  const hidden = all ? 0 : Math.max(0, section.blocks.length - TAIL_BLOCKS);
  const mark = section.conclusion
    ? { success: "✓", failure: "✕", timed_out: "✕", cancelled: "⊘", skipped: "-" }[section.conclusion]
    : live && last
      ? "…"
      : null;
  return (
    <section className="rounded-sm border border-rivet bg-housing">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setChosen(!open)}
        className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left font-mono text-xs text-bone"
      >
        <span aria-hidden="true" className="w-3 text-fog">
          {open ? "▾" : "▸"}
        </span>
        <span
          aria-hidden="true"
          className={cn("w-3 text-center", failed(section) ? "text-failed" : section.conclusion === "success" ? "text-passed" : "text-fog")}
        >
          {mark ?? ""}
        </span>
        <span className="min-w-0 flex-1 truncate font-semibold">{section.name}</span>
        <span className="text-fog">{section.lines} lines</span>
      </button>
      {open && (
        <div className="border-t border-rivet">
          {hidden > 0 && (
            <button type="button" className="button button-small m-2" onClick={() => setAll(true)}>
              Show {hidden * BLOCK_LINES} earlier lines
            </button>
          )}
          <pre className="log-lines">
            {section.blocks.map((block, i) => (i < hidden ? null : <Block key={i} lines={block} />))}
          </pre>
        </div>
      )}
    </section>
  );
});

/** Up to BLOCK_LINES lines. Memoized on the block's identity, so only the block that grew renders again. */
const Block = memo(function Block({ lines }: { lines: string[] }) {
  return (
    <>
      {lines.map((line, i) => (
        <div key={i} className="log-line">
          {segments(line).map((piece, j) =>
            piece.className ? (
              <span key={j} className={piece.className}>
                {piece.text}
              </span>
            ) : (
              piece.text
            ),
          )}
          {line === "" && "​"}
        </div>
      ))}
    </>
  );
});
