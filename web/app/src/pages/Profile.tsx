// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Check, Copy, Search, Terminal } from "lucide-react";
import { useMemo, useState } from "react";

import { ApiError, listTenants, signOut, type TenantSummary, type User } from "@/api";
import { Avatar } from "@/components/Avatar";
import { Page } from "@/components/Page";
import { Await } from "@/components/Status";
import { recents } from "@/lib/storage";
import { orgUrl } from "@/lib/urls";
import { useLoad } from "@/load";
import { Link } from "@/router";
import { BTN, FIELD } from "./formStyles";

export const CLI_INSTALL = "brew install hatchrun/tap/hatchrun";
const PAGE_SIZE = 8;

/** Who is signed in: the GitHub identity of the session, the organizations on hatchrun, sign out. */
export function Profile({ user }: { user: User }) {
  const [tenants, reload] = useLoad(() => listTenants(), "tenants");
  const [busy, setBusy] = useState(false);
  const leave = async () => {
    setBusy(true);
    try {
      await signOut();
    } catch (err) {
      if (!(err instanceof ApiError && err.status === 401)) {
        setBusy(false);
        return;
      }
    }
    window.location.assign("/signin");
  };
  const [filter, setFilter] = useState("");
  const [shown, setShown] = useState(PAGE_SIZE);
  const [copied, setCopied] = useState(false);
  const back = recents("orgs")[0] ?? null;
  const copy = () => {
    void navigator.clipboard?.writeText(CLI_INSTALL).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    }, () => undefined);
  };
  const name = user.name ?? user.login;
  return (
    <Page
      title="Your profile"
      narrow
      meta={
        back && (
          <Link href={orgUrl(back)} className="text-fg-2 quiet-underline hover:text-fg">
            ← Back to {back}
          </Link>
        )
      }
    >
      <section aria-label="GitHub identity" className="flex items-center gap-4 rounded-md border border-rule bg-panel px-4.5 py-4">
        <span className="flex size-14 flex-none items-center justify-center rounded-md bg-raised text-xl font-semibold">
          <Avatar src={user.avatarUrl} name={name} />
        </span>
        <div className="min-w-0">
          <div className="truncate text-lg font-semibold">{name}</div>
          <div className="truncate font-mono text-[13px] text-fg-2">@{user.login}</div>
          <a href={`https://github.com/${encodeURIComponent(user.login)}`} target="_blank" rel="noreferrer" className="font-mono text-xs text-fg-3 quiet-underline hover:text-fg">
            github.com/{user.login}
          </a>
        </div>
      </section>

      <section aria-label="Organizations" className="overflow-hidden rounded-md border border-rule">
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-rule bg-panel px-3.5 py-2.5">
          <h2 className="cap">Organizations</h2>
          <label className="relative">
            <span className="sr-only">Search organizations</span>
            <Search size={14} aria-hidden="true" className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-fg-3" />
            <input
              type="search"
              value={filter}
              onChange={(event) => {
                setFilter(event.target.value);
                setShown(PAGE_SIZE);
              }}
              placeholder="Search organizations"
              autoComplete="off"
              spellCheck={false}
              className={`${FIELD} !h-8 !w-[220px] !pl-8`}
            />
          </label>
        </div>
        <Await loaded={tenants} retry={reload}>
          {(list) => <OrgList list={list} filter={filter} shown={shown} more={() => setShown((n) => n + PAGE_SIZE)} />}
        </Await>
      </section>

      <section aria-label="Install the CLI" className="flex flex-col gap-2 rounded-md border border-rule bg-panel px-4 py-3.5">
        <h2 className="cap">Install the CLI</h2>
        <div className="flex h-9 items-center gap-2 rounded-sm border border-control bg-background pl-2.5">
          <Terminal size={14} aria-hidden="true" className="shrink-0 text-fg-3" />
          <span className="min-w-0 flex-1 truncate font-mono text-[13px] text-fg-2" title={CLI_INSTALL}>
            {CLI_INSTALL}
          </span>
          <button type="button" aria-label={copied ? "Copied" : "Copy install command"} onClick={copy} className="cursor-pointer border-0 bg-transparent p-2 text-fg-3 hover:text-fg">
            {copied ? <Check size={14} className="text-fg-2" /> : <Copy size={14} />}
          </button>
        </div>
      </section>

      <div>
        <button type="button" className={BTN} disabled={busy} onClick={() => void leave()}>
          Sign out
        </button>
      </div>
    </Page>
  );
}

function OrgList({ list, filter, shown, more }: { list: TenantSummary[]; filter: string; shown: number; more: () => void }) {
  const needle = filter.trim().toLowerCase();
  const matching = useMemo(() => list.filter((tenant) => tenant.login.toLowerCase().includes(needle)), [list, needle]);
  if (list.length === 0) {
    return <p className="m-0 px-3.5 py-4 font-mono text-[13px] text-fg-3">You are not in an organization on hatchrun yet.</p>;
  }
  if (matching.length === 0) {
    return <p className="m-0 px-3.5 py-4 font-mono text-[13px] text-fg-3">No organization matches that name.</p>;
  }
  return (
    <>
      <ul className="m-0 list-none p-0">
        {matching.slice(0, shown).map((tenant) => (
          <li key={tenant.id} className="border-t border-rule first:border-t-0">
            <Link href={orgUrl(tenant.login)} className="flex items-center gap-3 px-3.5 py-2.5 hover:bg-panel">
              <img src={tenant.avatarUrl} alt="" width={24} height={24} className="size-6 rounded-sm bg-raised" />
              <span className="font-mono text-[13px] font-bold">{tenant.login}</span>
            </Link>
          </li>
        ))}
      </ul>
      <div className="flex items-center justify-between gap-3 border-t border-rule px-3.5 py-2 font-mono text-xs text-fg-3">
        <span>
          {Math.min(shown, matching.length)} of {matching.length} organizations
        </span>
        {shown < matching.length && (
          <button type="button" onClick={more} className="cursor-pointer border-0 bg-transparent p-0 font-mono text-xs link">
            Show more
          </button>
        )}
      </div>
    </>
  );
}
