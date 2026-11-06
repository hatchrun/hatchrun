// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ExternalLink, Search } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

import {
  getRepo,
  patchEnabledRepos,
  listRepoBranches,
  listRepoPulls,
  listRepoRuns,
  listRepoWorkflows,
  type CiPullRef,
  type CiRepo,
  type CiRun,
  type Page as ApiPage,
  type PullState,
} from "@/api";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { FilterBar, useUrlFilters } from "@/components/FilterBar";
import { JobSquares } from "@/components/JobSquares";
import { Await } from "@/components/Status";
import { StatusPill, ciStatus } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { Button } from "@/components/ui/button";
import { abbreviate, ago, shortSha } from "@/lib/format";
import { orgUrl, repoUrl, runUrl, workflowUrl, type RepoTab } from "@/lib/urls";
import { cn } from "@/lib/utils";
import { useLoad, useNow } from "@/load";
import { Link, navigate, setQuery, useQuery } from "@/router";
import { RunsTable } from "./runs/RunsTable";
import { DispatchDialog } from "./ci/DispatchDialog";
import type { OrgPageProps } from "./types";

const TH = "cap px-3 py-2.5 text-left font-normal";
const PAGE_SIZE = 50;
const SEP = (
  <span aria-hidden="true" className="text-control">
    /
  </span>
);

interface RepositoryProps extends OrgPageProps {
  repo: string;
  tab: RepoTab;
  /** Set on a workflow's page. */
  workflow: string | null;
}

/** A repository: header, tabs (Runs, Workflows, Branches, Pull requests) and the tab's list. */
export function Repository({ org, repo, tab, workflow, tenant }: RepositoryProps) {
  const [loaded, reload] = useLoad<CiRepo>(() => getRepo(org, repo), `${org}/${repo}`);
  const [dispatch, setDispatch] = useState(false);

  return (
    <div className="mx-auto flex w-full max-w-[1400px] flex-col gap-4">
      <Await loaded={loaded} retry={reload}>
        {(info) => (
          <>
            <Crumbs org={org} repo={repo} workflow={tab === "workflows" ? workflow : null} />
            <Header info={info} org={org} admin={tenant.role === "admin"} onRun={() => setDispatch(true)} onEnabled={reload} />
            <Tabs info={info} tab={tab} />
            {tab === "runs" && <RunsTab org={org} repo={repo} workflow={null} />}
            {tab === "workflows" && (workflow ? <RunsTab org={org} repo={repo} workflow={workflow} /> : <WorkflowsTab org={org} repo={repo} />)}
            {tab === "branches" && <BranchesTab org={org} repo={repo} defaultBranch={info.default_branch} />}
            {tab === "pulls" && <PullsTab org={org} repo={repo} />}
            {dispatch && <DispatchDialog owner={org} repo={repo} open onOpenChange={(open) => !open && setDispatch(false)} onStarted={(run) => navigate(runUrl(info.name, run))} />}
          </>
        )}
      </Await>
    </div>
  );
}

function Crumbs({ org, repo, workflow }: { org: string; repo: string; workflow: string | null }) {
  const link = "min-w-0 truncate underline decoration-control underline-offset-4 hover:text-fg";
  return (
    <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-2 font-mono text-[13px] text-fg-3">
      <Link href={orgUrl(org)} className="text-fg hover:text-brand">
        {org}
      </Link>
      {SEP}
      {workflow ? (
        <Link href={repoUrl({ org, repo })} className={link}>
          {repo}
        </Link>
      ) : (
        <span aria-current="page" className="min-w-0 truncate text-fg">
          {repo}
        </span>
      )}
      {workflow && (
        <>
          {SEP}
          <Link href={repoUrl({ org, repo }, "workflows")} className={link}>
            Workflows
          </Link>
          {SEP}
          <span aria-current="page" title={workflow} className="min-w-0 truncate text-fg">
            {workflow}
          </span>
        </>
      )}
    </nav>
  );
}

