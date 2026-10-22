// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A small router over the History API: the app has a handful of
// screens and needs nothing more.

import { useSyncExternalStore, type AnchorHTMLAttributes, type MouseEvent } from "react";

const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

const location = () => window.location.pathname + window.location.search;

/** The current path and query. */
export function useLocation(): string {
  return useSyncExternalStore(subscribe, location);
}

export function navigate(to: string, replace = false) {
  if (replace) {
    window.history.replaceState(null, "", to);
  } else {
    window.history.pushState(null, "", to);
  }
  window.scrollTo(0, 0);
  for (const listener of listeners) {
    listener();
  }
}

/**
 * Sets or, for null and "", drops query parameters of the current
 * location in place: no new history entry and no scroll, for a view's
 * filters as they change.
 */
export function setQuery(params: Record<string, string | null>) {
  const url = new URL(window.location.href);
  for (const [name, value] of Object.entries(params)) {
    if (value) {
      url.searchParams.set(name, value);
    } else {
      url.searchParams.delete(name);
    }
  }
  window.history.replaceState(null, "", url.pathname + url.search + url.hash);
  for (const listener of listeners) {
    listener();
  }
}

/** A query parameter of the current location. */
export function useQuery(name: string): string | null {
  const current = useLocation();
  return new URL(current, window.location.origin).searchParams.get(name);
}

export function Link({ href, onClick, ...rest }: AnchorHTMLAttributes<HTMLAnchorElement> & { href: string }) {
  const follow = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) {
      return;
    }
    event.preventDefault();
    navigate(href);
  };
  return <a href={href} onClick={follow} {...rest} />;
}

/** The parts of a `/ci/:owner/:repo[/runs/:run[/jobs/:job]]` path, null for any other. */
export interface CiRoute {
  owner: string;
  repo: string;
  run: string | null;
  job: string | null;
}

export function matchCi(path: string): CiRoute | null {
  const found = /^\/ci\/([A-Za-z0-9._-]+)\/([A-Za-z0-9._-]+)(?:\/runs\/(\d+)(?:\/jobs\/(\d+))?)?\/?$/.exec(path);
  return found?.[1] && found[2] ? { owner: found[1], repo: found[2], run: found[3] ?? null, job: found[4] ?? null } : null;
}
