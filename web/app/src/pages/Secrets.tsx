// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Secrets and variables of an organization, a repository or one of its
// environments. The scope lives in the URL (?scope=org|repo|env&repo=&env=).
// A secret's value is write-only; a variable's is shown.

import { ChevronRight, Plus, Search, X } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";

import {
  deleteCiItem,
  listCiSecrets,
  listCiVars,
  putCiItem,
  type CiItemKind,
  type CiScope,
  type CiSecret,
  type CiVar,
  type Page as ApiPage,
} from "@/api";
import { CopyButton } from "@/components/CopyButton";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { Page } from "@/components/Page";
import { SidePanel, useRoomForPanel } from "@/components/SidePanel";
import { RepoMultiSelect, summarizeRepos, type RepoSelection } from "@/components/RepoMultiSelect";
import { ScopePicker, type ScopeValue } from "@/components/ScopePicker";
import { Await } from "@/components/Status";
import { Truncate } from "@/components/Truncate";
import { abbreviate, isoDateTime, tableDate } from "@/lib/format";
import { useDebounced } from "@/lib/useRepos";
import { cn } from "@/lib/utils";
import { useLoad, useNow } from "@/load";
import { orgUrl, repoUrl } from "@/lib/urls";
import { Link, setQuery, useQuery } from "@/router";
import { BTN, BTN_PRIMARY, BTN_SM, FIELD, TD, TH } from "./formStyles";
import type { OrgPageProps } from "./types";

type Row = CiSecret | CiVar;

const word = (kind: CiItemKind) => (kind === "secrets" ? "secret" : "variable");

/** What a name must look like: the API uppercases it and takes A-Z, 0-9 and _ only. */
const fixName = (raw: string) => {
  const fixed = raw
    .toUpperCase()
    .replace(/[^A-Z0-9_]+/g, "_");
  return /^[0-9]/.test(fixed) ? `_${fixed}` : fixed;
};

/** Two or more KEY=VALUE lines (comments and `export` allowed); anything else is a plain value. */
function parsePaste(text: string): { name: string; value: string }[] {
  const lines = text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"));
  const matches = lines.map((line) => /^(?:export\s+)?([A-Za-z_][A-Za-z0-9_.-]*)\s*=\s*(.*)$/.exec(line));
  if (lines.length < 2 || !matches.every(Boolean)) return [];
  return matches.map((match) => {
    let value = match?.[2] ?? "";
    const quote = value.charAt(0);
    if (value.length > 1 && (quote === '"' || quote === "'") && value.endsWith(quote)) value = value.slice(1, -1);
    return { name: fixName(match?.[1] ?? ""), value };
  });
}

/** A pasted entry that cannot be saved: no usable name or no value. */
const unusable = (item: { name: string; value: string }) => !item.name || item.value === "";

/** Whether this scope itself has an item of this name; the list search is a substring match, so read every page. An inherited row of a wider scope does not count, a narrower one overrides it. */
async function taken(scope: CiScope, kind: CiItemKind, name: string): Promise<boolean> {
  const own = "org" in scope ? "org" : "environment" in scope ? "env" : "repo";
  let cursor: string | null = null;
  do {
    const query = { q: name, cursor, limit: 50 };
    const page: ApiPage<Row> = await (kind === "secrets" ? listCiSecrets(scope, query) : listCiVars(scope, query));
    if (page.items.some((item) => item.name === name && item.scope === own)) return true;
    cursor = page.next;
  } while (cursor);
  return false;
}

const reference = (kind: CiItemKind, name: string) => `\${{ ${kind === "secrets" ? "secrets" : "vars"}.${name || "NAME"} }}`;

function scopeOf(org: string, value: ScopeValue): CiScope | null {
  if (value.scope === "org") return { org };
  const [owner = "", repo = ""] = (value.repo ?? "").split("/");
  if (!value.repo) return null;
  if (value.scope === "repo") return { owner, repo };
  return value.env ? { owner, repo, environment: value.env } : null;
}

const scopeWord = (org: string, value: ScopeValue) =>
  value.scope === "org" ? `organization ${org}` : value.scope === "repo" ? `repository ${value.repo}` : `environment ${value.env} of ${value.repo}`;

