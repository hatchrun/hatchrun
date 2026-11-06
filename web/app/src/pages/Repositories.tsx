// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Search } from "lucide-react";
import { useEffect, useState } from "react";

import { listOrgRepos, patchEnabledRepos, type Page as ApiPage, type CiOrgRepo } from "@/api";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { Page } from "@/components/Page";
import { Await } from "@/components/Status";
import { StatusPill, ciStatus } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { Button } from "@/components/ui/button";
import { abbreviate, ago } from "@/lib/format";
import { repoUrl, runUrl } from "@/lib/urls";
import { cn } from "@/lib/utils";
import { useLoad, useNow } from "@/load";
import { Link, navigate, setQuery, useQuery } from "@/router";
import { DispatchDialog } from "./ci/DispatchDialog";
import type { OrgPageProps } from "./types";

const TH = "cap px-3 py-2.5 text-left font-normal";
const SHOW = [
  { id: "all", label: "All", enabled: undefined },
  { id: "enabled", label: "Enabled", enabled: true },
  { id: "disabled", label: "Disabled", enabled: false },
] as const;
const PAGE_SIZE = 50;

/** Every repository of the App installation, with its latest run and whether it starts runs. */
export function Repositories({ org, tenant }: OrgPageProps) {
  const admin = tenant.role === "admin";
  const q = useQuery("q") ?? "";
  const showId = useQuery("show");
  const show = SHOW.find((item) => item.id === showId) ?? SHOW[0];
  // A draft belongs to the URL query it was typed over; Back or Forward makes it stale.
  const [typed, setDraft] = useState<{ from: string; text: string } | null>(null);
  const draft = typed !== null && typed.from === q ? typed.text : null;
  const [dispatch, setDispatch] = useState<string | null>(null);
  const [enabling, setEnabling] = useState<string | null>(null);
  const [enableError, setEnableError] = useState<Error | null>(null);
  const cursors = useCursors();
  const now = useNow(30_000);

  // Typing writes the URL after a pause, then the box follows the URL again.
  useEffect(() => {
    if (draft === null) return;
    const timer = window.setTimeout(() => {
      setQuery({ q: draft });
      setDraft(null);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [draft]);

  const key = `${org}|${q}|${show.id}`;
  useEffect(() => {
    cursors.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const [repos, reload] = useLoad<ApiPage<CiOrgRepo>>(
    () =>
      listOrgRepos(org, {
        q: q || undefined,
        enabled: show.enabled,
        cursor: cursors.cursor,
        limit: PAGE_SIZE,
      }),
    `${key}#${cursors.key}`,
  );

  const enable = async (name: string) => {
    setEnabling(name);
    setEnableError(null);
    try {
      await patchEnabledRepos(org, { add: [name] });
      reload();
    } catch (err) {
      setEnableError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setEnabling(null);
    }
  };

  return (
    <Page
      title="Repositories"
      meta={
        repos.state === "ready"
          ? `${abbreviate(repos.data.total)} repositories`
          : org
      }
    >
      <section
        aria-label="Repositories"
        className="rounded-md border border-rule"
      >
        <div className="flex flex-wrap items-center justify-between gap-2.5 rounded-t-md border-b border-rule bg-panel px-3.5 py-2.5">
          <label className="relative">
            <span className="sr-only">Search repositories</span>
            <Search
              size={14}
              aria-hidden="true"
              className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3"
            />
            <input
              type="search"
              value={draft ?? q}
              onChange={(event) => setDraft({ from: q, text: event.target.value })}
              placeholder="Search repositories"
              className="h-8 w-[260px] rounded-sm border border-control bg-background pr-2 pl-8 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
            />
          </label>
          <div
            role="tablist"
            aria-label="Filter by enabled state"
            className="flex overflow-hidden rounded-sm border border-control font-mono text-[13px]"
          >
            {SHOW.map((item, index) => (
              <button
                key={item.id}
                type="button"
                role="tab"
                aria-selected={item.id === show.id}
                onClick={() =>
                  setQuery({ show: item.id === "all" ? null : item.id })
                }
                className={cn(
                  "cursor-pointer px-3 py-1.5",
                  index < SHOW.length - 1 && "border-r border-control",
                  item.id === show.id
                    ? "bg-fg text-background"
                    : "text-fg-2 hover:text-fg",
                )}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>

        {enableError && (
          <p className="border-b border-rule px-3.5 py-2 font-mono text-[13px] text-failed" role="alert">
            {enableError.message}
          </p>
        )}

        <Await loaded={repos} retry={reload}>
          {(page) =>
            page.items.length === 0 ? (
              <div className="border-t border-rule px-3.5 py-6 font-mono text-[13px] text-fg-3">
                {q
                  ? `No repositories match "${q}".`
                  : show.enabled === undefined
                    ? "No repositories here yet."
                    : `No ${show.label.toLowerCase()} repositories.`}
              </div>
            ) : (
              <div className="relative overflow-auto">
                <table className="w-full min-w-[820px] border-collapse">
                  <thead className="bg-background">
                    <tr>
                      <th className={`${TH} pl-3.5`}>Repository</th>
                      <th className={TH}>Default branch</th>
                      <th className={TH}>Last run</th>
                      <th className={TH}>Started</th>
                      <th className={TH}>State</th>
                      <th className={`${TH} pr-3.5`}>
                        <span className="sr-only">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {page.items.map((repo) => (
                      <tr
                        key={repo.name}
                        className="border-t border-rule hover:bg-raised"
                      >
                        <td className="max-w-[360px] py-2.5 pr-3 pl-3.5 font-semibold whitespace-nowrap">
                          <Link href={repoUrl(repo.name)} className="block min-w-0 hover:underline">
                            <Truncate value={repo.name} className="block" />
                          </Link>
                        </td>
                        <td className="max-w-[220px] px-3 py-2.5 whitespace-nowrap">
                          <Truncate value={repo.default_branch} mono copy />
                        </td>
                        <td className="px-3 py-2.5 whitespace-nowrap">
                          {repo.last_run ? (
                            <span className="inline-flex items-center gap-2.5">
                              <StatusPill
                                status={ciStatus(
                                  repo.last_run.status,
                                  repo.last_run.conclusion,
                                )}
                                className="h-5 text-xs"
                              />
                              <Link
                                href={runUrl(repo.name, repo.last_run.id)}
                                className="font-mono text-[13px] text-fg-2 hover:text-fg hover:underline"
                              >
                                #{repo.last_run.run_number}
                              </Link>
                            </span>
                          ) : (
                            <span className="font-mono text-[13px] text-fg-3">
                              no runs yet
                            </span>
                          )}
                        </td>
                        <td className="px-3 py-2.5 text-[13px] whitespace-nowrap text-fg-3">
                          {repo.last_run
                            ? ago(repo.last_run.created_at, now)
                            : "-"}
                        </td>
                        <td className="px-3 py-2.5 whitespace-nowrap">
                          <span className={cn("inline-flex items-center gap-2 font-mono text-[13px]", repo.enabled ? "text-fg" : "text-fg-3")}>
                            <span className={cn("size-1.5 rounded-full", repo.enabled ? "bg-passed" : "bg-queued")} />
                            {repo.enabled ? "enabled" : "disabled"}
                          </span>
                        </td>
                        <td className="px-3.5 py-1.5 text-right whitespace-nowrap">
                          {repo.enabled ? (
                            <Button variant="secondary" size="sm" onClick={() => setDispatch(repo.name)} aria-label={`Run workflow in ${repo.name}`}>
                              Run workflow
                            </Button>
                          ) : (
                            admin && (
                              <Button variant="secondary" size="sm" disabled={enabling === repo.name} onClick={() => enable(repo.name)} aria-label={`Enable CI for ${repo.name}`}>
                                {enabling === repo.name ? "Enabling" : "Enable"}
                              </Button>
                            )
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )
          }
        </Await>

        {repos.state === "ready" && (
          <div className="rounded-b-md border-t border-rule bg-panel px-3.5 py-2.5">
            <CursorPager
              page={repos.data}
              index={cursors.index}
              pageSize={PAGE_SIZE}
              hasPrev={cursors.hasPrev}
              onPrev={cursors.prev}
              onNext={cursors.next}
              range="latest run first"
              noun="repositories"
              prevLabel="Previous"
              nextLabel="Next"
            />
          </div>
        )}
      </section>
      {dispatch && (
        <DispatchDialog
          owner={dispatch.slice(0, dispatch.indexOf("/"))}
          repo={dispatch.slice(dispatch.indexOf("/") + 1)}
          open
          onOpenChange={(open) => !open && setDispatch(null)}
          onStarted={(run) => navigate(runUrl(dispatch, run))}
        />
      )}
    </Page>
  );
}
