// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronsUpDown } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";

import { listOrgRepos } from "@/api";
import { abbreviate } from "@/lib/format";
import { recentFirst, recents } from "@/lib/storage";
import { useRepoSearch } from "@/lib/useRepos";
import { cn } from "@/lib/utils";
import { PickList } from "./PickList";
import { Popover } from "./Popover";

export interface RepoSelection {
  /** Every repository; `repos` is ignored. */
  all: boolean;
  repos: string[];
}

/**
 * "acme/web + 12 more": the summary of a selection for a trigger or a
 * table cell. `all` reads "All repositories". Names are cut by CSS, not here.
 */
export function summarizeRepos({ all, repos }: RepoSelection): string {
  if (all) return "All repositories";
  const [first, ...rest] = repos;
  if (first === undefined) return "No repositories";
  return rest.length === 0 ? first : `${first} + ${rest.length} more`;
}

/** Every repository matching `q`, read 100 at a time; for "select all matching". */
async function allMatching(org: string, q: string): Promise<string[]> {
  const names: string[] = [];
  let cursor: string | null = null;
  do {
    const page: Awaited<ReturnType<typeof listOrgRepos>> = await listOrgRepos(org, { q, limit: 100, cursor });
    names.push(...page.items.map((repo) => repo.name));
    cursor = page.next;
  } while (cursor);
  return names;
}

const LINK = "cursor-pointer font-mono text-[13px] text-fg underline decoration-control underline-offset-3 hover:decoration-fg-3 disabled:cursor-default disabled:opacity-50 disabled:no-underline";

/**
 * Choose repositories of an organization: an "All repositories" switch,
 * search over the whole list (server side), "N selected, clear, show
 * selected only" and "select all matching". Controlled by `value`. The
 * trigger shows the summary. Used by the filter bar, secrets access and
 * the enabled-repos setting.
 */
export function RepoMultiSelect({
  org,
  value,
  onChange,
  trigger,
  allowAll = true,
  label = "Repositories",
}: {
  org: string;
  value: RepoSelection;
  onChange: (value: RepoSelection) => void;
  /** Replaces the default trigger button's content. */
  trigger?: ReactNode;
  /** Shows the "All repositories" switch. */
  allowAll?: boolean;
  label?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [onlySelected, setOnlySelected] = useState(false);
  const [busy, setBusy] = useState(false);
  const q = query.trim();
  const { repos, total, loading } = useRepoSearch(org, q, 50, open && !onlySelected);

  const chosen = useMemo(() => new Set(value.repos), [value.repos]);
  const shown = useMemo(() => {
    if (onlySelected) {
      return value.repos.filter((name) => name.toLowerCase().includes(q.toLowerCase())).map((name) => ({ name }));
    }
    return recentFirst(repos, (repo) => repo.name, q ? [] : recents(`repos:${org}`)).items;
  }, [onlySelected, value.repos, repos, q, org]);
  const matches = onlySelected ? shown.length : total;

  // Picking a row while "All repositories" is on turns it off and starts a selection.
  const toggle = (name: string) =>
    onChange({ all: false, repos: value.all ? [name] : chosen.has(name) ? value.repos.filter((seen) => seen !== name) : [...value.repos, name] });
  const selectMatching = async () => {
    setBusy(true);
    try {
      const names = await allMatching(org, q);
      onChange({ all: false, repos: [...new Set([...(value.all ? [] : value.repos), ...names])] });
    } finally {
      setBusy(false);
    }
  };

  const items = shown.map((repo) => ({
    key: repo.name,
    mono: true,
    title: repo.name,
    selected: !value.all && chosen.has(repo.name),
    label: (
      <>
        <span aria-hidden="true" className="inline-block w-4 text-fg">
          {!value.all && chosen.has(repo.name) ? "✓" : ""}
        </span>
        {repo.name}
      </>
    ),
  }));

  const controls = (
    <div className="flex flex-col gap-2 border-t border-rule px-3 py-2">
      {allowAll && (
        <label className="flex cursor-pointer items-center justify-between gap-3 text-sm font-semibold">
          All repositories
          <button
            type="button"
            role="switch"
            aria-checked={value.all}
            aria-label="All repositories"
            onClick={() => onChange({ all: !value.all, repos: value.repos })}
            className={cn("relative h-5 w-9 cursor-pointer rounded-full border border-control", value.all ? "bg-fg" : "bg-background")}
          >
            <span className={cn("absolute top-0.5 size-3.5 rounded-full transition-[left]", value.all ? "left-[18px] bg-background" : "left-0.5 bg-fg-3")} />
          </button>
        </label>
      )}
      {!value.all && (
        <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1 font-mono text-[13px] text-fg-2">
          <span>{value.repos.length} selected</span>
          <span className="flex gap-3">
            <button type="button" className={LINK} disabled={value.repos.length === 0} onClick={() => onChange({ all: false, repos: [] })}>
              clear
            </button>
            <button type="button" className={LINK} aria-pressed={onlySelected} onClick={() => setOnlySelected(!onlySelected)}>
              {onlySelected ? "show all" : "show selected only"}
            </button>
          </span>
        </div>
      )}
      {q && !onlySelected && total > 0 && (
        <button type="button" className={cn(LINK, "self-start")} disabled={busy} onClick={() => void selectMatching()}>
          {busy ? "Selecting" : `Select all matching (${abbreviate(total)})`}
        </button>
      )}
    </div>
  );

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) {
          setQuery("");
          setOnlySelected(false);
        }
      }}
      width={380}
      trigger={
        <button
          type="button"
          aria-haspopup="listbox"
          aria-label={`${label}: ${summarizeRepos(value)}`}
          className="inline-flex h-8 max-w-[300px] cursor-pointer items-center gap-2 rounded-sm border border-control bg-background px-2.5 font-mono text-[13px] text-fg-2 hover:border-fg-3 hover:text-fg"
        >
          {trigger ?? (
            <span className="truncate" title={value.repos.join(", ")}>
              {summarizeRepos(value)}
            </span>
          )}
          <ChevronsUpDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" />
        </button>
      }
    >
      <div className={cn(value.all && "[&_[role=listbox]]:opacity-50")}>
        <PickList
          items={items}
          query={query}
          onQuery={setQuery}
          label="Search repositories"
          placeholder={`Search ${total ? abbreviate(total) : ""} repositories`.replace("  ", " ")}
          empty={loading ? "Loading" : onlySelected ? "Nothing selected." : "No repositories match."}
          header={controls}
          footer={
            <>
              <span>
                {`${abbreviate(matches)} ${q ? "matches" : "repositories"}`}
                {!value.all && !onlySelected && total > shown.length && ` · showing ${shown.length}`}
              </span>
              <button type="button" className={LINK} onClick={() => setOpen(false)}>
                Done
              </button>
            </>
          }
          onPick={(item) => toggle(item.key)}
        />
      </div>
    </Popover>
  );
}
