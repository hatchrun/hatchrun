// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { cn } from "@/lib/utils";
import { orgUrl } from "@/lib/urls";
import { Link } from "@/router";

export type SettingsPage = "billing" | "setup" | "audit";

/** The sub-navigation of /o/:org/settings/*: Billing, Setup and, for admins, the Audit log. */
export function SettingsNav({ org, current, admin }: { org: string; current: SettingsPage; admin: boolean }) {
  const items: [SettingsPage, string][] = [
    ["billing", "Billing"],
    ["setup", "Setup"],
    ...(admin ? ([["audit", "Audit log"]] as [SettingsPage, string][]) : []),
  ];
  return (
    <nav aria-label="Settings" className="flex gap-4 border-b border-rule font-mono text-[13px]">
      {items.map(([id, label]) => (
        <Link
          key={id}
          href={orgUrl(org, `settings/${id}`)}
          aria-current={current === id ? "page" : undefined}
          className={cn("-mb-px border-b-2 py-1.5", current === id ? "border-brand text-fg" : "border-transparent text-fg-3 hover:text-fg")}
        >
          {label}
        </Link>
      ))}
    </nav>
  );
}
