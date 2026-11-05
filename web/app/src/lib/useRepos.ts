// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState } from "react";

import { listOrgRepos, type CiOrgRepo } from "@/api";

/** `value`, settled for `ms` after the last change: for search boxes that call the API. */
export function useDebounced<T>(value: T, ms = 200): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(value), ms);
    return () => window.clearTimeout(timer);
  }, [value, ms]);
  return settled;
}

export interface RepoSearch {
  repos: CiOrgRepo[];
  /** How many match in all; more than `repos.length` when the list is cut. */
  total: number;
  loading: boolean;
  error: Error | null;
}

interface Answer {
  key: string;
  repos: CiOrgRepo[];
  total: number;
  error: Error | null;
}

/**
 * The org's repositories matching `query`, `limit` at most (last run
 * newest first), read from `GET /api/ci/orgs/{org}/repos?q=`. Calls are
 * debounced, and an answer for an old query never replaces a newer one.
 */
export function useRepoSearch(org: string, query: string, limit = 50, enabled = true): RepoSearch {
  const q = useDebounced(query.trim(), 180);
  const key = `${org}\u0000${q}\u0000${limit}`;
  const [answer, setAnswer] = useState<Answer | null>(null);

  useEffect(() => {
    if (!enabled) {
      return;
    }
    let current = true;
    listOrgRepos(org, { q, limit }).then(
      (page) => current && setAnswer({ key, repos: page.items, total: page.total, error: null }),
      (error: unknown) => current && setAnswer({ key, repos: [], total: 0, error: error instanceof Error ? error : new Error(String(error)) }),
    );
    return () => {
      current = false;
    };
  }, [org, q, limit, enabled, key]);

  const fresh = answer?.key === key;
  // While a new answer is on its way the last one stays on screen.
  return { repos: answer?.repos ?? [], total: answer?.total ?? 0, loading: enabled && !fresh, error: fresh ? answer.error : null };
}
