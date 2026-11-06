// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/** The states a run, job or step shows. Colour never stands alone: every one has a glyph and a word. */
export type Status = "passed" | "running" | "failed" | "waiting" | "queued" | "cancelled" | "skipped";

interface Look {
  label: string;
  glyph: string;
  /** Text colour of the glyph. queued, cancelled and skipped have no glyph colour; they show a dot. */
  text: string;
  /** The 8px status dot. */
  dot: string;
  /** Colour of the status word: text-2, except failed and waiting. */
  word: string;
  /** The 2px left edge of a row. Only failed and waiting rows have one. */
  edge: string;
  /** A fill for job squares and chart bars. Passed is control grey; only failures are coloured. */
  fill: string;
}

export const STATUS: Record<Status, Look> = {
  passed: { label: "passed", glyph: "✓", text: "text-passed", dot: "bg-passed", word: "text-fg-2", edge: "", fill: "bg-control" },
  running: { label: "running", glyph: "▶", text: "text-running", dot: "bg-running", word: "text-fg-2", edge: "", fill: "bg-fg-3" },
  failed: {
    label: "failed",
    glyph: "✕",
    text: "text-failed",
    dot: "bg-failed",
    word: "text-failed",
    edge: "border-l-2 border-l-failed/60",
    fill: "bg-failed",
  },
  waiting: {
    label: "waiting",
    glyph: "!",
    text: "text-waiting",
    dot: "bg-waiting",
    word: "text-waiting",
    edge: "border-l-2 border-l-waiting/60",
    fill: "bg-fg-3",
  },
  queued: { label: "queued", glyph: "", text: "text-fg-3", dot: "bg-queued", word: "text-fg-2", edge: "", fill: "bg-rule" },
  cancelled: { label: "cancelled", glyph: "⊘", text: "text-fg-3", dot: "bg-queued", word: "text-fg-2", edge: "", fill: "bg-rule" },
  skipped: { label: "skipped", glyph: "-", text: "text-fg-3", dot: "bg-queued", word: "text-fg-2", edge: "", fill: "bg-rule" },
};

export function ciStatus(status: string, conclusion?: string | null, held?: string | null): Status {
  if (held) {
    return "waiting";
  }
  const word = status === "completed" ? (conclusion ?? "success") : status;
  switch (word) {
    case "success":
    case "neutral":
    case "passed":
      return "passed";
    case "failure":
    case "timed_out":
    case "failed":
      return "failed";
    case "cancelled":
      return "cancelled";
    case "skipped":
      return "skipped";
    case "action_required":
    case "waiting":
      return "waiting";
    case "in_progress":
    case "running":
      return "running";
    default:
      return "queued";
  }
}

/** The glyph alone, for dense rows. It carries the status as its accessible name. */
/** The 8px dot in the status colour. */
export function StatusDot({ status, className }: { status: Status; className?: string }) {
  return <span aria-hidden="true" className={cn("inline-block size-2 flex-none rounded-full", STATUS[status].dot, className)} />;
}

/** The glyph in the status colour, for dense lists. queued has no glyph and shows its dot. */
export function StatusGlyph({ status, className }: { status: Status; className?: string }) {
  const look = STATUS[status];
  if (!look.glyph) {
    return (
      <span role="img" aria-label={look.label} className={cn("inline-flex w-4 items-center justify-center", className)}>
        <StatusDot status={status} />
      </span>
    );
  }
  return (
    <span role="img" aria-label={look.label} className={cn("inline-block w-4 text-center font-semibold", look.text, className)}>
      {look.glyph}
    </span>
  );
}

/** A status: the dot and the word. No tinted pill; only failed and waiting colour the word. */
export function StatusPill({
  status,
  label,
  children,
  className,
}: {
  status: Status;
  /** Replaces the word, for example "needs approval". */
  label?: string;
  children?: ReactNode;
  className?: string;
}) {
  const look = STATUS[status];
  return (
    <span className={cn("inline-flex h-6 items-center gap-2 text-[13px] whitespace-nowrap", look.word, className)}>
      <StatusDot status={status} />
      {label ?? look.label}
      {children}
    </span>
  );
}
