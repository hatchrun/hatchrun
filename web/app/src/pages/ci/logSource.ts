// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Where the log viewer gets its lines. A finished job's log is read in
// byte ranges around what is on screen, using the index for the line
// to byte anchors; a running job's log is kept in memory as it arrives.
// Both answer the same questions, so the viewer does not care which.

import { getCiLogRange, searchCiLog, type LogError, type LogIndex } from "@/api";

/** A step of the log. `line` is its `== name` header line, 0-based. */
export interface Step {
  index: number;
  name: string;
  line: number;
  /** Lines of the step including its header. */
  lines: number;
  conclusion: string | null;
  durationMs: number | null;
}

export interface Found {
  line: number;
  step: number;
}

export interface SearchResult {
  matches: Found[];
  total: number;
  truncated: boolean;
}

export interface LogSource {
  /** Changes whenever the lines, steps or errors do. */
  version: number;
  lines: number;
  bytes: number | null;
  steps: Step[];
  errors: LogError[];
  /** The line, or undefined while it is not loaded. */
  line(n: number): string | undefined;
  /** Asks for the line ranges [from, to) to be loaded; `onChange` fires as they arrive. */
  ensure(ranges: [number, number][]): void;
  search(q: string): Promise<SearchResult>;
  /** The whole log as text, for a download. */
  all(): Promise<string>;
  failure: Error | null;
  /** Calls `listener` on every change; returns the way to stop. */
  subscribe(listener: () => void): () => void;
}

export const HEADER = "== ";
export const isError = (line: string) => line.startsWith("##[error]");

const encoder = new TextEncoder();

interface Chunk {
  start: number;
  lines: string[];
  /** Byte offset just past the last line. */
  end: number;
}

interface Anchor {
  offset: number;
  line: number;
}

/** Bytes asked for in one range. */
const RANGE = 512 * 1024;
/** Chunks kept; the ones farthest from the last request go first. */
const KEEP = 14;

/** A finished job's log, read by range. */
export class StoredLog implements LogSource {
  version = 0;
  failure: Error | null = null;
  private listeners = new Set<() => void>();
  lines: number;
  bytes: number;
  steps: Step[];
  errors: LogError[];
  private chunks: Chunk[] = [];
  private anchors: Anchor[];
  private want: [number, number][] = [];
  private pumping = false;

  constructor(
    private job: number,
    index: LogIndex,
    conclusions: Map<number, { conclusion: string | null; durationMs: number | null }>,
  ) {
    this.lines = index.lines;
    this.bytes = index.bytes;
    this.errors = index.errors;
    this.steps = index.steps
      .slice()
      .sort((a, b) => a.line - b.line)
      .map((step) => ({
        index: step.index,
        name: step.name,
        line: step.line,
        lines: step.lines,
        conclusion: conclusions.get(step.index)?.conclusion ?? null,
        durationMs: conclusions.get(step.index)?.durationMs ?? null,
      }));
    this.anchors = [
      { offset: 0, line: 0 },
      ...index.steps.map((step) => ({ offset: step.offset, line: step.line })),
      ...index.errors.map((error) => ({ offset: error.offset, line: error.line })),
    ];
  }

