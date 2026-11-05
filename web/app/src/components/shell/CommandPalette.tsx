// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Dialog } from "radix-ui";
import { useEffect, useMemo, useState } from "react";

import { listOrgRuns, type CiRun } from "@/api";
import { PickList, type PickItem } from "@/components/PickList";
import { recents, remember } from "@/lib/storage";
import { runUrl } from "@/lib/urls";
import { useDebounced, useRepoSearch } from "@/lib/useRepos";
import { navigate } from "@/router";

const PAGES = [
  { label: "Runs", path: "" },
  { label: "Repos", path: "repos" },
  { label: "Secrets", path: "secrets" },
  { label: "Environments", path: "environments" },
  { label: "Settings", path: "settings" },
  { label: "Billing", path: "settings/billing" },
  { label: "Setup", path: "setup" },
];

/**
 * The command palette (⌘K): jump to a page, a repository or a run. Repo
 * results come from `GET /api/ci/orgs/{org}/repos?q=`, run results from
 * the org runs list with `q`. With an empty query it offers the pages
 * and the repositories visited last. Keys: ↑↓, Enter, Esc.
 */
export function CommandPalette({ org, open, onOpenChange }: { org: string; open: boolean; onOpenChange: (open: boolean) => void }) {
  const [query, setQuery] = useState("");
  const q = query.trim();
  const { repos } = useRepoSearch(org, q, 8, open && q !== "");
  const [found, setFound] = useState<{ q: string; runs: CiRun[] }>({ q: "", runs: [] });
  const runQuery = useDebounced(q, 250);

  useEffect(() => {
    if (!open || runQuery.length < 2) {
      return;
    }
    let current = true;
    listOrgRuns(org, { q: runQuery, limit: 5, since: 0 }).then(
      (page) => current && setFound({ q: runQuery, runs: page.items }),
      () => current && setFound({ q: runQuery, runs: [] }),
    );
    return () => {
      current = false;
    };
  }, [org, runQuery, open]);

  const runs = useMemo(() => (open && found.q === runQuery && runQuery.length >= 2 ? found.runs : []), [open, found, runQuery]);
  const items = useMemo(() => {
    const needle = q.toLowerCase();
    const out: (PickItem & { go: string })[] = [];
    if (!needle) {
      for (const name of recents(`repos:${org}`).slice(0, 4)) {
        out.push({ key: `r:${name}`, group: "Recent repositories", label: name, mono: true, go: `/o/${org}?repo=${encodeURIComponent(name)}`, note: "runs" });
      }
    }
    for (const page of PAGES.filter((item) => !needle || item.label.toLowerCase().includes(needle))) {
      out.push({ key: `p:${page.path}`, group: "Pages", label: page.label, go: `/o/${org}${page.path ? `/${page.path}` : ""}` });
    }
    if (needle) {
      for (const repo of repos) {
        out.push({ key: `r:${repo.name}`, group: "Repositories", label: repo.name, mono: true, go: `/o/${org}?repo=${encodeURIComponent(repo.name)}`, note: "runs" });
      }
      for (const run of runs) {
        out.push({
          key: `u:${run.id}`,
          group: "Runs",
          label: `${run.repository} · ${run.workflow} #${run.run_number}`,
          mono: true,
          go: runUrl(run.repository, run.id),
          note: run.branch ?? undefined,
        });
      }
    }
    return out;
  }, [q, repos, runs, org]);

  const close = (next: boolean) => {
    onOpenChange(next);
    if (!next) {
      setQuery("");
    }
  };

  return (
    <Dialog.Root open={open} onOpenChange={close}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/50" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed top-[14vh] left-1/2 z-50 w-[560px] max-w-[calc(100vw-16px)] -translate-x-1/2 rounded-md border border-control bg-raised shadow-[0_10px_28px_rgba(0,0,0,0.55)] outline-none"
        >
          <Dialog.Title className="sr-only">Search</Dialog.Title>
          <PickList
            items={items}
            query={query}
            onQuery={setQuery}
            label="Search pages, repositories and runs"
            placeholder="Jump to a repo, run or page"
            maxHeight={380}
            empty="Nothing matches."
            footer={<span>↑↓ move · Enter open · Esc close</span>}
            onPick={(item) => {
              const picked = items.find((candidate) => candidate.key === item.key);
              if (picked) {
                if (picked.key.startsWith("r:")) {
                  remember(`repos:${org}`, picked.key.slice(2));
                }
                close(false);
                navigate(picked.go);
              }
            }}
          />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
