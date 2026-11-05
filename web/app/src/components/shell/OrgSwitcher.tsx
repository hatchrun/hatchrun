// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronsUpDown } from "lucide-react";
import { useMemo, useState } from "react";

import { Logo } from "@/components/Logo";
import { PickList } from "@/components/PickList";
import { Popover } from "@/components/Popover";
import { recentFirst, recents } from "@/lib/storage";
import { navigate } from "@/router";
import { NAV_ITEM } from "./Sidebar";
import { useTenants } from "./tenants";

/**
 * The organization button at the top of the sidebar and its popover:
 * search, the recent organizations first, "Add an organization" at the
 * bottom. `suffix` is the page to keep when switching ("repos", "").
 */
export function OrgSwitcher({
  current,
  suffix,
  open,
  onOpenChange,
}: {
  current: string;
  suffix: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { tenants, error, retry } = useTenants();
  const [query, setQuery] = useState("");

  const items = useMemo(() => {
    const q = query.trim().toLowerCase();
    const all = [...(tenants ?? [])].sort((a, b) => a.login.localeCompare(b.login));
    const matching = all.filter((tenant) => !q || tenant.login.toLowerCase().includes(q));
    const { items: ordered, recent } = recentFirst(matching, (tenant) => tenant.login, recents("orgs"));
    return ordered.map((tenant, index) => ({
      key: tenant.login,
      label: tenant.login,
      note: tenant.login === current ? "current" : index < recent ? "recent" : undefined,
      selected: tenant.login === current,
    }));
  }, [tenants, query, current]);

  const total = tenants?.length ?? 0;
  const pick = (login: string) => {
    onOpenChange(false);
    setQuery("");
    navigate(`/o/${login}${suffix ? `/${suffix}` : ""}`);
  };

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next);
        if (!next) {
          setQuery("");
        }
      }}
      width={260}
      trigger={
        <button
          type="button"
          className={`${NAV_ITEM} h-10 text-fg`}
          aria-haspopup="listbox"
          aria-label={`Switch organization, current ${current}`}
        >
          <Logo size={22} />
          <span className="min-w-0 flex-1 truncate">{current}</span>
          <ChevronsUpDown size={12} aria-hidden="true" className="shrink-0 text-fg-3" />
        </button>
      }
    >
      <PickList
        items={items}
        query={query}
        onQuery={setQuery}
        onPick={(item) => pick(item.key)}
        label="Choose an organization"
        placeholder={`Search ${total || ""} organizations`.replace("  ", " ")}
        empty={error ? `Could not load organizations: ${error.message}` : tenants === null ? "Loading" : "No organizations match."}
        footer={
          <>
            {error ? (
              <button type="button" className="cursor-pointer text-fg hover:underline" onClick={retry}>
                Retry
              </button>
            ) : (
              <span>{query ? `${items.length} matches` : `Recent first · ${total} organizations`}</span>
            )}
            <a href="/new" className="text-fg hover:underline" onClick={(event) => { event.preventDefault(); onOpenChange(false); navigate("/new"); }}>
              Add
            </a>
          </>
        }
      />
    </Popover>
  );
}
