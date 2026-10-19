// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import type { Job, Run } from "@/api";
import { cn } from "@/lib/utils";

/** How a status looks: running is heat, done is passed, a launch that needs a hand is failed. */
export type Tone = "running" | "starting" | "waiting" | "done" | "retrying" | "failed";

export const TONE_TEXT: Record<Tone, string> = {
  running: "text-heat",
  starting: "text-blush",
  waiting: "text-fog",
  done: "text-passed",
  retrying: "text-chick",
  failed: "text-failed",
};

export interface Shown {
  text: string;
  glyph: string;
  tone: Tone;
}

/** How a completed job ended, by GitHub's conclusion. A job from before hatchrun kept conclusions is just done. */
const ENDED: Record<string, Shown> = {
  success: { text: "Passed", glyph: "✓", tone: "done" },
  failure: { text: "Failed", glyph: "✕", tone: "failed" },
  timed_out: { text: "Timed out", glyph: "✕", tone: "failed" },
  cancelled: { text: "Cancelled", glyph: "⊘", tone: "waiting" },
  skipped: { text: "Skipped", glyph: "-", tone: "waiting" },
  neutral: { text: "Neutral", glyph: "✓", tone: "done" },
  action_required: { text: "Needs approval", glyph: "!", tone: "retrying" },
};

/** What the job is doing, in one word or two. */
export function state(job: Pick<Job, "status" | "launchState" | "launchRetry" | "conclusion">): Shown {
  if (job.launchState === "failed" && job.status === "queued") {
    if (job.launchRetry === "backoff") {
      return { text: "Retrying", glyph: "↻", tone: "retrying" };
    }
    if (job.launchRetry === "setup") {
      return { text: "Needs setup", glyph: "✕", tone: "failed" };
    }
  }
  if (job.launchState === "failed") {
    return { text: "Launch failed", glyph: "✕", tone: "failed" };
  }
  if (job.status === "queued") {
    if (job.launchState === "launching") {
      return { text: "Launching", glyph: "↑", tone: "starting" };
    }
    if (job.launchState === "launched") {
      return { text: "Starting", glyph: "▷", tone: "starting" };
    }
    return { text: "Waiting", glyph: "…", tone: "waiting" };
  }
  if (job.status === "in_progress") {
    return { text: "Running", glyph: "▶", tone: "running" };
  }
  return (job.conclusion && ENDED[job.conclusion]) || { text: "Done", glyph: "✓", tone: "done" };
}

/**
 * Which job state a run shows, first match wins: a launch that needs a
 * hand comes first, since that job will never run and GitHub keeps the
 * run waiting for up to a day; then whatever is still going; then how
 * the jobs ended, the worst first.
 */
const RUN_ORDER = [
  "Needs setup",
  "Launch failed",
  "Running",
  "Launching",
  "Starting",
  "Retrying",
  "Waiting",
  "Needs approval",
  "Failed",
  "Timed out",
  "Cancelled",
  "Done",
  "Passed",
  "Neutral",
  "Skipped",
];

/** What a run is doing, rolled up from its current jobs. */
export function runState(jobs: Job[]): Shown {
  const shown = jobs.map(state);
  const rank = (s: Shown) => {
    const at = RUN_ORDER.indexOf(s.text);
    return at < 0 ? RUN_ORDER.length : at;
  };
  return shown.reduce<Shown | null>((worst, s) => (worst === null || rank(s) < rank(worst) ? s : worst), null) ?? {
    text: "Waiting",
    glyph: "…",
    tone: "waiting",
  };
}

/** The jobs by how they show, for the count strip beside a run's state: most urgent first. */
export function tally(jobs: Job[]): { shown: Shown; count: number }[] {
  const byGlyph = new Map<string, { shown: Shown; count: number }>();
  for (const job of jobs) {
    const shown = state(job);
    const key = `${shown.glyph}${shown.tone}`;
    const seen = byGlyph.get(key);
    if (seen) {
      seen.count += 1;
    } else {
      byGlyph.set(key, { shown, count: 1 });
    }
  }
  const order = (s: Shown) => {
    const at = RUN_ORDER.indexOf(s.text);
    return at < 0 ? RUN_ORDER.length : at;
  };
  return [...byGlyph.values()].sort((a, b) => order(a.shown) - order(b.shown));
}

/** When a job ended: GitHub's time when it has one, else when hatchrun last heard of it. */
export function endOf(job: Pick<Job, "completedAt" | "updatedAt">): number {
  return job.completedAt ?? job.updatedAt;
}

/*
 * Took, for a run and for a job alike, runs from when the job was queued
 * to when it ended, and counts up while it has not: a job waiting for a
 * runner is taking time too. The list's took order measures runs the
 * same way.
 */

/** How long a job took, or has taken so far. */
export function jobSpan(job: Pick<Job, "status" | "queuedAt" | "completedAt" | "updatedAt">, now: number): { ms: number; live: boolean } {
  const live = job.status !== "completed";
  return { ms: (live ? now : endOf(job)) - job.queuedAt, live };
}

/** How long a run took, or has taken so far: from its newest attempt's first job to its last job's end. */
export function runSpan(run: Pick<Run, "queuedAt">, jobs: Job[], now: number): { ms: number; live: boolean } {
  const live = jobs.some((job) => job.status !== "completed");
  const end = live ? now : Math.max(run.queuedAt, ...jobs.map(endOf));
  return { ms: end - run.queuedAt, live };
}

/** A run's name: its workflow, or its file or id when GitHub gave no name. */
export function runName(run: Pick<Run, "workflowName" | "path" | "id">): string {
  return run.workflowName ?? run.path?.split("/").pop() ?? `Run ${run.id}`;
}

/** A run as GitHub numbers it, or by id until hatchrun has read it. */
export function runRef(run: Pick<Run, "runNumber" | "id">): string {
  return run.runNumber === null ? String(run.id) : `#${run.runNumber}`;
}

export function StatusMark({ shown, children }: { shown: Shown; children?: ReactNode }) {
  return (
    <span className={cn("inline-flex items-center gap-2 font-mono text-xs font-bold", TONE_TEXT[shown.tone])}>
      <span aria-hidden="true" className="w-3.5 text-center">
        {shown.glyph}
      </span>
      {shown.text}
      {children}
    </span>
  );
}

/** The instance's page in the EC2 console. */
export function instanceUrl(region: string, instanceId: string): string {
  return `https://${region}.console.aws.amazon.com/ec2/home?region=${region}#InstanceDetails:instanceId=${instanceId}`;
}

export function ago(ms: number): string {
  const seconds = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 48) {
    return `${hours} h ago`;
  }
  return new Date(ms).toLocaleDateString();
}

export function span(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) {
    return `${seconds} s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ${seconds % 60} s`;
  }
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}
