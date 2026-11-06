// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";

import type { CiRun } from "@/api";
import { JobSquares } from "@/components/JobSquares";
import { STATUS, StatusPill, ciStatus } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { ago, duration, shortSha } from "@/lib/format";
import { repoUrl, runUrl, workflowUrl } from "@/lib/urls";
import { Link } from "@/router";

const TH = "cap px-3 py-2.5 text-left font-normal";
// Under 1500px actor, duration and jobs fold into a second line under the
// run; under 1000px workflow, branch, commit and message fold in too.
const WIDE = "max-[999px]:hidden";
const WIDER = "max-[1499px]:hidden";
const NARROW = "min-[1000px]:hidden";
const NARROWER = "min-[1500px]:hidden";

/** One page of runs: status edge, repo and workflow, branch, sha, commit message, actor, status, jobs, duration, age. Narrow widths move the lower-priority columns to a second line per row. */
export function RunsTable({ runs, now, inRepo = false, inWorkflow = false }: { runs: CiRun[]; now: number; /** On a repository page: the repository column is left out. */ inRepo?: boolean; /** On a workflow's page: the workflow column is left out. */ inWorkflow?: boolean }) {
  return (
    <div className="relative max-h-[560px] overflow-y-auto overflow-x-hidden">
      <table className="w-full border-collapse">
        <thead className="sticky top-0 z-[1] bg-background">
          <tr>
            <th className={`${TH} pl-3.5`}>{inRepo ? "Run" : "Repository"}</th>
            {!inWorkflow && <th className={`${TH} ${WIDE}`}>Workflow</th>}
            <th className={`${TH} ${WIDE}`}>Branch</th>
            <th className={`${TH} ${WIDE}`}>Commit</th>
            <th className={`${TH} ${WIDE}`}>Message</th>
            <th className={`${TH} ${WIDER}`}>Actor</th>
            <th className={TH}>Status</th>
            <th className={`${TH} ${WIDER}`}>Jobs</th>
            <th className={`${TH} ${WIDER} text-right`}>Duration</th>
            <th className={`${TH} pr-3.5`}>Started</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => {
            const status = ciStatus(run.status, run.conclusion);
            const duration_ = run.duration_ms === null ? (status === "running" ? "running" : "-") : duration(run.duration_ms);
            const durationClass = status === "failed" ? STATUS.failed.text : "text-fg-2";
            const branch = (
              <span className="inline-flex max-w-full items-center gap-2">
                {run.pull_request && (
                  <Link href={repoUrl(run.repository, "runs", { pull: run.pull_request.number })} title={run.pull_request.title} className="shrink-0 font-mono text-[13px] text-fg-2 hover:text-fg hover:underline">
                    PR #{run.pull_request.number}
                  </Link>
                )}
                {run.branch ? <Truncate value={run.branch} copy mono /> : <span className="font-mono text-[13px] text-fg-3">{run.event}</span>}
              </span>
            );
            const sha = (
              <Truncate value={run.sha} copy mono>
                {shortSha(run.sha)}
              </Truncate>
            );
            return (
              <tr key={run.id} className="group border-t border-rule hover:bg-raised">
                <td className={`max-w-0 min-w-[200px] py-2.5 pr-3 pl-3.5 whitespace-nowrap min-[1000px]:max-w-[320px] ${STATUS[status].edge}`}>
                  <span className="flex max-w-full items-center gap-2.5">
                    {!inRepo && (
                      <Link href={repoUrl(run.repository)} className="min-w-0 truncate font-semibold hover:underline" title={run.repository}>
                        {run.repository}
                      </Link>
                    )}
                    <Link href={runUrl(run.repository, run.id)} aria-label={`Run ${run.repository} number ${run.run_number}, open details`} className="inline-flex shrink-0 items-center gap-1.5 font-mono text-[13px] text-fg-3 hover:text-fg hover:underline">
                      #{run.run_number}
                      <ChevronRight size={12} aria-hidden="true" className="shrink-0 text-fg-2" />
                    </Link>
                  </span>
                  <span className={`${NARROWER} mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[13px]`}>
                    {!inWorkflow && (
                      <Link href={workflowUrl(run.repository, run.workflow)} className={`${NARROW} max-w-[180px] min-w-0 truncate text-fg-2 hover:text-fg hover:underline`} title={run.workflow}>
                        {run.workflow}
                      </Link>
                    )}
                    <span className={`${NARROW} inline-flex max-w-full items-center gap-3`}>
                      {branch}
                      {sha}
                    </span>
                    <span className="max-w-[160px] truncate text-fg-2" title={run.actor}>
                      {run.actor}
                    </span>
                    <span className={`font-mono ${durationClass}`}>{duration_}</span>
                    <JobSquares jobs={run.jobs} />
                    {run.message && (
                      <span className={`${NARROW} block w-full truncate text-fg-2`} title={run.message}>
                        {run.message}
                      </span>
                    )}
                  </span>
                </td>
                {!inWorkflow && (
                  <td className={`max-w-[220px] px-3 py-2.5 whitespace-nowrap text-fg-2 ${WIDE}`}>
                    <Link href={workflowUrl(run.repository, run.workflow)} className="block min-w-0 hover:text-fg hover:underline">
                      <Truncate value={run.workflow} className="block" />
                    </Link>
                  </td>
                )}
                <td className={`max-w-[200px] px-3 py-2.5 whitespace-nowrap ${WIDE}`}>{branch}</td>
                <td className={`px-3 py-2.5 whitespace-nowrap ${WIDE}`}>{sha}</td>
                <td className={`w-full max-w-0 min-w-[100px] px-3 py-2.5 whitespace-nowrap text-fg-2 ${WIDE}`}>
                  {run.message ? <Truncate value={run.message} className="block" /> : <span className="text-fg-3">-</span>}
                </td>
                <td className={`max-w-[130px] px-3 py-2.5 whitespace-nowrap text-fg-2 ${WIDER}`}>
                  <Truncate value={run.actor} className="block" />
                </td>
                <td className="px-3 py-2.5 whitespace-nowrap align-top min-[1500px]:align-middle">
                  <StatusPill status={status} className="h-5 text-xs" />
                </td>
                <td className={`px-3 py-2.5 whitespace-nowrap ${WIDER}`}>
                  <JobSquares jobs={run.jobs} />
                </td>
                <td className={`px-3 py-2.5 text-right font-mono text-[13px] whitespace-nowrap ${durationClass} ${WIDER}`}>{duration_}</td>
                <td className="px-3.5 py-2.5 text-[13px] whitespace-nowrap text-fg-3 align-top min-[1500px]:align-middle">{ago(run.created_at, now)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
