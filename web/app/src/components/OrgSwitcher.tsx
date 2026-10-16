// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useRef, useState } from "react";

import { listTenants, type TenantSummary } from "../api";
import { Link } from "../router";

/** The current organization in the top bar, and the others to switch to. */
export function OrgSwitcher({ current }: { current: { login: string; avatarUrl: string } }) {
  const [open, setOpen] = useState(false);
  const [others, setOthers] = useState<TenantSummary[] | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) {
      return;
    }
    if (others === null) {
      listTenants().then(setOthers, () => setOthers([]));
    }
    const close = (event: MouseEvent | KeyboardEvent) => {
      if (event instanceof KeyboardEvent ? event.key === "Escape" : !ref.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", close);
    };
  }, [open, others]);

  return (
    <div className="menu" ref={ref}>
      <button
        type="button"
        className="switcher"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        {current.avatarUrl ? (
          <img className="avatar avatar-square" src={current.avatarUrl} alt="" width={22} height={22} />
        ) : (
          <span className="avatar avatar-square avatar-blank" />
        )}
        <span className="switcher-name">{current.login}</span>
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true" fill="currentColor">
          <path d="M4.5 6 8 2.5 11.5 6zM4.5 10 8 13.5 11.5 10z" />
        </svg>
      </button>
      {open && (
        <div className="menu-popover menu-popover-left" role="menu">
          {others === null ? (
            <p className="menu-item muted">Loading</p>
          ) : (
            others.map((tenant) => (
              <Link
                key={tenant.id}
                role="menuitem"
                className="menu-item menu-item-org"
                href={`/o/${tenant.login}`}
                onClick={() => setOpen(false)}
              >
                <img className="avatar avatar-square" src={tenant.avatarUrl} alt="" width={20} height={20} />
                {tenant.login}
              </Link>
            ))
          )}
          <Link role="menuitem" className="menu-item menu-item-new" href="/new" onClick={() => setOpen(false)}>
            Add an organization
          </Link>
        </div>
      )}
    </div>
  );
}