export function Secrets({ org, tenant }: OrgPageProps) {
  const scopeParam = useQuery("scope");
  const repoParam = useQuery("repo");
  const envParam = useQuery("env");
  const kindParam = useQuery("kind");
  const value: ScopeValue = {
    scope: scopeParam === "repo" || scopeParam === "env" ? scopeParam : "org",
    repo: scopeParam === "repo" || scopeParam === "env" ? repoParam : null,
    env: scopeParam === "env" ? envParam : null,
  };
  const kind: CiItemKind = kindParam === "vars" ? "vars" : "secrets";
  const scope = scopeOf(org, value);
  const noun = word(kind);
  const now = useNow(60_000);
  const room = useRoomForPanel();
  const showParam = useQuery("show");
  const show: "all" | "own" | "inherited" | "stale" = showParam === "own" || showParam === "inherited" || showParam === "stale" ? showParam : "all";
  const [inheritedOpen, setInheritedOpen] = useState(false);
  // Own counts seen on a list's first page, for the pages after it.
  const ownKnown = useRef(new Map<string, number>());

  const cursors = useCursors();
  const [search, setSearch] = useState("");
  const q = useDebounced(search.trim(), 250);
  const [panel, setPanel] = useState<{ edit: Row | null; id: number } | null>(null);
  const panelSeq = useRef(0);
  const openPanel = (edit: Row | null) => setPanel({ edit, id: (panelSeq.current += 1) });
  const [toast, setToast] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(toastTimer.current), []);

  const scopeId = `${org}/${value.scope}/${value.repo ?? ""}/${value.env ?? ""}`;
  // Whatever moves the page to another list (the top bar, history, the
  // scope or tab controls) drops the open editor, the delete prompt and
  // the page cursors of the old one.
  const [seen, setSeen] = useState(`${scopeId}/${kind}`);
  if (seen !== `${scopeId}/${kind}`) {
    setSeen(`${scopeId}/${kind}`);
    cursors.reset();
    setPanel(null);
    setConfirming(null);
  }
  // Each read says which list it is of, so rows of the previous scope or tab are never shown or acted on.
  const list = async (which: CiItemKind, cursor: string | null, limit: number): Promise<ApiPage<Row> & { of: string }> => {
    const of = `${scopeId}/${which}`;
    if (!scope) return { items: [], total: 0, next: null, of };
    const page = await (which === "secrets" ? listCiSecrets(scope, { q, cursor, limit }) : listCiVars(scope, { q, cursor, limit }));
    return { ...page, of };
  };
  const [rows, reloadRows] = useLoad(() => list(kind, cursors.cursor, 50), `${scopeId}/${kind}/${q}/${cursors.key}`, true);
  const other: CiItemKind = kind === "secrets" ? "vars" : "secrets";
  const [otherRows, reloadOther] = useLoad(() => list(other, null, 1), `${scopeId}/${other}/${q}`, true);
  const reload = () => {
    reloadRows();
    reloadOther();
  };

  const setScope = (next: ScopeValue) => {
    cursors.reset();
    setPanel(null);
    setConfirming(null);
    setQuery({ scope: next.scope === "org" ? null : next.scope, repo: next.repo, env: next.env, show: null });
  };
  /** Moves to the wider scope an inherited row lives in, with the name searched. */
  const openInherited = (row: Row) => {
    cursors.reset();
    setPanel(null);
    setConfirming(null);
    setSearch(row.name);
    setQuery(row.scope === "org" ? { scope: null, repo: null, env: null, show: null } : { scope: "repo", repo: value.repo, env: null, show: null });
  };
  const setKind = (next: CiItemKind) => {
    cursors.reset();
    setPanel(null);
    setConfirming(null);
    setQuery({ kind: next === "vars" ? "vars" : null });
  };

  const notify = (message: string) => {
    setToast(message);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 6000);
  };

  const remove = async (row: Row) => {
    if (!scope) return;
    setFailure(null);
    try {
      await deleteCiItem(scope, kind, row.name);
      setConfirming(null);
      notify(`Deleted ${row.name}`);
      reload();
    } catch (error) {
      setFailure(error instanceof Error ? error.message : String(error));
    }
  };

  const current = (state: typeof rows, which: CiItemKind) => state.state === "ready" && state.data.of === `${scopeId}/${which}`;
  const total = (state: typeof rows, which: CiItemKind) => (state.state === "ready" && current(state, which) ? state.data.total : null);
  const secretsTotal = kind === "secrets" ? total(rows, kind) : total(otherRows, other);
  const varsTotal = kind === "vars" ? total(rows, kind) : total(otherRows, other);
  const own = total(rows, kind);
  const shown: typeof rows = rows.state === "ready" && !current(rows, kind) ? { state: "loading" } : rows;
  const inherited = shown.state === "ready" ? shown.data.items.filter((row) => row.scope !== value.scope).length : 0;
  const summary = own === null ? "" : `${abbreviate(own)} ${noun}${own === 1 ? "" : "s"} visible at this scope${inherited > 0 ? ` · ${abbreviate(inherited)} inherited on this page` : ""}`;
  const tabs: [CiItemKind, string, number | null][] = [
    ["secrets", "Secrets", secretsTotal],
    ["vars", "Variables", varsTotal],
  ];

  /** One table of rows; the stale hint shows at every scope. */
  const itemTable = (items: Row[]) => (
    <div className="overflow-x-auto rounded-sm border border-control">
      <table className="w-full border-collapse text-sm">
        <thead>
          <tr>
            <th className={TH}>Name</th>
            <th className={TH}>Scope / access</th>
            <th className={TH}>Value</th>
            <th className={TH}>Updated</th>
            <th className={cn(TH, "text-right")}>Actions</th>
          </tr>
        </thead>
        <tbody>
          {items.map((row) => {
            const inheritedRow = row.scope !== value.scope;
            const staleDays = staleFor(row, now);
            const edge = staleDays !== null ? { boxShadow: "inset 3px 0 0 var(--waiting)" } : undefined;
            return (
              <Fragment key={`${row.scope}/${row.name}`}>
                <tr className={cn("border-t border-rule hover:bg-panel", row.shadowed && "opacity-50")}>
                  <td className={cn(TD, "max-w-[260px]")} style={edge}>
                    <span className="inline-flex max-w-full items-center gap-2">
                      <Truncate value={row.name} mono className="font-bold" />
                      {(row.shadowed || inheritedRow) && (
                        <span className="shrink-0 rounded-xs border border-control px-1.5 font-mono text-[11px] tracking-[0.04em] text-fg-2 uppercase">
                          {row.shadowed ? "overridden" : `from ${row.scope}`}
                        </span>
                      )}
                    </span>
                  </td>
                  <td className={cn(TD, "max-w-[250px] font-mono text-[13px] text-fg-2")}>{access(value, row)}</td>
                  <td className={cn(TD, "max-w-[200px] font-mono text-[13px] text-fg-2")}>
                    {"value" in row ? <Truncate value={row.value} mono /> : "write-only"}
                  </td>
                  <td className={cn(TD, "font-mono text-[13px] text-fg-2")} title={isoDateTime(row.updatedAt)}>
                    {tableDate(row.updatedAt, now)}
                  </td>
                  <td className={cn(TD, "text-right")}>
                    {inheritedRow ? (
                      <button type="button" className={BTN_SM} aria-label={`Open ${row.name} in ${row.scope} scope`} onClick={() => openInherited(row)}>
                        Open in {row.scope}
                      </button>
                    ) : confirming === row.name ? (
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
                        <button type="button" className={BTN_SM} aria-label={`${kind === "vars" ? "Change" : "Replace"} ${row.name}`} onClick={() => openPanel(row)}>
                          {kind === "vars" ? "Change" : "Replace"}
                        </button>
                        <button type="button" className={BTN_SM} aria-label={`Delete ${row.name}`} onClick={() => setConfirming(row.name)}>
                          Delete
                        </button>
                      </span>
                    )}
                  </td>
                </tr>
                {staleDays !== null && (
                  <tr>
                    <td colSpan={5} className="px-3 pb-2.5 pl-9 font-mono text-[13px] whitespace-normal text-waiting" style={edge}>
                      <span className="text-fg-3">└ </span>
                      {row.last_used_at === null ? "never used by a job." : `not used in ${staleDays} days.`}
                      {inheritedRow ? "" : " Delete it if nothing needs it, or replace the value to rotate it."}
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );

  return (
    <Page
      title="Secrets"
      meta={
        <>
          {summary}
          {tenant.role === "admin" && (
            <>
              {summary && " · "}
              <Link href={orgUrl(org, "settings/audit")} className="text-brand quiet-underline">
                audit log →
              </Link>
            </>
          )}
        </>
      }
      actions={
        <button type="button" className={BTN_PRIMARY} disabled={!scope} onClick={() => openPanel(null)}>
          <Plus size={12} aria-hidden="true" />
          New {noun}
        </button>
      }
    >
      <section aria-label="Scope" className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <span className="cap">Scope</span>
        <ScopePicker org={org} value={value} onChange={setScope} />
        <span className="meta">Resolution order: environment, repository, organization. The narrowest scope wins.</span>
      </section>

      <div className={cn("grid items-start gap-4", panel && room ? "grid-cols-[minmax(0,1fr)_360px]" : "grid-cols-1")}>
        <section aria-label="Secrets and variables list" className="flex min-w-0 flex-col gap-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div role="tablist" aria-label="Secrets or variables" className="inline-flex gap-4 border-b border-rule font-mono text-[13px]">
              {tabs.map(([id, label, count]) => (
                <button
                  key={id}
                  type="button"
                  role="tab"
                  aria-selected={kind === id}
                  onClick={() => setKind(id)}
                  className={cn(
                    "-mb-px cursor-pointer border-b-2 py-1.5",
                    kind === id ? "border-brand text-fg" : "border-transparent text-fg-3 hover:text-fg",
                  )}
                >
                  {label} <span className="text-fg-3">{count === null ? "" : abbreviate(count)}</span>
                </button>
              ))}
            </div>
            <div className="flex flex-wrap items-center gap-3">
            <div role="group" aria-label="Show" className="inline-flex overflow-hidden rounded-sm border border-control font-mono text-xs">
              {(value.scope === "org" ? (["all", "stale"] as const) : (["all", "own", "inherited", "stale"] as const)).map((id) => (
                <button
                  key={id}
                  type="button"
                  aria-pressed={show === id}
                  onClick={() => setQuery({ show: id === "all" ? null : id })}
                  className={cn("h-8 cursor-pointer border-0 border-r border-control px-2.5 last:border-r-0", show === id ? "bg-raised text-fg" : "bg-transparent text-fg-3 hover:text-fg")}
                >
                  {id === "all" ? "All" : id === "own" ? "Own" : id === "inherited" ? "Inherited" : "Stale"}
                </button>
              ))}
            </div>
            <label className="relative">
              <span className="sr-only">Search by name</span>
              <Search size={14} aria-hidden="true" className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3" />
              <input
                type="search"
                value={search}
                onChange={(event) => {
                  setSearch(event.target.value);
                  cursors.reset();
                }}
                placeholder="Search by name"
                autoComplete="off"
                spellCheck={false}
                className="h-8 w-[240px] rounded-sm border border-control bg-background pr-2 pl-8 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
              />
            </label>
            </div>
          </div>

          {failure && (
            <p className="notice notice-error !mb-0" role="alert">
              {failure}
            </p>
          )}

          {!scope ? (
            <p className="meta rounded-sm border border-control px-4 py-6">
              {value.scope === "env" && value.repo ? "Choose an environment to see its " : "Choose a repository to see its "}
              {noun}s.
            </p>
          ) : (
            <Await loaded={shown} retry={reloadRows}>
              {(page) => {
                const ownRows = page.items.filter((row) => row.scope === value.scope);
                const inheritedRows = page.items.filter((row) => row.scope !== value.scope);
                const keep = (row: Row) => show !== "stale" || staleFor(row, now) !== null;
                const ownShown = show === "inherited" ? [] : ownRows.filter(keep);
                const inheritedShown = show === "own" ? [] : inheritedRows.filter(keep);
                const sectioned = value.scope !== "org";
                const ownCount = !sectioned ? page.total : cursors.index === 0 && (page.next === null || inheritedRows.length > 0) ? ownRows.length : ownKnown.current.get(scopeId + kind + q) ?? ownRows.length;
                if (sectioned && cursors.index === 0 && (page.next === null || inheritedRows.length > 0)) ownKnown.current.set(scopeId + kind + q, ownRows.length);
                const inheritedCount = Math.max(page.total - ownCount, inheritedRows.length);
                const from = value.scope === "env" ? "repository and organization" : "organization";
                const open = inheritedOpen || show === "inherited" || show === "stale";
                const scopeName = value.scope === "env" ? "environment" : "repository";
                return (
                  <>
                    {page.items.length === 0 ? (
                      <div className="flex flex-col items-start gap-3 rounded-sm border border-control px-4 py-6 font-mono text-[13px] text-fg-2">
                        {q ? `No ${noun}s match that name.` : `No ${noun}s at this scope.`}
                        <button type="button" className={BTN} onClick={() => openPanel(null)}>
                          New {noun}
                        </button>
                      </div>
                    ) : !sectioned ? (
                      ownShown.length === 0 ? (
                        <p className="meta rounded-sm border border-control px-4 py-6">Nothing on this page matches that filter.</p>
                      ) : (
                        itemTable(ownShown)
                      )
                    ) : (
                      <>
                        {show !== "inherited" && (
                          <section aria-label={`Own ${noun}s`} className="flex flex-col gap-2">
                            <h2 className="section-title">
                              In this {scopeName} <span className="text-fg-3">({abbreviate(ownCount)})</span>
                            </h2>
                            {ownShown.length === 0 ? (
                              <div className="flex flex-wrap items-center gap-3 rounded-sm border border-control px-4 py-4 font-mono text-[13px] text-fg-2">
                                {show === "stale" ? `No stale ${noun}s of this ${scopeName} on this page.` : `No ${noun}s are set on this ${scopeName}.`}
                                {show !== "stale" && (
                                  <button type="button" className={BTN} onClick={() => openPanel(null)}>
                                    New {noun}
                                  </button>
                                )}
                              </div>
                            ) : (
                              itemTable(ownShown)
                            )}
                          </section>
                        )}
                        {show !== "own" && (
                          <section aria-label={`Inherited ${noun}s`} className="flex flex-col gap-2">
                            <button
                              type="button"
                              aria-expanded={open}
                              disabled={show === "inherited" || show === "stale"}
                              onClick={() => setInheritedOpen(!open)}
                              className="section-title inline-flex w-fit cursor-pointer items-center gap-1.5 border-0 bg-transparent p-0 text-left text-fg disabled:cursor-default"
                            >
                              <ChevronRight size={14} aria-hidden="true" className={cn("shrink-0 transition-transform", open && "rotate-90")} />
                              Inherited from {from} <span className="text-fg-3">({abbreviate(inheritedCount)})</span>
                            </button>
                            {open &&
                              (inheritedShown.length === 0 ? (
                                <p className="meta rounded-sm border border-control px-4 py-4">{show === "stale" ? `No stale inherited ${noun}s on this page.` : `Nothing is inherited at this scope.`}</p>
                              ) : (
                                itemTable(inheritedShown)
                              ))}
                          </section>
                        )}
                      </>
                    )}
                    <CursorPager page={page} index={cursors.index} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} noun={`${noun}s`} />
                    <p className="meta">{kind === "secrets" ? "Values are encrypted at rest and masked in logs." : "Values are stored as plain text and shown to everyone in the organization."}</p>
                  </>
                );
              }}
            </Await>
          )}
        </section>

        {panel && scope && (
          <SidePanel inline={room} labelledBy="item-title" onClose={() => setPanel(null)}>
            <ItemPanel
              // A fresh form for each item, so a half-typed value never carries over.
              key={`${panel.id}/${scopeId}/${kind}/${panel.edit?.name ?? ""}`}
              org={org}
              scope={scope}
              scopeLabel={scopeWord(org, value)}
              kind={kind}
              edit={panel.edit}
              onClose={() => setPanel(null)}
              onSaved={(message) => {
                // A late save must not close a panel opened since.
                setPanel((open) => (open?.id === panel.id ? null : open));
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

const DAY = 86_400_000;

/** Whole days since a job last received the item once that is over 90, else null. Never used counts as stale (0 days). */
function staleFor(row: Row, now: number): number | null {
  if (row.last_used_at === null) return 0;
  const days = Math.floor((now - row.last_used_at) / DAY);
  return days > 90 ? days : null;
}

/** The access cell: who can read the item at this scope. */
function access(value: ScopeValue, row: Row) {
  const repoLink = value.repo && (
    <Link href={repoUrl(value.repo)} className="underline decoration-control underline-offset-3 hover:decoration-fg-3">
      {value.repo}
    </Link>
  );
  if (row.scope === "env") return <>env {value.env ?? ""} of {repoLink}</>;
  if (row.scope === "repo") return <>repo {repoLink} only</>;
  if (row.repositories === null) return "all repos";
  return (
    <span className="inline-flex max-w-full items-center gap-2" title={row.repositories.join(", ")}>
      <span>selected repos</span>
      <span className="min-w-0 truncate rounded-xs border border-control px-1.5 text-xs text-fg-2">
        {summarizeRepos({ all: false, repos: row.repositories })}
      </span>
    </span>
  );
}

function ItemPanel({
  org,
  scope,
  scopeLabel,
  kind,
  edit,
  onClose,
  onSaved,
}: {
  org: string;
  scope: CiScope;
  scopeLabel: string;
  kind: CiItemKind;
  edit: Row | null;
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const noun = word(kind);
  const isOrg = "org" in scope;
  const [name, setName] = useState(edit?.name ?? "");
  const [value, setValue] = useState(edit && "value" in edit ? edit.value : "");
  const [grant, setGrant] = useState<RepoSelection>({ all: edit?.repositories == null, repos: edit?.repositories ?? [] });
  const [tried, setTried] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  const fixed = fixName(name);
  const upper = name.toUpperCase();
  const nameBad = upper.length > 0 && upper !== fixed;
  const pasted = useMemo(() => (edit ? [] : parsePaste(value)), [edit, value]);
  // A name typed by hand makes the whole value one literal.
  const multi = pasted.length > 0 && name.trim() === "";
  const invalid = pasted.filter(unusable).length;
  const noRepos = isOrg && !grant.all && grant.repos.length === 0;
  const nameError = multi ? null : nameBad ? (/\s/.test(name) ? "Spaces are not allowed." : "Only A-Z, 0-9 and _ are allowed.") : tried && !fixed ? "Name is required." : null;
  const valueError = tried && !multi && value === "";
  const preview = reference(kind, fixed);

  const save = async () => {
    setTried(true);
    if (multi ? pasted.every(unusable) : !fixed || nameBad || value === "") return;
    if (noRepos) return;
    const items = multi ? pasted.filter((item) => !unusable(item)) : [{ name: fixed, value }];
    const repositories = isOrg ? (grant.all ? null : grant.repos) : undefined;
    if (multi) {
      const names = items.map((item) => item.name);
      const dup = names.find((n, i) => names.indexOf(n) !== i);
      if (dup) {
        setProblem(`${dup} appears more than once in the pasted lines.`);
        return;
      }
    }
    setBusy(true);
    setProblem(null);
    let saved = 0;
    try {
      if (!edit) {
        for (const item of items) {
          if (await taken(scope, kind, item.name)) {
            setProblem(`${item.name} already exists at this scope. Replace it from the list instead.`);
            setBusy(false);
            return;
          }
        }
      }
      for (const item of items) {
        await putCiItem(scope, kind, item.name, item.value, repositories);
        saved += 1;
      }
      onSaved(multi ? `Saved ${saved} ${noun}s` : `Saved ${fixed}`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      const at = items[saved]?.name;
      setProblem(`${multi ? `${saved} saved, ${at} failed: ` : ""}${message}`);
      setBusy(false);
    }
  };

  return (
    <section aria-labelledby="item-title" className="flex flex-col gap-4 rounded-sm border border-control bg-panel p-4">
      <div className="flex items-center justify-between">
        <h2 id="item-title" className="cap">
          {edit ? `${kind === "vars" ? "Change" : "Replace"} ${edit.name}` : `New ${noun}`}
        </h2>
        <button type="button" aria-label="Close panel" onClick={onClose} className="cursor-pointer text-fg-3 hover:text-fg">
          <X size={14} aria-hidden="true" />
        </button>
      </div>

      <div className="flex flex-col gap-1.5">
        <label className="cap" htmlFor="item-name">
          <span className="text-fg-2">01</span> Name
        </label>
        <input
          id="item-name"
          type="text"
          className={cn(FIELD, "font-bold")}
          value={name}
          readOnly={edit !== null}
          onChange={(event) => setName(event.target.value.toUpperCase())}
          aria-invalid={nameError !== null}
          aria-describedby="item-name-help item-name-error"
          autoComplete="off"
          spellCheck={false}
          autoFocus={!edit}
        />
        <div id="item-name-help" className="meta">
          A-Z 0-9 _ only. Uppercased as you type.
        </div>
        <div id="item-name-error" aria-live="polite">
          {nameError && (
            <div className="flex flex-wrap items-center gap-2 font-mono text-[13px] text-failed">
              <span>✕ {nameError}</span>
              {nameBad && fixed && (
                <button type="button" className={BTN_SM} onClick={() => setName(fixed)}>
                  Use {fixed}
                </button>
              )}
            </div>
          )}
        </div>
        {multi ? (
          <div className="meta">Names are taken from the pasted lines below.</div>
        ) : (
          <div className="flex items-center justify-between gap-2 rounded-sm border border-control bg-background px-2 py-1">
            <code className="min-w-0 truncate font-mono text-[13px] text-fg" title={preview}>
              {preview}
            </code>
            <CopyButton text={preview} label={`Copy reference ${preview}`} />
          </div>
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        <label className="cap" htmlFor="item-value">
          <span className="text-fg-2">02</span> Value
        </label>
        <textarea
          id="item-value"
          rows={5}
          className={cn(FIELD, "resize-y leading-normal")}
          value={value}
          onChange={(event) => setValue(event.target.value)}
          aria-describedby="item-value-help"
          aria-invalid={valueError}
          spellCheck={false}
          placeholder={edit ? (kind === "secrets" ? "new value" : "value") : "value, or paste KEY=VALUE lines"}
        />
        <div id="item-value-help" className="meta">
          {kind === "secrets"
            ? "Write-only after save."
            : `Visible to everyone in ${org} after save.`}
          {!edit && " Paste KEY=VALUE lines to add several."}
        </div>
        {valueError && (
          <div role="alert" className="font-mono text-[13px] text-failed">
            ✕ Value is empty.
          </div>
        )}
        {multi && (
          <div className="mt-1 flex flex-col gap-1">
            <div className="cap">
              {abbreviate(pasted.length)} {noun}s detected · {abbreviate(pasted.length - invalid)} valid · {abbreviate(invalid)} invalid
            </div>
            <div className="flex max-h-40 flex-col gap-0.5 overflow-y-auto rounded-sm border border-control bg-background p-2 font-mono text-[13px]">
              {[...pasted]
                .sort((a, b) => Number(unusable(a)) - Number(unusable(b)))
                .map((item, index) => (
                  <div key={index} className={cn("flex justify-between gap-3", unusable(item) && "text-failed")}>
                    <span className="min-w-0 truncate">
                      {unusable(item) ? "✕" : "✓"} {item.name || "(invalid name)"}{item.name && item.value === "" ? " (empty value)" : ""}
                    </span>
                    <span className="shrink-0 text-fg-3">
                      {kind === "secrets" ? `write-only · ${item.value.length} chars` : item.value.length > 22 ? `${item.value.slice(0, 22)}...` : item.value}
                    </span>
                  </div>
                ))}
            </div>
          </div>
        )}
      </div>

      {isOrg && (
        <fieldset className="flex flex-col gap-2">
          <legend className="cap mb-1">
            <span className="text-fg-2">03</span> Access
          </legend>
          <label className="flex cursor-pointer items-start gap-2">
            <input type="radio" name="access" className="mt-1 accent-brand" checked={grant.all} onChange={() => setGrant({ all: true, repos: grant.repos })} />
            <span className="flex flex-col">
              <span className="font-mono text-[13px]">All repositories</span>
              <span className="meta">every job in {org} can read it</span>
            </span>
          </label>
          <label className="flex cursor-pointer items-start gap-2">
            <input type="radio" name="access" className="mt-1 accent-brand" checked={!grant.all} onChange={() => setGrant({ all: false, repos: grant.repos })} />
            <span className="flex flex-col">
              <span className="font-mono text-[13px]">Selected repositories</span>
              <span className="meta">only jobs in the repos you pick</span>
            </span>
          </label>
          {!grant.all && (
            <div className="flex flex-col gap-1.5 pl-6">
              <RepoMultiSelect org={org} value={grant} onChange={setGrant} allowAll={false} label="Choose repositories" />
              {tried && noRepos && (
                <div role="alert" className="font-mono text-[13px] text-failed">
                  ✕ Select at least one repository.
                </div>
              )}
            </div>
          )}
        </fieldset>
      )}

      <div className="meta">
        Stored at: <span className="text-fg-2">{scopeLabel}</span> · jobs read it as{" "}
        <span className="text-fg-2">{kind === "secrets" ? "secrets" : "vars"}.NAME</span>
      </div>

      {problem && (
        <p className="notice notice-error !mb-0" role="alert">
          {problem}
        </p>
      )}

      <div className="flex gap-2">
        <button type="button" className={BTN_PRIMARY} disabled={busy} onClick={() => void save()}>
          {busy ? "Saving" : multi ? `Save ${abbreviate(pasted.length - invalid)} ${noun}s` : `Save ${noun}`}
        </button>
        <button type="button" className={BTN} onClick={onClose}>
          Cancel
        </button>
      </div>
    </section>
  );
}
