// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";

import type { CiRun } from "@/api";
import { JobSquares } from "@/components/JobSquares";
import { STATUS, StatusPill, ciStatus, type Status } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { ago, duration, shortSha } from "@/lib/format";
import { runUrl } from "@/lib/urls";
import { Link } from "@/router";

const EDGE: Partial<Record<Status, string>> = { failed: "--failed", waiting: "--waiting", running: "--running" };

const TH = "cap px-3 py-2.5 text-left font-normal";

/** One page of runs: status edge, repo and workflow, branch, sha, actor, status, jobs, duration, age. */
export function RunsTable({ runs, now }: { runs: CiRun[]; now: number }) {
  return (
    <div className="max-h-[560px] overflow-auto">
      <table className="w-full min-w-[1080px] border-collapse">
        <thead className="sticky top-0 z-[1] bg-background">
          <tr>
            <th className={`${TH} pl-3.5`}>Run</th>
            <th className={TH}>Workflow</th>
            <th className={TH}>Branch</th>
            <th className={TH}>Commit</th>
            <th className={TH}>Actor</th>
            <th className={TH}>Status</th>
            <th className={TH}>Jobs</th>
            <th className={`${TH} text-right`}>Duration</th>
            <th className={`${TH} pr-3.5`}>Started</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => {
            const status = ciStatus(run.status, run.conclusion);
            const open = run.status !== "completed";
            return (
              <tr key={run.id} className="group border-t border-rule hover:bg-raised">
                <td className="py-2.5 pr-3 pl-3.5 whitespace-nowrap" style={EDGE[status] ? { boxShadow: `inset 3px 0 0 var(${EDGE[status]})` } : undefined}>
                  <Link href={runUrl(run.repository, run.id)} aria-label={`Run ${run.repository} number ${run.run_number}, open details`} className="inline-flex max-w-[300px] items-center gap-2.5 hover:underline">
                    <span className="max-w-[220px] truncate font-semibold" title={run.repository}>
                      {run.repository}
                    </span>
                    <span className="shrink-0 font-mono text-[13px] text-fg-3">#{run.run_number}</span>
                    <ChevronRight size={12} aria-hidden="true" className="shrink-0 text-fg-2" />
                  </Link>
                </td>
                <td className="max-w-[220px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                  <Truncate value={run.workflow} className="block" />
                </td>
                <td className="max-w-[240px] px-3 py-2.5 whitespace-nowrap">
                  {run.branch ? <Truncate value={run.branch} copy mono /> : <span className="font-mono text-[13px] text-fg-3">{run.event}</span>}
                </td>
                <td className="px-3 py-2.5 whitespace-nowrap">
                  <Truncate value={run.sha} copy mono>
                    {shortSha(run.sha)}
                  </Truncate>
                </td>
                <td className="max-w-[130px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                  <Truncate value={run.actor} className="block" />
                </td>
                <td className="px-3 py-2.5 whitespace-nowrap">
                  <StatusPill status={status} className="h-5 text-xs" />
                </td>
                <td className="px-3 py-2.5 whitespace-nowrap">
                  <JobSquares jobs={run.jobs} />
                </td>
                <td className={`px-3 py-2.5 text-right font-mono text-[13px] whitespace-nowrap ${status === "failed" ? STATUS.failed.text : "text-fg-2"}`}>
                  {run.duration_ms === null ? (open ? "running" : "-") : duration(run.duration_ms)}
                </td>
                <td className="px-3.5 py-2.5 text-[13px] whitespace-nowrap text-fg-3">{ago(run.created_at, now)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
