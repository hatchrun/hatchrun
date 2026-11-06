// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Deployment environments across the organization: which branches may
// deploy to one, whether an admin approves each job, and how many
// secrets and variables only its jobs get.

import { Plus, Search, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { deleteCiEnvironment, listOrgEnvironments, putCiEnvironment, type CiOrgEnvironment } from "@/api";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { Page } from "@/components/Page";
import { SidePanel, useRoomForPanel } from "@/components/SidePanel";
import { RepoMultiSelect } from "@/components/RepoMultiSelect";
import { Await } from "@/components/Status";
import { Truncate } from "@/components/Truncate";
import { abbreviate, isoDateTime, tableDate } from "@/lib/format";
import { useDebounced } from "@/lib/useRepos";
import { orgUrl, repoUrl } from "@/lib/urls";
import { cn } from "@/lib/utils";
import { useLoad } from "@/load";
import { Link, setQuery, useQuery } from "@/router";
import { BTN, BTN_PRIMARY, BTN_SM, FIELD, TD, TH } from "./formStyles";
import type { OrgPageProps } from "./types";

const patterns = (text: string) =>
  text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");

/** A single repository out of the multi-select: the one picked last, none when cleared. */
const onlyRepo = (current: string | null, next: { all: boolean; repos: string[] }) =>
  next.all || next.repos.length === 0 ? null : (next.repos.find((name) => name !== current) ?? next.repos[next.repos.length - 1] ?? null);

/** Whether the repository already has an environment of this name; the list search is a substring match, so read every page. */
async function taken(org: string, repository: string, name: string): Promise<boolean> {
  let cursor: string | null = null;
  do {
    const page: Awaited<ReturnType<typeof listOrgEnvironments>> = await listOrgEnvironments(org, { q: name, repo: repository, cursor, limit: 50 });
    if (page.items.some((item) => item.repository === repository && item.name === name)) return true;
    cursor = page.next;
  } while (cursor);
  return false;
}

export function Environments({ org }: OrgPageProps) {
  const repo = useQuery("repo");
  const cursors = useCursors();
  const [search, setSearch] = useState("");
  const q = useDebounced(search.trim(), 250);
  const room = useRoomForPanel();
  const [panel, setPanel] = useState<{ edit: CiOrgEnvironment | null; id: number } | null>(null);
  const panelSeq = useRef(0);
  const openPanel = (edit: CiOrgEnvironment | null) => setPanel({ edit, id: (panelSeq.current += 1) });
  const [confirming, setConfirming] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  // A panel belongs to one organization: its repository is that org's.
  const [seenOrg, setSeenOrg] = useState(org);
  if (seenOrg !== org) {
    setSeenOrg(org);
    cursors.reset();
    setPanel(null);
    setConfirming(null);
  }
  const [toast, setToast] = useState<string | null>(null);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const [rows, reload] = useLoad(
    () => listOrgEnvironments(org, { q, repo: repo ?? undefined, cursor: cursors.cursor, limit: 50 }),
    `${org}/${repo ?? ""}/${q}/${cursors.key}`,
    true,
  );
  const notify = (message: string) => {
    setToast(message);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setToast(null), 6000);
  };

  const remove = async (row: CiOrgEnvironment) => {
    const [owner = "", name = ""] = row.repository.split("/");
    setFailure(null);
    try {
      await deleteCiEnvironment(owner, name, row.name);
      setConfirming(null);
      notify(`Deleted ${row.name}`);
      // The last row of a later page is gone: go back a page, whose key reloads.
      if (rows.state === "ready" && rows.data.items.length === 1 && cursors.hasPrev) cursors.prev();
      else reload();
    } catch (error) {
      setFailure(error instanceof Error ? error.message : String(error));
    }
  };

  const total = rows.state === "ready" ? rows.data.total : null;
  return (
    <Page
      title="Environments"
      meta={total === null ? "" : `${abbreviate(total)} environment${total === 1 ? "" : "s"}${repo || q ? " match" : ""}`}
      actions={
        <button type="button" className={BTN_PRIMARY} onClick={() => openPanel(null)}>
          <Plus size={12} aria-hidden="true" />
          New environment
        </button>
      }
    >
      <div className="flex flex-wrap items-center gap-2">
        <label className="relative">
          <span className="sr-only">Search environments</span>
          <Search size={14} aria-hidden="true" className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3" />
          <input
            type="search"
            value={search}
            onChange={(event) => {
              setSearch(event.target.value);
              cursors.reset();
            }}
            placeholder="Search environments"
            autoComplete="off"
            spellCheck={false}
            className="h-8 w-[240px] rounded-sm border border-control bg-background pr-2 pl-8 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
          />
        </label>
        <RepoMultiSelect
          org={org}
          value={{ all: repo === null, repos: repo ? [repo] : [] }}
          onChange={(next) => {
            cursors.reset();
            setQuery({ repo: onlyRepo(repo, next) });
          }}
          label="Repo filter"
          trigger={
            <>
              <span className="text-fg-3">Repo</span>
              <span className="truncate text-fg">{repo ?? "any"}</span>
            </>
          }
        />
        {repo && (
          <button
            type="button"
            className="cursor-pointer font-mono text-[13px] text-fg underline decoration-control underline-offset-3 hover:decoration-fg-3"
            onClick={() => {
              cursors.reset();
              setQuery({ repo: null });
            }}
          >
            clear
          </button>
        )}
      </div>

      {failure && (
        <p className="notice notice-error !mb-0" role="alert">
          {failure}
        </p>
      )}

      <div className={cn("grid items-start gap-4", panel && room ? "grid-cols-[minmax(0,1fr)_360px]" : "grid-cols-1")}>
        <section aria-label="Environments list" className="flex min-w-0 flex-col gap-3">
          <Await loaded={rows} retry={reload}>
            {(page) => (
              <>
                {page.items.length === 0 ? (
                  <div className="flex flex-col items-start gap-3 rounded-sm border border-control px-4 py-6 font-mono text-[13px] text-fg-2">
                    {q || repo ? "No environments match those filters." : "No environments yet. Jobs that name one deploy through it."}
                    <button type="button" className={BTN} onClick={() => openPanel(null)}>
                      New environment
                    </button>
                  </div>
                ) : (
                  <div className="overflow-x-auto rounded-sm border border-control">
                    <table className="w-full border-collapse text-sm">
                      <thead>
                        <tr>
                          <th className={TH}>Environment</th>
                          <th className={TH}>Repository</th>
                          <th className={TH}>Branches</th>
                          <th className={TH}>Approval</th>
                          <th className={cn(TH, "text-right")}>Secrets</th>
                          <th className={cn(TH, "text-right")}>Variables</th>
                          <th className={TH}>Updated</th>
                          <th className={cn(TH, "text-right")}>Actions</th>
                        </tr>
                      </thead>
                      <tbody>
                        {page.items.map((row) => {
                          const id = `${row.repository}/${row.name}`;
                          const secretsHref = `${orgUrl(org, "secrets")}?scope=env&repo=${encodeURIComponent(row.repository)}&env=${encodeURIComponent(row.name)}`;
                          return (
                            <tr key={id} className="border-t border-rule hover:bg-panel">
                              <td className={cn(TD, "max-w-[220px]")}>
                                <Truncate value={row.name} mono className="font-bold" />
                              </td>
                              <td className={cn(TD, "max-w-[260px]")}>
                                <Link href={repoUrl(row.repository)} className="inline-flex max-w-full min-w-0 hover:underline">
                                  <Truncate value={row.repository} mono />
                                </Link>
                              </td>
                              <td className={cn(TD, "max-w-[240px] font-mono text-[13px] text-fg-2")}>
                                {row.branches.length === 0 ? "any branch" : <Truncate value={row.branches.join(", ")} mono />}
                              </td>
                              <td className={cn(TD, "font-mono text-[13px]", row.approval ? "text-waiting" : "text-fg-2")}>
                                {row.approval ? "required" : "not required"}
                              </td>
                              <td className={cn(TD, "text-right font-mono text-[13px]")}>
                                {row.secrets === 0 ? (
                                  "0"
                                ) : (
                                  <Link href={secretsHref} className="underline decoration-control underline-offset-3 hover:decoration-fg-3">
                                    {abbreviate(row.secrets)}
                                  </Link>
                                )}
                              </td>
                              <td className={cn(TD, "text-right font-mono text-[13px]")}>
                                {row.vars === 0 ? (
                                  "0"
                                ) : (
                                  <Link href={`${secretsHref}&kind=vars`} className="underline decoration-control underline-offset-3 hover:decoration-fg-3">
                                    {abbreviate(row.vars)}
                                  </Link>
                                )}
                              </td>
                              <td className={cn(TD, "font-mono text-[13px] text-fg-2")} title={isoDateTime(row.updatedAt)}>
                                {tableDate(row.updatedAt)}
                              </td>
                              <td className={cn(TD, "text-right")}>
                                {confirming === id ? (
                                  <span className="inline-flex items-center gap-2">
                                    <span className="cap max-w-[160px] truncate" title={`Delete ${row.name}?`}>
                                      Delete {row.name}?
                                    </span>
                                    <button type="button" className={BTN_SM} aria-label={`Confirm delete ${row.name}`} onClick={() => void remove(row)}>
                                      Confirm
                                    </button>
                                    <button type="button" className={BTN_SM} onClick={() => setConfirming(null)}>
                                      Cancel
                                    </button>
                                  </span>
                                ) : (
                                  <span className="inline-flex gap-2">
                                    <button type="button" className={BTN_SM} aria-label={`Edit ${row.name} in ${row.repository}`} onClick={() => openPanel(row)}>
                                      Edit
                                    </button>
                                    <button type="button" className={BTN_SM} aria-label={`Delete ${row.name} in ${row.repository}`} onClick={() => setConfirming(id)}>
                                      Delete
                                    </button>
                                  </span>
                                )}
                              </td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </div>
                )}
                <CursorPager
                  page={page}
                  index={cursors.index}
                  hasPrev={cursors.hasPrev}
                  onPrev={cursors.prev}
                  onNext={cursors.next}
                  noun="environments"
                />
                <p className="meta">Deleting an environment deletes its secrets and variables, and jobs that use it fail.</p>
              </>
            )}
          </Await>
        </section>

        {panel && (
          <SidePanel inline={room} labelledBy="env-title" onClose={() => setPanel(null)}>
          <EnvPanel
            key={`${panel.id}/${panel.edit ? `${panel.edit.repository}/${panel.edit.name}` : "new"}`}
            org={org}
            edit={panel.edit}
            defaultRepo={repo}
            onClose={() => setPanel(null)}
            onSaved={(message) => {
              // A late save must not close a panel opened since.
              setPanel((current) => (current?.id === panel.id ? null : current));
              notify(message);
              reload();
            }}
          />
          </SidePanel>
        )}
      </div>

      {toast && (
        <div
          role="status"
          className="fixed right-6 bottom-6 z-30 flex items-center gap-3 rounded-sm border border-control bg-raised px-4 py-2 font-mono text-[13px] shadow-lg"
        >
          <span aria-hidden="true" className="text-passed">
            ✓
          </span>
          <span>{toast}</span>
          <button type="button" className={BTN_SM} aria-label="Dismiss notification" onClick={() => setToast(null)}>
            Dismiss
          </button>
        </div>
      )}
    </Page>
  );
}

function EnvPanel({
  org,
  edit,
  defaultRepo,
  onClose,
  onSaved,
}: {
  org: string;
  edit: CiOrgEnvironment | null;
  defaultRepo: string | null;
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const [repository, setRepository] = useState<string | null>(edit?.repository ?? defaultRepo);
  const [name, setName] = useState(edit?.name ?? "");
  const [branches, setBranches] = useState(edit?.branches.join("\n") ?? "");
  const [approval, setApproval] = useState(edit?.approval ?? false);
  const [tried, setTried] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  const given = name.trim();
  const save = async () => {
    setTried(true);
    if (!repository || given === "") return;
    const [owner = "", repoName = ""] = repository.split("/");
    setBusy(true);
    setProblem(null);
    try {
      if (!edit && (await taken(org, repository, given))) {
        setProblem(`${given} already exists in ${repository}. Edit it instead.`);
        setBusy(false);
        return;
      }
      const stored = await putCiEnvironment(owner, repoName, given, { branches: patterns(branches), approval });
      onSaved(`Saved ${stored.name} in ${repository}`);
    } catch (error) {
      setProblem(error instanceof Error ? error.message : String(error));
      setBusy(false);
    }
  };

  return (
    <section aria-labelledby="env-title" className="flex flex-col gap-4 rounded-sm border border-control bg-panel p-4">
      <div className="flex items-center justify-between">
        <h2 id="env-title" className="cap">
          {edit ? `Edit ${edit.name}` : "New environment"}
        </h2>
        <button type="button" aria-label="Close panel" onClick={onClose} className="cursor-pointer text-fg-3 hover:text-fg">
          <X size={14} aria-hidden="true" />
        </button>
      </div>

      <div className="flex flex-col gap-1.5">
        <span className="cap">
          <span className="text-fg-2">01</span> Repository
        </span>
        {edit ? (
          <Truncate value={edit.repository} mono />
        ) : (
          <div>
            <RepoMultiSelect
              org={org}
              value={{ all: false, repos: repository ? [repository] : [] }}
              onChange={(next) => setRepository(onlyRepo(repository, next))}
              allowAll={false}
              label="Repository"
              trigger={<span className="truncate">{repository ?? "Select a repository"}</span>}
            />
          </div>
        )}
        {tried && !repository && (
          <div role="alert" className="font-mono text-[13px] text-failed">
            ✕ Select a repository.
          </div>
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        <label className="cap" htmlFor="env-name">
          <span className="text-fg-2">02</span> Name
        </label>
        <input
          id="env-name"
          className={FIELD}
          value={name}
          readOnly={edit !== null}
          onChange={(event) => setName(event.target.value)}
          aria-invalid={tried && given === ""}
          autoComplete="off"
          spellCheck={false}
        />
        {tried && given === "" && (
          <div role="alert" className="font-mono text-[13px] text-failed">
            ✕ Name is required.
          </div>
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        <label className="cap" htmlFor="env-branches">
          <span className="text-fg-2">03</span> Branches
        </label>
        <textarea
          id="env-branches"
          rows={3}
          className={cn(FIELD, "resize-y leading-normal")}
          value={branches}
          onChange={(event) => setBranches(event.target.value)}
          aria-describedby="env-branches-help"
          spellCheck={false}
          placeholder="main"
        />
        <div id="env-branches-help" className="meta">
          One pattern per line. Leave empty to allow any branch.
        </div>
      </div>

      <label className="flex cursor-pointer items-center gap-2 font-mono text-[13px]">
        <input type="checkbox" className="accent-brand" checked={approval} onChange={(event) => setApproval(event.target.checked)} />
        Require approval from an organization admin
      </label>

      {problem && (
        <p className="notice notice-error !mb-0" role="alert">
          {problem}
        </p>
      )}

      <div className="flex gap-2">
        <button type="button" className={BTN_PRIMARY} disabled={busy} onClick={() => void save()}>
          {busy ? "Saving" : "Save environment"}
        </button>
        <button type="button" className={BTN} onClick={onClose}>
          Cancel
        </button>
      </div>
    </section>
  );
}
