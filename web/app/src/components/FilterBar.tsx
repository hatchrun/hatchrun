// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronsUpDown, Search, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import type { RunFilters } from "@/api";
import { cn } from "@/lib/utils";
import { setQuery, useQuery, useQueryList } from "@/router";
import { PickList } from "./PickList";
import { Popover } from "./Popover";
import { RepoMultiSelect } from "./RepoMultiSelect";

/** The URL parameters the bar owns. Other parameters (status, range) are left alone. */
export const FILTER_PARAMS = ["q", "repo", "branch", "actor"] as const;

export interface UrlFilters {
  q: string;
  /** `owner/name` list. */
  repos: string[];
  branch: string | null;
  /** A login or "me". */
  actor: string | null;
  /** Changes whenever a filter does: reset the page cursor on it. */
  key: string;
  /** The same filters in the shape of the run list calls. */
  asRunFilters: Pick<RunFilters, "q" | "repo" | "branch" | "actor">;
}

/** The bar's filters as the URL holds them: `?q=&repo=a/b,c/d&branch=&actor=`. Reading is a hook; use `setFilters` to write. */
export function useUrlFilters(): UrlFilters {
  const q = useQuery("q") ?? "";
  const repos = useQueryList("repo");
  const branch = useQuery("branch");
  const actor = useQuery("actor");
  const key = [q, repos.join(","), branch ?? "", actor ?? ""].join("|");
  return {
    q,
    repos,
    branch,
    actor,
    key,
    asRunFilters: { q: q || undefined, repo: repos.length ? repos : undefined, branch: branch ?? undefined, actor: actor ?? undefined },
  };
}

/** Writes filters to the URL in place (no history entry); null or "" drops one. */
export function setFilters(next: { q?: string | null; repos?: string[] | null; branch?: string | null; actor?: string | null }) {
  setQuery({
    ...("q" in next && { q: next.q ?? null }),
    ...("repos" in next && { repo: next.repos && next.repos.length ? next.repos.join(",") : null }),
    ...("branch" in next && { branch: next.branch ?? null }),
    ...("actor" in next && { actor: next.actor ?? null }),
  });
}

const CHIP =
  "inline-flex h-8 max-w-[260px] cursor-pointer items-center gap-2 rounded-sm border border-control bg-background px-2.5 font-mono text-[13px] text-fg-2 hover:border-fg-3 hover:text-fg";