function Header({ info, org, admin, onRun, onEnabled }: { info: CiRepo; org: string; admin: boolean; onRun: () => void; onEnabled: () => void }) {
  const [enabling, setEnabling] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const enable = async () => {
    setEnabling(true);
    setError(null);
    try {
      await patchEnabledRepos(org, { add: [info.name] });
      onEnabled();
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setEnabling(false);
    }
  };
  return (
    <div className="flex flex-wrap items-end justify-between gap-3">
      <div className="flex min-w-0 flex-col gap-2">
        <h1 className="page-title min-w-0 break-words" title={info.name}>
          {info.name}
        </h1>
        <div className="flex flex-wrap items-center gap-x-5 gap-y-1.5 font-mono text-[13px]">
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">default branch</span>
            <Truncate value={info.default_branch} copy className="max-w-[240px] text-fg" />
          </span>
          {info.private && <span className="rounded-xs border border-control px-1.5 text-xs text-fg-2">private</span>}
          <span className={cn("inline-flex items-center gap-2", info.enabled ? "text-fg" : "text-fg-3")}>
            <span className={cn("size-1.5 rounded-full", info.enabled ? "bg-passed" : "bg-queued")} />
            {info.enabled ? "enabled" : "disabled"}
          </span>
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Button asChild variant="outline">
          <a href={info.html_url} target="_blank" rel="noreferrer" aria-label={`View ${info.name} on GitHub, opens github.com`}>
            View on GitHub
            <ExternalLink aria-hidden="true" />
          </a>
        </Button>
        {info.enabled && info.access !== "read" && <Button onClick={onRun}>Run workflow</Button>}
      </div>
      {!info.enabled && (
        <div className="flex basis-full flex-wrap items-center gap-x-4 gap-y-2 rounded-md border border-rule bg-panel px-3.5 py-2.5 font-mono text-[13px]" role="status">
          <span className="text-fg">CI is off for this repository.</span>
          {admin ? (
            <>
              <span className="text-fg-3">Pushes and pull requests start no runs until it is enabled.</span>
              <Button size="sm" variant="secondary" disabled={enabling} onClick={enable}>
                {enabling ? "Enabling" : "Enable"}
              </Button>
            </>
          ) : (
            <span className="text-fg-3">An organization admin can turn it on.</span>
          )}
          {error && (
            <span className="text-failed" role="alert">
              {error.message}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

function Tabs({ info, tab }: { info: CiRepo; tab: RepoTab }) {
  const items: { id: RepoTab; label: string; count: number | null }[] = [
    { id: "runs", label: "Runs", count: null },
    { id: "workflows", label: "Workflows", count: info.counts.workflows },
    { id: "branches", label: "Branches", count: info.counts.branches },
    { id: "pulls", label: "Pull requests", count: info.counts.open_pulls },
  ];
  return (
    <nav aria-label="Repository" className="flex gap-5 border-b border-rule font-mono text-[13px]">
      {items.map((item) => (
        <Link
          key={item.id}
          href={repoUrl(info.name, item.id)}
          aria-current={item.id === tab ? "page" : undefined}
          className={cn("-mb-px border-b-2 py-2", item.id === tab ? "border-brand text-fg" : "border-transparent text-fg-3 hover:text-fg")}
        >
          {item.label} {item.count !== null && <span className="text-fg-3">{abbreviate(item.count)}</span>}
        </Link>
      ))}
    </nav>
  );
}

/** The frame of a list: tools on top, the table, the pager under it. */
function ListFrame({ label, tools, children, footer }: { label: string; tools?: ReactNode; children: ReactNode; footer?: ReactNode }) {
  return (
    <section aria-label={label} className="rounded-md border border-rule">
      {tools && <div className="flex flex-col gap-2.5 rounded-t-md border-b border-rule bg-panel px-3.5 py-2.5">{tools}</div>}
      {children}
      {footer && <div className="rounded-b-md border-t border-rule bg-panel px-3.5 py-2.5">{footer}</div>}
    </section>
  );
}

const Empty = ({ children }: { children: ReactNode }) => <div className="px-3.5 py-6 font-mono text-[13px] text-fg-3">{children}</div>;

/** A search box that writes `?q=` after a pause. */
function SearchBox({ placeholder }: { placeholder: string }) {
  const q = useQuery("q") ?? "";
  const [typed, setDraft] = useState<{ from: string; text: string } | null>(null);
  const draft = typed !== null && typed.from === q ? typed.text : null;
  useEffect(() => {
    if (draft === null) return;
    const timer = window.setTimeout(() => {
      setQuery({ q: draft });
      setDraft(null);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [draft]);
  return (
    <label className="relative">
      <span className="sr-only">{placeholder}</span>
      <Search size={14} aria-hidden="true" className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3" />
      <input
        type="search"
        value={draft ?? q}
        onChange={(event) => setDraft({ from: q, text: event.target.value })}
        placeholder={placeholder}
        className="h-8 w-[260px] rounded-sm border border-control bg-background pr-2 pl-8 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
      />
    </label>
  );
}

/** The last run of a row: status, number, age. */
function LastRun({ run, now }: { run: CiRun | null; now: number }) {
  if (!run) {
    return <span className="font-mono text-[13px] text-fg-3">no runs yet</span>;
  }
  return (
    <span className="inline-flex items-center gap-2.5">
      <StatusPill status={ciStatus(run.status, run.conclusion)} className="h-5 text-xs" />
      <Link href={runUrl(run.repository, run.id)} className="font-mono text-[13px] text-fg-2 hover:text-fg hover:underline">
        #{run.run_number}
      </Link>
      <span className="text-[13px] text-fg-3">{ago(run.created_at, now)}</span>
    </span>
  );
}

// ---- Runs, and a workflow's runs ----------------------------------------------------

function RunsTab({ org, repo, workflow }: { org: string; repo: string; workflow: string | null }) {
  const filters = useUrlFilters();
  const pullParam = useQuery("pull");
  const pull = pullParam && /^\d+$/.test(pullParam) ? Number(pullParam) : undefined;
  const cursors = useCursors();
  const now = useNow(15_000);
  const key = `${org}/${repo}|${workflow ?? ""}|${filters.key}|${pull ?? ""}`;

  useEffect(() => {
    cursors.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const [runs, reload] = useLoad<ApiPage<CiRun>>(
    () => listRepoRuns(org, repo, { q: filters.q || undefined, branch: filters.branch ?? undefined, actor: filters.actor ?? undefined, workflow: workflow ?? undefined, pull, cursor: cursors.cursor, limit: PAGE_SIZE }),
    `${key}#${cursors.key}`,
    true,
  );
  const live = runs.state === "ready" && runs.data.items.some((run) => run.status !== "completed");
  useEffect(() => {
    const timer = window.setInterval(reload, live ? 5_000 : 30_000);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, key, cursors.key]);

  const branches = useMemo(() => (runs.state === "ready" ? [...new Set(runs.data.items.flatMap((run) => (run.branch ? [run.branch] : [])))].sort() : []), [runs]);
  const actors = useMemo(() => (runs.state === "ready" ? [...new Set(runs.data.items.map((run) => run.actor))].sort() : []), [runs]);

  return (
    <ListFrame
      label={workflow ? `Runs of ${workflow}` : "Runs"}
      tools={
        <>
        {(pull !== undefined || filters.branch) && <ScopeHeader pull={pull} pullRef={runs.state === "ready" ? (runs.data.items.find((run) => run.pull_request?.number === pull)?.pull_request ?? null) : null} branch={filters.branch} />}
        <FilterBar
          org={org}
          showRepo={false}
          branches={branches}
          actors={actors}
          extra={pull === undefined ? [] : [{ id: "pull", label: `Pull request: #${pull}`, remove: () => setQuery({ pull: null }) }]}
        />
        </>
      }
      footer={runs.state === "ready" && <CursorPager page={runs.data} index={cursors.index} pageSize={PAGE_SIZE} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range="newest first" noun="runs" />}
    >
      <Await loaded={runs} retry={reload}>
        {(page) =>
          page.items.length === 0 ? (
            <Empty>{workflow ? `No runs of ${workflow} match these filters.` : "No runs match these filters."}</Empty>
          ) : (
            <RunsTable runs={page.items} now={now} inRepo inWorkflow={workflow !== null} />
          )
        }
      </Await>
    </ListFrame>
  );
}

/** What a narrowed run list is about: a pull request (title, head into base) or a branch. */
function ScopeHeader({ pull, pullRef, branch }: { pull: number | undefined; pullRef: CiPullRef | null; branch: string | null }) {
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[13px]">
      {pull !== undefined ? (
        <>
          <span className="shrink-0 font-semibold text-fg">PR #{pull}</span>
          {pullRef && (
            <>
              <Truncate value={pullRef.title} className="max-w-[420px] font-sans text-fg-2" />
              <span className="inline-flex min-w-0 items-center gap-1.5">
                <Truncate value={pullRef.head_ref} mono copy className="max-w-[220px]" />
                <span aria-label="into" className="shrink-0 text-fg-3">
                  →
                </span>
                <Truncate value={pullRef.base_ref} mono className="max-w-[160px] text-fg-2" />
              </span>
            </>
          )}
        </>
      ) : (
        <>
          <span className="cap">Branch</span>
          <Truncate value={branch ?? ""} mono copy className="max-w-[360px] font-semibold" />
        </>
      )}
    </div>
  );
}

// ---- Workflows ----------------------------------------------------------------------

function WorkflowsTab({ org, repo }: { org: string; repo: string }) {
  const cursors = useCursors();
  const now = useNow(30_000);
  const [workflows, reload] = useLoad(() => listRepoWorkflows(org, repo, { cursor: cursors.cursor, limit: PAGE_SIZE }), `${org}/${repo}#${cursors.key}`);
  return (
    <ListFrame
      label="Workflows"
      footer={workflows.state === "ready" && <CursorPager page={workflows.data} index={cursors.index} pageSize={PAGE_SIZE} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range="latest activity first" noun="workflows" prevLabel="Previous" nextLabel="Next" />}
    >
      <Await loaded={workflows} retry={reload}>
        {(page) =>
          page.items.length === 0 ? (
            <Empty>No workflow has run in this repository yet.</Empty>
          ) : (
            <div className="relative overflow-auto">
              <table className="w-full min-w-[900px] border-collapse">
                <thead className="bg-background">
                  <tr>
                    <th className={`${TH} pl-3.5`}>Workflow</th>
                    <th className={TH}>Last run</th>
                    <th className={TH}>Branch</th>
                    <th className={`${TH} text-right`}>Runs, 7 d</th>
                    <th className={`${TH} text-right`}>Passed</th>
                    <th className={`${TH} text-right`}>Failed</th>
                    <th className={`${TH} pr-3.5`}>Pass rate</th>
                  </tr>
                </thead>
                <tbody>
                  {page.items.map((item) => {
                    const done = item.passed_7d + item.failed_7d;
                    const rate = done === 0 ? null : Math.round((item.passed_7d / done) * 100);
                    return (
                      <tr key={item.name} className="border-t border-rule hover:bg-raised">
                        <td className="max-w-[300px] py-2.5 pr-3 pl-3.5 font-semibold whitespace-nowrap">
                          <Link href={workflowUrl({ org, repo }, item.name)} className="block min-w-0 hover:underline">
                            <Truncate value={item.name} className="block" />
                          </Link>
                        </td>
                        <td className="px-3 py-2.5 whitespace-nowrap">
                          <LastRun run={item.last_run} now={now} />
                        </td>
                        <td className="max-w-[240px] px-3 py-2.5 whitespace-nowrap">
                          {item.last_run?.branch ? <Truncate value={item.last_run.branch} mono copy /> : <span className="text-fg-3">-</span>}
                        </td>
                        <td className="px-3 py-2.5 text-right font-mono text-[13px] text-fg-2">{abbreviate(item.runs_7d)}</td>
                        <td className="px-3 py-2.5 text-right font-mono text-[13px] text-passed">{abbreviate(item.passed_7d)}</td>
                        <td className={cn("px-3 py-2.5 text-right font-mono text-[13px]", item.failed_7d > 0 ? "text-failed" : "text-fg-3")}>{abbreviate(item.failed_7d)}</td>
                        <td className="px-3 py-2.5 pr-3.5 font-mono text-[13px] whitespace-nowrap text-fg-2">
                          {rate === null ? (
                            "-"
                          ) : (
                            <span className="inline-flex items-center gap-2.5">
                              <span aria-hidden="true" className="h-1.5 w-20 overflow-hidden rounded-full bg-failed/40">
                                <span className="block h-full bg-passed" style={{ width: `${rate}%` }} />
                              </span>
                              {rate}%
                            </span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )
        }
      </Await>
    </ListFrame>
  );
}

// ---- Branches -----------------------------------------------------------------------

function BranchesTab({ org, repo, defaultBranch }: { org: string; repo: string; defaultBranch: string }) {
  const q = useQuery("q") ?? "";
  const cursors = useCursors();
  const now = useNow(30_000);
  const key = `${org}/${repo}|${q}`;
  useEffect(() => {
    cursors.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  const [branches, reload] = useLoad(() => listRepoBranches(org, repo, { q: q || undefined, cursor: cursors.cursor, limit: PAGE_SIZE }), `${key}#${cursors.key}`);
  return (
    <ListFrame
      label="Branches"
      tools={<SearchBox placeholder="Search branches" />}
      footer={branches.state === "ready" && <CursorPager page={branches.data} index={cursors.index} pageSize={PAGE_SIZE} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range={`${defaultBranch} first, then latest run`} noun="branches" prevLabel="Previous" nextLabel="Next" />}
    >
      <Await loaded={branches} retry={reload}>
        {(page) =>
          page.items.length === 0 ? (
            <Empty>{q ? `No branch with runs matches "${q}".` : "No branch has run here yet."}</Empty>
          ) : (
            <div className="relative overflow-auto">
              <table className="w-full min-w-[980px] border-collapse">
                <thead className="bg-background">
                  <tr>
                    <th className={`${TH} pl-3.5`}>Branch</th>
                    <th className={TH}>Last run</th>
                    <th className={TH}>Commit</th>
                    <th className={TH}>Message</th>
                    <th className={TH}>Actor</th>
                    <th className={`${TH} text-right`}>Runs</th>
                    <th className={`${TH} pr-3.5`}>Jobs</th>
                  </tr>
                </thead>
                <tbody>
                  {page.items.map((branch) => (
                    <tr key={branch.name} className="border-t border-rule hover:bg-raised">
                      <td className="max-w-[320px] py-2.5 pr-3 pl-3.5 whitespace-nowrap">
                        <span className="inline-flex max-w-full items-center gap-2">
                          <Link href={repoUrl({ org, repo }, "runs", { branch: branch.name })} title={`Runs on ${branch.name}`} className="min-w-0 font-mono text-[13px] font-semibold hover:underline">
                            <Truncate value={branch.name} className="block" />
                          </Link>
                          {branch.default && <span className="shrink-0 rounded-xs border border-control px-1.5 font-mono text-xs text-fg-2">default</span>}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 whitespace-nowrap">
                        <LastRun run={branch.last_run} now={now} />
                      </td>
                      <td className="px-3 py-2.5 whitespace-nowrap">
                        <Truncate value={branch.last_run.sha} mono copy>
                          {shortSha(branch.last_run.sha)}
                        </Truncate>
                      </td>
                      <td className="max-w-[300px] min-w-[180px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                        {branch.last_run.message ? <Truncate value={branch.last_run.message} className="block" /> : <span className="text-fg-3">-</span>}
                      </td>
                      <td className="max-w-[130px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                        <Truncate value={branch.last_run.actor} className="block" />
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-[13px] whitespace-nowrap">
                        <Link href={repoUrl({ org, repo }, "runs", { branch: branch.name })} className="underline decoration-control underline-offset-3 hover:decoration-fg-3">
                          {abbreviate(branch.runs)}
                        </Link>
                      </td>
                      <td className="px-3 py-2.5 pr-3.5 whitespace-nowrap">
                        <JobSquares jobs={branch.last_run.jobs} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )
        }
      </Await>
    </ListFrame>
  );
}

// ---- Pull requests ------------------------------------------------------------------

const STATES: { id: PullState | "all"; label: string }[] = [
  { id: "open", label: "Open" },
  { id: "closed", label: "Closed" },
  { id: "merged", label: "Merged" },
  { id: "all", label: "All" },
];

const PULL_LOOK: Record<PullState, { glyph: string; text: string }> = {
  open: { glyph: "●", text: "text-passed" },
  merged: { glyph: "◆", text: "text-brand" },
  closed: { glyph: "⊘", text: "text-queued" },
};

function PullsTab({ org, repo }: { org: string; repo: string }) {
  const q = useQuery("q") ?? "";
  const stateParam = useQuery("state");
  const state = STATES.find((item) => item.id === stateParam)?.id ?? "open";
  const cursors = useCursors();
  const now = useNow(30_000);
  const key = `${org}/${repo}|${q}|${state}`;
  useEffect(() => {
    cursors.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  const [pulls, reload] = useLoad(() => listRepoPulls(org, repo, { state, q: q || undefined, cursor: cursors.cursor, limit: PAGE_SIZE }), `${key}#${cursors.key}`);
  return (
    <ListFrame
      label="Pull requests"
      tools={
        <div className="flex flex-wrap items-center justify-between gap-2.5">
          <SearchBox placeholder="Search pull requests" />
          <div role="tablist" aria-label="Filter by state" className="flex overflow-hidden rounded-sm border border-control font-mono text-[13px]">
            {STATES.map((item, index) => (
              <button
                key={item.id}
                type="button"
                role="tab"
                aria-selected={item.id === state}
                onClick={() => setQuery({ state: item.id === "open" ? null : item.id })}
                className={cn("cursor-pointer px-3 py-1.5", index < STATES.length - 1 && "border-r border-control", item.id === state ? "bg-fg text-background" : "text-fg-2 hover:text-fg")}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>
      }
      footer={pulls.state === "ready" && <CursorPager page={pulls.data} index={cursors.index} pageSize={PAGE_SIZE} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range="latest update first" noun="pull requests" prevLabel="Previous" nextLabel="Next" />}
    >
      <Await loaded={pulls} retry={reload}>
        {(page) =>
          page.items.length === 0 ? (
            <Empty>{q ? `No ${state === "all" ? "" : `${state} `}pull request matches "${q}".` : state === "all" ? "No pull request has run here yet." : `No ${state} pull requests.`}</Empty>
          ) : (
            <div className="relative overflow-auto">
              <table className="w-full min-w-[1040px] border-collapse">
                <thead className="bg-background">
                  <tr>
                    <th className={`${TH} pl-3.5`}>Pull request</th>
                    <th className={TH}>Branches</th>
                    <th className={TH}>Author</th>
                    <th className={TH}>State</th>
                    <th className={TH}>Last run</th>
                    <th className={`${TH} pr-3.5`}>Updated</th>
                  </tr>
                </thead>
                <tbody>
                  {page.items.map((pull) => {
                    const look = PULL_LOOK[pull.state];
                    const runs = repoUrl({ org, repo }, "runs", { pull: pull.number });
                    return (
                      <tr key={pull.number} className="border-t border-rule hover:bg-raised">
                        <td className="max-w-[460px] py-2.5 pr-3 pl-3.5 whitespace-nowrap">
                          <Link href={runs} title={`Runs of #${pull.number}`} className="flex min-w-0 items-baseline gap-2.5 hover:underline">
                            <span className="shrink-0 font-mono text-[13px] text-fg-3">#{pull.number}</span>
                            <Truncate value={pull.title} className="block font-semibold" />
                          </Link>
                        </td>
                        <td className="max-w-[340px] px-3 py-2.5 whitespace-nowrap">
                          <span className="inline-flex max-w-full items-center gap-1.5 font-mono text-[13px]">
                            <Truncate value={pull.head_ref} mono />
                            <span aria-label="into" className="shrink-0 text-fg-3">
                              →
                            </span>
                            <Truncate value={pull.base_ref} mono className="text-fg-2" />
                          </span>
                        </td>
                        <td className="max-w-[130px] px-3 py-2.5 whitespace-nowrap text-fg-2">
                          <Truncate value={pull.author} className="block" />
                        </td>
                        <td className="px-3 py-2.5 whitespace-nowrap">
                          <span className={cn("inline-flex items-center gap-1.5 font-mono text-[13px]", look.text)}>
                            <span aria-hidden="true">{look.glyph}</span>
                            {pull.state}
                          </span>
                        </td>
                        <td className="px-3 py-2.5 whitespace-nowrap">
                          <LastRun run={pull.last_run} now={now} />
                        </td>
                        <td className="px-3.5 py-2.5 text-[13px] whitespace-nowrap text-fg-3">{ago(pull.updated_at, now)}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )
        }
      </Await>
    </ListFrame>
  );
}

