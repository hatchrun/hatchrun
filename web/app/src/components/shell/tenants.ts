// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState } from "react";

import { listTenants, type TenantSummary } from "@/api";

let cached: Promise<TenantSummary[]> | null = null;

/** The organizations the viewer can open, read once per page load. `null` while loading. */
export function useTenants(): TenantSummary[] | null {
  const [tenants, setTenants] = useState<TenantSummary[] | null>(null);
  useEffect(() => {
    cached ??= listTenants().catch(() => {
      cached = null;
      return [];
    });
    let current = true;
    void cached.then((list) => current && setTenants(list));
    return () => {
      current = false;
    };
  }, []);
  return tenants;
}
