// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";

import type { CiRun } from "@/api";
import { JobSquares } from "@/components/JobSquares";
import { STATUS, StatusPill, ciStatus, type Status } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { ago, duration, shortSha } from "@/lib/format";
import { repoUrl, runUrl, workflowUrl } from "@/lib/urls";
import { Link } from "@/router";

const EDGE: Partial<Record<Status, string>> = { failed: "--failed", waiting: "--waiting", running: "--running" };

const TH = "cap px-3 py-2.5 text-left font-normal";

/** One page of runs: status edge, repo and workflow, branch, sha, commit message, actor, status, jobs, duration, age. */
export function RunsTable({ runs, now, inRepo = false }: { runs: CiRun[]; now: number; /** On a repository page: the repository column is left out. */ inRepo?: boolean }) {
  return (
    <div className="max-h-[560px] overflow-auto">
      <table className="w-full min-w-[1320px] border-collapse">
        <thead className="sticky top-0 z-[1] bg-background">
          <tr>
            <th className={`${TH} pl-3.5`}>{inRepo ? "Run" : "Repository"}</th>
            <th className={TH}>Workflow</th>
            <th className={TH}>Branch</th>
            <th className={TH}>Commit</th>
            <th className={TH}>Message</th>
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
            return (
              <tr key={run.id} className="group border-t border-rule hover:bg-raised">
                <td className="py-2.5 pr-3 pl-3.5 whitespace-nowrap" style={EDGE[status] ? { boxShadow: `inset 3px 0 0 var(${EDGE[status]})` } : undefined}>
                  <span className="inline-flex max-w-[320px] items-center gap-2.5">
                    {!inRepo && (
                      <Link href={repoUrl(run.repository)} className="max-w-[220px] truncate font-semibold hover:underline" title={run.repository}>
                        {run.repository}
                      </Link>
                    )}
                    <Link href={runUrl(run.repository, run.id)} aria-label={`Run ${run.repository} number ${run.run_number}, open details`} className="inline-flex shrink-0 items-center gap-1.5 font-mono text-[13px] text-fg-3 hover:text-fg hover:underline">
                      #{run.run_number}
                      <ChevronRight size={12} aria-hidden="true" className="shrink-0 text-fg-2" />
                    </Link>
                  </span>
                </td>
                <td className="max-w-[220px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                  <Link href={workflowUrl(run.repository, run.workflow)} className="block min-w-0 hover:text-fg hover:underline">
                    <Truncate value={run.workflow} className="block" />
                  </Link>
                </td>
                <td className="max-w-[240px] px-3 py-2.5 whitespace-nowrap">
                  <span className="inline-flex max-w-full items-center gap-2">
                    {run.pull_request && (
                      <Link href={repoUrl(run.repository, "runs", { pull: run.pull_request.number })} title={run.pull_request.title} className="shrink-0 font-mono text-[13px] text-fg-2 hover:text-fg hover:underline">
                        PR #{run.pull_request.number}
                      </Link>
                    )}
                    {run.branch ? <Truncate value={run.branch} copy mono /> : <span className="font-mono text-[13px] text-fg-3">{run.event}</span>}
                  </span>
                </td>
                <td className="px-3 py-2.5 whitespace-nowrap">
                  <Truncate value={run.sha} copy mono>
                    {shortSha(run.sha)}
                  </Truncate>
                </td>
                <td className="max-w-[300px] min-w-[200px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                  {run.message ? <Truncate value={run.message} className="block" /> : <span className="text-fg-3">-</span>}
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
                  {run.duration_ms === null ? (status === "running" ? "running" : "-") : duration(run.duration_ms)}
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
