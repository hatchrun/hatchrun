// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect, useRef, useState, type ReactNode } from "react";

import { signOut, type User } from "../api";
import { Link } from "../router";
import { Logo } from "./Logo";

export function Shell({ user, nav, children }: { user: User; nav?: ReactNode; children: ReactNode }) {
  return (
    <div className="shell">
      <header className="topbar">
        <Link href="/" className="brand" aria-label="hatchrun home">
          <Logo size={24} />
          <span>hatchrun</span>
        </Link>
        {nav && <span className="topbar-sep" aria-hidden="true">/</span>}
        <div className="topbar-nav">{nav}</div>
        <a className="topbar-link" href="https://hatchrun.io/docs/overview/">
          Docs
        </a>
        <UserMenu user={user} />
      </header>
      <main className="page">{children}</main>
    </div>
  );
}

function UserMenu({ user }: { user: User }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) {
      return;
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
  }, [open]);

  const leave = async () => {
    setBusy(true);
    try {
      await signOut();
    } finally {
      window.location.assign("/signin");
    }
  };

  return (
    <div className="menu" ref={ref}>
      <button
        type="button"
        className="avatar-button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Account menu for ${user.login}`}
        onClick={() => setOpen(!open)}
      >
        <img className="avatar" src={user.avatarUrl} alt="" width={28} height={28} />
      </button>
      {open && (
        <div className="menu-popover" role="menu">
          <div className="menu-header">
            <strong>{user.name ?? user.login}</strong>
            <span className="muted">@{user.login}</span>
          </div>
          <button type="button" role="menuitem" className="menu-item" disabled={busy} onClick={leave}>
            Sign out
          </button>
        </div>
      )}
    </div>
  );
}
