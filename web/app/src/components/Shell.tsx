// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState, type ReactNode } from "react";

import { signOut, type User } from "@/api";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import { Link } from "@/router";
import { OpenIcon } from "./icons";
import { Logo } from "./Logo";

/**
 * The top bar and the page under it. `nav` follows the brand after a
 * slash (the organization and its pages); `status` sits on the right
 * before Docs.
 */
export function Shell({
  user,
  nav,
  status,
  children,
}: {
  user: User;
  nav?: ReactNode;
  status?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="border-b border-rivet bg-graphite">
        <div className="flex flex-wrap items-center gap-x-3.5 px-4 sm:px-6">
          <Link href="/" aria-label="hatchrun home" className="flex h-13 items-center gap-2">
            <Logo size={26} />
            <span className="hidden text-base font-extrabold tracking-[-0.01em] [font-stretch:125%] sm:inline">hatchrun</span>
          </Link>
          {nav && (
            <span aria-hidden="true" className="text-xl text-rivet">
              /
            </span>
          )}
          {nav}
          <div className="ml-auto flex h-13 items-center gap-4.5">
            {status}
            <a
              href="https://hatchrun.io/docs/overview/"
              className="inline-flex items-center gap-1.5 text-[13px] text-bone-dim hover:text-bone"
            >
              Docs
              <OpenIcon />
            </a>
            <UserMenu user={user} />
          </div>
        </div>
      </header>
      <main className="px-4 pt-5 pb-8 sm:px-6">{children}</main>
    </div>
  );
}

export interface Tab {
  href: string;
  label: string;
  current: boolean;
}

/** An organization's pages as tabs with a yellow underline. */
export function PageTabs({ label, tabs }: { label: string; tabs: Tab[] }) {
  return (
    <nav
      aria-label={label}
      className="order-last -mx-4 flex w-[calc(100%+2rem)] overflow-x-auto border-t border-seam px-1 sm:order-none sm:mx-0 sm:ml-2.5 sm:w-auto sm:border-0 sm:px-0"
    >
      {tabs.map((tab) => (
        <Link
          key={tab.href}
          href={tab.href}
          aria-current={tab.current ? "page" : undefined}
          className={cn(
            "flex h-13 items-center border-b-2 border-transparent px-3.5 text-sm font-semibold text-fog hover:text-bone",
            "aria-[current=page]:border-chick aria-[current=page]:text-bone",
          )}
        >
          {tab.label}
        </Link>
      ))}
    </nav>
  );
}

function UserMenu({ user }: { user: User }) {
  const [busy, setBusy] = useState(false);

  const leave = async () => {
    setBusy(true);
    try {
      await signOut();
    } finally {
      window.location.assign("/signin");
    }
  };

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={`Account menu for ${user.login}`}
        className="size-7 cursor-pointer overflow-hidden rounded-full border border-rivet bg-housing p-0"
      >
        <img src={user.avatarUrl || "/brand/pip-avatar.svg"} alt="" width={28} height={28} className="size-full" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-52">
        <DropdownMenuLabel className="flex flex-col">
          <span>{user.name ?? user.login}</span>
          <span className="font-mono text-xs font-normal text-fog">@{user.login}</span>
        </DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuItem disabled={busy} onSelect={leave}>
          Sign out
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
