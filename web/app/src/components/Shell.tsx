// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState, type ReactNode } from "react";

import { signInUrl, type User } from "@/api";
import { remember } from "@/lib/storage";
import { matchOrg, useLocation } from "@/router";
import { CommandPalette } from "./shell/CommandPalette";
import { OrgSwitcher } from "./shell/OrgSwitcher";
import { ProfileMenu } from "./shell/ProfileMenu";
import { ShortcutsDialog } from "./shell/ShortcutsDialog";
import { BAR_BUTTON, DocsLink, LogoLink, OrgTabs, SearchButton, Slash, TopBarFrame } from "./shell/TopBar";

/**
 * The one shell of the app: a sticky top bar and the page under it.
 *
 * - `org` set: logo, `/`, org switcher and the tabs Runs, Repositories, Secrets,
 *   Environments, Settings on the left; Search (Ctrl or ⌘ K), Docs and the profile
 *   menu on the right. The current tab is read from the URL.
 * - `publicRepo` set (a public CI page, `user` may be null): the bar
 *   shrinks to logo, repository and Docs (plus sign in when signed out).
 * - neither: the signed-in start page (`/`, `/new`): logo, Docs, profile.
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
  const suffix = current === "runs" || current === null ? "" : current === "billing" || current === "setup" || current === "audit" ? `settings/${current}` : current === "repository" || current === "run" ? "repositories" : current;

  const left =
    login && user ? (
      <>
        <LogoLink signedIn />
        <Slash />
        <OrgSwitcher current={login} suffix={suffix} open={orgOpen} onOpenChange={setOrgOpen} />
        <OrgTabs org={login} current={current} />
      </>
    ) : (
      <>
        <LogoLink signedIn={user !== null} />
        {publicRepo && (
          <>
            <Slash />
            <span className="min-w-0 truncate font-mono text-[13px] text-fg" title={publicRepo}>
              {publicRepo}
            </span>
          </>
        )}
      </>
    );
  const right = (
    <>
      {login && user && <SearchButton onClick={() => setSearchOpen(true)} />}
      <DocsLink />
      {user ? (
        <ProfileMenu user={user} org={login} role={org?.role ?? null} onSwitchOrg={() => setOrgOpen(true)} onShortcuts={() => setShortcuts(true)} />
      ) : (
        <a className={`${BAR_BUTTON} text-fg`} href={signInUrl(window.location.pathname)}>
          Sign in
        </a>
      )}
    </>
  );

  return (
    <div className="flex min-h-screen flex-col bg-background text-foreground">
      <a
        href="#main"
        className="skip-link"
        onClick={(event) => {
          event.preventDefault();
          document.getElementById("main")?.focus();
        }}
      >
        Skip to content
      </a>
      <TopBarFrame label={publicRepo ? "Repository" : "Main"} left={left} right={right} />
      <main id="main" tabIndex={-1} className="min-w-0 flex-1 p-6 max-md:p-4 outline-none">{children}</main>
      {login && user && <CommandPalette org={login} open={searchOpen} onOpenChange={setSearchOpen} />}
      <ShortcutsDialog open={shortcuts} onOpenChange={setShortcuts} />
    </div>
  );
}
