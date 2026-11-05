// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The log of one job: steps that fold, only the rows on screen in the
// DOM, lines fetched by byte range around the viewport (see
// logSource.ts), find, error navigation and following a live job.

import { ChevronDown, ChevronRight, ChevronUp } from "lucide-react";
import { Fragment, useCallback, useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState, type ReactNode } from "react";

import { abbreviate, duration } from "@/lib/format";
import { cn } from "@/lib/utils";
import { StatusGlyph, ciStatus } from "@/components/StatusPill";
import { segments } from "./ansi";
import { isError, type LogSource, type SearchResult, type Step } from "./logSource";
import { BTN, BTN_PRIMARY, BTN_SM, FIELD } from "./runUi";

/** Row height in pixels. */
const ROW = 20;
/** Rows drawn beyond each edge of the viewport. */
const OVERSCAN = 40;
const TIMESTAMP = /^(\d{4}-\d\d-\d\dT)(\d\d:\d\d:\d\d(?:\.\d+)?)Z? ?/;

interface Section {
  id: number;
  name: string;
  step: Step | null;
  /** The `== name` line; -1 for the text before the first step. */
  header: number;
  /** First line below the header. */
  body: number;
  count: number;
}

function sectionsOf(source: LogSource): Section[] {
  const { steps, lines } = source;
  const out: Section[] = [];
  const first = steps[0]?.line ?? lines;
  if (first > 0) {
    out.push({ id: -1, name: "Output", step: null, header: -1, body: 0, count: first });
  }
  steps.forEach((step, i) => {
    const end = steps[i + 1]?.line ?? lines;
    out.push({ id: i, name: step.name, step, header: step.line, body: step.line + 1, count: Math.max(0, end - step.line - 1) });
  });
  return out;
}

const failedStep = (step: Step | null) => step?.conclusion === "failure" || step?.conclusion === "timed_out";

/** The steps open on load: the failed ones, else the last, else all of a short log. */
function initialOpen(source: LogSource, live: boolean): Set<number> {
  const open = new Set<number>();
  const sections = sectionsOf(source);
  const first = source.errors[0];
  for (const section of sections) {
    if (failedStep(section.step) || (first && first.line >= section.header && first.line < section.body + section.count)) {
      open.add(section.id);
    }
  }
  if (live) {
    // A running log follows its last step, whatever failed before it.
    const last = sections[sections.length - 1];
    if (last) {
      open.add(last.id);
    }
  }
  if (open.size === 0) {
    if (live || source.lines > 2000) {
      const last = sections[sections.length - 1];
      if (last) {
        open.add(last.id);
      }
    } else {
      sections.forEach((section) => open.add(section.id));
    }
  }
  return open;
}

/** Splits `text` at case-insensitive hits of `q` and marks them. */
function mark(text: string, q: string, key: string): ReactNode {
  if (q === "") {
    return text;
  }
  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const out: ReactNode[] = [];
  let at = 0;
  for (let hit = lower.indexOf(needle); hit >= 0; hit = lower.indexOf(needle, at)) {
    if (hit > at) {
      out.push(text.slice(at, hit));
    }
    out.push(
      <mark key={`${key}${hit}`} className="rounded-xs bg-brand/35 text-inherit">
        {text.slice(hit, hit + needle.length)}
      </mark>,
    );
    at = hit + needle.length;
  }
  if (at === 0) {
    return text;
  }
  out.push(text.slice(at));
  return out;
}

function LineText({ text, q }: { text: string; q: string }) {
  return (
    <>
      {segments(text).map((piece, i) =>
        piece.className ? (
          <span key={i} className={piece.className}>
            {mark(piece.text, q, `${i}:`)}
          </span>
        ) : (
          <Fragment key={i}>{mark(piece.text, q, `${i}:`)}</Fragment>
        ),
      )}
    </>
  );
}

type Search =
  | { state: "idle" }
  | { state: "loading" }
  | { state: "error"; message: string }
  | ({ state: "ready"; q: string } & SearchResult);

