// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronsUpDown } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { listOrgRuns, type CiRunStatus, type RunCounts, type Page as ApiPage, type CiRun } from "@/api";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { FilterBar, useUrlFilters } from "@/components/FilterBar";
import { Page } from "@/components/Page";
import { Popover } from "@/components/Popover";
import { Await } from "@/components/Status";
import { abbreviate } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useLoad, useNow } from "@/load";
import { setQuery, useQuery } from "@/router";
import { ActivityPanel } from "./runs/ActivityPanel";
import { NeedsYou } from "./runs/NeedsYou";
import { RunsTable } from "./runs/RunsTable";
import type { OrgPageProps } from "./types";

const HOUR = 3_600_000;
const RANGES = [
  { id: "1h", label: "Last hour", ms: HOUR },
  { id: "24h", label: "Last 24 hours", ms: 24 * HOUR },
  { id: "7d", label: "Last 7 days", ms: 7 * 24 * HOUR },
  { id: "30d", label: "Last 30 days", ms: 30 * 24 * HOUR },
] as const;
const DEFAULT_RANGE = "7d"; // RANGES[2]

const TABS: [Tab, ...Tab[]] = [
  { id: "all", label: "All", status: undefined },
  { id: "running", label: "Running", status: ["running"] },
  { id: "waiting", label: "Waiting", status: ["waiting"] },
  { id: "failed", label: "Failed", status: ["failure"] },
  { id: "passed", label: "Passed", status: ["success"] },
];

interface Tab {
  id: string;
  label: string;
  status: CiRunStatus[] | undefined;
}


/** The landing page: needs-you block, activity of the range, filters and the runs table. */
export function Runs({ org }: OrgPageProps) {
  const filters = useUrlFilters();
  const tabId = useQuery("status") ?? "all";
  const tab = TABS.find((item) => item.id === tabId) ?? TABS[0];
  const rangeId = useQuery("range");
  const range = RANGES.find((item) => item.id === rangeId) ?? RANGES[2];
  const cursors = useCursors();
  const [rangeOpen, setRangeOpen] = useState(false);
  const now = useNow(15_000);

  const scope = `${filters.key}|${range.id}`;
  const listKey = `${scope}|${tab.id}`;
  const since = () => Date.now() - range.ms;
  const [runs, reload] = useLoad<ApiPage<CiRun> & { counts: RunCounts }>(
    () => listOrgRuns(org, { ...filters.asRunFilters, status: tab.status, since: since(), cursor: cursors.cursor, limit: 50 }),
    `${org}|${listKey}#${cursors.key}`,
    true,
  );
  const counts = runs.state === "ready" ? TABS.map((item) => runs.data.counts[item.id as keyof RunCounts]) : null;

  // A new filter, tab or range starts at the newest page.
  useEffect(() => {
    cursors.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [org, listKey]);

  const live = counts !== null && ((counts[1] ?? 0) > 0 || (counts[2] ?? 0) > 0);
  useEffect(() => {
    const timer = window.setInterval(reload, live ? 5_000 : 30_000);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, listKey, cursors.key]);

  const total = counts;
  const branches = useMemo(() => (runs.state === "ready" ? [...new Set(runs.data.items.flatMap((run) => (run.branch ? [run.branch] : [])))].sort() : []), [runs]);
  const actors = useMemo(() => (runs.state === "ready" ? [...new Set(runs.data.items.map((run) => run.actor))].sort() : []), [runs]);

  return (
    <Page title="Runs" meta={total ? `${abbreviate(total[0] ?? 0)} runs, ${range.label.toLowerCase()}` : org}>
      <NeedsYou org={org} onChange={reload} />

      <ActivityPanel org={org} repos={filters.asRunFilters.repo ?? []} />

      <section aria-label="CI runs" className="rounded-md border border-rule">
        <div className="flex flex-col gap-2.5 rounded-t-md border-b border-rule bg-panel px-3.5 py-2.5">
          <div className="flex flex-wrap items-center justify-between gap-2.5">
            <div role="tablist" aria-label="Filter by status" className="flex overflow-hidden rounded-sm border border-control font-mono text-[13px]">
              {TABS.map((item, index) => {
                const on = item.id === tab.id;
                return (
                  <button
                    key={item.id}
                    type="button"
                    role="tab"
                    aria-selected={on}
                    onClick={() => setQuery({ status: item.id === "all" ? null : item.id })}
                    className={cn("cursor-pointer px-3 py-1.5", index < TABS.length - 1 && "border-r border-control", on ? "bg-fg text-background" : "text-fg-2 hover:text-fg")}
                  >
                    {item.label} <span className={on ? "text-background" : "text-fg-3"}>{total ? abbreviate(total[index] ?? 0) : ""}</span>
                  </button>
                );
              })}
            </div>
            <div className="flex flex-wrap items-center gap-3">
              <span className={cn("inline-flex items-center gap-2 font-mono text-[13px]", live ? "text-passed" : "text-fg-3")}>
                <span className={cn("size-1.5 rounded-full", live ? "bg-passed" : "bg-fg-3")} />
                {live ? "Live, refreshes every 5 s" : "Idle, nothing running"}
              </span>
              <Popover
                open={rangeOpen}
                onOpenChange={setRangeOpen}
                width={200}
                align="end"
                trigger={
                  <button
                    type="button"
                    aria-haspopup="listbox"
                    aria-label={`Time range, ${range.label}`}
                    className="inline-flex h-8 cursor-pointer items-center gap-2 rounded-sm border border-control bg-background px-2.5 font-mono text-[13px] text-fg-2 hover:border-fg-3 hover:text-fg"
                  >
                    {range.label}
                    <ChevronsUpDown size={12} aria-hidden="true" className="text-fg-3" />
                  </button>
                }
              >
                <div role="listbox" aria-label="Time range" className="py-1">
                  {RANGES.map((item) => (
                    <button
                      key={item.id}
                      type="button"
                      role="option"
                      aria-selected={item.id === range.id}
                      onClick={() => {
                        setQuery({ range: item.id === DEFAULT_RANGE ? null : item.id });
                        setRangeOpen(false);
                      }}
                      className={cn("flex w-full cursor-pointer items-center gap-2.5 px-3 py-1.5 text-left font-mono text-[13px] hover:bg-rule", item.id === range.id && "bg-brand/10 shadow-[inset_2px_0_0_var(--brand)]")}
                    >
                      <span className="w-2.5">{item.id === range.id ? "✓" : ""}</span>
                      {item.label}
                    </button>
                  ))}
                </div>
              </Popover>
            </div>
          </div>
          <FilterBar org={org} branches={branches} actors={actors} />
        </div>

        <Await loaded={runs} retry={reload}>
          {(page) =>
            page.items.length === 0 ? (
              <div className="border-t border-rule px-3.5 py-6 font-mono text-[13px] text-fg-3">
                No runs match these filters in the {range.label.toLowerCase()}.
              </div>
            ) : (
              <RunsTable runs={page.items} now={now} />
            )
          }
        </Await>

        {runs.state === "ready" && (
          <div className="rounded-b-md border-t border-rule bg-panel px-3.5 py-2.5">
            <CursorPager page={runs.data} index={cursors.index} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range={cursors.index === 0 ? `${range.label.toLowerCase()}, newest first` : range.label.toLowerCase()} noun="runs" />
          </div>
        )}
      </section>
    </Page>
  );
}
