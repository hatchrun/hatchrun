// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ExternalLink, Search } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";
import { Link, type OrgPage } from "@/router";
import { Logo } from "../Logo";

/** A top bar control: 36px, fg-3 until hovered. */
export const BAR_BUTTON =
  "flex h-9 cursor-pointer items-center gap-2 rounded-sm border-0 bg-transparent px-2.5 text-sm font-semibold whitespace-nowrap text-fg-3 hover:bg-raised hover:text-fg";

interface Item {
  page: OrgPage[];
  label: string;
  href: string;
}

/** The pages the bar links to, for the org it shows. */
export function orgNav(org: string): Item[] {
  const base = `/o/${org}`;
  return [
    { page: ["runs", "run"], label: "Runs", href: base },
    { page: ["repos"], label: "Repos", href: `${base}/repos` },
    { page: ["secrets"], label: "Secrets", href: `${base}/secrets` },
    { page: ["environments"], label: "Environments", href: `${base}/environments` },
    { page: ["settings", "billing", "setup"], label: "Settings", href: `${base}/settings` },
  ];
}

/** The org pages as tabs: current one in full colour with a 2px accent underline. */
export function OrgTabs({ org, current }: { org: string; current: OrgPage | null }) {
  return (
    <nav aria-label={`${org} pages`} className="flex h-full min-w-0 flex-1 overflow-x-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
      {orgNav(org).map((item) => (
        <Link
          key={item.label}
          href={item.href}
          aria-current={current && item.page.includes(current) ? "page" : undefined}
          className={cn(
            "box-border flex h-full shrink-0 items-center border-b-2 border-transparent px-3.5 text-sm font-semibold whitespace-nowrap text-fg-3",
            "hover:text-fg aria-[current=page]:border-brand aria-[current=page]:text-fg",
          )}
        >
          {item.label}
        </Link>
      ))}
    </nav>
  );
}

export function SearchButton({ onClick }: { onClick: () => void }) {
  return (
    <button type="button" className={BAR_BUTTON} aria-label="Search, shortcut command K" onClick={onClick}>
      <Search size={16} aria-hidden="true" className="shrink-0" />
      <span className="rounded-xs border border-control px-1.5 font-mono text-xs leading-4 font-normal max-[899px]:hidden">⌘K</span>
    </button>
  );
}

export function DocsLink() {
  return (
    <a href="https://hatchrun.io/docs" className={BAR_BUTTON} target="_blank" rel="noreferrer">
      <span>Docs</span>
      <ExternalLink size={12} aria-hidden="true" className="shrink-0" />
    </a>
  );
}

export function LogoLink({ signedIn }: { signedIn: boolean }) {
  return (
    <Link href={signedIn ? "/" : "https://hatchrun.io"} aria-label="hatchrun home" className="flex h-9 shrink-0 items-center rounded-sm px-1">
      <Logo size={24} />
    </Link>
  );
}

/** The bar frame: sticky, 52px, rule underneath. `left` grows, `right` keeps its size. */
export function TopBarFrame({ left, right, label }: { left: ReactNode; right: ReactNode; label: string }) {
  return (
    <header aria-label={label} className="sticky top-0 z-30 flex h-[52px] shrink-0 items-center gap-3 border-b border-rule bg-background px-4">
      <div className="flex h-full min-w-0 flex-1 items-center gap-2">{left}</div>
      <div className="flex shrink-0 items-center gap-1">{right}</div>
    </header>
  );
}

export function Slash() {
  return (
    <span aria-hidden="true" className="shrink-0 text-lg font-light text-fg-3">
      /
    </span>
  );
}
