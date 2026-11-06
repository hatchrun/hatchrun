// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The org's audit log of secret, variable, environment and enabled-repo
// changes (org admin). Newest first, one page at a time.

import { useState } from "react";

import { listAudit, type AuditAction } from "@/api";
import { CursorPager, useCursors } from "@/components/CursorPager";
import { Page } from "@/components/Page";
import { Await } from "@/components/Status";
import { Truncate } from "@/components/Truncate";
import { ago } from "@/lib/format";
import { useDebounced } from "@/lib/useRepos";
import { cn } from "@/lib/utils";
import { useLoad } from "@/load";
import { orgUrl } from "@/lib/urls";
import { Link } from "@/router";
import { FIELD, TD, TH } from "./formStyles";
import type { OrgPageProps } from "./types";

const WORDS: Record<AuditAction, string> = {
  "secret.put": "secret saved",
  "secret.delete": "secret deleted",
  "var.put": "variable saved",
  "var.delete": "variable deleted",
  "environment.put": "environment saved",
  "environment.delete": "environment deleted",
  "repos.enabled": "enabled repositories changed",
};

export function Audit({ org, tenant }: OrgPageProps) {
  const cursors = useCursors();
  const [repo, setRepo] = useState("");
  const filter = useDebounced(repo.trim(), 250);
  const [page, reload] = useLoad(() => listAudit(org, { repo: filter || undefined, cursor: cursors.cursor, limit: 50 }), `${org}/${filter}/${cursors.key}`, true);

  if (tenant.role !== "admin") {
    return (
      <Page title="Audit log">
        <p className="meta rounded-sm border border-control px-4 py-6">Only organization admins can read the audit log.</p>
      </Page>
    );
  }

  return (
    <Page
      title="Audit log"
      meta={
        <Link href={orgUrl(org, "secrets")} className="text-fg-2 quiet-underline hover:text-fg">
          ← Secrets
        </Link>
      }
      actions={
        <label>
          <span className="sr-only">Filter by repository</span>
          <input
            type="search"
            value={repo}
            onChange={(event) => {
              setRepo(event.target.value);
              cursors.reset();
            }}
            placeholder="Filter by owner/repo"
            autoComplete="off"
            spellCheck={false}
            className={cn(FIELD, "!h-8 !w-[240px]")}
          />
        </label>
      }
    >
      <Await loaded={page} retry={reload}>
        {(data) => (
          <section aria-label="Audit log" className="flex flex-col gap-3">
            {data.items.length === 0 ? (
              <p className="meta rounded-sm border border-control px-4 py-6">{filter ? "No entries for that repository." : "Nothing has changed yet."}</p>
            ) : (
              <div className="overflow-x-auto rounded-sm border border-control">
                <table className="w-full min-w-[760px] border-collapse text-sm">
                  <thead>
                    <tr>
                      <th className={TH}>When</th>
                      <th className={TH}>Actor</th>
                      <th className={TH}>Action</th>
                      <th className={TH}>Scope</th>
                      <th className={TH}>Target</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.items.map((entry) => (
                      <tr key={entry.id} className="border-t border-rule hover:bg-panel">
                        <td className={cn(TD, "font-mono text-[13px] text-fg-2")} title={new Date(entry.at).toLocaleString()}>
                          {ago(entry.at)}
                        </td>
                        <td className={cn(TD, "max-w-[160px] font-mono text-[13px]")}>
                          <Truncate value={entry.actor} mono />
                        </td>
                        <td className={cn(TD, "font-mono text-[13px]", entry.action.endsWith(".delete") ? "text-failed-soft" : "text-fg")} title={entry.action}>
                          {WORDS[entry.action] ?? entry.action}
                        </td>
                        <td className={cn(TD, "max-w-[280px] font-mono text-[13px] text-fg-2")}>
                          <Truncate value={entry.scope} mono />
                        </td>
                        <td className={cn(TD, "max-w-[260px] font-mono text-[13px]")}>
                          <Truncate value={entry.target} mono />
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            <CursorPager page={data} index={cursors.index} hasPrev={cursors.hasPrev} onPrev={cursors.prev} onNext={cursors.next} range="newest first" noun="entries" prevLabel="Newer" nextLabel="Older" />
          </section>
        )}
      </Await>
    </Page>
  );
}