/** One chip that opens a list of suggestions and also takes a typed value. */
function ValueChip({
  name,
  value,
  suggestions,
  extra,
  onPick,
}: {
  name: string;
  value: string | null;
  suggestions: string[];
  /** Fixed first choices, such as "me". */
  extra?: { value: string; note: string }[];
  onPick: (value: string | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const typed = query.trim();
  const items = useMemo(() => {
    const needle = typed.toLowerCase();
    const fixed = (extra ?? []).filter((item) => !needle || item.value.includes(needle)).map((item) => ({ key: item.value, label: item.value, note: item.note, group: "Quick", mono: true, selected: value === item.value }));
    const rest = suggestions
      .filter((item) => !(extra ?? []).some((fixedItem) => fixedItem.value === item) && (!needle || item.toLowerCase().includes(needle)))
      .slice(0, 100)
      .map((item) => ({ key: item, label: item, group: "Seen on this page", mono: true, selected: value === item, title: item }));
    const free = typed && ![...fixed, ...rest].some((item) => item.key === typed) ? [{ key: typed, label: `Use "${typed}"`, group: "Typed", mono: true, selected: false }] : [];
    return [...fixed, ...rest, ...free];
  }, [typed, suggestions, extra, value]);

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
      width={300}
      trigger={
        <button type="button" className={CHIP} aria-haspopup="listbox" aria-label={`${name} filter, ${value ?? "any"}`}>
          <span className="text-fg-3">{name}</span>
          <span className="truncate text-fg" title={value ?? undefined}>
            {value ?? "any"}
          </span>
          <ChevronsUpDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" />
        </button>
      }
    >
      <PickList
        items={items}
        query={query}
        onQuery={setQuery}
        label={`Filter by ${name.toLowerCase()}`}
        placeholder={`Type a ${name.toLowerCase()}`}
        empty={`No ${name.toLowerCase()} seen yet. Type one and press Enter.`}
        footer={
          <>
            <span>Enter to apply</span>
            {value && (
              <button
                type="button"
                className="cursor-pointer text-fg underline decoration-control underline-offset-3"
                onClick={() => {
                  onPick(null);
                  setOpen(false);
                }}
              >
                clear
              </button>
            )}
          </>
        }
        onPick={(item) => {
          onPick(item.key);
          setOpen(false);
        }}
      />
    </Popover>
  );
}

function ActiveChip({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <span className="inline-flex max-w-[300px] items-center rounded-sm border border-control bg-raised font-mono text-[13px]">
      <span className="truncate py-0.5 pr-1 pl-2 text-fg" title={label}>
        {label}
      </span>
      <button type="button" aria-label={`Remove filter ${label}`} onClick={onRemove} className="cursor-pointer py-0.5 pr-2 pl-1 text-fg-3 hover:text-failed">
        <X size={12} aria-hidden="true" />
      </button>
    </span>
  );
}

/**
 * Search box plus Repo, Branch and Actor chips, and the active filters
 * as removable chips under them. State lives in the URL (`useUrlFilters`
 * reads it), so a filtered list is a link. The text box writes after a
 * short pause. `branches` and `actors` are suggestions (values seen on
 * the page); any value can still be typed. The Actor chip offers "me".
 * `children` go at the end of the row (a range picker, a refresh).
 */
export function FilterBar({
  org,
  branches = [],
  actors = [],
  placeholder = "Search runs",
  children,
  className,
}: {
  org: string;
  branches?: string[];
  actors?: string[];
  placeholder?: string;
  children?: React.ReactNode;
  className?: string;
}) {
  const filters = useUrlFilters();
  // What is typed but not yet in the URL; null when the box shows the URL's text.
  const [draft, setDraft] = useState<string | null>(null);
  const text = draft ?? filters.q;

  // Typing writes to the URL after a pause, then the box follows the URL again, so back and clear-all show.
  useEffect(() => {
    if (draft === null) return;
    const timer = window.setTimeout(() => {
      setFilters({ q: draft });
      setDraft(null);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [draft]);

  const active = [
    ...(filters.repos.length > 0 ? [{ id: "repo", label: filters.repos.length > 2 ? `Repo: ${filters.repos.slice(0, 2).join(", ")} + ${filters.repos.length - 2} more` : `Repo: ${filters.repos.join(", ")}`, remove: () => setFilters({ repos: null }) }] : []),
    ...(filters.branch ? [{ id: "branch", label: `Branch: ${filters.branch}`, remove: () => setFilters({ branch: null }) }] : []),
    ...(filters.actor ? [{ id: "actor", label: `Actor: ${filters.actor}`, remove: () => setFilters({ actor: null }) }] : []),
    ...(filters.q ? [{ id: "q", label: `Text: ${filters.q}`, remove: () => {
      setDraft(null);
      setFilters({ q: null });
    } }] : []),
  ];

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      <div className="flex flex-wrap items-center gap-2">
        <label className="relative">
          <span className="sr-only">{placeholder}</span>
          <Search size={14} aria-hidden="true" className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3" />
          <input
            type="search"
            value={text}
            onChange={(event) => setDraft(event.target.value)}
            placeholder={placeholder}
            className="h-8 w-[240px] rounded-sm border border-control bg-background pr-2 pl-8 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
          />
        </label>
        <RepoMultiSelect
          org={org}
          value={{ all: filters.repos.length === 0, repos: filters.repos }}
          onChange={(next) => setFilters({ repos: next.all ? null : next.repos })}
          label="Repo filter"
          trigger={
            <>
              <span className="text-fg-3">Repo</span>
              <span className="truncate text-fg">{filters.repos.length === 0 ? "any" : filters.repos.length === 1 ? filters.repos[0] : `${filters.repos.length} selected`}</span>
            </>
          }
        />
        <ValueChip name="Branch" value={filters.branch} suggestions={branches} onPick={(branch) => setFilters({ branch })} />
        <ValueChip name="Actor" value={filters.actor} suggestions={actors} extra={[{ value: "me", note: "you" }]} onPick={(actor) => setFilters({ actor })} />
        {children}
      </div>
      {active.length > 0 && (
        <div className="flex flex-wrap items-center gap-2" aria-label="Active filters">
          {active.map((chip) => (
            <ActiveChip key={chip.id} label={chip.label} onRemove={chip.remove} />
          ))}
          <button
            type="button"
            className="cursor-pointer font-mono text-[13px] text-fg underline decoration-control underline-offset-3 hover:decoration-fg-3"
            onClick={() => {
              setDraft(null);
              setFilters({ q: null, repos: null, branch: null, actor: null });
            }}
          >
            clear all
          </button>
        </div>
      )}
    </div>
  );
}
