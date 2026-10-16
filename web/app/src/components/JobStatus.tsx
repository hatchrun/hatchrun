// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import type { Job } from "@/api";
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

/** What the job is doing, in one word or two. */
export function state(job: Pick<Job, "status" | "launchState" | "launchRetry">): Shown {
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
  return { text: "Done", glyph: "✓", tone: "done" };
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