export interface Props {
  source: LogSource;
  /** The job name, in the toolbar. */
  name: string;
  live: boolean;
  feed: string;
  /** A failed job: its first error is offered as a summary line. */
  failed: boolean;
  onRetry: () => void;
  download: () => void;
  downloading: boolean;
  /** Why the last download failed; null when it did not. */
  downloadError: string | null;
}

export function LogViewer({ source, name, live, feed, failed, onRetry, download, downloading, downloadError }: Props) {
  const [, rerender] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    let frame = 0;
    const stop = source.subscribe(() => {
      if (frame === 0) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          rerender();
        });
      }
    });
    return () => {
      cancelAnimationFrame(frame);
      stop();
    };
  }, [source]);

  const sections = useMemo(() => sectionsOf(source), [source, source.version]); // eslint-disable-line react-hooks/exhaustive-deps
  const [open, setOpen] = useState(() => initialOpen(source, live));
  const [wrap, setWrap] = useState(false);
  const [times, setTimes] = useState(false);
  const [follow, setFollow] = useState(live);
  const [q, setQ] = useState("");
  const [search, setSearch] = useState<Search>({ state: "idle" });
  const [matchAt, setMatchAt] = useState(-1);
  const [errAt, setErrAt] = useState(-1);
  const [menu, setMenu] = useState<"steps" | "errors" | null>(null);
  const [target, setTarget] = useState<{ line: number; seq: number } | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);
  const scroller = useRef<HTMLDivElement>(null);
  const finder = useRef<HTMLInputElement>(null);
  const handled = useRef(0);
  const lastTop = useRef(0);
  const searchedQ = useRef("");

  // Rows: each section is its header, then its lines when open.
  const layout = useMemo(() => {
    const starts = [0];
    sections.forEach((section) => starts.push((starts[starts.length - 1] as number) + 1 + (open.has(section.id) ? section.count : 0)));
    return { starts, rows: starts[starts.length - 1] as number };
  }, [sections, open]);

  const sectionAt = useCallback(
    (row: number) => {
      let lo = 0;
      let hi = sections.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if ((layout.starts[mid] as number) <= row) {
          lo = mid;
        } else {
          hi = mid - 1;
        }
      }
      return lo;
    },
    [sections, layout],
  );

  const rowOf = useCallback(
    (line: number) => {
      const index = sections.findIndex((s) => line === s.header || (line >= s.body && line < s.body + s.count));
      const section = sections[index];
      if (!section) {
        return -1;
      }
      return (layout.starts[index] as number) + (line === section.header ? 0 : 1 + line - section.body);
    },
    [sections, layout],
  );

  // The viewport.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) {
      return;
    }
    const measure = () => setHeight(el.clientHeight);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const first = Math.max(0, Math.floor(scrollTop / ROW) - OVERSCAN);
  const last = Math.min(layout.rows, Math.ceil((scrollTop + height) / ROW) + OVERSCAN);

  // Ask the source for the lines of the rows in view.
  useEffect(() => {
    const ranges: [number, number][] = [];
    for (let s = sectionAt(first); s < sections.length && (layout.starts[s] as number) < last; s++) {
      const section = sections[s] as Section;
      if (!open.has(section.id) || section.count === 0) {
        continue;
      }
      const from = Math.max(0, first - (layout.starts[s] as number) - 1);
      const to = Math.min(section.count, last - (layout.starts[s] as number) - 1);
      if (to > from) {
        ranges.push([section.body + from, section.body + to]);
      }
    }
    source.ensure(ranges);
  }, [source, source.version, sections, layout, open, first, last, sectionAt]);

  const scrollToRow = useCallback((row: number, where: "top" | "third") => {
    const el = scroller.current;
    if (el) {
      const to = Math.max(0, row * ROW - (where === "third" ? el.clientHeight / 3 : 0));
      el.scrollTop = to;
      setScrollTop(to);
    }
  }, []);

  const jump = useCallback(
    (line: number) => {
      const section = sections.find((s) => line === s.header || (line >= s.body && line < s.body + s.count));
      if (section && !open.has(section.id)) {
        setOpen((current) => new Set(current).add(section.id));
      }
      setFollow(false);
      setTarget((current) => ({ line, seq: (current?.seq ?? 0) + 1 }));
    },
    [sections, open],
  );

  useLayoutEffect(() => {
    if (target && handled.current !== target.seq) {
      const row = rowOf(target.line);
      if (row >= 0) {
        handled.current = target.seq;
        scrollToRow(row, "third");
      }
    }
  }, [target, rowOf, scrollToRow]);

  // On load: bring the first failed step into view.
  const opened = useRef(false);
  useLayoutEffect(() => {
    if (opened.current || live || sections.length === 0) {
      return;
    }
    opened.current = true;
    const at = sections.findIndex((s) => failedStep(s.step));
    if (at > 0) {
      scrollToRow(layout.starts[at] as number, "top");
    }
  }, [live, sections, layout, scrollToRow]);

  // A running job: new steps open, and the view follows the end.
  const seen = useRef(sections.length);
  useLayoutEffect(() => {
    if (!live) {
      return;
    }
    if (sections.length > seen.current) {
      const added = sections.slice(seen.current).map((s) => s.id);
      seen.current = sections.length;
      if (follow) {
        setOpen((current) => new Set([...current, ...added]));
      }
    }
    if (follow && scroller.current) {
      scroller.current.scrollTop = scroller.current.scrollHeight;
    }
  }, [live, follow, sections, layout, source.version]);

  // Find, asked of the server (or memory, for a running job) once typing pauses.
  useEffect(() => {
    if (q === "") {
      searchedQ.current = "";
      return;
    }
    let current = true;
    const timer = window.setTimeout(() => {
      source.search(q).then(
        (found) => {
          if (current) {
            setSearch({ state: "ready", q, ...found });
            if (searchedQ.current !== q) {
              searchedQ.current = q;
              setMatchAt(-1);
            }
          }
        },
        (err: unknown) => current && setSearch({ state: "error", message: err instanceof Error ? err.message : String(err) }),
      );
    }, 300);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [q, source, source.version]);

  const matches = search.state === "ready" ? search.matches : [];
  const stepMatch = (delta: number) => {
    if (matches.length === 0) {
      return;
    }
    const next = (matchAt + delta + matches.length) % matches.length;
    setMatchAt(next);
    jump((matches[next] as { line: number }).line);
  };
  const errors = source.errors;
  const stepError = (delta: number) => {
    if (errors.length === 0) {
      return;
    }
    const next = (errAt + delta + errors.length) % errors.length;
    setErrAt(next);
    jump((errors[next] as { line: number }).line);
  };

  // Keyboard: / and the find shortcut focus find; n N step matches; e E step errors.
  const keys = useRef({ stepMatch, stepError });
  useLayoutEffect(() => {
    keys.current = { stepMatch, stepError };
  });
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const mod = event.metaKey || event.ctrlKey;
      const typing = event.target instanceof HTMLElement && (event.target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName));
      if ((mod && event.key.toLowerCase() === "f") || (!typing && !mod && event.key === "/")) {
        event.preventDefault();
        finder.current?.focus();
        finder.current?.select();
      } else if (!typing && !mod && !event.altKey) {
        if (event.key === "n" || event.key === "N") {
          keys.current.stepMatch(event.key === "n" ? 1 : -1);
        } else if (event.key === "e" || event.key === "E") {
          keys.current.stepError(event.key === "e" ? 1 : -1);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const toggle = (id: number) =>
    setOpen((current) => {
      const next = new Set(current);
      if (!next.delete(id)) {
        next.add(id);
      }
      return next;
    });

  const firstError = errors[0];
  const stepOf = (index: number) => source.steps.find((s) => s.index === index)?.name ?? "";
  const matchLabel =
    search.state === "loading"
      ? "searching"
      : search.state === "error"
        ? "search failed"
        : search.state === "ready"
          ? search.total === 0
            ? "no matches"
            : `${matchAt >= 0 ? matchAt + 1 : "-"} of ${abbreviate(search.total)}${search.truncated ? "+" : ""}`
          : "";
  const errorLabel = `${errors.length}${errors.length >= 200 ? "+" : ""} ${errors.length === 1 ? "error" : "errors"}`;

  // The window of rows.
  const rows: ReactNode[] = [];
  let anyStamp = false;
  for (let s = sectionAt(first); s < sections.length && (layout.starts[s] as number) < last; s++) {
    const section = sections[s] as Section;
    const start = layout.starts[s] as number;
    const expanded = open.has(section.id);
    if (first <= start && start < last) {
      const step = section.step;
      const status = step?.conclusion ? ciStatus("completed", step.conclusion) : live && s === sections.length - 1 ? "running" : null;
      rows.push(
        <button
          key={`h${section.id}`}
          type="button"
          aria-expanded={expanded}
          onClick={() => toggle(section.id)}
          className={cn(
            "flex h-[20px] w-full cursor-pointer items-center gap-2 border-0 bg-raised px-2 text-left font-mono text-[13px] text-fg hover:bg-control/40",
            target?.line === section.header && "bg-brand/15",
          )}
        >
          {expanded ? <ChevronDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" /> : <ChevronRight size={12} aria-hidden="true" className="shrink-0 text-fg-3" />}
          {status ? <StatusGlyph status={status} /> : <span className="w-4" />}
          <span className="min-w-0 flex-1 truncate font-semibold">{section.name}</span>
          <span className="text-fg-3">
            {abbreviate(section.count)} {section.count === 1 ? "line" : "lines"}
          </span>
          {step?.durationMs != null && <span className="w-14 text-right text-fg-3">{duration(step.durationMs)}</span>}
        </button>,
      );
    }
    if (!expanded) {
      continue;
    }
    const from = Math.max(0, first - start - 1);
    const to = Math.min(section.count, last - start - 1);
    for (let n = from; n < to; n++) {
      const line = section.body + n;
      const raw = source.line(line);
      let stamp: string | null = null;
      let text = raw ?? "";
      const found = raw === undefined ? null : TIMESTAMP.exec(raw);
      if (found) {
        anyStamp = true;
        stamp = found[2] as string;
        text = raw?.slice(found[0].length) ?? "";
      }
      const bad = isError(text);
      rows.push(
        <div
          key={line}
          className={cn(
            "flex min-h-[20px] font-mono text-[12.5px] leading-[20px] hover:bg-raised",
            bad && "bg-failed/10",
            target?.line === line && "bg-brand/15 shadow-[inset_2px_0_0_var(--brand)]",
          )}
        >
          <span aria-hidden="true" className="sticky left-0 w-[68px] shrink-0 bg-inherit pr-3 text-right text-fg-3 tabular-nums select-none">
            {line + 1}
          </span>
          {times && stamp !== null && <span className="mr-3 shrink-0 text-fg-3 tabular-nums">{stamp}</span>}
          <span className={cn("min-w-0 pr-3", wrap ? "break-words whitespace-pre-wrap" : "whitespace-pre", bad && "text-failed")}>
            {raw === undefined ? "" : <LineText text={text} q={search.state === "ready" ? search.q : ""} />}
            {text === "" && "​"}
          </span>
        </div>,
      );
    }
  }

  // Any upward move away from the end (wheel, scrollbar, keys, touch)
  // stops following. The viewer's own scrolls only go down to the end.
  const onScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const el = event.currentTarget;
    if (el.scrollTop < lastTop.current && el.scrollHeight - el.scrollTop - el.clientHeight > 4) {
      setFollow(false);
    }
    lastTop.current = el.scrollTop;
    setScrollTop(el.scrollTop);
  };

  return (
    <section aria-label="Log" className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded-md border border-rule bg-panel">
      {failed && firstError && (
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-rule bg-failed/10 px-3 py-2">
          <div className="flex min-w-0 flex-col gap-0.5 font-mono text-[13px]">
            <div className="flex min-w-0 items-center gap-2">
              <span aria-hidden="true" className="text-failed">
                ✕
              </span>
              <span className="shrink-0 text-fg">step {stepOf(firstError.step)}</span>
              <span className="shrink-0 text-fg-3">failed</span>
            </div>
            <div className="flex min-w-0 gap-2 text-fg-2">
              <span aria-hidden="true" className="text-fg-3">
                └
              </span>
              <span className="min-w-0 truncate" title={firstError.text}>
                {firstError.text.replace(/^##\[error\]/, "")}
              </span>
            </div>
          </div>
          <button type="button" className={BTN_PRIMARY} onClick={() => (setErrAt(0), jump(firstError.line))}>
            Jump to first error
            <ChevronDown size={12} aria-hidden="true" />
          </button>
        </div>
      )}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-rule px-3 py-2">
        <span className="cap">Log</span>
        <span className="max-w-[200px] min-w-0 truncate font-mono text-[13px] text-fg" title={name}>
          {name}
        </span>
        <div className="flex items-center gap-1.5">
          <label className="cap" htmlFor="log-find">
            Find
          </label>
          <input
            id="log-find"
            ref={finder}
            type="search"
            value={q}
            placeholder="text in log"
            className={cn(FIELD, "w-44")}
            onChange={(event) => {
              setQ(event.target.value);
              setSearch(event.target.value === "" ? { state: "idle" } : { state: "loading" });
              setMatchAt(-1);
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                stepMatch(event.shiftKey ? -1 : 1);
              } else if (event.key === "Escape") {
                event.currentTarget.blur();
              }
            }}
          />
          <span className="min-w-16 font-mono text-[13px] text-fg-2" role="status">
            {matchLabel}
          </span>
          <button type="button" className={BTN_SM} aria-label="Previous match" disabled={matches.length === 0} onClick={() => stepMatch(-1)}>
            <ChevronUp size={12} aria-hidden="true" />
          </button>
          <button type="button" className={BTN_SM} aria-label="Next match" disabled={matches.length === 0} onClick={() => stepMatch(1)}>
            <ChevronDown size={12} aria-hidden="true" />
          </button>
        </div>
        <div role="group" aria-label="Errors in this log" className="relative flex items-center gap-1.5">
          <button
            type="button"
            className={BTN_SM}
            aria-haspopup="true"
            aria-expanded={menu === "errors"}
            disabled={errors.length === 0}
            onClick={() => setMenu(menu === "errors" ? null : "errors")}
          >
            {errors.length === 0 ? "no errors" : errorLabel}
            <ChevronDown size={11} aria-hidden="true" />
          </button>
          {errors.length > 0 && (
            <span className="min-w-12 font-mono text-[13px] text-fg-2">{errAt >= 0 ? `${errAt + 1} of ${errors.length}` : ""}</span>
          )}
          <button type="button" className={BTN_SM} aria-label="Previous error" disabled={errors.length === 0} onClick={() => stepError(-1)}>
            <ChevronUp size={12} aria-hidden="true" />
          </button>
          <button type="button" className={BTN_SM} aria-label="Next error" disabled={errors.length === 0} onClick={() => stepError(1)}>
            <ChevronDown size={12} aria-hidden="true" />
          </button>
          {menu === "errors" && (
            <div role="menu" aria-label="Errors in this log" className="absolute top-full left-0 z-30 mt-1 max-h-80 w-[520px] max-w-[80vw] overflow-y-auto rounded-md border border-control bg-raised py-1 shadow-xl">
              {errors.map((error, i) => (
                <button
                  key={error.line}
                  type="button"
                  role="menuitem"
                  className="flex w-full cursor-pointer items-baseline gap-3 px-3 py-1 text-left font-mono text-[13px] hover:bg-control/40"
                  onClick={() => {
                    setMenu(null);
                    setErrAt(i);
                    jump(error.line);
                  }}
                >
                  <span className="w-14 shrink-0 text-right text-fg-3 tabular-nums">{error.line + 1}</span>
                  <span className="min-w-0 flex-1 truncate text-failed" title={error.text}>
                    {error.text.replace(/^##\[error\]/, "")}
                  </span>
                  <span className="max-w-28 shrink-0 truncate text-fg-3">{stepOf(error.step)}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="flex-1" />
        <div className="flex items-center gap-1.5">
          <button type="button" className={BTN} aria-pressed={wrap} onClick={() => setWrap(!wrap)}>
            Wrap
          </button>
          <button
            type="button"
            className={BTN}
            aria-pressed={times}
            disabled={!times && !anyStamp}
            title={anyStamp || times ? "Show the time of each line" : "The lines in view carry no timestamps"}
            onClick={() => setTimes(!times)}
          >
            Timestamps
          </button>
          <button type="button" className={BTN} disabled={downloading} aria-label="Download this job log as a text file" onClick={download}>
            {downloading ? "Preparing" : "Download"}
          </button>
          {downloadError !== null && (
            <span role="alert" className="max-w-[240px] truncate font-mono text-[13px] text-failed" title={downloadError}>
              Download failed: {downloadError}
            </span>
          )}
          <button
            type="button"
            className={BTN}
            aria-pressed={live && follow}
            disabled={!live}
            title={live ? "Keep the newest line in view" : "The job has ended"}
            onClick={() => setFollow(!follow)}
          >
            Follow live
          </button>
        </div>
      </div>
      <div className="relative flex flex-wrap items-center gap-2 border-b border-rule px-3 py-1.5">
        <span className="font-mono text-[13px] text-fg-2">
          {feed === "reconnecting" ? "reconnecting" : `${abbreviate(source.lines)} lines${source.bytes ? ` · ${(source.bytes / 1048576).toFixed(source.bytes > 1048576 ? 1 : 2)} MiB` : ""}`}
        </span>
        <div className="flex-1" />
        <div className="relative">
          <button type="button" className={BTN_SM} aria-haspopup="true" aria-expanded={menu === "steps"} onClick={() => setMenu(menu === "steps" ? null : "steps")}>
            Jump to step
            <ChevronDown size={11} aria-hidden="true" />
          </button>
          {menu === "steps" && (
            <div role="menu" aria-label="Jump to step" className="absolute top-full right-0 z-30 mt-1 max-h-80 w-72 overflow-y-auto rounded-md border border-control bg-raised py-1 shadow-xl">
              {sections.map((section) => (
                <button
                  key={section.id}
                  type="button"
                  role="menuitem"
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-1 text-left font-mono text-[13px] hover:bg-control/40"
                  onClick={() => {
                    setMenu(null);
                    setOpen((current) => new Set(current).add(section.id));
                    setFollow(false);
                    setTarget((current) => ({ line: section.header >= 0 ? section.header : 0, seq: (current?.seq ?? 0) + 1 }));
                  }}
                >
                  {section.step?.conclusion ? <StatusGlyph status={ciStatus("completed", section.step.conclusion)} /> : <span className="w-4" />}
                  <span className="min-w-0 flex-1 truncate" title={section.name}>
                    {section.name}
                  </span>
                  <span className="text-fg-3">{abbreviate(section.count)}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        <button type="button" className={BTN_SM} onClick={() => setOpen(new Set())}>
          Collapse all
        </button>
        <button type="button" className={BTN_SM} onClick={() => setOpen(new Set(sections.filter((s) => failedStep(s.step)).map((s) => s.id)))}>
          Expand failed
        </button>
      </div>
      <div ref={scroller} className="relative min-h-0 flex-1 overflow-auto bg-background" onScroll={onScroll} onClick={() => setMenu(null)}>
        <div style={{ paddingTop: first * ROW, paddingBottom: Math.max(0, layout.rows - last) * ROW }} className="w-max min-w-full">
          {rows}
        </div>
        {source.failure && (
          <div role="alert" className="sticky bottom-0 flex items-center justify-between gap-3 border-t border-failed/40 bg-raised px-3 py-2 font-mono text-[13px] text-failed">
            <span>Could not read the log: {source.failure.message}</span>
            <button type="button" className={BTN_SM} onClick={onRetry}>
              Retry
            </button>
          </div>
        )}
      </div>
      <div className="flex flex-wrap items-center justify-between gap-2 border-t border-rule px-3 py-1.5 font-mono text-[13px] text-fg-3">
        <span>
          <span aria-hidden="true">└ </span>
          {live ? "live" : `end of log · ${abbreviate(source.lines)} lines`}
        </span>
        <span>n N next and previous match · e E next and previous error · / find</span>
      </div>
    </section>
  );
}
