// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Menu, Search, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";

import { signInUrl, type User } from "@/api";
import { remember } from "@/lib/storage";
import { cn } from "@/lib/utils";
import { Link, matchOrg, useLocation } from "@/router";
import { Logo } from "./Logo";
import { CommandPalette } from "./shell/CommandPalette";
import { OrgSwitcher } from "./shell/OrgSwitcher";
import { ProfileMenu } from "./shell/ProfileMenu";
import { ShortcutsDialog } from "./shell/ShortcutsDialog";
import { DocsLink, NAV_ITEM, NavLink, SidebarFrame, orgNav } from "./shell/Sidebar";

/**
 * The one shell of the app: a left sidebar and the page beside it.
 *
 * - `org` set: org switcher, Search (⌘K), Runs, Repos, Secrets,
 *   Environments, then Settings, Docs and the profile menu at the bottom.
 *   The current page is read from the URL.
 * - `publicRepo` set (a public CI page, `user` may be null): the sidebar
 *   shrinks to the repository, Docs and sign in.
 * - neither: the signed-in start page (`/`, `/new`): brand, Docs, profile.
 *
 * Page content is `children`; the page brings its own padding wrapper
 * (use `<Page>` from components/Page).
 */
export function Shell({
  user,
  org,
  publicRepo,
  children,
}: {
  /** Null for a viewer who is not signed in. */
  user: User | null;
  org?: { login: string; role: "admin" | "member" | null } | null;
  /** `owner/name` of the repository of a public page. */
  publicRepo?: string;
  children: ReactNode;
}) {
  const location = useLocation();
  const route = matchOrg(location.split("?")[0] ?? "/");
  const [orgOpen, setOrgOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [shortcuts, setShortcuts] = useState(false);
  // The drawer belongs to the location it opened on, so any navigation closes it.
  const [drawerAt, setDrawerAt] = useState<string | null>(null);
  const drawer = drawerAt === location;
  const setDrawer = (open: boolean) => setDrawerAt(open ? location : null);
  const login = org?.login ?? null;

  useEffect(() => {
    if (login) {
      remember("orgs", login);
    }
  }, [login]);

  useEffect(() => {
    if (!user) {
      return;
    }
    const onKey = (event: KeyboardEvent) => {
      const mod = event.metaKey || event.ctrlKey;
      const typing = event.target instanceof HTMLElement && (event.target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName));
      if (mod && event.key.toLowerCase() === "k" && login) {
        event.preventDefault();
        setSearchOpen((open) => !open);
      } else if (mod && event.key.toLowerCase() === "o" && login) {
        event.preventDefault();
        setOrgOpen(true);
      } else if (event.key === "?" && !typing && !mod) {
        event.preventDefault();
        setShortcuts(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [login, user]);

  const current = route?.page ?? null;
  const suffix = current === "runs" || current === "run" || current === null ? "" : current === "billing" ? "settings/billing" : current;

  const sidebar = (
    <SidebarFrame label={publicRepo ? "Repository" : "Main"}>
      {login && user ? (
        <>
          <OrgSwitcher current={login} suffix={suffix} open={orgOpen} onOpenChange={setOrgOpen} />
          <button type="button" className={cn(NAV_ITEM, "border border-transparent")} aria-label="Search, shortcut command K" onClick={() => setSearchOpen(true)}>
            <Search size={16} aria-hidden="true" className="shrink-0" />
            <span className="flex-1">Search</span>
            <span className="rounded-xs border border-control px-1.5 font-mono text-xs leading-4 font-normal text-fg-3">⌘K</span>
          </button>
          <nav aria-label={`${login} pages`} className="flex flex-col gap-0.5">
            {orgNav(login).main.map((item) => (
              <NavLink key={item.label} item={item} current={current} />
            ))}
          </nav>
        </>
      ) : (
        <>
          <Link href={user ? "/" : "https://hatchrun.io"} aria-label="hatchrun home" className={cn(NAV_ITEM, "h-10 text-fg")}>
            <Logo size={22} />
            <span className="flex-1 truncate">hatchrun</span>
          </Link>
          {publicRepo && (
            <nav aria-label="Repository" className="mt-1 flex flex-col gap-0.5">
              <span className="cap px-2.5 pt-2 pb-1">Repository</span>
              <span className={cn(NAV_ITEM, "cursor-default")} aria-current="page" title={publicRepo}>
                <span className="min-w-0 flex-1 truncate font-mono text-[13px] font-normal">{publicRepo}</span>
              </span>
            </nav>
          )}
        </>
      )}
      <nav aria-label="Account and help" className="mt-auto flex flex-col gap-0.5 pt-4">
        {login && user && orgNav(login).bottom.map((item) => <NavLink key={item.label} item={item} current={current} />)}
        <DocsLink />
      </nav>
      <div className="mt-2 mb-1 h-px bg-rule" />
      {user ? (
        <ProfileMenu user={user} org={login} role={org?.role ?? null} onSwitchOrg={() => setOrgOpen(true)} onShortcuts={() => setShortcuts(true)} />
      ) : (
        <a className={cn(NAV_ITEM, "h-10 text-fg")} href={signInUrl(window.location.pathname)}>
          Sign in
        </a>
      )}
    </SidebarFrame>
  );

  return (
    <div className="flex min-h-screen bg-background text-foreground">
      <div className="hidden md:block">{sidebar}</div>
      {drawer && (
        <div className="fixed inset-0 z-40 md:hidden" role="dialog" aria-label="Navigation">
          <button type="button" className="absolute inset-0 bg-black/50" aria-label="Close navigation" onClick={() => setDrawer(false)} />
          <div className="relative w-[232px]">{sidebar}</div>
        </div>
      )}
      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex h-12 items-center gap-3 border-b border-rule px-4 md:hidden">
          <button type="button" aria-label={drawer ? "Close navigation" : "Open navigation"} className="text-fg-2" onClick={() => setDrawer(!drawer)}>
            {drawer ? <X size={20} /> : <Menu size={20} />}
          </button>
          <Logo size={22} />
          <span className="truncate text-sm font-semibold">{login ?? "hatchrun"}</span>
        </div>
        <main className="min-w-0 flex-1 p-6 max-md:p-4">{children}</main>
      </div>
      {login && user && <CommandPalette org={login} open={searchOpen} onOpenChange={setSearchOpen} />}
      <ShortcutsDialog open={shortcuts} onOpenChange={setShortcuts} />
    </div>
  );
}
