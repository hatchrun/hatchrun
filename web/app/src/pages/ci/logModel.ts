// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The text of a job's log as steps and blocks of lines. A change makes
// new objects only along the path it touches (the sections array, the
// one section, its last block), so React skips every block that stayed
// as it was while a running job appends.

import type { LogIndex } from "@/api";

/** Lines per block: the unit React compares and the unit a list item renders. */
export const BLOCK_LINES = 500;

export interface Section {
  /** Unique within the log, in order of appearance. */
  key: number;
  /** The step's position in the job; null for text before the first step. */
  index: number | null;
  name: string;
  /** How the step ended, once it is known. */
  conclusion: string | null;
  blocks: string[][];
  lines: number;
}

export interface LogState {
  sections: Section[];
  /** The next section key. */
  next: number;
}

export const EMPTY: LogState = { sections: [], next: 0 };

export type LogAction =
  | { type: "load"; text: string; index: LogIndex | null; conclusions: Map<number, string> }
  | { type: "events"; events: LogEvent[] };

/** What a running job's socket sends in an `events` message. */
export type LogEvent =
  | { type: "step_started"; index: number; name: string }
  | { type: "line"; text: string }
  | { type: "annotation"; annotation: { level?: string; message?: string; title?: string | null } }
  | { type: "step_finished"; index: number; summary: { conclusion: string } };

function pushLine(section: Section, line: string): Section {
  const blocks = section.blocks.slice();
  const last = blocks[blocks.length - 1];
  if (last !== undefined && last.length < BLOCK_LINES) {
    blocks[blocks.length - 1] = [...last, line];
  } else {
    blocks.push([line]);
  }
  return { ...section, blocks, lines: section.lines + 1 };
}

function lines(text: string): string[] {
  if (text === "") {
    return [];
  }
  const all = text.split("\n");
  if (all[all.length - 1] === "") {
    all.pop();
  }
  return all;
}

function section(key: number, index: number | null, name: string): Section {
  return { key, index, name, conclusion: null, blocks: [], lines: 0 };
}

const HEADER = "== ";

/**
 * Splits a log by the step marks of its index, which are byte offsets
 * of the `== name` lines. Without an index a line that starts with `== `
 * is taken as a header, which a step's own output can imitate.
 */
function split(text: string, index: LogIndex | null, conclusions: Map<number, string>): LogState {
  const sections: Section[] = [];
  const open = (idx: number | null, name: string) => {
    const made = { ...section(sections.length, idx, name), conclusion: idx === null ? null : (conclusions.get(idx) ?? null) };
    sections.push(made);
    return made;
  };
  const add = (target: Section, body: string[]) => {
    for (let i = 0; i < body.length; i += BLOCK_LINES) {
      target.blocks.push(body.slice(i, i + BLOCK_LINES));
    }
    target.lines += body.length;
  };
  if (index !== null && index.steps.length > 0) {
    const bytes = new TextEncoder().encode(text);
    const decoder = new TextDecoder();
    const marks = [...index.steps].sort((a, b) => a.offset - b.offset);
    if (marks[0] !== undefined && marks[0].offset > 0) {
      add(open(null, "Output"), lines(decoder.decode(bytes.subarray(0, marks[0].offset))));
    }
    marks.forEach((mark, i) => {
      const end = marks[i + 1]?.offset ?? bytes.length;
      const body = lines(decoder.decode(bytes.subarray(mark.offset, end)));
      // The first line is the `== name` header itself.
      add(open(mark.index, mark.name), body.slice(1));
    });
    return { sections, next: sections.length };
  }
  let current: Section | null = null;
  const plain: string[] = [];
  const flush = () => {
    if (current !== null) {
      add(current, plain.splice(0));
    }
  };
  for (const line of lines(text)) {
    if (line.startsWith(HEADER)) {
      flush();
      current = open(sections.filter((s) => s.index !== null).length, line.slice(HEADER.length));
    } else {
      current ??= open(null, "Output");
      plain.push(line);
    }
  }
  flush();
  return { sections, next: sections.length };
}

function annotationLine(a: { level?: string; message?: string; title?: string | null }): string {
  const title = a.title ? `${a.title}: ` : "";
  return `[${a.level ?? "notice"}] ${title}${a.message ?? ""}`;
}

export function reduce(state: LogState, action: LogAction): LogState {
  if (action.type === "load") {
    return split(action.text, action.index, action.conclusions);
  }
  const sections = state.sections.slice();
  let next = state.next;
  const append = (line: string) => {
    const last = sections[sections.length - 1];
    if (last === undefined) {
      sections.push(pushLine(section(next++, null, "Output"), line));
    } else {
      sections[sections.length - 1] = pushLine(last, line);
    }
  };
  for (const event of action.events) {
    if (event.type === "step_started") {
      sections.push(section(next++, event.index, event.name));
    } else if (event.type === "line") {
      append(event.text);
    } else if (event.type === "annotation") {
      append(annotationLine(event.annotation));
    } else {
      const at = sections.findIndex((s) => s.index === event.index);
      const found = sections[at];
      if (found !== undefined) {
        sections[at] = { ...found, conclusion: event.summary.conclusion };
      }
    }
  }
  return { sections, next };
}