  subscribe(listener: () => void) {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  line(n: number): string | undefined {
    for (const chunk of this.chunks) {
      if (n >= chunk.start && n < chunk.start + chunk.lines.length) {
        return chunk.lines[n - chunk.start];
      }
    }
    return undefined;
  }

  private missing(): number {
    for (const [from, to] of this.want) {
      for (let n = Math.max(0, from); n < Math.min(to, this.lines); n++) {
        if (this.line(n) === undefined) {
          return n;
        }
      }
    }
    return -1;
  }

  ensure(ranges: [number, number][]) {
    this.want = ranges;
    if (!this.pumping && this.failure === null) {
      void this.pump();
    }
  }

  /** The anchor nearest `n`, before it (a forward read) or after it (a backward read). */
  private nearest(n: number, onlyForward: boolean): { anchor: Anchor; forward: boolean } {
    let before: Anchor = { offset: 0, line: 0 };
    let after: Anchor = { offset: this.bytes, line: this.lines };
    for (const anchor of this.anchors) {
      if (anchor.line <= n && anchor.line >= before.line) {
        before = anchor;
      } else if (anchor.line > n && anchor.line < after.line) {
        after = anchor;
      }
    }
    for (const chunk of this.chunks) {
      const next = { offset: chunk.end, line: chunk.start + chunk.lines.length };
      if (next.line <= n && next.line > before.line) {
        before = next;
      }
    }
    return onlyForward || n - before.line <= after.line - n ? { anchor: before, forward: true } : { anchor: after, forward: false };
  }

  private async pump() {
    this.pumping = true;
    // Lines a backward read did not reach: read them forward.
    const forced = new Set<number>();
    try {
      for (;;) {
        const n = this.missing();
        if (n < 0) {
          return;
        }
        const { anchor, forward } = this.nearest(n, forced.has(n));
        const offset = forward ? anchor.offset : Math.max(0, anchor.offset - RANGE);
        const length = forward ? Math.min(RANGE, this.bytes - offset) : anchor.offset - offset;
        if (length <= 0) {
          return;
        }
        const got = await getCiLogRange(this.job, offset, length);
        const text = got.text;
        const parts = text.split("\n");
        // A range that stops mid-line drops that line, unless it reaches the end of the log.
        if (parts[parts.length - 1] === "" || got.offset + encoder.encode(text).length < this.bytes) {
          parts.pop();
        }
        if (forward) {
          const lines = parts;
          const end = got.offset + encoder.encode(lines.length ? lines.join("\n") + "\n" : "").length;
          this.add({ start: anchor.line, lines, end });
        } else {
          // The first line of a backward range may be cut; drop it unless the range starts the log.
          const lines = got.offset === 0 ? parts : parts.slice(1);
          this.add({ start: anchor.line - lines.length, lines, end: anchor.offset });
          if (anchor.line - lines.length > n) {
            forced.add(n);
          }
        }
        if (parts.length === 0) {
          return;
        }
      }
    } catch (err) {
      this.failure = err instanceof Error ? err : new Error(String(err));
      this.changed();
    } finally {
      this.pumping = false;
    }
  }

  private add(chunk: Chunk) {
    this.chunks.push(chunk);
    if (this.chunks.length > KEEP) {
      const first = this.want[0] ?? [0, 0];
      const middle = (first[0] + first[1]) / 2;
      this.chunks.sort((a, b) => Math.abs(a.start - middle) - Math.abs(b.start - middle));
      this.chunks.length = KEEP;
    }
    this.changed();
  }

  private changed() {
    this.version++;
    this.listeners.forEach((listener) => listener());
  }

  retry() {
    this.failure = null;
    this.changed();
  }

  async search(q: string): Promise<SearchResult> {
    const found = await searchCiLog(this.job, q);
    // Matches are exact anchors: later reads can start at them.
    this.anchors.push(...found.matches.slice(0, 200).map((m) => ({ offset: m.offset, line: m.line })));
    return { matches: found.matches.map((m) => ({ line: m.line, step: m.step })), total: found.total, truncated: found.truncated };
  }

  async all(): Promise<string> {
    const parts: string[] = [];
    for (let offset = 0; offset < this.bytes; offset += RANGE * 2) {
      parts.push((await getCiLogRange(this.job, offset, Math.min(RANGE * 2, this.bytes - offset))).text);
    }
    return parts.join("");
  }
}

/** What a running job's socket sends in an `events` message. */
export type LogEvent =
  | { type: "step_started"; index: number; name: string }
  | { type: "line"; text: string }
  | { type: "annotation"; annotation: { level?: string; message?: string; title?: string | null } }
  | { type: "step_finished"; index: number; summary: { conclusion: string; duration_ms?: number } };

const annotationLine = (a: { level?: string; message?: string; title?: string | null }) =>
  `[${a.level ?? "notice"}] ${a.title ? `${a.title}: ` : ""}${a.message ?? ""}`;

/** A log held in memory: a running job's, or a finished one that has no index. */
export class MemoryLog implements LogSource {
  version = 0;
  failure: Error | null = null;
  private listeners = new Set<() => void>();
  bytes: number | null = null;
  steps: Step[] = [];
  errors: LogError[] = [];
  private text: string[] = [];

