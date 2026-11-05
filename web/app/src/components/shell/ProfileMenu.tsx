// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import {
  ArrowLeftRight,
  Check,
  ChevronRight,
  CircleHelp,
  Copy,
  ExternalLink,
  Keyboard,
  LogOut,
  UserRound,
  Sparkles,
  Terminal,
} from "lucide-react";
import { DropdownMenu as Menu } from "radix-ui";
import { useState, type ReactNode } from "react";

import { ApiError, signOut, type User } from "@/api";
import { THEMES, setTheme, useTheme } from "@/lib/theme";
import { cn } from "@/lib/utils";
import { Link } from "@/router";
import { NAV_ITEM } from "./Sidebar";

export const CLI_INSTALL = "brew install hatchrun/tap/hatchrun";

const ITEM =
  "flex h-9 w-full cursor-pointer items-center gap-2.5 rounded-sm px-2.5 text-sm text-fg outline-none select-none data-[highlighted]:bg-raised";
const GROUP = "border-b border-rule p-1";
const SURFACE = "rounded-md border border-rule bg-panel shadow-[0_12px_32px_rgba(0,0,0,0.5)]";

function Row({ icon, children, hint, trailing }: { icon: ReactNode; children: ReactNode; hint?: string; trailing?: ReactNode }) {
  return (
    <>
      <span aria-hidden="true" className="flex w-4 shrink-0 justify-center">
        {icon}
      </span>
      <span className={cn("min-w-0 truncate", hint ? "shrink-0" : "flex-1")}>{children}</span>
      {hint && <span className="min-w-0 flex-1 truncate text-right font-mono text-xs whitespace-nowrap text-fg-3">{hint}</span>}
      {trailing}
    </>
  );
}

const Kbd = ({ children }: { children: ReactNode }) => (
  <span className="rounded-xs border border-control px-1.5 font-mono text-xs leading-4 text-fg-3">{children}</span>
);

/**
 * The profile button at the bottom of the sidebar and the menu that
 * opens upward from it (Radix menu: arrows, Enter, Esc, type-ahead and
 * Right/Left for the Theme submenu all work). `org` and `role` describe
 * the current organization; without them the org rows are left out.
 */
