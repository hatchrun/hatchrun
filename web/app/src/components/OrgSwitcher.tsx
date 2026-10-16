// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useState } from "react";

import { listTenants, type TenantSummary } from "@/api";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { navigate } from "@/router";
import { UpDownIcon } from "./icons";

/** The current organization in the top bar, and the others to switch to. */
export function OrgSwitcher({ current }: { current: { login: string; avatarUrl: string } }) {
  const [others, setOthers] = useState<TenantSummary[] | null>(null);

  const opened = (open: boolean) => {
    if (open && others === null) {
      listTenants().then(setOthers, () => setOthers([]));
    }
  };

  return (
    <DropdownMenu onOpenChange={opened}>
      <DropdownMenuTrigger
        aria-label={`Switch organization, current ${current.login}`}
        className="-ml-2 flex cursor-pointer items-center gap-2 rounded-sm px-2 py-1.5 font-bold text-bone hover:bg-seam data-[state=open]:bg-seam"
      >
        <OrgAvatar login={current.login} avatarUrl={current.avatarUrl} />
        <span className="max-w-[12rem] truncate">{current.login}</span>
        <UpDownIcon className="text-fog" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-56">
        {others === null ? (
          <DropdownMenuItem disabled>Loading</DropdownMenuItem>
        ) : (
          others.map((tenant) => (
            <DropdownMenuItem key={tenant.id} onSelect={() => navigate(`/o/${tenant.login}`)}>
              <OrgAvatar login={tenant.login} avatarUrl={tenant.avatarUrl} />
              {tenant.login}
            </DropdownMenuItem>
          ))
        )}
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => navigate("/new")}>Add an organization</DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function OrgAvatar({ login, avatarUrl }: { login: string; avatarUrl: string }) {
  if (avatarUrl) {
    return <img src={avatarUrl} alt="" width={20} height={20} className="size-5 rounded-sm bg-housing" />;
  }
  return (
    <span
      aria-hidden="true"
      className="flex size-5 items-center justify-center rounded-sm bg-bone text-[11px] font-black text-graphite uppercase"
    >
      {login.charAt(0)}
    </span>
  );
}
