// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// Calls to the Worker's /api. Every request that changes something
// carries the session's CSRF token, which /api/session hands out.

export interface User {
  id: number;
  login: string;
  name: string | null;
  avatarUrl: string;
}

export interface Session {
  user: User;
  csrfToken: string;
}

export interface OrgChoice {
  id: number;
  login: string;
  avatarUrl: string;
  /** The tenant's id when the organization is already set up. */
  tenant: string | null;
}

export interface TenantSummary {
  id: string;
  login: string;
  avatarUrl: string;
}

export interface Aws {
  roleArn: string;
  region: string;
  launchTemplateId: string;
  subnetIds: string[];
}

export interface Billing {
  beta: boolean;
  licensed: boolean;
  subscription: null | {
    status: string;
    currentPeriodEnd: string | null;
    cancelAtPeriodEnd: boolean;
  };
}

export interface Plan {
  name: string;
  description: string | null;
  /** Smallest currency unit, such as cents. */
  amount: number | null;
  currency: string | null;
  interval: string | null;
}

export interface Steps {
  organization: boolean;
  billing: boolean;
  aws: boolean;
  app: boolean;
}

export interface Tenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  aws: Aws | null;
  maxRunners: number;
  billing: Billing;
  steps: Steps;
  installUrl: string;
}

export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message.charAt(0).toUpperCase() + message.slice(1));
    this.status = status;
  }
}

let csrfToken = "";

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method !== "GET") {
    headers["X-CSRF-Token"] = csrfToken;
  }
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const answer = (await response.json()) as { error?: string };
      if (answer.error) {
        message = answer.error;
      }
    } catch {
      // Not JSON: keep the status line.
    }
    throw new ApiError(response.status, message);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

export const get = <T>(path: string) => request<T>("GET", path);
export const post = <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {});
export const put = <T>(path: string, body?: unknown) => request<T>("PUT", path, body ?? {});

/** The signed-in session, or null when nobody is signed in. */
export async function getSession(): Promise<Session | null> {
  try {
    const session = await get<Session>("/api/session");
    csrfToken = session.csrfToken;
    return session;
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) {
      return null;
    }
    throw err;
  }
}

export function signInUrl(returnTo: string): string {
  return `/api/auth/login?return_to=${encodeURIComponent(returnTo)}`;
}

export const signOut = () => post<undefined>("/api/auth/logout");

export const listOrgs = () => get<OrgChoice[]>("/api/orgs");
export const listTenants = () => get<TenantSummary[]>("/api/tenants");
export const getTenant = (org: string) => get<Tenant>(`/api/tenants/${encodeURIComponent(org)}`);
export const createTenant = (org: string) => post<Tenant>("/api/tenants", { org });

const tenantPath = (org: string) => `/api/tenants/${encodeURIComponent(org)}`;
export const getPlan = () => get<Plan>("/api/plan");
export const startCheckout = (org: string) => post<{ url: string }>(`${tenantPath(org)}/checkout`);
export const openPortal = (org: string) => post<{ url: string }>(`${tenantPath(org)}/portal`);
export const redeemInvite = (org: string, code: string) => post<Tenant>(`${tenantPath(org)}/beta`, { code });
