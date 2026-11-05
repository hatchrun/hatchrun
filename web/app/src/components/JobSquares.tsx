// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { CiJobCounts } from "@/api";
import { abbreviate } from "@/lib/format";
import { cn } from "@/lib/utils";
import { STATUS, type Status } from "./StatusPill";

/** At most this many squares are drawn. */
export const MAX_SQUARES = 12;

/** The squares for a run's jobs, worst first, so a failure is never in the "+N". */
export function squaresFor(jobs: CiJobCounts): Status[] {
  const queued = Math.max(0, jobs.total - jobs.passed - jobs.failed - jobs.running - jobs.waiting - jobs.cancelled - jobs.skipped);
  const all: [Status, number][] = [
    ["failed", jobs.failed],
    ["waiting", jobs.waiting],
    ["running", jobs.running],
    ["queued", queued],
    ["passed", jobs.passed],
    ["cancelled", jobs.cancelled],
    ["skipped", jobs.skipped],
  ];
  return all.flatMap(([status, count]) => Array<Status>(count).fill(status)).slice(0, MAX_SQUARES);
}

/**
 * A run's jobs as squares, 12 at most then "+N", with the pass and fail
 * count after. Squares are not the only signal: the count reads
 * "37/40" (passed of total) and "2 failed" in text.
 */
export function JobSquares({ jobs, className }: { jobs: CiJobCounts; className?: string }) {
  const squares = squaresFor(jobs);
  const more = jobs.total - squares.length;
  return (
    <span
      className={cn("inline-flex items-center gap-2 font-mono text-[13px] text-fg-2", className)}
      title={`${jobs.passed} passed, ${jobs.failed} failed, ${jobs.running} running, ${jobs.waiting} waiting of ${jobs.total}`}
    >
      <span aria-hidden="true" className="inline-flex items-center gap-0.5">
        {squares.map((status, index) => (
          <span key={index} className={cn("size-2.5 rounded-xs", STATUS[status].fill)} />
        ))}
      </span>
      {more > 0 && <span>+{abbreviate(more)}</span>}
      <span>
        {abbreviate(jobs.passed)}/{abbreviate(jobs.total)}
      </span>
      {jobs.failed > 0 && <span className="text-failed">{abbreviate(jobs.failed)} failed</span>}
    </span>
  );
}
