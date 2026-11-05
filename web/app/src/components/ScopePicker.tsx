// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronsUpDown } from "lucide-react";
import { useMemo, useState } from "react";

import { listCiEnvironments } from "@/api";
import { useLoad } from "@/load";
import { recentFirst, recents, remember } from "@/lib/storage";
import { abbreviate } from "@/lib/format";
import { useRepoSearch } from "@/lib/useRepos";
import { cn } from "@/lib/utils";
import { PickList } from "./PickList";
import { Popover } from "./Popover";

export type Scope = "org" | "repo" | "env";

export interface ScopeValue {
  scope: Scope;
  /** `owner/name`, for the repo and env scopes. */
  repo: string | null;
  env: string | null;
}

const LABELS: [Scope, string][] = [
  ["org", "Organization"],
  ["repo", "Repository"],
  ["env", "Environment"],
];

const TRIGGER =
  "inline-flex h-8 max-w-[260px] cursor-pointer items-center gap-2 rounded-sm border border-control bg-background px-2.5 font-mono text-[13px] text-fg-2 hover:border-fg-3 hover:text-fg disabled:cursor-default disabled:opacity-50";

/**
 * Whose items a page shows: a three-way switch (Organization,
 * Repository, Environment) and, for the last two, a searchable popover to
 * pick the repository (recent first) and the environment. Repositories
 * and environments are never tabs. Controlled: `value` in, `onChange`
 * out; keep it in the URL with `setQuery` if the page should be linkable.
 * Picking another scope keeps the repo when there is one; going back to
 * Organization clears both.
 */
export function ScopePicker({
  org,
  value,
  onChange,
  className,
}: {
  org: string;
  value: ScopeValue;
  onChange: (value: ScopeValue) => void;
  className?: string;
}) {
  const setScope = (scope: Scope) =>
    onChange({ scope, repo: scope === "org" ? null : value.repo, env: scope === "env" ? value.env : null });
  return (
    <div className={cn("flex flex-wrap items-center gap-2", className)}>
      <div role="radiogroup" aria-label="Scope" className="inline-flex overflow-hidden rounded-sm border border-control">
        {LABELS.map(([id, label]) => (
          <button
            key={id}
            type="button"
            role="radio"
            aria-checked={value.scope === id}
            onClick={() => setScope(id)}
            className={cn(
              "h-8 cursor-pointer border-r border-control px-3 text-sm font-semibold last:border-r-0",
              value.scope === id ? "bg-raised text-fg" : "bg-background text-fg-3 hover:text-fg",
            )}
          >
            {label}
          </button>
        ))}
      </div>
      {value.scope !== "org" && (
        <RepoPicker org={org} value={value.repo} onPick={(repo) => onChange({ ...value, repo, env: null })} />
      )}
      {value.scope === "env" && <EnvPicker repo={value.repo} value={value.env} onPick={(env) => onChange({ ...value, env })} />}
    </div>
  );
}

function RepoPicker({ org, value, onPick }: { org: string; value: string | null; onPick: (repo: string) => void }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const { repos, total, loading } = useRepoSearch(org, query, 50, open);
  const items = useMemo(() => {
    const { items: ordered, recent } = recentFirst(repos, (repo) => repo.name, query ? [] : recents(`repos:${org}`));
    return ordered.map((repo, index) => ({
      key: repo.name,
      label: repo.name,
      mono: true,
      selected: repo.name === value,
      note: index < recent ? "recent" : undefined,
      title: repo.name,
    }));
  }, [repos, query, value, org]);

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
      width={340}
      trigger={
        <button type="button" className={TRIGGER} aria-haspopup="listbox" aria-label={`Repository, ${value ?? "none selected"}`}>
          <span className="truncate" title={value ?? undefined}>
            {value ?? "Select a repository"}
          </span>
          <ChevronsUpDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" />
        </button>
      }
    >
      <PickList
        items={items}
        query={query}
        onQuery={setQuery}
        label="Choose a repository"
        placeholder={`Search ${total ? abbreviate(total) : ""} repositories`.replace("  ", " ")}
        empty={loading ? "Loading" : "No repositories match."}
        footer={<span>{query ? `${abbreviate(total)} matches` : "Recent first"}{total > repos.length && ` · showing ${repos.length} of ${abbreviate(total)}, type to narrow`}</span>}
        onPick={(item) => {
          remember(`repos:${org}`, item.key);
          onPick(item.key);
          setOpen(false);
        }}
      />
    </Popover>
  );
}

function EnvPicker({ repo, value, onPick }: { repo: string | null; value: string | null; onPick: (env: string) => void }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [owner = "", name = ""] = (repo ?? "").split("/");
  const [loaded] = useLoad(() => (repo ? listCiEnvironments(owner, name) : Promise.resolve([])), `envs:${repo ?? ""}`);
  const names = loaded.state === "ready" ? loaded.data.map((env) => env.name) : [];
  const items = names
    .filter((env) => env.toLowerCase().includes(query.trim().toLowerCase()))
    .map((env) => ({ key: env, label: env, mono: true, selected: env === value }));

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
      width={300}
      trigger={
        <button type="button" className={TRIGGER} disabled={!repo} aria-haspopup="listbox" aria-label={`Environment, ${value ?? "none selected"}`}>
          <span className="truncate" title={value ?? undefined}>
            {value ?? (repo ? "Select an environment" : "Choose a repo first")}
          </span>
          <ChevronsUpDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" />
        </button>
      }
    >
      <PickList
        items={items}
        query={query}
        onQuery={setQuery}
        label="Choose an environment"
        placeholder={`Search ${names.length || ""} environments`.replace("  ", " ")}
        empty={loaded.state === "loading" ? "Loading" : "No environments match."}
        footer={<span>{names.length} in {repo}</span>}
        onPick={(item) => {
          onPick(item.key);
          setOpen(false);
        }}
      />
    </Popover>
  );
}