  get lines() {
    return this.text.length;
  }

  /** Text with `== name` header lines starts the steps at those lines. */
  static fromText(text: string, conclusions: Map<number, { conclusion: string | null; durationMs: number | null }>): MemoryLog {
    const log = new MemoryLog();
    const all = text === "" ? [] : text.split("\n");
    if (all[all.length - 1] === "") {
      all.pop();
    }
    for (const line of all) {
      log.pushText(line);
    }
    for (const step of log.steps) {
      step.conclusion = conclusions.get(step.index)?.conclusion ?? null;
      step.durationMs = conclusions.get(step.index)?.durationMs ?? null;
    }
    return log;
  }

  private pushText(line: string) {
    if (line.startsWith(HEADER)) {
      const last = this.steps[this.steps.length - 1];
      if (last) {
        last.lines = this.text.length - last.line;
      }
      this.steps.push({ index: this.steps.length, name: line.slice(HEADER.length), line: this.text.length, lines: 1, conclusion: null, durationMs: null });
    } else if (this.steps.length > 0) {
      (this.steps[this.steps.length - 1] as Step).lines++;
    }
    if (isError(line)) {
      this.errors.push({ offset: 0, line: this.text.length, step: Math.max(0, this.steps.length - 1), text: line.slice(0, 300) });
    }
    this.text.push(line);
  }

  /** Replaces the log with the socket's backlog. */
  load(text: string) {
    this.text = [];
    this.steps = [];
    this.errors = [];
    const all = text === "" ? [] : text.split("\n");
    if (all[all.length - 1] === "") {
      all.pop();
    }
    for (const line of all) {
      this.pushText(line);
    }
    this.changed();
  }

  apply(events: LogEvent[]) {
    for (const event of events) {
      if (event.type === "step_started") {
        const last = this.steps[this.steps.length - 1];
        if (last) {
          last.lines = this.text.length - last.line;
        }
        this.steps.push({ index: event.index, name: event.name, line: this.text.length, lines: 1, conclusion: null, durationMs: null });
        this.text.push(`${HEADER}${event.name}`);
      } else if (event.type === "line") {
        this.pushLive(event.text);
      } else if (event.type === "annotation") {
        this.pushLive(annotationLine(event.annotation), event.annotation.level === "error");
      } else {
        const step = this.steps.find((s) => s.index === event.index);
        if (step) {
          step.conclusion = event.summary.conclusion;
          step.durationMs = event.summary.duration_ms ?? null;
        }
      }
    }
    this.changed();
  }

  private pushLive(line: string, error = false) {
    const last = this.steps[this.steps.length - 1];
    if (last) {
      last.lines++;
    }
    if (error || isError(line)) {
      this.errors.push({ offset: 0, line: this.text.length, step: Math.max(0, this.steps.length - 1), text: line.slice(0, 300) });
    }
    this.text.push(line);
  }

  private changed() {
    this.version++;
    this.listeners.forEach((listener) => listener());
  }

  subscribe(listener: () => void) {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  line(n: number) {
    return this.text[n];
  }

  ensure() {}

  search(q: string): Promise<SearchResult> {
    const needle = q.toLowerCase();
    const matches: Found[] = [];
    let total = 0;
    let step = 0;
    const marks = this.steps;
    this.text.forEach((line, n) => {
      if (line.toLowerCase().includes(needle)) {
        total++;
        if (matches.length < 1000) {
          while (step + 1 < marks.length && (marks[step + 1] as Step).line <= n) {
            step++;
          }
          matches.push({ line: n, step: marks[step]?.index ?? 0 });
        }
      }
    });
    return Promise.resolve({ matches, total, truncated: total > matches.length });
  }

  all() {
    return Promise.resolve(this.text.join("\n") + "\n");
  }
}
