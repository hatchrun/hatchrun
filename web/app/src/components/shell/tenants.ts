// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useCallback, useEffect, useState } from "react";

import { listTenants, type TenantSummary } from "@/api";

let cached: Promise<TenantSummary[]> | null = null;

/**
 * The organizations the viewer can open, read once per page load.
 * `tenants` is null while loading or after a failure; `error` tells
 * a failed read from an empty list, and `retry` reads again.
 */
export function useTenants(): { tenants: TenantSummary[] | null; error: Error | null; retry: () => void } {
  const [state, setState] = useState<{ tenants: TenantSummary[] | null; error: Error | null }>({ tenants: null, error: null });
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    cached ??= listTenants().catch((error: unknown) => {
      cached = null;
      throw error;
    });
    let current = true;
    cached.then(
      (tenants) => current && setState({ tenants, error: null }),
      (error: unknown) => current && setState({ tenants: null, error: error instanceof Error ? error : new Error(String(error)) }),
    );
    return () => {
      current = false;
    };
  }, [attempt]);
  const retry = useCallback(() => {
    cached = null;
    setState({ tenants: null, error: null });
    setAttempt((n) => n + 1);
  }, []);
  return { ...state, retry };
}
