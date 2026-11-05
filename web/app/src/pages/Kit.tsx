// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

import { listOrgRuns, type Page as ApiPage, type CiRun } from "@/api";
import { CopyButton } from "@/components/CopyButton";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { FilterBar, useUrlFilters } from "@/components/FilterBar";
import { JobSquares } from "@/components/JobSquares";
import { Page } from "@/components/Page";
import { RepoMultiSelect, type RepoSelection } from "@/components/RepoMultiSelect";
import { ScopePicker, type ScopeValue } from "@/components/ScopePicker";
import { SegmentedBar } from "@/components/SegmentedBar";
import { StatusGlyph, StatusPill, ciStatus, type Status } from "@/components/StatusPill";
import { Await } from "@/components/Status";
import { Truncate } from "@/components/Truncate";
import { abbreviate, duration } from "@/lib/format";
import { useLoad } from "@/load";
import type { OrgPageProps } from "./types";

const STATUSES: Status[] = ["passed", "running", "failed", "waiting", "queued", "cancelled", "skipped"];

/**
 * The component gallery, at /o/:org/kit under `npm run dev:mock` only.
 * Every shared component with the mock org's data: look here before
 * building a screen, and add a case when you add a component.
 */
export function Kit({ org }: OrgPageProps) {
  const [scope, setScope] = useState<ScopeValue>({ scope: "org", repo: null, env: null });
  const [repos, setRepos] = useState<RepoSelection>({ all: true, repos: [] });
  const filters = useUrlFilters();
  const cursors = useCursors();
  const [runs, reload] = useLoad<ApiPage<CiRun>>(
    () => listOrgRuns(org, { ...filters.asRunFilters, cursor: cursors.cursor, limit: 10 }),
    `${filters.key}#${cursors.key}`,
  );
  void reload;
  return (
    <Page title="Kit" meta="shared components, mock data">
      <section className="flex flex-col gap-2">
        <h2 className="section-title">StatusPill, StatusGlyph</h2>
        <div className="flex flex-wrap items-center gap-2">
          {STATUSES.map((status) => (
            <StatusPill key={status} status={status} />
          ))}
          <StatusPill status="waiting" label="needs approval" />
          {STATUSES.map((status) => (
            <StatusGlyph key={status} status={status} />
          ))}
          <span className="meta">ciStatus("completed", "failure") = {ciStatus("completed", "failure")}</span>
        </div>
      </section>
      <section className="flex flex-col gap-2">
        <h2 className="section-title">ScopePicker</h2>
        <ScopePicker org={org} value={scope} onChange={setScope} />
        <span className="meta">{JSON.stringify(scope)}</span>
      </section>
      <section className="flex flex-col gap-2">
        <h2 className="section-title">RepoMultiSelect</h2>
        <div>
          <RepoMultiSelect org={org} value={repos} onChange={setRepos} />
        </div>
      </section>
      <section className="flex flex-col gap-2">
        <h2 className="section-title">Truncate, CopyButton, abbreviate, SegmentedBar, JobSquares</h2>
        <div className="flex w-[280px] flex-col gap-2">
          <Truncate value="feature/very-long-branch-name-for-truncation-checks-with-ticket-ENG-12345" copy mono />
          <span className="font-mono text-[13px]">
            {abbreviate(999)} {abbreviate(12_400)} {abbreviate(1_250_000)} {duration(134_000)} <CopyButton text="x" />
          </span>
        </div>
        <SegmentedBar segments={[{ status: "passed", count: 120 }, { status: "failed", count: 9 }, { status: "running", count: 4 }, { status: "queued", count: 30 }]} />
        <JobSquares jobs={{ total: 104, passed: 66, failed: 4, running: 14, waiting: 1 }} />
        <JobSquares jobs={{ total: 3, passed: 3, failed: 0, running: 0, waiting: 0 }} />
      </section>
      <section className="flex flex-col gap-2">
        <h2 className="section-title">FilterBar, CursorPager</h2>
        <FilterBar org={org} branches={["main", "develop", "feat/nav"]} actors={["gamunu", "alice"]} />
        <Await loaded={runs} retry={reload}>
          {(page) => (
            <>
              <ul className="font-mono text-[13px] text-fg-2">
                {page.items.map((run) => (
                  <li key={run.id} className="flex gap-3">
                    <StatusGlyph status={ciStatus(run.status, run.conclusion)} />
                    <Truncate value={run.repository} />
                    <span className="text-fg-3">{run.branch}</span>
                    <JobSquares jobs={run.jobs} />
                  </li>
                ))}
              </ul>
              <CursorPager page={page} index={cursors.index} pageSize={10} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range="last 7 days" noun="runs" />
            </>
          )}
        </Await>
      </section>
    </Page>
  );
}
