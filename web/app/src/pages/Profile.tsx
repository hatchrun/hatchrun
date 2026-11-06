// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

import { ApiError, listTenants, signOut, type User } from "@/api";
import { Page } from "@/components/Page";
import { Await } from "@/components/Status";
import { orgUrl } from "@/lib/urls";
import { useLoad } from "@/load";
import { Link } from "@/router";
import { BTN } from "./formStyles";

/** Who is signed in: the GitHub identity of the session, the organizations on hatchrun, sign out. */
export function Profile({ user }: { user: User }) {
  const [tenants, reload] = useLoad(() => listTenants(), "tenants");
  const [busy, setBusy] = useState(false);
  const leave = async () => {
    setBusy(true);
    try {
      await signOut();
    } catch (err) {
      if (!(err instanceof ApiError && err.status === 401)) {
        setBusy(false);
        return;
      }
    }
    window.location.assign("/signin");
  };
  return (
    <Page title="Your profile" narrow>
      <section aria-label="GitHub identity" className="flex items-center gap-4 rounded-md border border-rule bg-panel px-4.5 py-4">
        <img src={user.avatarUrl} alt="" width={56} height={56} className="size-14 rounded-md bg-raised" />
        <div className="min-w-0">
          <div className="truncate text-lg font-semibold">{user.name ?? user.login}</div>
          <div className="truncate font-mono text-[13px] text-fg-2">@{user.login}</div>
          <a href={`https://github.com/${encodeURIComponent(user.login)}`} target="_blank" rel="noreferrer" className="font-mono text-xs text-fg-3 quiet-underline hover:text-fg">
            github.com/{user.login}
          </a>
        </div>
      </section>

      <section aria-label="Organizations" className="overflow-hidden rounded-md border border-rule">
        <div className="border-b border-rule bg-panel px-3.5 py-2.5">
          <h2 className="cap">Organizations</h2>
        </div>
        <Await loaded={tenants} retry={reload}>
          {(list) =>
            list.length === 0 ? (
              <p className="m-0 px-3.5 py-4 font-mono text-[13px] text-fg-3">You are not in an organization on hatchrun yet.</p>
            ) : (
              <ul className="m-0 list-none p-0">
                {list.map((tenant) => (
                  <li key={tenant.id} className="border-t border-rule first:border-t-0">
                    <Link href={orgUrl(tenant.login)} className="flex items-center gap-3 px-3.5 py-2.5 hover:bg-panel">
                      <img src={tenant.avatarUrl} alt="" width={24} height={24} className="size-6 rounded-sm bg-raised" />
                      <span className="font-mono text-[13px] font-bold">{tenant.login}</span>
                    </Link>
                  </li>
                ))}
              </ul>
            )
          }
        </Await>
      </section>

      <div>
        <button type="button" className={BTN} disabled={busy} onClick={() => void leave()}>
          Sign out
        </button>
      </div>
    </Page>
  );
}
