// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Small things the browser remembers: recent organizations and
// repositories, newest first. A failed read or write (private mode, a
// full quota) is ignored: the lists just stay empty.

const PREFIX = "hatchrun.";
const MAX = 8;

/** The recents under `name` ("orgs", or "repos:<org>"), newest first. */
export function recents(name: string): string[] {
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(PREFIX + name) ?? "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  } catch {
    return [];
  }
}

/** Moves `value` to the front of the recents under `name`. */
export function remember(name: string, value: string) {
  try {
    const next = [value, ...recents(name).filter((seen) => seen !== value)].slice(0, MAX);
    window.localStorage.setItem(PREFIX + name, JSON.stringify(next));
  } catch {
    // Not remembered.
  }
}

/** Orders `all` with the recents first, each once, then the rest as they come. Returns the recent count. */
export function recentFirst<T>(all: T[], key: (item: T) => string, recent: string[]): { items: T[]; recent: number } {
  const byKey = new Map(all.map((item) => [key(item), item]));
  const first = recent.flatMap((name) => {
    const found = byKey.get(name);
    return found === undefined ? [] : [found];
  });
  const seen = new Set(first.map(key));
  return { items: [...first, ...all.filter((item) => !seen.has(key(item)))], recent: first.length };
}

export function getItem(name: string): string | null {
  try {
    return window.localStorage.getItem(PREFIX + name);
  } catch {
    return null;
  }
}

export function setItem(name: string, value: string) {
  try {
    window.localStorage.setItem(PREFIX + name, value);
  } catch {
    // Not remembered.
  }
}
