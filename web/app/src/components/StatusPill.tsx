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
  /** Text colour of the glyph and label. */
  text: string;
  /** The 12% tint behind a pill. */
  tint: string;
  /** The 2px edge of a row that needs action. */
  edge: string;
  /** A fill, for bars and squares. */
  fill: string;
}

export const STATUS: Record<Status, Look> = {
  passed: { label: "passed", glyph: "✓", text: "text-passed", tint: "bg-passed/12", edge: "border-l-passed", fill: "bg-passed" },
  running: { label: "running", glyph: "▶", text: "text-running", tint: "bg-running/12", edge: "border-l-running", fill: "bg-running" },
  failed: { label: "failed", glyph: "✕", text: "text-failed", tint: "bg-failed/12", edge: "border-l-failed", fill: "bg-failed" },
  waiting: { label: "waiting", glyph: "!", text: "text-waiting", tint: "bg-waiting/12", edge: "border-l-waiting", fill: "bg-waiting" },
  queued: { label: "queued", glyph: "•••", text: "text-queued", tint: "bg-queued/12", edge: "border-l-queued", fill: "bg-queued" },
  cancelled: { label: "cancelled", glyph: "⊘", text: "text-queued", tint: "bg-queued/12", edge: "border-l-queued", fill: "bg-queued" },
  skipped: { label: "skipped", glyph: "-", text: "text-queued", tint: "bg-queued/12", edge: "border-l-queued", fill: "bg-queued" },
};

/**
 * Maps what /api/ci answers (a run or job's `status` and `conclusion`,
 * or a status filter value such as "success") to a `Status`. A job
 * `held` for approval is waiting.
 */
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
export function StatusGlyph({ status, className }: { status: Status; className?: string }) {
  const look = STATUS[status];
  return (
    <span role="img" aria-label={look.label} className={cn("inline-block w-4 text-center font-semibold", look.text, className)}>
      {look.glyph}
    </span>
  );
}

/** Glyph and word on the status colour at 12%. `children` follow the word, such as a count. */
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
    <span
      className={cn(
        "inline-flex h-6 items-center gap-1.5 rounded-sm px-2.5 text-[13px] font-semibold whitespace-nowrap",
        look.text,
        look.tint,
        className,
      )}
    >
      <span aria-hidden="true">{look.glyph}</span>
      {label ?? look.label}
      {children}
    </span>
  );
}