export function ProfileMenu({
  user,
  org,
  role,
  onSwitchOrg,
  onShortcuts,
}: {
  user: User;
  org: string | null;
  role: "admin" | "member" | null;
  onSwitchOrg: () => void;
  onShortcuts: () => void;
}) {
  const theme = useTheme();
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const name = user.name ?? user.login;
  const initial = (name.charAt(0) || "?").toUpperCase();
  const avatar = user.avatarUrl ? (
    <img src={user.avatarUrl} alt="" className="size-full rounded-[inherit]" />
  ) : (
    initial
  );

  const leave = async () => {
    setBusy(true);
    try {
      await signOut();
    } catch (err) {
      // Not signed in any more counts as signed out; any other failure keeps the session, so stay.
      if (!(err instanceof ApiError && err.status === 401)) {
        setBusy(false);
        return;
      }
    }
    window.location.assign("/signin");
  };
  const copy = () => {
    void navigator.clipboard?.writeText(CLI_INSTALL).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    }, () => undefined);
  };
  const icon = { size: 16, "aria-hidden": true } as const;

  return (
    <Menu.Root>
      <Menu.Trigger asChild>
        <button type="button" className={cn(NAV_ITEM, "h-12 gap-2.5 px-2")} aria-label={`Account menu for ${user.login}`}>
          <span className="flex size-7 flex-none items-center justify-center rounded-sm bg-raised font-semibold text-fg">{avatar}</span>
          <span className="flex min-w-0 flex-1 flex-col text-left leading-tight">
            <span className="truncate text-sm font-semibold text-fg">{name}</span>
            <span className="truncate font-mono text-xs font-normal text-fg-3">
              @{user.login}
              {role && ` · ${role}`}
            </span>
          </span>
          <ChevronsUpDownMini />
        </button>
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Content side="top" align="start" sideOffset={6} collisionPadding={8} aria-label="Account menu" className={cn(SURFACE, "z-50 w-[300px] max-w-[calc(100vw-16px)] outline-none")}>
          <div className="flex flex-col gap-2.5 border-b border-rule px-3.5 pt-3.5 pb-3">
            <div className="flex items-center gap-3">
              <span className="flex size-10 flex-none items-center justify-center rounded-md bg-raised text-lg font-semibold">{avatar}</span>
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold">{name}</div>
                <div className="truncate font-mono text-[13px] text-fg-3">@{user.login} · GitHub</div>
              </div>
            </div>
            {org && role && (
              <div className="flex items-center gap-2 text-[13px] text-fg-2">
                <span className="truncate">
                  {org} · Organization {role}
                </span>
                <span className="rounded-xs border border-control px-1 font-mono text-xs leading-4 font-semibold tracking-[0.04em] text-fg-2 uppercase">{role}</span>
              </div>
            )}
          </div>

          <Menu.Group className={GROUP}>
            <Menu.Item asChild className={ITEM}>
              <Link href="/profile">
                <Row icon={<UserRound {...icon} />}>Your profile</Row>
              </Link>
            </Menu.Item>
            <Menu.Item className={ITEM} onSelect={onShortcuts}>
              <Row icon={<Keyboard {...icon} />} trailing={<Kbd>?</Kbd>}>
                Keyboard shortcuts
              </Row>
            </Menu.Item>
          </Menu.Group>

          <Menu.Group className={GROUP}>
            <Menu.Sub>
              <Menu.SubTrigger className={cn(ITEM, "data-[state=open]:bg-raised")}>
                <Row
                  icon={<span className="size-4 rounded-full border border-current" style={{ background: "linear-gradient(90deg, transparent 50%, currentColor 50%)" }} />}
                  hint={THEMES.find((item) => item.id === theme)?.label}
                  trailing={<ChevronRight size={12} aria-hidden="true" className="shrink-0 text-fg-3" />}
                >
                  Theme
                </Row>
              </Menu.SubTrigger>
              <Menu.Portal>
                <Menu.SubContent sideOffset={6} alignOffset={-5} collisionPadding={8} aria-label="Theme" className={cn(SURFACE, "z-50 w-[200px] p-1 outline-none")}>
                  <Menu.RadioGroup value={theme} onValueChange={(id) => setTheme(id as typeof theme)}>
                    {THEMES.map((item) => (
                      <Menu.RadioItem key={item.id} value={item.id} className={ITEM}>
                        <span aria-hidden="true" className="size-4 flex-none rounded-sm border border-control" style={{ background: item.swatch }} />
                        <span className="flex-1">{item.label}</span>
                        <Menu.ItemIndicator className="w-3.5 text-right text-fg">
                          <Check size={14} aria-hidden="true" />
                        </Menu.ItemIndicator>
                      </Menu.RadioItem>
                    ))}
                  </Menu.RadioGroup>
                </Menu.SubContent>
              </Menu.Portal>
            </Menu.Sub>
          </Menu.Group>

          <Menu.Group className={GROUP}>
            {org && (
              <Menu.Item className={ITEM} onSelect={onSwitchOrg}>
                <Row icon={<ArrowLeftRight {...icon} />} hint={org} trailing={<Kbd>⌘O</Kbd>}>
                  Switch organization
                </Row>
              </Menu.Item>
            )}
            <div className="px-2.5 pt-1 pb-1.5">
              <Menu.Item
                className="flex h-9 cursor-pointer items-center gap-2 rounded-sm border border-control bg-background pl-2.5 outline-none data-[highlighted]:border-fg-3"
                onSelect={(event) => {
                  event.preventDefault();
                  copy();
                }}
                aria-label={copied ? "Copied" : "Copy install command"}
              >
                <Terminal size={14} aria-hidden="true" className="shrink-0 text-fg-3" />
                <span className="min-w-0 flex-1 truncate font-mono text-[13px] text-fg-2" title={CLI_INSTALL}>
                  {CLI_INSTALL}
                </span>
                <span className="p-2 text-fg-3">{copied ? <Check size={14} className="text-passed" /> : <Copy size={14} />}</span>
              </Menu.Item>
              <div className="meta px-0.5 pt-1">Install the CLI</div>
            </div>
          </Menu.Group>

          <Menu.Group className={GROUP}>
            <Menu.Item asChild className={ITEM}>
              <a href="https://status.hatchrun.io" target="_blank" rel="noreferrer">
                <Row icon={<span className="size-2 rounded-full bg-passed" />} hint="All systems normal" trailing={<ExternalLink size={11} aria-hidden="true" className="shrink-0" />}>
                  Status
                </Row>
              </a>
            </Menu.Item>
            <Menu.Item asChild className={ITEM}>
              <a href="https://hatchrun.io/changelog" target="_blank" rel="noreferrer">
                <Row icon={<Sparkles {...icon} />} trailing={<ExternalLink size={11} aria-hidden="true" className="shrink-0" />}>
                  Changelog
                </Row>
              </a>
            </Menu.Item>
            <Menu.Item asChild className={ITEM}>
              <a href="mailto:support@hatchrun.io">
                <Row icon={<CircleHelp {...icon} />}>Contact support</Row>
              </a>
            </Menu.Item>
          </Menu.Group>

          <Menu.Group className="p-1">
            <Menu.Item className={cn(ITEM, "data-[highlighted]:text-failed")} disabled={busy} onSelect={() => void leave()}>
              <Row icon={<LogOut {...icon} />}>Sign out</Row>
            </Menu.Item>
          </Menu.Group>
          <div className="border-t border-rule px-3.5 py-2 font-mono text-xs text-fg-3">↑↓ move · Enter select · Esc close</div>
        </Menu.Content>
      </Menu.Portal>
    </Menu.Root>
  );
}

function ChevronsUpDownMini() {
  return (
    <svg aria-hidden="true" width="12" height="12" viewBox="0 0 16 16" fill="currentColor" className="shrink-0 text-fg-3">
      <path d="M4.5 6 8 2.5 11.5 6zM4.5 10 8 13.5 11.5 10z" />
    </svg>
  );
}
