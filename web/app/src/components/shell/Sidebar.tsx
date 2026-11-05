// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { FileText, Layers, Lock, Play, Settings, BookOpen, ExternalLink, type LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";
import { Link, type OrgPage } from "@/router";

/**
 * A sidebar row: 36px, text-3 until hovered, 2px yellow bar on the left
 * when `aria-current="page"`. Reuse it for any other sidebar control.
 */
export const NAV_ITEM = cn(
  "relative flex h-9 w-full cursor-pointer items-center gap-2.5 rounded-sm border-0 bg-transparent px-2.5 text-left text-sm font-semibold text-fg-3",
  "hover:bg-raised hover:text-fg",
  "aria-[current=page]:bg-raised aria-[current=page]:text-fg",
  "aria-[current=page]:before:absolute aria-[current=page]:before:inset-y-0 aria-[current=page]:before:left-0 aria-[current=page]:before:w-0.5 aria-[current=page]:before:bg-brand",
);

interface Item {
  page: OrgPage[];
  label: string;
  href: string;
  icon: LucideIcon;
}

/** The pages the sidebar links to, for the org it shows. */
export function orgNav(org: string): { main: Item[]; bottom: Item[] } {
  const base = `/o/${org}`;
  return {
    main: [
      { page: ["runs", "run"], label: "Runs", href: base, icon: Play },
      { page: ["repos"], label: "Repos", href: `${base}/repos`, icon: BookOpen },
      { page: ["secrets"], label: "Secrets", href: `${base}/secrets`, icon: Lock },
      { page: ["environments"], label: "Environments", href: `${base}/environments`, icon: Layers },
    ],
    bottom: [{ page: ["settings", "billing", "setup"], label: "Settings", href: `${base}/settings`, icon: Settings }],
  };
}

export function NavLink({ item, current }: { item: Item; current: OrgPage | null }) {
  const Icon = item.icon;
  return (
    <Link href={item.href} className={NAV_ITEM} aria-current={current && item.page.includes(current) ? "page" : undefined}>
      <Icon size={16} aria-hidden="true" className="shrink-0" />
      <span className="min-w-0 flex-1 truncate">{item.label}</span>
    </Link>
  );
}

export function DocsLink() {
  return (
    <a href="https://hatchrun.io/docs" className={NAV_ITEM} target="_blank" rel="noreferrer">
      <FileText size={16} aria-hidden="true" className="shrink-0" />
      <span className="flex-1">Docs</span>
      <ExternalLink size={11} aria-hidden="true" className="shrink-0" />
    </a>
  );
}

/** The sidebar frame: sticky, full height, 232px. The parts go in as children. */
export function SidebarFrame({ children, label }: { children: ReactNode; label: string }) {
  return (
    <aside aria-label={label} className="sticky top-0 flex h-screen w-[232px] shrink-0 flex-col gap-1 border-r border-rule bg-background p-3">
      {children}
    </aside>
  );
}
